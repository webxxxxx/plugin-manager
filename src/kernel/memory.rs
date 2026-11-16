//! 纯内存的 [`Wire`]：给 conformance 与后续编排单测用，不需要起进程。
//!
//! 三种变体用来证明断言不是空跑：
//! - 默认：遵守取消、能停能起；
//! - [`MemoryWire::ignoring_cancel`]：取消后仍返回 `Ok`（必须被 conformance 抓到）；
//! - [`MemoryWire::that_never_stops`]：`Quiesce` 之后仍报 `Ready`（同样必须被抓到）。

#![cfg(test)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use super::contract::{Contract, Control, Instance, Invocation, Reply, Settled, Wire};
use super::failure::{Class, Failure, Retry};

pub struct MemoryWire {
    replies: HashMap<String, Result<Reply, Failure>>,
    running: bool,
    honor_cancel: bool,
    stop_works: bool,
    calls: Arc<AtomicUsize>,
}

impl MemoryWire {
    pub fn new() -> MemoryWire {
        MemoryWire {
            replies: HashMap::new(),
            running: true,
            honor_cancel: true,
            stop_works: true,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn with_value(mut self, capability: &str, value: Value) -> MemoryWire {
        self.replies
            .insert(capability.to_string(), Ok(Reply::Value(value)));
        self
    }

    pub fn with_failure(mut self, capability: &str, failure: Failure) -> MemoryWire {
        self.replies
            .insert(capability.to_string(), Err(failure));
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

    /// 调用计数：证明"内核拒绝时实现一次都没被叫到"。
    pub fn calls(&self) -> Arc<AtomicUsize> {
        self.calls.clone()
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
    fn call(&mut self, invocation: Invocation) -> Result<Reply, Failure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if invocation.cancel.is_cancelled() && self.honor_cancel {
            return Err(Failure::new(Class::Cancelled, Retry::Never, "cancelled"));
        }
        if !self.running {
            return Err(Failure::new(
                Class::NotRunning,
                Retry::After(Duration::from_secs(1)),
                "not running",
            ));
        }
        match self.replies.get(invocation.capability.as_str()) {
            Some(reply) => reply.clone(),
            None => Ok(Reply::Value(json!({}))),
        }
    }

    fn lifecycle(&mut self, control: Control) -> Result<Settled, Failure> {
        match control {
            Control::Ensure => {
                self.running = true;
                Ok(Settled::Ready)
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
                Settled::Ready
            } else {
                Settled::Stopped
            }),
        }
    }
}
