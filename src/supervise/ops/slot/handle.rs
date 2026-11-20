//! The daemon-facing hand-off into a slot: the bounded request queue, the
//! cloneable handle callers submit through, and the `Wire` adapter the daemon
//! holds. None of this touches the supervised process.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

#[cfg(test)]
use serde_json::Value;

#[cfg(test)]
use crate::kernel::contract::Reply;
use crate::kernel::contract::{Control, Invocation, ReplySink, Settled, Wire};
use crate::kernel::failure::{Class, Failure, Rejection, Retry};
use crate::platform::host::wake::Wake;

#[cfg(test)]
use super::CALL_QUEUE;

/// A handle to one slot. Submitting never waits: the slot event loop owns the
/// process and calls `done` when the call finishes.
#[derive(Clone)]
pub(crate) struct SlotHandle {
    pub(super) requests: mpsc::SyncSender<SlotRequest>,
    pub(super) wake: Option<Arc<Wake>>,
    pub(super) health: Arc<AtomicU8>,
}

pub(super) struct SlotRequest {
    pub(super) invocation: Invocation,
    /// Progress still goes straight to the caller's channel; the final response
    /// goes through `done`.
    pub(super) client: Option<crate::wire::control::channel::Reply>,
    pub(super) done: ReplySink,
}

impl SlotHandle {
    /// Submit a call. The event loop drains the request on its next pump and
    /// invokes `done`; the caller keeps running.
    pub(crate) fn submit(
        &self,
        invocation: Invocation,
        client: Option<crate::wire::control::channel::Reply>,
        done: ReplySink,
    ) -> Result<(), (Failure, ReplySink)> {
        let request = SlotRequest {
            invocation,
            client,
            done,
        };
        match self.requests.try_send(request) {
            Ok(()) => {
                if let Some(wake) = &self.wake {
                    wake.notify();
                }
                Ok(())
            }
            Err(mpsc::TrySendError::Full(request)) => Err((
                Failure::new(
                    Class::Busy,
                    Retry::After(std::time::Duration::from_millis(100)),
                    "too many queued calls",
                ),
                request.done,
            )),
            Err(mpsc::TrySendError::Disconnected(request)) => Err((
                Failure::new(
                    Class::NotRunning,
                    Retry::After(std::time::Duration::from_secs(1)),
                    "service runtime is gone",
                ),
                request.done,
            )),
        }
    }

    pub(crate) fn probe(&self) -> Settled {
        match self.health.load(Ordering::SeqCst) {
            1 => Settled::Live,
            2 => Settled::Ready,
            3 => Settled::Degraded("degraded".into()),
            _ => Settled::Stopped,
        }
    }
}

/// The `Wire` used by the daemon. It never touches the process; it submits to
/// the slot event loop and returns, so the daemon itself stays non-blocking.
pub(crate) struct SlotWire {
    handle: SlotHandle,
    client: Option<crate::wire::control::channel::Reply>,
}

impl SlotWire {
    pub(crate) fn new(
        handle: SlotHandle,
        client: Option<crate::wire::control::channel::Reply>,
    ) -> SlotWire {
        SlotWire { handle, client }
    }
}

/// Test-only handle: answers submitted calls with `respond` on its own thread.
/// The thread ends when the handle is dropped, so the conformance suite can build
/// a fresh handle per assertion. It exercises the real `SlotWire` plumbing
/// (submit / done callback) without spawning a supervised process; the event
/// loop's own behaviour stays covered by the slot tests below.
#[cfg(test)]
impl SlotHandle {
    pub(crate) fn scripted(
        respond: impl Fn(&Invocation) -> Result<Value, Failure> + Send + 'static,
    ) -> SlotHandle {
        let (requests, receiver) = mpsc::sync_channel::<SlotRequest>(CALL_QUEUE);
        std::thread::spawn(move || {
            for request in receiver {
                let outcome = respond(&request.invocation);
                (request.done)(outcome.map(Reply::Value));
            }
        });
        SlotHandle {
            requests,
            wake: None,
            // Ready: a delegating adapter must still answer Probe.
            health: Arc::new(AtomicU8::new(2)),
        }
    }
}

impl Wire for SlotWire {
    fn call(&mut self, invocation: Invocation, done: ReplySink) -> Result<(), Rejection> {
        // 内核对「取消后不得返回 Ok」的承诺必须在这一层兑现：submit 之后就没有
        // 取消通道了（事件循环的 deadline 只覆盖超时），一个已取消的调用一旦进槽
        // 就会被真正执行，还可能以 Ok 回到调用方。
        if invocation.cancel.is_cancelled() {
            done(Err(Failure::new(
                Class::Cancelled,
                Retry::Never,
                "call was cancelled before it reached the service",
            )));
            return Ok(());
        }
        if let Err((failure, done)) = self.handle.submit(invocation, self.client.clone(), done) {
            done(Err(failure));
        }
        Ok(())
    }

    fn lifecycle(&mut self, control: Control) -> Result<Settled, Failure> {
        match control {
            Control::Probe => Ok(self.handle.probe()),
            _ => Err(Failure::new(
                Class::ImplementationError,
                Retry::Never,
                "slot lifecycle must go through the daemon",
            )),
        }
    }
}
