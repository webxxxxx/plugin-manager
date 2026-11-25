use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use crate::kernel::failure::{Problem, ProblemError};
use crate::kernel::projection::Status;
use crate::kernel::registry::{Kind, Registry, State, RESERVED_ID};
use crate::kernel::schema::{MAX_PROTOCOL, MAX_SCHEMA};
use crate::platform::clock::Clock;
use crate::platform::compat;
use crate::platform::log;
use crate::supervise::check::start;
use crate::supervise::model::supervisor::{self, Event};
use crate::supervise::ops::host::Supervisor;

impl Supervisor {
    pub(crate) fn start_service(
        &mut self,
        id: &str,
        clock: &Clock,
    ) -> Result<Status, ProblemError> {
        if self.registry.get(id).is_none() {
            return Err(ProblemError::new(
                Problem::UnknownService,
                format!("unknown service: {}", id),
            ));
        }
        if id == RESERVED_ID {
            return Ok(self.status_for_id(id, clock.now()).expect("builtin"));
        }
        // A restricted package must never start on a host it does not support,
        // even if the marker changed after it was installed.
        if let Some(service) = self.registry.get(id) {
            let devices = service
                .compat
                .as_ref()
                .map(|spec| spec.devices.clone())
                .unwrap_or_default();
            if let Some(reason) = compat::verdict_reason(compat::evaluate(
                crate::platform::target(),
                &devices,
                &compat::detect(&self.config.paths, MAX_SCHEMA, MAX_PROTOCOL),
            )) {
                return Err(Problem::Internal.error(format!("{}: {}", id, reason)));
            }
        }
        if let Some(service) = self.registry.get(id) {
            let exec = service.exec.clone();
            let lifecycle = service.lifecycle.clone();
            let restart = service.restart;
            let remain_after_exit = service.remain_after_exit;
            if let Some(slot) = self.slots.get_mut(id) {
                if let Some(exec) = exec {
                    slot.set_exec(exec);
                }
                slot.set_lifecycle(lifecycle);
                slot.set_restart(restart);
                slot.set_remain_after_exit(remain_after_exit);
            }
        }

        // A finished one-shot is `Running` with no pid. An explicit start must
        // run it again, while a dependency in the same state stays satisfied
        // (start_layer skips Running services).
        let now = clock.now();
        if let Some(slot) = self.slots.get_mut(id) {
            if slot.machine.state == State::Running
                && slot.machine.pid.is_none()
                && slot.machine.remain_after_exit
            {
                slot.apply(Event::Start, now.mono_ms);
            }
        }

        let layers = self.registry.start_layers(&[id.to_string()])?;
        for dependency in layers
            .iter()
            .flatten()
            .filter(|service_id| service_id.as_str() != id)
        {
            match self.slots.get(dependency).map(|slot| slot.machine.state) {
                Some(State::Failed) => {
                    return Err(ProblemError::new(
                        Problem::Internal,
                        format!("dependency {} is failed", dependency),
                    ));
                }
                Some(State::Backoff) => {
                    return Err(ProblemError::new(
                        Problem::Internal,
                        format!("dependency {} is backing off", dependency),
                    ));
                }
                Some(State::Stopping) => {
                    return Err(ProblemError::new(
                        Problem::Internal,
                        format!("dependency {} is stopping", dependency),
                    ));
                }
                _ => {}
            }
        }
        for layer in layers {
            let deadline = clock
                .now()
                .mono_ms
                .saturating_add(supervisor::START_TIMEOUT_MS)
                .saturating_add(self.start_hook_budget(layer.iter().map(String::as_str)));
            self.start_layer(&layer, deadline, clock)?;
        }

        self.status_for_id(id, clock.now()).ok_or_else(|| {
            ProblemError::new(Problem::UnknownService, format!("unknown service: {}", id))
        })
    }

