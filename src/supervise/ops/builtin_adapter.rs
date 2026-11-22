//! 内置 adapter：\`tag=builtin\`，把 svcd 自己的 \`svc\` 服务放进契约。
//!
//! 它不是进程：没有 pid、管道或退出码。\`Ensure\` / \`Quiesce\` 只改变一个进程内的
//! 可用位；\`call\` 直接返回结果。这样 \`svc\` 也能和插件一样被 \`Instance\` 调用，
//! 而不需要一条特殊路径。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::kernel::contract::{
    Contract, Control, ImplRef, Invocation, Launcher, Reply, ReplySink, Settled, Wire,
};
use crate::kernel::failure::{Failure, Rejection};

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

    fn value(&self, capability: &str) -> Result<Value, Failure> {
        match capability {
            "version" => Ok(json!({ "version": env!("CARGO_PKG_VERSION") })),
            "health" => Ok(json!({ "state": "ready" })),
            // The shared conformance fixture contract declares `status`.
            "status" => Ok(json!({ "ok": true })),
            _ => Err(Failure::new(
                crate::kernel::failure::Class::ImplementationError,
                crate::kernel::failure::Retry::Never,
                format!("builtin has no implementation for {capability}"),
            )),
        }
    }
}

impl Wire for BuiltinWire {
    fn call(&mut self, invocation: Invocation, done: ReplySink) -> Result<(), Rejection> {
        if let Some(class) = self.forced_failure {
            done(Err(Failure::new(
                class,
                class.default_retry(),
                "forced failure",
            )));
            return Ok(());
        }
        if self.contract.capability(&invocation.capability).is_none() {
            done(Err(Failure::new(
                crate::kernel::failure::Class::ImplementationError,
                crate::kernel::failure::Retry::Never,
                format!(
                    "builtin has no capability {}",
                    invocation.capability.as_str()
                ),
            )));
            return Ok(());
        }
        if invocation.cancel.is_cancelled() {
            done(Err(Failure::new(
                crate::kernel::failure::Class::Cancelled,
                crate::kernel::failure::Retry::Never,
                "call was cancelled before dispatch",
            )));
            return Ok(());
        }
        if !self.running.load(Ordering::SeqCst) {
            done(Err(Failure::new(
                crate::kernel::failure::Class::NotRunning,
                crate::kernel::failure::Retry::After(std::time::Duration::from_secs(1)),
                "builtin service is stopped",
            )));
            return Ok(());
        }
        let value = self.value(invocation.capability.as_str());
        done(value.map(Reply::Value));
        Ok(())
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
    use crate::kernel::contract::{
        Capability, CapabilityName, Control, Effect, Instance, Limits, Obligations, ServiceId,
    };
    use crate::kernel::failure::Class;

    fn contract() -> Contract {
        sample_contract(Limits::default())
    }

    fn normal() -> Instance {
        Instance::new(contract(), Box::new(BuiltinWire::new(contract()))).expect("valid contract")
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
        match launchers.launch(
            contract(),
            &ImplRef {
                tag: "builtin".into(),
                binding: json!({ "service": "svc" }),
            },
        ) {
            Ok(_) => {}
            Err(error) => panic!("the builtin tag must launch: {error:?}"),
        }
    }

    #[test]
    fn the_builtin_reports_version_and_health_by_capability_name() {
        let capability = |name: &str| Capability {
            name: CapabilityName::new(name),
            desc: String::new(),
            input_schema: None,
            output_schema: None,
            effect: Effect::Read,
            idempotent: true,
        };
        let contract = Contract::new(
            ServiceId::new("svc"),
            "1.0.0",
            vec![capability("version"), capability("health")],
            Obligations::default(),
            Class::ALL.to_vec(),
            Limits::default(),
        );
        let mut instance = Instance::new(contract.clone(), Box::new(BuiltinWire::new(contract)))
            .expect("valid contract");

        match instance
            .call_blocking(Invocation::new(
                "version",
                json!({}),
                std::time::Duration::from_secs(1),
            ))
            .expect("version call")
        {
            Reply::Value(value) => assert_eq!(value["version"], env!("CARGO_PKG_VERSION")),
            other => panic!("expected a value reply, got {other:?}"),
        }
        match instance
            .call_blocking(Invocation::new(
                "health",
                json!({}),
                std::time::Duration::from_secs(1),
            ))
            .expect("health call")
        {
            Reply::Value(value) => assert_eq!(value["state"], "ready"),
            other => panic!("expected a value reply, got {other:?}"),
        }
    }

    #[test]
    fn a_declared_but_unimplemented_capability_is_an_implementation_error() {
        let capability = Capability {
            name: CapabilityName::new("missing"),
            desc: String::new(),
            input_schema: None,
            output_schema: None,
            effect: Effect::Read,
            idempotent: true,
        };
        let contract = Contract::new(
            ServiceId::new("svc"),
            "1.0.0",
            vec![capability],
            Obligations::default(),
            Class::ALL.to_vec(),
            Limits::default(),
        );
        let mut instance = Instance::new(contract.clone(), Box::new(BuiltinWire::new(contract)))
            .expect("valid contract");
        match instance.call_blocking(Invocation::new(
            "missing",
            json!({}),
            std::time::Duration::from_secs(1),
        )) {
            Err(crate::kernel::failure::CallError::Failed(failure)) => {
                assert_eq!(failure.class, Class::ImplementationError)
            }
            other => panic!("expected a classified failure, got {other:?}"),
        }
    }

    #[test]
    fn the_builtin_reports_version_and_stops() {
        let mut instance = normal();
        let version = instance
            .call_blocking(Invocation::new(
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
