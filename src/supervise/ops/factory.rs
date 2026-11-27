//! One place that turns a service definition into a slot.
//!
//! Every slot needs the same twelve knobs (contract, call timeout, wake,
//! stdio, raw hub, notifier, protocol flags...). They used to be copied by hand
//! at boot, rescan, install and staged install; a missed setter silently
//! disabled notifications or the call budget on one path (#229).

use std::sync::Arc;

use serde_json::Value;

use crate::kernel::registry::{Kind, Service};
use crate::platform::config::Config;
use crate::platform::host::wake::Wake;
use crate::supervise::ops::slot::Slot;
use crate::wire::contract::rawhub::RawHub;
use crate::wire::contract::stdio::Target;

/// (service id, method, params) -> the UI bus. Kept as a callback so the
/// supervisor never names a face type.
pub(crate) type Notifier = Arc<dyn Fn(&str, &str, &Value) + Send + Sync>;

/// Builds slots from definitions. The supervisor owns one; every slot-creating
/// path (boot, rescan, install) goes through it.
pub(crate) struct SlotFactory {
    paths: Arc<crate::platform::paths::Paths>,
    limits: Arc<crate::platform::limits::Limits>,
    wake: Arc<Wake>,
    stdio: Target,
    raw_hub: Arc<RawHub>,
    notifier: Notifier,
}

impl SlotFactory {
    pub(crate) fn new(
        config: Arc<Config>,
        wake: Arc<Wake>,
        raw_hub: Arc<RawHub>,
        notifier: Notifier,
    ) -> Self {
        SlotFactory {
            paths: config.paths.clone(),
            limits: config.limits.clone(),
            wake,
            stdio: Target::threaded(),
            raw_hub,
            notifier,
        }
    }

    pub(crate) fn set_wake(&mut self, wake: Arc<Wake>) {
        self.wake = wake;
    }

    pub(crate) fn set_stdio(&mut self, stdio: Target) {
        self.stdio = stdio;
    }

    pub(crate) fn set_raw_hub(&mut self, raw_hub: Arc<RawHub>) {
        self.raw_hub = raw_hub;
    }

    #[cfg(test)]
    pub(crate) fn raw_hub(&self) -> Arc<RawHub> {
        self.raw_hub.clone()
    }

    /// A service without `exec` has no slot (builtin or future carriers).
    pub(crate) fn build(&self, service: &Service) -> Option<Slot> {
        let exec = service.exec.clone()?;
        let mut slot = Slot::new(
            service.id.clone(),
            exec,
            service.lifecycle.clone(),
            service.restart,
            self.paths.clone(),
            self.limits.clone(),
        );
        slot.set_protocol(service.protocol);
        slot.set_capability_hooks(service.capability_hooks.clone());
        slot.set_plugin_dir(service.install_dir.as_ref().map(std::path::PathBuf::from));
        slot.set_protocol_only(service.kind == Kind::Plugin);
        slot.set_remain_after_exit(service.remain_after_exit);
        slot.set_initial_state(service.state);
        slot.set_wake(self.wake.clone());
        slot.set_stdio(self.stdio.clone());
        slot.set_raw_hub(Some(self.raw_hub.clone()));
        slot.set_contract(service.contract.clone());
        slot.set_call_timeout(self.limits.default_timeout);
        let id = service.id.clone();
        let notify = self.notifier.clone();
        slot.set_notifier(Arc::new(move |method, params| notify(&id, method, params)));
        Some(slot)
    }
}
