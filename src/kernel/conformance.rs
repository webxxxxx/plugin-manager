//! 一致性断言：**任何** adapter 都要过同一套检查。
//!
//! 接口是三个纯函数：`check` 返回违规清单（空 = 通过），`assert_conforming` 是
//! 给它包一层 panic 的便利函数。返回清单而不是直接 panic，是为了让「故意说谎的
//! adapter 必须被抓到」这件事本身可以被测试，而不是靠 `catch_unwind`。
//!
//! `make` 造正常实例，`make_failing` 按类别造「返回该类失败」的实例；后者让
//! 「8 类失败可表达」成为可断言的事实，而不是样例契约里的一个列表。

#![cfg(test)]

use std::time::Duration;

use serde_json::{json, Value};

use super::contract::{
    inline_len, Capability, CapabilityName, Contract, Control, Effect, Health, ImplRef, Instance,
    Invocation, Launchers, Limits, Obligations, ServiceId, Settled,
};
use super::failure::{CallError, Class, Failure, Rejection};

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
pub fn check(
    contract: &Contract,
    make: impl Fn() -> Instance,
    make_failing: impl Fn(Class) -> Instance,
) -> Vec<String> {
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

    // 2. 契约声明的每个失败类别都必须能被表达，且不能被内核归一掉。
    for class in Class::ALL {
        if !contract.failures.contains(&class) {
            continue;
        }
        let mut instance = make_failing(class);
        match instance.call(Invocation::new(
            first.clone(),
            json!({}),
            Duration::from_secs(1),
        )) {
            Err(CallError::Failed(failure)) if failure.class == class => {}
            other => violations.push(format!(
                "declared failure class {} was not expressible: {other:?}",
                class.as_str()
            )),
        }
    }

    // 3. 内联上限：恰好等于上限通过，多一字节被拒。
    {
        let max = contract.limits.max_inline;
        let exact = Value::String("x".repeat(max.saturating_sub(2)));
        let exact_len = inline_len(&exact);
        if exact_len != max {
            violations.push(format!(
                "exact boundary fixture is {exact_len} bytes, want {max}"
            ));
        }
        let mut instance = make();
        match instance.call(Invocation::new(
            first.clone(),
            exact,
            Duration::from_secs(1),
        )) {
            Ok(_) => {}
            other => violations.push(format!(
                "arguments exactly at max_inline must reach the implementation: {other:?}"
            )),
        }

        let over = Value::String("x".repeat(max.saturating_sub(1)));
        let mut instance = make();
        match instance.call(Invocation::new(first.clone(), over, Duration::from_secs(1))) {
            Err(CallError::Rejected(Rejection::BadArguments { .. })) => {}
            other => violations.push(format!(
                "arguments one byte over max_inline must be rejected: {other:?}"
            )),
        }
    }

    // 4. 取消后不得返回 Ok，且类别必须是 Cancelled。
    {
        let mut instance = make();
        let invocation = Invocation::new(first.clone(), json!({}), Duration::from_secs(1));
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

    // 5. 生命周期：Ensure 之后 Probe 不得是 Stopped；Quiesce 之后必须是 Stopped。
    {
        let mut instance = make();
        if let Err(error) = instance.lifecycle(Control::Ensure) {
            violations.push(format!("Ensure failed: {error:?}"));
        }
        match instance.lifecycle(Control::Probe) {
            Ok(settled) => {
                let health = Health::from_settled(settled);
                if !health.is_alive() {
                    violations.push("after Ensure, Probe must not be Stopped".into());
                }
            }
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

    // 6. 能力面稳定：契约校验之后，排序与唯一性必须仍然成立。
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

pub fn assert_conforming(
    contract: &Contract,
    make: impl Fn() -> Instance,
    make_failing: impl Fn(Class) -> Instance,
) {
    let violations = check(contract, make, make_failing);
    assert!(
        violations.is_empty(),
        "adapter is not conforming: {violations:#?}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::failure::Retry;
    use crate::kernel::memory::{MemoryLauncher, MemoryWire};

    fn sample() -> Contract {
        sample_contract(Limits::default())
    }

    fn normal() -> Instance {
        MemoryWire::new().instance(sample())
    }

    fn failing(class: Class) -> Instance {
        MemoryWire::new()
            .with_failure(
                "status",
                Failure::new(class, Retry::Never, "injected failure"),
            )
            .instance(sample())
    }

    #[test]
    fn the_memory_adapter_conforms() {
        let contract = sample();
        assert_conforming(&contract, normal, failing);
    }

    #[test]
    fn an_adapter_that_ignores_cancel_is_caught() {
        let contract = sample();
        let violations = check(
            &contract,
            || MemoryWire::new().ignoring_cancel().instance(sample()),
            |class| {
                MemoryWire::new()
                    .ignoring_cancel()
                    .with_failure(
                        "status",
                        Failure::new(class, Retry::Never, "injected failure"),
                    )
                    .instance(sample())
            },
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("cancel")),
            "an adapter that returns Ok after cancel must be caught: {violations:#?}"
        );
    }

    #[test]
    fn an_adapter_that_never_stops_is_caught() {
        let contract = sample();
        let violations = check(
            &contract,
            || MemoryWire::new().that_never_stops().instance(sample()),
            |class| {
                MemoryWire::new()
                    .that_never_stops()
                    .with_failure(
                        "status",
                        Failure::new(class, Retry::Never, "injected failure"),
                    )
                    .instance(sample())
            },
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("Quiesce")),
            "an adapter whose Quiesce does nothing must be caught: {violations:#?}"
        );
    }

    #[test]
    fn an_adapter_that_swallows_a_declared_failure_is_caught() {
        let contract = sample();
        let violations = check(&contract, normal, |_class| normal());
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("failure class")),
            "an adapter that returns Ok for a declared failure must be caught: {violations:#?}"
        );
    }

    #[test]
    fn the_memory_adapter_reports_live_ready_degraded_and_stopped() {
        let mut live = MemoryWire::new().live_before_ready().instance(sample());
        assert_eq!(live.lifecycle(Control::Ensure), Ok(Settled::Live));
        assert_eq!(live.lifecycle(Control::Probe), Ok(Settled::Live));
        assert!(Health::from_settled(Settled::Live).is_alive());
        assert!(!Health::from_settled(Settled::Live).is_usable());

        let mut ready = MemoryWire::new().instance(sample());
        assert_eq!(ready.lifecycle(Control::Ensure), Ok(Settled::Ready));
        assert_eq!(ready.lifecycle(Control::Probe), Ok(Settled::Ready));
        assert!(Health::from_settled(Settled::Ready).is_usable());

        let mut degraded = MemoryWire::new().degraded("disk").instance(sample());
        assert_eq!(
            degraded.lifecycle(Control::Ensure),
            Ok(Settled::Degraded("disk".into()))
        );
        assert_eq!(
            degraded.lifecycle(Control::Probe),
            Ok(Settled::Degraded("disk".into()))
        );
        assert!(Health::from_settled(Settled::Degraded("disk".into())).is_usable());

        let mut stopped = MemoryWire::new().instance(sample());
        assert_eq!(stopped.lifecycle(Control::Ensure), Ok(Settled::Ready));
        assert_eq!(stopped.lifecycle(Control::Quiesce), Ok(Settled::Stopped));
        assert_eq!(stopped.lifecycle(Control::Probe), Ok(Settled::Stopped));
        assert!(!Health::from_settled(Settled::Stopped).is_alive());
    }

    #[test]
    fn the_live_and_degraded_adapters_conform() {
        let live = sample();
        assert_conforming(
            &live,
            || MemoryWire::new().live_before_ready().instance(sample()),
            |class| {
                MemoryWire::new()
                    .live_before_ready()
                    .with_failure(
                        "status",
                        Failure::new(class, Retry::Never, "injected failure"),
                    )
                    .instance(sample())
            },
        );

        let degraded = sample();
        assert_conforming(
            &degraded,
            || MemoryWire::new().degraded("disk").instance(sample()),
            |class| {
                MemoryWire::new()
                    .degraded("disk")
                    .with_failure(
                        "status",
                        Failure::new(class, Retry::Never, "injected failure"),
                    )
                    .instance(sample())
            },
        );
    }

    #[test]
    fn the_memory_tag_is_registered_at_the_test_assembly_point() {
        let mut launchers = Launchers::default();
        launchers.register("memory", Box::new(MemoryLauncher));
        let instance = launchers.launch(
            sample(),
            &ImplRef {
                tag: "memory".into(),
                binding: json!({}),
            },
        );
        match instance {
            Ok(_) => {}
            Err(error) => panic!("the memory tag must launch: {error:?}"),
        }
    }
}
