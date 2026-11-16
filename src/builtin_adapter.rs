//! 内置 adapter：\`tag=builtin\`，把 svcd 自己的 \`svc\` 服务放进契约。
//!
//! 它不是进程：没有 pid、管道或退出码。\`Ensure\` / \`Quiesce\` 只改变一个进程内的
//! 可用位；\`call\` 直接返回结果。这样 \`svc\` 也能和插件一样被 \`Instance\` 调用，
//! 而不需要一条特殊路径。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::kernel::contract::{
    Control, Contract, ImplRef, Invocation, Launcher, Reply, Settled, Wire,
};
use crate::kernel::failure::Failure;

/// 内置 adapter 的装配入口：\`tag=builtin\`。
pub struct BuiltinLauncher;

impl Launcher for BuiltinLauncher {
    fn launch(&self, contract: &Contract, _spec: &ImplRef) -> Result<Box<dyn Wire>, Failure> {
        Ok(Box::new(BuiltinWire::new(contract.clone())))
    }
}

/// 进程内的 \`svc\`。\`call\` 的能力面由契约声明；本轮只做最小的版本/健康能力，
/// 具体投影仍由调用方决定。
pub struct BuiltinWire {
    contract: Contract,
    running: Arc<AtomicBool>,
    forced_failure: Option<crate::kernel::failure::Class>,
}

impl BuiltinWire {
    pub fn new(contract: Contract) -> BuiltinWire {
        BuiltinWire {
            contract,
            running: Arc::new(AtomicBool::new(true)),
            forced_failure: None,
        }
    }

    #[cfg(test)]
    pub fn forced_failure(mut self, class: crate::kernel::failure::Class) -> BuiltinWire {
        self.forced_failure = Some(class);
        self
    }

    fn value(&self, capability: &str) -> Value {
        match capability {
            "svc.version" => json!({ "version": env!("CARGO_PKG_VERSION") }),
            "svc.describe" => json!({
                "id": "svc",
                "version": env!("CARGO_PKG_VERSION"),
                "kind": "core",
            }),
            "svc.health" => json!({ "state": "ready" }),
            _ => json!({ "ok": true }),
        }
    }
}

impl Wire for BuiltinWire {
    fn call(&mut self, invocation: Invocation) -> Result<Reply, Failure> {
        if let Some(class) = self.forced_failure {
            return Err(Failure::new(class, class.default_retry(), "forced failure"));
        }
        if self.contract.capability(&invocation.capability).is_none() {
            return Err(Failure::new(
                crate::kernel::failure::Class::ImplementationError,
                crate::kernel::failure::Retry::Never,
                format!(
                    "builtin has no capability {}",
                    invocation.capability.as_str()
                ),
            ));
        }
        if invocation.cancel.is_cancelled() {
            return Err(Failure::new(
                crate::kernel::failure::Class::Cancelled,
                crate::kernel::failure::Retry::Never,
                "call was cancelled before dispatch",
            ));
        }
        if !self.running.load(Ordering::SeqCst) {
            return Err(Failure::new(
                crate::kernel::failure::Class::NotRunning,
                crate::kernel::failure::Retry::After(std::time::Duration::from_secs(1)),
                "builtin service is stopped",
            ));
        }
        Ok(Reply::Value(self.value(invocation.capability.as_str())))
    }

    fn lifecycle(&mut self, control: Control) -> Result<Settled, Failure> {
        match control {
            Control::Ensure => {
                self.running.store(true, Ordering::SeqCst);
                Ok(Settled::Ready)
            }
            Control::Quiesce => {
                self.running.store(false, Ordering::SeqCst);
                Ok(Settled::Stopped)
            }
            Control::Probe => Ok(if self.running.load(Ordering::SeqCst) {
                Settled::Ready
            } else {
                Settled::Stopped
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::conformance::{assert_conforming, sample_contract};
    use crate::kernel::contract::{Control, Instance, Limits};
    use crate::kernel::failure::Class;

    fn contract() -> Contract {
        sample_contract(Limits::default())
    }

    fn normal() -> Instance {
        Instance::new(
            contract(),
            Box::new(BuiltinWire::new(contract())),
        )
        .expect("valid contract")
    }

    fn failing(class: Class) -> Instance {
        Instance::new(
            contract(),
            Box::new(BuiltinWire::new(contract()).forced_failure(class)),
        )
        .expect("valid contract")
    }

    #[test]
    fn the_builtin_adapter_conforms() {
        assert_conforming(&contract(), normal, failing);
    }

    #[test]
    fn the_svc_tag_is_registered_at_the_test_assembly_point() {
        let mut launchers = crate::kernel::contract::Launchers::default();
        launchers.register("builtin", Box::new(BuiltinLauncher));
        match launchers.launch(contract(), &ImplRef {
            tag: "builtin".into(),
            binding: json!({ "service": "svc" }),
        }) {
            Ok(_) => {}
            Err(error) => panic!("the builtin tag must launch: {error:?}"),
        }
    }

    #[test]
    fn the_builtin_reports_version_and_stops() {
        let mut instance = normal();
        let version = instance
            .call(Invocation::new(
                "status",
                json!({}),
                std::time::Duration::from_secs(1),
            ))
            .expect("call");
        match version {
            Reply::Value(value) => assert_eq!(value["ok"], true),
            other => panic!("expected a value reply, got {other:?}"),
        }

        assert_eq!(instance.lifecycle(Control::Quiesce), Ok(Settled::Stopped));
        assert_eq!(instance.lifecycle(Control::Probe), Ok(Settled::Stopped));
    }
}
