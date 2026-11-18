//! 纯内存的 [`Wire`]：给 conformance 与后续编排单测用，不需要起进程。
//!
//! 三种变体用来证明断言不是空跑：
//! - 默认：遵守取消、能停能起；
//! - [`MemoryWire::ignoring_cancel`]：取消后仍返回 `Ok`（必须被 conformance 抓到）；
//! - [`MemoryWire::that_never_stops`]：`Quiesce` 之后仍报 `Ready`（同样必须被抓到）。

#![cfg(test)]

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;

use super::contract::{
    Contract, Control, ImplRef, Instance, Invocation, Launcher, Reply, ReplySink, Settled, Wire,
};
use super::failure::{Class, Failure, Rejection, Retry};

pub struct MemoryWire {
    replies: HashMap<String, Result<Reply, Failure>>,
    running: bool,
    degraded: Option<String>,
    live: bool,
    honor_cancel: bool,
    stop_works: bool,
}

impl MemoryWire {
    pub fn new() -> MemoryWire {
        MemoryWire {
            replies: HashMap::new(),
            running: true,
            degraded: None,
            live: false,
            honor_cancel: true,
            stop_works: true,
        }
    }

    pub fn with_failure(mut self, capability: &str, failure: Failure) -> MemoryWire {
        self.replies.insert(capability.to_string(), Err(failure));
        self
    }

    pub fn ignoring_cancel(mut self) -> MemoryWire {
        self.honor_cancel = false;
        self
    }

    pub fn that_never_stops(mut self) -> MemoryWire {
        self.stop_works = false;
        self
    }

    /// 一直报「降级」：活着、能接受调用，但实现层已经报过失败。
    pub fn degraded(mut self, reason: impl Into<String>) -> MemoryWire {
        self.degraded = Some(reason.into());
        self
    }

    /// 只活着、还没就绪：Probe 返回 `Live`，不是 `Ready`。
    pub fn live_before_ready(mut self) -> MemoryWire {
        self.live = true;
        self
    }

    pub fn instance(self, contract: Contract) -> Instance {
        Instance::new(contract, Box::new(self)).expect("memory contract is valid")
    }
}

impl Default for MemoryWire {
    fn default() -> Self {
        MemoryWire::new()
    }
}

impl Wire for MemoryWire {
    fn call(&mut self, invocation: Invocation, done: ReplySink) -> Result<(), Rejection> {
        let outcome = if invocation.cancel.is_cancelled() && self.honor_cancel {
            Err(Failure::new(Class::Cancelled, Retry::Never, "cancelled"))
        } else if !self.running {
            Err(Failure::new(
                Class::NotRunning,
                Retry::After(Duration::from_secs(1)),
                "not running",
            ))
        } else {
            match self.replies.get(invocation.capability.as_str()) {
                Some(reply) => reply.clone(),
                None => Ok(Reply::Value(json!({}))),
            }
        };
        done(outcome);
        Ok(())
    }

    fn lifecycle(&mut self, control: Control) -> Result<Settled, Failure> {
        match control {
            Control::Ensure => {
                self.running = true;
                Ok(self.current())
            }
            Control::Quiesce => {
                if self.stop_works {
                    self.running = false;
                    Ok(Settled::Stopped)
                } else {
                    Ok(Settled::Ready)
                }
            }
            Control::Probe => Ok(if self.running {
                self.current()
            } else {
                Settled::Stopped
            }),
        }
    }
}

impl MemoryWire {
    fn current(&self) -> Settled {
        if let Some(reason) = &self.degraded {
            Settled::Degraded(reason.clone())
        } else if self.live {
            Settled::Live
        } else {
            Settled::Ready
        }
    }
}

/// 测试装配点的 `memory` tag：让 `Launchers` 真的能按 tag 造出 fake。
/// 生产路径不注册它；只有 `#[cfg(test)]` 的装配点会用到。
pub struct MemoryLauncher;

impl Launcher for MemoryLauncher {
    fn launch(&self, _contract: &Contract, _spec: &ImplRef) -> Result<Box<dyn Wire>, Failure> {
        Ok(Box::new(MemoryWire::new()))
    }
}
