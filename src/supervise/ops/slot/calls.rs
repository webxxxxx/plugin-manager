//! In-flight `tools/call` bookkeeping: the pending map and the JSON-RPC id
//! space it shares with the readiness ping.

use std::collections::BTreeMap;

use super::*;

/// The calls a slot is waiting on. Ids come from one JSON-RPC request space
/// shared with the readiness ping, so a late ping answer cannot be mistaken
/// for the answer to a call.
pub(super) struct PendingCalls {
    calls: BTreeMap<u64, PendingCall>,
    next_id: u64,
}

impl PendingCalls {
    pub(super) fn new() -> PendingCalls {
        PendingCalls {
            calls: BTreeMap::new(),
            next_id: 1,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.calls.len()
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    pub(super) fn values(&self) -> impl Iterator<Item = &PendingCall> + '_ {
        self.calls.values()
    }

    pub(super) fn insert(&mut self, id: u64, call: PendingCall) {
        self.calls.insert(id, call);
    }

    pub(super) fn remove(&mut self, id: u64) -> Option<PendingCall> {
        self.calls.remove(&id)
    }

    /// Allocate the next request id.
    pub(super) fn allocate(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    /// A fresh process restarts the id space at 1.
    pub(super) fn reset(&mut self) {
        self.next_id = 1;
    }

    /// Ids of the calls that passed their deadline.
    pub(super) fn expired(&self, now: u64) -> Vec<u64> {
        self.calls
            .iter()
            .filter(|(_, call)| call.deadline <= now)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Every pending call, for failing them all with one reason.
    pub(super) fn take_all(&mut self) -> BTreeMap<u64, PendingCall> {
        std::mem::take(&mut self.calls)
    }
}

impl Slot {
    pub(super) fn drain_requests(&mut self, now: u64) {
        while let Ok(request) = self.request_rx.try_recv() {
            let mut sink = Some(request.done);
            if let Err(error) =
                self.request_invocation_sink(request.invocation, now, request.client, &mut sink)
            {
                if let Some(sink) = sink.take() {
                    self.finish_sink(sink, Err(call_error_failure(error)));
                }
            }
        }
    }

    /// Send one tools/call frame and remember the caller until the plugin
    /// answers, times out, or the service goes away.
    #[cfg(test)]
    pub(crate) fn request_capability(
        &mut self,
        capability: &str,
        arguments: Value,
        external_id: Option<Value>,
        now: u64,
        progress_token: Option<Value>,
        reply: crate::wire::control::channel::Reply,
    ) -> Result<(), CallError> {
        let mut invocation = Invocation::new(capability, arguments, self.call_timeout);
        invocation.progress = progress_token.and_then(|token| match token {
            Value::String(token) => Some(ProgressToken::new(token)),
            Value::Number(token) => Some(ProgressToken::new(token.to_string())),
            _ => None,
        });
        self.request_invocation(invocation, external_id, now, reply)
    }

    /// Contract-aware call entry: validation, inline limit, timeout ceiling and
    /// failure classification all go through `kernel`.
    #[cfg(test)]
    pub(crate) fn request_invocation(
        &mut self,
        invocation: Invocation,
        external_id: Option<Value>,
        now: u64,
        reply: crate::wire::control::channel::Reply,
    ) -> Result<(), CallError> {
        let client = Some(reply.clone());
        let mut sink = Some(test_reply_sink(reply, external_id));
        self.request_invocation_sink(invocation, now, client, &mut sink)
    }

    pub(super) fn request_invocation_sink(
        &mut self,
        invocation: Invocation,
        now: u64,
        client: Option<crate::wire::control::channel::Reply>,
        sink: &mut Option<ReplySink>,
    ) -> Result<(), CallError> {
        if let Some(contract) = &self.contract {
            if contract.capability(&invocation.capability).is_none() {
                return Err(CallError::Rejected(Rejection::UnknownCapability {
                    capability: invocation.capability.as_str().to_string(),
                }));
            }
            if crate::kernel::contract::inline_len(&invocation.arguments)
                > contract.limits.max_inline
            {
                return Err(CallError::Rejected(Rejection::BadArguments {
                    detail: "arguments exceed the contract inline limit".into(),
                }));
            }
        }
        if invocation.timeout.is_zero() {
            return Err(CallError::Rejected(Rejection::BadArguments {
                detail: "timeout must be greater than zero".into(),
            }));
        }
        if self.machine.state != State::Running {
            return Err(CallError::Failed(Failure::new(
                Class::NotRunning,
                Retry::After(std::time::Duration::from_secs(1)),
                format!(
                    "plugin {} is not running; call {}.start first",
                    self.id, self.id
                ),
            )));
        }
        if self.pending.len() >= MAX_IN_FLIGHT {
            return Err(CallError::Failed(Failure::new(
                Class::Busy,
                Retry::After(std::time::Duration::from_millis(100)),
                "too many in-flight tool calls",
            )));
        }
        let timeout = match &self.contract {
            Some(contract) => invocation.timeout.min(contract.limits.max_timeout),
            None => invocation.timeout,
        };
        let id = self.pending.allocate();
        let mut params = json!({
            "name": invocation.capability.as_str(),
            "arguments": invocation.arguments,
        });
        if let Some(token) = &invocation.progress {
            params["_meta"] = json!({ "progressToken": token.as_str() });
        }
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": params,
        })
        .to_string();
        if frame.len() > ipc::CALL_FRAME_MAX {
            return Err(CallError::Rejected(Rejection::BadArguments {
                detail: "tools/call request exceeds the 1 MiB frame limit".into(),
            }));
        }
        if let Err(error) = self.write_json_frame(&frame, ipc::CALL_FRAME_MAX) {
            return Err(CallError::Failed(Failure::new(
                Class::WriteFailed,
                Retry::Never,
                format!("cannot write to plugin: {error}"),
            )));
        }
        let deadline = now.saturating_add(timeout.as_millis().min(u64::MAX as u128) as u64);
        self.pending.insert(
            id,
            PendingCall {
                capability: invocation.capability.as_str().to_string(),
                deadline,
                progress_token: invocation
                    .progress
                    .as_ref()
                    .map(|token| token.as_str().to_string()),
                client,
                sink: sink.take().expect("call sink must be present"),
            },
        );
        Ok(())
    }

    pub(super) fn complete_pending(&mut self, id: u64, message: &serde_json::Map<String, Value>) {
        let Some(pending) = self.pending.remove(id) else {
            warn!("{}: ignoring response for unknown request {}", self.id, id);
            return;
        };
        let outcome = if let Some(error) = message.get("error") {
            Err(Failure::from_plugin_error(error))
        } else if let Some(result) = message.get("result") {
            let mut result = result.clone();
            match crate::platform::storage::materialize_result(&self.paths, &mut result) {
                Ok(()) => Ok(result),
                Err(error) => Err(Failure::new(
                    Class::ImplementationError,
                    Retry::Never,
                    error.detail,
                )),
            }
        } else {
            Err(Failure::new(
                Class::ImplementationError,
                Retry::Never,
                "plugin returned neither result nor error",
            ))
        };
        self.mark_degraded(&outcome);
        self.finish_pending(pending, outcome);
    }

    pub(super) fn finish_pending(&self, pending: PendingCall, outcome: Result<Value, Failure>) {
        self.finish_sink(pending.sink, outcome);
    }

    /// One completion point: the slot turns the call outcome into a [`Reply`]
    /// and hands it to the sink the caller supplied. It does not encode
    /// anything — the face that owns the protocol supplied the sink.
    pub(super) fn finish_sink(&self, sink: ReplySink, outcome: Result<Value, Failure>) {
        sink(outcome.map(Reply::Value));
    }

    pub(super) fn mark_degraded(&mut self, outcome: &Result<Value, Failure>) {
        if let Err(failure) = outcome {
            if matches!(
                failure.class,
                Class::ImplementationError | Class::Timeout | Class::WriteFailed
            ) {
                self.machine.mark_degraded(failure.detail.clone());
            }
        }
    }

    pub(super) fn expire_pending(&mut self, now: u64) {
        let expired = self.pending.expired(now);
        for id in expired {
            self.write_notification(
                "notifications/cancelled",
                json!({ "requestId": id, "reason": "timeout" }),
            );
            let Some(pending) = self.pending.remove(id) else {
                continue;
            };
            error!("{}: tools/call {} timed out", self.id, pending.capability);
            let failure = Failure::new(
                Class::Timeout,
                Retry::After(std::time::Duration::from_secs(1)),
                "tools/call timed out",
            );
            let outcome = Err(failure);
            self.mark_degraded(&outcome);
            self.finish_pending(pending, outcome);
        }
    }

    pub(super) fn fail_pending(&mut self, reason: &str) {
        let pending = self.pending.take_all();
        for (_, call) in pending {
            let failure = Failure::new(
                Class::NotRunning,
                Retry::After(std::time::Duration::from_secs(1)),
                reason,
            );
            self.machine.mark_degraded(reason.to_string());
            self.finish_pending(call, Err(failure));
        }
    }
}

pub(super) struct PendingCall {
    pub(super) capability: String,
    pub(super) deadline: u64,
    pub(super) progress_token: Option<String>,
    /// Progress notifications go here while the call is in flight.
    pub(super) client: Option<crate::wire::control::channel::Reply>,
    /// Final result, delivered to the caller's completion sink. The slot does
    /// not encode it: the face that owns the protocol supplied the sink.
    pub(super) sink: ReplySink,
}

#[derive(Debug)]
pub(crate) enum CallError {
    Rejected(Rejection),
    Failed(Failure),
}

pub(crate) fn call_error_failure(error: CallError) -> Failure {
    match error {
        CallError::Failed(failure) => failure,
        CallError::Rejected(Rejection::BadArguments { detail }) => {
            Failure::new(Class::FrameTooLarge, Retry::Never, detail)
        }
        CallError::Rejected(Rejection::UnknownCapability { capability }) => Failure::new(
            Class::ImplementationError,
            Retry::Never,
            format!("unknown capability {capability}"),
        ),
    }
}

/// Test-only sink factory: it encodes a completed call exactly the way the old
/// injected projector did, so the slot tests can still assert on the JSON line
/// while the slot itself holds no protocol knowledge.
#[cfg(test)]
pub(super) fn test_reply_sink(
    reply: crate::wire::control::channel::Reply,
    external_id: Option<Value>,
) -> ReplySink {
    Box::new(move |outcome| {
        let line = match outcome {
            Ok(Reply::Value(value)) => rpc::to_line(&rpc::ok(external_id, value)),
            Err(failure) => json!({
                "jsonrpc": "2.0",
                "id": external_id,
                "result": {
                    "content": [{ "type": "text", "text": failure.detail }],
                    "isError": true,
                },
            })
            .to_string(),
        };
        reply.send(line);
    })
}