    pub(crate) fn start_layer(
        &mut self,
        ids: &[String],
        deadline: u64,
        clock: &Clock,
    ) -> Result<(), ProblemError> {
        let pending: Vec<String> = ids
            .iter()
            .filter(|id| self.slots.contains_key(*id))
            .filter(|id| self.slots[*id].machine.state != State::Running)
            .cloned()
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        for id in &pending {
            if self.slots[id].machine.state == State::Stopping {
                return Err(ProblemError::new(
                    Problem::Internal,
                    format!("service {} is stopping", id),
                ));
            }
            if self.slots[id].machine.state == State::Starting {
                info!(
                    "{}: start requested while already Starting; no new spawn for this call",
                    id
                );
            }
            let now = clock.now().mono_ms;
            self.slots
                .get_mut(id)
                .expect("slot")
                .apply(Event::Start, now);
        }

        loop {
            self.pump(clock.now());
            let mut all_settled = true;
            for id in &pending {
                let slot = &self.slots[id];
                let state = slot.machine.state;
                match start::start_step(state, slot.is_channel_peer()) {
                    start::Step::Settled => {}
                    start::Step::Finished => {
                        // It reached ready and then exited cleanly before this
                        // pass looked (`ready: exec` one-shots do exactly that).
                        // Waiting for Running burns the whole window and reports
                        // a timeout for a start that did succeed.
                        info!("{}: exited cleanly during start; the start is complete", id);
                    }
                    start::Step::Waiting => all_settled = false,
                    start::Step::Failed => {
                        return Err(ProblemError::new(
                            Problem::Internal,
                            format!("service {} failed to start", id),
                        ));
                    }
                    start::Step::ChannelLost => {
                        // It answered ready and then left. The channel callers
                        // depend on is gone, so this start produced nothing
                        // usable even though the exit itself was clean.
                        let code = slot
                            .machine
                            .last_exit
                            .map(|code| code.to_string())
                            .unwrap_or_else(|| "unknown".into());
                        error!(
                            "{}: exited during start (exit code {}); the start failed",
                            id, code
                        );
                        return Err(ProblemError::new(
                            Problem::Internal,
                            format!("service {} exited during start (exit code {})", id, code),
                        ));
                    }
                }
            }
            if all_settled {
                return Ok(());
            }
            if clock.now().mono_ms >= deadline {
                for id in &pending {
                    error!(
                        "{}: start timed out waiting for ready ({})",
                        id,
                        self.slots[id].handshake_snapshot()
                    );
                }
                return Err(ProblemError::new(
                    Problem::Timeout,
                    format!("start timed out: {}", pending.join(", ")),
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub(crate) fn set_enabled(
        &mut self,
        id: &str,
        enabled: bool,
        clock: &Clock,
    ) -> Result<Status, ProblemError> {
        if self.registry.get(id).is_none() {
            return Err(ProblemError::new(
                Problem::UnknownService,
                format!("unknown service: {}", id),
            ));
        }
        if id == RESERVED_ID {
            return Err(ProblemError::new(
                Problem::Conflict,
                "the built-in svc service cannot be enabled or disabled",
            ));
        }
        let kind = self
            .registry
            .get(id)
            .map(|service| service.kind)
            .ok_or_else(|| {
                ProblemError::new(Problem::UnknownService, format!("unknown service: {}", id))
            })?;
        let (marker, create) = match kind {
            Kind::Plugin => (
                crate::kernel::layout::plugin_dir(&self.config.paths, id).join("enabled"),
                enabled,
            ),
            Kind::Core => (
                self.config
                    .paths
                    .services_dir
                    .join(format!("{}.disabled", id)),
                !enabled,
            ),
        };
        if create {
            if let Some(parent) = marker.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    ProblemError::new(
                        Problem::Internal,
                        format!("cannot create {}: {}", parent.display(), error),
                    )
                })?;
            }
            fs::write(&marker, b"").map_err(|error| {
                ProblemError::new(
                    Problem::Internal,
                    format!("cannot write {}: {}", marker.display(), error),
                )
            })?;
        } else if marker.exists() {
            fs::remove_file(&marker).map_err(|error| {
                ProblemError::new(
                    Problem::Internal,
                    format!("cannot remove {}: {}", marker.display(), error),
                )
            })?;
        }
        if let Some(service) = self.registry.get_mut(id) {
            service.enabled = enabled;
        }
        // `disable` changes the boot flag only. The crash-restart policy is a
        // separate service property and must not be turned off behind the
        // operator's back.
        self.status_for_id(id, clock.now()).ok_or_else(|| {
            ProblemError::new(Problem::UnknownService, format!("unknown service: {}", id))
        })
    }

    /// `svc restart` = stop then start, on one RPC so no other caller can slip
    /// in between. A finished one-shot is stopped (settles immediately) and
    /// then runs again.
    pub(crate) fn restart_service(
        &mut self,
        id: &str,
        clock: &Clock,
    ) -> Result<Status, ProblemError> {
        if self.registry.get(id).is_none() {
            return Err(ProblemError::new(
                Problem::UnknownService,
                format!("unknown service: {}", id),
            ));
        }
        if id == RESERVED_ID {
            return Ok(self.status_for_id(id, clock.now()).expect("builtin"));
        }
        self.stop_service(id, clock)?;
        self.start_service(id, clock)
    }

    /// How long the start actions of these services may take before their
    /// process windows even open.
    pub(super) fn start_hook_budget<'a>(&self, ids: impl Iterator<Item = &'a str>) -> u64 {
        ids.filter_map(|id| self.registry.get(id))
            .filter_map(|service| service.lifecycle.as_ref())
            .filter_map(|lifecycle| lifecycle.start.as_ref())
            .map(|hook| hook.timeout_ms)
            .max()
            .unwrap_or(0)
    }

    /// How long the stop action of this service may take after its process is
    /// gone.
    fn stop_hook_budget(&self, id: &str) -> u64 {
        self.registry
            .get(id)
            .and_then(|service| service.lifecycle.as_ref())
            .and_then(|lifecycle| lifecycle.stop.as_ref())
            .map(|hook| hook.timeout_ms)
            .unwrap_or(0)
    }

    pub(crate) fn stop_service(&mut self, id: &str, clock: &Clock) -> Result<Status, ProblemError> {
        if self.registry.get(id).is_none() {
            return Err(ProblemError::new(
                Problem::UnknownService,
                format!("unknown service: {}", id),
            ));
        }
        if id == RESERVED_ID {
            return Ok(self.status_for_id(id, clock.now()).expect("builtin"));
        }
        let Some(state) = self.slots.get(id).map(|slot| slot.machine.state) else {
            return self.status_for_id(id, clock.now()).ok_or_else(|| {
                ProblemError::new(Problem::UnknownService, format!("unknown service: {}", id))
            });
        };

        let now = clock.now();
        self.slots
            .get_mut(id)
            .expect("slot")
            .request_stop(now.mono_ms);
        if matches!(state, State::Stopped | State::Failed) {
            return self.status_for_id(id, clock.now()).ok_or_else(|| {
                ProblemError::new(Problem::UnknownService, format!("unknown service: {}", id))
            });
        }
        let deadline = now
            .mono_ms
            .saturating_add(supervisor::STOP_TIMEOUT_MS)
            .saturating_add(supervisor::KILL_TIMEOUT_MS)
            .saturating_add(self.stop_hook_budget(id))
            .saturating_add(1_000);
        loop {
            self.pump(clock.now());
            match self.slots[id].machine.state {
                State::Stopped => break,
                State::Failed => {
                    return Err(ProblemError::new(
                        Problem::Internal,
                        format!("service {} did not stop", id),
                    ))
                }
                _ => {}
            }
            if clock.now().mono_ms >= deadline {
                return Err(ProblemError::new(
                    Problem::Timeout,
                    format!("stop timed out: {}", id),
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.status_for_id(id, clock.now()).ok_or_else(|| {
            ProblemError::new(Problem::UnknownService, format!("unknown service: {}", id))
        })
    }

    pub(crate) fn stop_all(&mut self, clock: &Clock) {
        let started = Instant::now();
        let ids: Vec<String> = self.slots.keys().cloned().collect();
        let (layers, forced) = shutdown_layers(&self.registry, ids);
        if forced {
            for slot in self.slots.values_mut() {
                if !matches!(slot.machine.state, State::Stopped | State::Failed) {
                    slot.signal_force("the dependency graph is inconsistent at shutdown");
                }
            }
        }
        let global_deadline = clock
            .now()
            .mono_ms
            .saturating_add(supervisor::SHUTDOWN_BUDGET_MS);
        'layers: for layer in layers.into_iter().rev() {
            let active: Vec<String> = layer
                .into_iter()
                .filter(|id| {
                    self.slots
                        .get(id)
                        .map(|slot| {
                            matches!(
                                slot.machine.state,
                                State::Starting | State::Running | State::Stopping | State::Backoff
                            )
                        })
                        .unwrap_or(false)
                })
                .collect();
            if active.is_empty() {
                continue;
            }
            for id in &active {
                let now = clock.now().mono_ms;
                self.slots.get_mut(id).expect("slot").request_stop(now);
            }
            let deadline = clock
                .now()
                .mono_ms
                .saturating_add(supervisor::STOP_TIMEOUT_MS)
                .saturating_add(supervisor::KILL_TIMEOUT_MS)
                .saturating_add(
                    active
                        .iter()
                        .map(|id| self.stop_hook_budget(id))
                        .max()
                        .unwrap_or(0),
                )
                .min(global_deadline);
            loop {
                self.pump(clock.now());
                if active.iter().all(|id| {
                    matches!(self.slots[id].machine.state, State::Stopped | State::Failed)
                }) {
                    break;
                }
                if clock.now().mono_ms >= deadline {
                    if clock.now().mono_ms >= global_deadline {
                        for slot in self.slots.values_mut() {
                            if !matches!(slot.machine.state, State::Stopped | State::Failed) {
                                slot.signal_force("daemon shutdown past its global budget");
                            }
                        }
                        break 'layers;
                    }
                    for id in &active {
                        if !matches!(self.slots[id].machine.state, State::Stopped | State::Failed) {
                            if let Some(slot) = self.slots.get_mut(id) {
                                slot.signal_force("daemon shutdown per-layer timeout");
                            }
                        }
                    }
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
        log::note_slow("stop_all", started);
    }
}
/// Shutdown order for every live slot. A cycle cannot survive the registry
/// door, but if one is ever there, logging it and forcing everything down in
/// one layer beats the old silent `unwrap_or_else(|_| vec![ids])`.
fn shutdown_layers(registry: &Registry, ids: Vec<String>) -> (Vec<Vec<String>>, bool) {
    match registry.start_layers(&ids) {
        Ok(layers) => (layers, false),
        Err(error) => {
            warn!("shutdown: {}; stopping every service in one layer", error);
            (vec![ids], true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::registry::{contract_for, Carrier, Exec, Ready, Restart, Service};

    fn slot_backed(id: &str, deps: &[&str]) -> Service {
        let limits = crate::platform::limits::test_default();
        let mut service = Service::builtin_svc(&limits);
        service.id = id.into();
        service.contract = contract_for(
            id,
            &service.version,
            service.contract.surface().to_vec(),
            Carrier::Slot,
            &limits,
        );
        service.impl_ref = None;
        service.exec = Some(Exec {
            path: "/bin/true".into(),
            args: Vec::new(),
            env: Default::default(),
            ready: Ready::Exec,
            interpreter: None,
        });
        service.deps = deps.iter().map(|dep| dep.to_string()).collect();
        service.restart = Restart::OnFailure;
        service
    }

    #[test]
    fn a_dependency_cycle_is_reported_instead_of_silently_collapsing_to_one_layer() {
        let mut registry = Registry::new();
        registry.insert(slot_backed("a", &["b"])).expect("a");
        registry.insert(slot_backed("b", &["a"])).expect("b");

        let (layers, forced) = shutdown_layers(&registry, vec!["a".into(), "b".into()]);

        assert!(forced, "the cycle must be reported");
        assert_eq!(
            layers,
            vec![vec!["a".to_string(), "b".to_string()]],
            "every service still gets a stop pass"
        );
    }
}
