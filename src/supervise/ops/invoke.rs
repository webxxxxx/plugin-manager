//! Pick the carrier for one capability call.
//!
//! A service with an `impl_ref` is a builtin launcher; everything else is a
//! supervised slot. The protocol projection (MCP result envelopes) stays in
//! compose, which hands in one completion sink per carrier.

use crate::kernel::contract::{Instance, Invocation, ReplySink};
use crate::kernel::failure::Rejection;
use crate::kernel::failure::{Class, Failure, Retry};
use crate::kernel::registry::Service;
use crate::supervise::ops::host::Supervisor;
use crate::supervise::ops::slot::SlotWire;
use crate::wire::control::channel::Reply;

/// What compose must do after `call` returns.
pub(crate) enum Outcome {
    /// The carrier owns the invocation; the completion sink will fire.
    Started,
    /// A slot-backed service has no slot (not installed as a running shape).
    NoRuntime,
    BuiltinFailed(Failure),
    SlotFailed(Failure),
    /// The carrier refused the invocation before accepting it.
    Rejected(Rejection),
}

pub(crate) fn call(
    host: &mut Supervisor,
    service: &Service,
    invocation: Invocation,
    reply: &Reply,
    on_builtin: ReplySink,
    on_slot: ReplySink,
) -> Outcome {
    if let Some(spec) = service.impl_ref.clone() {
        let mut instance = match host.launchers.launch(service.contract.clone(), &spec) {
            Ok(instance) => instance,
            Err(error) => return Outcome::BuiltinFailed(error.into_failure()),
        };
        return match instance.call(invocation, on_builtin) {
            Ok(()) => Outcome::Started,
            Err(rejection) => Outcome::Rejected(rejection),
        };
    }

    let Some(slot) = host.slots.get_mut(&service.id) else {
        return Outcome::NoRuntime;
    };
    let handle = slot.handle();
    let mut instance = match Instance::new(
        service.contract.clone(),
        Box::new(SlotWire::new(handle, Some(reply.clone()))),
    ) {
        Ok(instance) => instance,
        Err(error) => {
            return Outcome::SlotFailed(Failure::new(
                Class::ImplementationError,
                Retry::Never,
                error,
            ))
        }
    };
    match instance.call(invocation, on_slot) {
        Ok(()) => Outcome::Started,
        Err(rejection) => Outcome::Rejected(rejection),
    }
}
