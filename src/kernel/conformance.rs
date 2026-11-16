//! 一致性断言：**任何** adapter 都要过同一套检查。
//!
//! 接口是两个纯函数：`check` 返回违规清单（空 = 通过），`assert_conforming` 是
//! 给它包一层 panic 的便利函数。返回清单而不是直接 panic，是为了让"故意说谎的
//! adapter 必须被抓到"这件事本身可以被测试，而不是靠 `catch_unwind`。

#![cfg(test)]

use std::time::Duration;

use serde_json::json;

use super::contract::{
    Capability, CapabilityName, Contract, Control, Effect, Instance, Invocation, Limits,
    Obligations, ServiceId, Settled,
};
use super::failure::{CallError, Class, Rejection};

/// 给 conformance 用的样例契约：一个只读且幂等的能力。
pub fn sample_contract(limits: Limits) -> Contract {
    Contract::new(
        ServiceId::new("conformance"),
        "1.0.0",
        vec![Capability {
            name: CapabilityName::new("status"),
            desc: "conformance sample".into(),
            input_schema: None,
            effect: Effect::Read,
            idempotent: true,
        }],
        Obligations::default(),
        Class::ALL.to_vec(),
        limits,
    )
}

/// 逐项检查一个 adapter。每次检查都用 `make` 新建实例，避免状态互相污染。
pub fn check(contract: &Contract, make: impl Fn() -> Instance) -> Vec<String> {
    let mut violations = Vec::new();

    if let Err(error) = contract.validate() {
        violations.push(format!("contract is invalid: {error}"));
    }
    let Some(first) = contract
        .surface()
        .first()
        .map(|capability| capability.name.as_str().to_string())
    else {
        violations.push("contract has no capability to exercise".into());
        return violations;
    };

    // 1. 未知能力必须在进实现之前被拒。
    {
        let mut instance = make();
        match instance.call(Invocation::new(
            "__no_such_capability",
            json!({}),
            Duration::from_secs(1),
        )) {
            Err(CallError::Rejected(Rejection::UnknownCapability { .. })) => {}
            other => violations.push(format!(
                "unknown capability was not rejected by the kernel: {other:?}"
            )),
        }
    }

    // 2. 内联上限：恰好等于上限通过，超一字节被拒。
    {
        let max = contract.limits.max_inline;
        let mut instance = make();
        let oversized = json!({ "pad": "x".repeat(max.saturating_add(1)) });
        match instance.call(Invocation::new(
            first.clone(),
            oversized,
            Duration::from_secs(1),
        )) {
            Err(CallError::Rejected(Rejection::BadArguments { .. })) => {}
            other => violations.push(format!(
                "oversized inline arguments were not rejected: {other:?}"
            )),
        }
    }

    // 3. 取消后不得返回 Ok，且类别必须是 Cancelled。
    {
        let mut instance = make();
        let mut invocation = Invocation::new(first.clone(), json!({}), Duration::from_secs(1));
        invocation.cancel.cancel();
        match instance.call(invocation) {
            Err(CallError::Failed(failure)) if failure.class == Class::Cancelled => {}
            Err(CallError::Rejected(rejection)) => violations.push(format!(
                "a cancelled call was rejected instead of failed: {:?}",
                rejection
            )),
            other => violations.push(format!(
                "cancel: must not return Ok and must be Cancelled, got {other:?}"
            )),
        }
    }

    // 4. 生命周期：Ensure 之后 Probe 不得是 Stopped；Quiesce 之后必须是 Stopped。
    {
        let mut instance = make();
        if let Err(error) = instance.lifecycle(Control::Ensure) {
            violations.push(format!("Ensure failed: {error:?}"));
        }
        match instance.lifecycle(Control::Probe) {
            Ok(Settled::Stopped) => {
                violations.push("after Ensure, Probe must not be Stopped".into())
            }
            Ok(_) => {}
            Err(error) => violations.push(format!("Probe after Ensure failed: {error:?}")),
        }
        if let Err(error) = instance.lifecycle(Control::Quiesce) {
            violations.push(format!("Quiesce failed: {error:?}"));
        }
        match instance.lifecycle(Control::Probe) {
            Ok(Settled::Stopped) => {}
            Ok(other) => violations.push(format!(
                "after Quiesce, Probe must be Stopped, got {other:?}"
            )),
            Err(error) => violations.push(format!("Probe after Quiesce failed: {error:?}")),
        }
    }

    // 5. 能力面稳定：契约校验之后，排序与唯一性必须仍然成立。
    {
        let names: Vec<&str> = contract
            .surface()
            .iter()
            .map(|capability| capability.name.as_str())
            .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        if sorted != names {
            violations.push("surface is not sorted and unique".into());
        }
    }

    violations
}

pub fn assert_conforming(contract: &Contract, make: impl Fn() -> Instance) {
    let violations = check(contract, make);
    assert!(violations.is_empty(), "adapter is not conforming: {violations:#?}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::memory::MemoryWire;

    fn limits() -> Limits {
        Limits {
            // 小上限让"超一字节"这条检查跑得便宜。
            max_inline: 64,
            ..Limits::default()
        }
    }

    #[test]
    fn the_memory_adapter_conforms() {
        let contract = sample_contract(limits());
        assert_conforming(&contract, || {
            MemoryWire::new().instance(sample_contract(limits()))
        });
    }

    #[test]
    fn an_adapter_that_ignores_cancel_is_caught() {
        let contract = sample_contract(limits());
        let violations = check(&contract, || {
            MemoryWire::new()
                .ignoring_cancel()
                .instance(sample_contract(limits()))
        });
        assert!(
            violations.iter().any(|violation| violation.contains("cancel")),
            "an adapter that returns Ok after cancel must be caught: {violations:#?}"
        );
    }

    #[test]
    fn an_adapter_that_never_stops_is_caught() {
        let contract = sample_contract(limits());
        let violations = check(&contract, || {
            MemoryWire::new()
                .that_never_stops()
                .instance(sample_contract(limits()))
        });
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("Quiesce")),
            "an adapter whose Quiesce does nothing must be caught: {violations:#?}"
        );
    }
}
