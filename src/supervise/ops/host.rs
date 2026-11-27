use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::kernel::contract::Launchers;
use crate::kernel::failure::{Problem, ProblemError};
use crate::kernel::projection::{anchored_mono_ms, anchored_secs, anchored_secs_opt, Status};
use crate::kernel::registry::{Registry, Service, State, RESERVED_ID};
use crate::platform::clock::{Clock, Now};
use crate::platform::config::Config;
use crate::platform::host::wake::Wake;
use crate::platform::log;
use crate::supervise::check::stop_reason::StopReason;
use crate::supervise::model::runtime::{self, Record};
use crate::supervise::ops::factory::{Notifier, SlotFactory};
use crate::supervise::ops::slot::Slot;
use crate::wire::contract::reactor::Reactor;

/// The supervisor loop's bounded wait when it has no earlier deadline. With a
/// reactor this is the plugin poll's timeout, so an idle daemon wakes once per
/// tick.
pub(crate) const POLL_WAIT: Duration = Duration::from_millis(50);

/// The supervisor: the registry, every live slot, the runtime records and the
/// timers that drive them. The composition root only assembles it; the
/// supervision decisions live here and in the sibling ops files.
pub(crate) struct Supervisor {
    pub(crate) config: Arc<Config>,
    pub(crate) registry: Registry,
    pub(crate) slots: BTreeMap<String, Slot>,
    pub(crate) launchers: Arc<Launchers>,
    /// Last record written per service: state, pid, failures. Only a change
    /// here triggers a disk write.
    pub(crate) persisted: BTreeMap<String, (State, Option<u32>, u32)>,
    /// Boot identity captured once; every runtime record carries it.
    boot_id: Option<String>,
    /// Cheap fingerprint of services.d; a change triggers a reconcile.
    pub(crate) services_fingerprint: Vec<(String, u64, u64)>,
    pub(crate) next_rescan_ms: u64,
    pub(crate) rescan_interval_ms: u64,
    /// Temp-file retention and the supervisor's own cleanup cadence.
    temp_retention: Duration,
    pub(crate) temp_cleanup_interval_ms: u64,
    pub(crate) next_temp_cleanup_ms: u64,
    pub(crate) wake: Arc<Wake>,
    /// Who reads plugin stdio; the factory copies it into every new slot.
    stdio: crate::wire::contract::stdio::Target,
    /// Set whenever the registry projection changed; compose drains it.
    pub(crate) catalog_dirty: bool,
    pub(crate) reactor: Option<Arc<Reactor>>,
    /// One place that turns a definition into a slot.
    pub(crate) factory: SlotFactory,
}

impl Supervisor {
    pub(crate) fn new(
        config: Arc<Config>,
        registry: Registry,
        raw_hub: Arc<crate::wire::contract::rawhub::RawHub>,
        notifier: Notifier,
    ) -> Self {
        let wake = Arc::new(Wake::new());
        let factory = SlotFactory::new(config.clone(), wake.clone(), raw_hub, notifier);
        let mut slots = BTreeMap::new();
        for service in registry.managed_values() {
            if let Some(slot) = factory.build(service) {
                slots.insert(service.id.clone(), slot);
            }
        }
        let rescan_interval_ms = config.settings.rescan_interval_ms();
        let temp_retention = config.settings.temp_retention();
        let temp_cleanup_interval_ms = config.settings.temp_cleanup_interval().as_millis() as u64;
        let services_fingerprint =
            crate::supervise::ops::rescan::services_fingerprint(&config.paths.services_dir);
        let mut launchers = Launchers::default();
        launchers.register(
            "builtin",
            Box::new(crate::supervise::ops::builtin_adapter::BuiltinLauncher),
        );
        Supervisor {
            config,
            registry,
            slots,
            launchers: Arc::new(launchers),
            persisted: BTreeMap::new(),
            boot_id: crate::platform::host::process::boot_id(),
            services_fingerprint,
            rescan_interval_ms,
            next_rescan_ms: rescan_interval_ms,
            temp_retention,
            temp_cleanup_interval_ms,
            next_temp_cleanup_ms: 0,
            wake,
            stdio: crate::wire::contract::stdio::Target::threaded(),
            catalog_dirty: false,
            reactor: None,
            factory,
        }
    }

    pub(crate) fn service(&self, id: &str) -> Option<&Service> {
        self.registry.get(id)
    }

    pub(crate) fn is_running(&self, id: &str) -> bool {
        self.slots
            .get(id)
            .map(|slot| !matches!(slot.machine.state, State::Stopped | State::Failed))
            .unwrap_or(false)
    }

    /// Clear one plugin's log files: the service log (through the live writer
    /// when the plugin is running) plus the hooks log, which has no persistent
    /// writer because it exists only for the duration of one hook run.
    pub(crate) fn clear_logs(&mut self, id: &str) -> std::io::Result<usize> {
        let rotations = self.config.limits.log_rotations;
        let logs_dir = self.config.paths.logs_dir.clone();
        let mut removed = match self.slots.get_mut(id) {
            Some(slot) => slot.clear_logs()?,
            None => log::remove_log_files(&logs_dir, id, rotations)?,
        };
        removed += log::remove_log_files(&logs_dir, &format!("{}-hooks", id), rotations)?;
        if removed > 0 {
            // The audit line belongs to the plugin's own (freshly recreated) log,
            // not the daemon log: whoever reads `<id>.log` sees who cleared it.
            log::append_plugin_line(
                &logs_dir,
                id,
                &self.config.limits,
                log::Level::Info,
                &format!("logs cleared: {} ({} files)", id, removed),
            )?;
        }
        Ok(removed)
    }

    /// Replace one definition and, when it is not running, its slot. The caller
    /// (compose) decides whether to stop the service first.
    pub(crate) fn install(&mut self, service: Service, running: bool) -> Result<(), ProblemError> {
        if let Err(error) = service.carrier() {
            return Err(Problem::Internal.error(format!("plugin {}: {}", service.id, error)));
        }
        // The registry decides id collisions (and names both display names);
        // it must run before the slot table is touched so a rejected install
        // leaves the running service alone.
        self.registry
            .insert_or_replace(service.clone())
            .map_err(|detail| Problem::Conflict.error(detail))?;
        if !running {
            self.slots.remove(&service.id);
            if let Some(slot) = self.factory.build(&service) {
                self.slots.insert(service.id.clone(), slot);
            }
        }
        Ok(())
    }

    /// Whether the tool catalog changed since the last check. Compose owns the
    /// cache; the supervisor only reports that its projection moved.
    pub(crate) fn take_catalog_dirty(&mut self) -> bool {
        std::mem::take(&mut self.catalog_dirty)
    }

    pub(crate) fn forget(&mut self, id: &str) {
        self.slots.remove(id);
        self.registry.remove(id);
        self.persisted.remove(id);
        runtime::remove(id, &self.config.paths);
    }

    /// Probe a recorded pid once: the model takes a `runtime::ProcessStamp` and
    /// never touches the OS.
    pub(crate) fn process_probe(pid: u32) -> runtime::ProcessStamp {
        if crate::platform::host::process::alive(pid) {
            runtime::ProcessStamp::Alive {
                starttime: crate::platform::host::process::start_time(pid),
            }
        } else {
            runtime::ProcessStamp::Gone
        }
    }

    /// The adoption-side guard: a record with no `since` stays "never
    /// happened" (`0`) instead of being anchored to this daemon's boot. One
    /// helper so both adoption branches agree.
    pub(crate) fn adopted_since(now: Now, record: &Record) -> u64 {
        anchored_mono_ms(now, record.since).unwrap_or(0)
    }

    /// Reconnect to processes that survived a previous daemon. A record is only
    /// trusted when its boot id matches, the pid is alive and the pid's start
    /// time still matches (a reused pid fails this check). On Windows the job
    /// objects kill the tree with the daemon, so there is nothing to adopt.
    pub(crate) fn adopt_runtime(&mut self, now: Now) {
        let boot = self
            .boot_id
            .clone()
            .or_else(crate::platform::host::process::boot_id);
        for id in self.slots.keys().cloned().collect::<Vec<_>>() {
            let Some(record) = runtime::load(&id, &self.config.paths) else {
                continue;
            };
            if !runtime::same_boot(&record, boot.as_deref()) {
                info!("{}: discarding runtime record from another boot", id);
                runtime::remove(&id, &self.config.paths);
                continue;
            }
            let Some(pid) = record.pid else {
                // A finished one-shot has no pid but its effect survives: keep
                // reporting running instead of re-running the work on restart.
                // This works on every platform because no process is involved.
                let keep = record.state == State::Running
                    && self
                        .slots
                        .get(&id)
                        .map(|slot| slot.machine.remain_after_exit)
                        .unwrap_or(false);
                if keep {
                    let since = Self::adopted_since(now, &record);
                    let last_exit = record.last_exit;
                    let failures = record.failures;
                    if let Some(slot) = self.slots.get_mut(&id) {
                        slot.adopt_finished(since, last_exit);
                    }
                    self.persisted
                        .insert(id.clone(), (State::Running, None, failures));
                } else {
                    runtime::remove(&id, &self.config.paths);
                }
                continue;
            };
            // Windows has no process to adopt: the job object took the tree
            // down with the daemon. Records are only cleaned up there.
            if cfg!(windows) {
                runtime::remove(&id, &self.config.paths);
                continue;
            }
            match runtime::pid_state(&record, Self::process_probe(pid)) {
                runtime::PidState::Gone => {
                    runtime::remove(&id, &self.config.paths);
                    continue;
                }
                runtime::PidState::Reused { expected, actual } => {
                    warn!(
                        "{}: pid {} was reused (starttime {} != {}); not adopting",
                        id, pid, actual, expected
                    );
                    runtime::remove(&id, &self.config.paths);
                    continue;
                }
                runtime::PidState::Same | runtime::PidState::Unknown => {}
            }
            let since = Self::adopted_since(now, &record);
            let ready = anchored_mono_ms(now, record.ready_since);
            let failures = record.failures;
            let last_exit = record.last_exit;
            let state = record.state;
            let was_stopping = state == State::Stopping;
            if let Some(slot) = self.slots.get_mut(&id) {
                slot.adopt(pid, state, since, ready, failures, last_exit, now.mono_ms);
                if was_stopping {
                    // Finish the interrupted stop the same way a live stop
                    // does: stop action first, then signals and the kill
                    // window. Otherwise the service's cleanup would be lost.
                    // The record does not say who asked, so the action sees the
                    // plain `stop` reason rather than a guessed one.
                    slot.request_stop(now.mono_ms, StopReason::Stop);
                }
                let adopted = slot.machine.state;
                let adopted_pid = slot.machine.pid;
                self.persisted
                    .insert(id.clone(), (adopted, adopted_pid, failures));
            }
        }
    }

    /// Remove runtime records that no longer have a definition. `reload` and
    /// the daemon's own removal paths clean records for services they know
    /// about; a definition deleted while the daemon was down leaves an orphan
    /// that nothing else would ever visit.
    pub(crate) fn sweep_runtime_records(&mut self) {
        let directory = self.config.paths.run_dir.clone();
        let Ok(entries) = fs::read_dir(&directory) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(id) = name.strip_suffix(".json") else {
                continue;
            };
            if self.registry.get(id).is_none() {
                // A definition deleted while the daemon was down: take its
                // process group down too (validated by pid + starttime), then
                // forget the record.
                if let Some(record) = runtime::load(id, &self.config.paths) {
                    if let Some(pid) = record.pid {
                        let state = runtime::pid_state(&record, Self::process_probe(pid));
                        if matches!(state, runtime::PidState::Same | runtime::PidState::Unknown) {
                            crate::platform::host::process::terminate_tree(pid);
                        }
                    }
                }
                runtime::remove(id, &self.config.paths);
                self.persisted.remove(id);
                warn!("dropped orphan runtime record {}", id);
            }
        }
    }

    /// Write the runtime record of every service whose (state, pid, failures)
    /// tuple changed since the last write. Stopped/failed services with no pid
    /// need no record: there is nothing to reconnect.
    pub(crate) fn persist_states(&mut self, now: Now) {
        let boot = self.boot_id.clone();
        for id in self.slots.keys().cloned().collect::<Vec<_>>() {
            // Read the same facet the status projection reads, and anchor it
            // with the kernel's one guarded conversion. A `since_ms` of 0 means
            // "never started", so it must stay absent on disk too.
            let live = self.slots[&id].machine.live();
            let (state, pid, failures) = (live.state, live.pid, live.failures);
            if self.persisted.get(&id) == Some(&(state, pid, failures)) {
                continue;
            }
            if pid.is_none() && matches!(state, State::Stopped | State::Failed) {
                runtime::remove(&id, &self.config.paths);
                self.persisted.remove(&id);
                continue;
            }
            let record = Record {
                boot: boot.clone(),
                state,
                pid,
                starttime: pid.and_then(crate::platform::host::process::start_time),
                since: anchored_secs(now, live.since_ms),
                ready_since: anchored_secs_opt(now, live.ready_since_ms),
                failures,
                last_exit: live.last_exit,
            };
            match runtime::save(&id, &record, &self.config.paths) {
                Ok(()) => {
                    self.persisted.insert(id, (state, pid, failures));
                }
                Err(error) => error!("{}: cannot persist state: {}", id, error),
            }
        }
    }

    pub(crate) fn set_wake(&mut self, wake: Arc<Wake>) {
        self.wake = wake.clone();
        self.factory.set_wake(wake.clone());
        for slot in self.slots.values_mut() {
            slot.set_wake(wake.clone());
        }
    }

    pub(crate) fn set_reactor(&mut self, reactor: Option<Arc<Reactor>>) {
        // The supervisor loop polls plugin stdio itself. Attaching the poll's
        // self-pipe to the shared wake latch means a management request or a
        // queued raw frame interrupts that poll instead of waiting it out.
        if let Some(reactor) = &reactor {
            // Synchronous waits on this thread (an install hook, a status
            // hook) get an explicit poll closure from hook_pump.
            self.wake.set_interrupt(reactor.interrupt_handle());
        }
        self.reactor = reactor.clone();
        self.stdio = match reactor {
            Some(reactor) => crate::wire::contract::stdio::Target::polled(reactor),
            None => crate::wire::contract::stdio::Target::threaded(),
        };
        self.factory.set_stdio(self.stdio.clone());
        for slot in self.slots.values_mut() {
            slot.set_stdio(self.stdio.clone());
        }
    }

    pub(crate) fn set_raw_hub(&mut self, raw_hub: Arc<crate::wire::contract::rawhub::RawHub>) {
        self.factory.set_raw_hub(raw_hub.clone());
        for slot in self.slots.values_mut() {
            slot.set_raw_hub(Some(raw_hub.clone()));
        }
    }

    pub(crate) fn start_enabled(&mut self, clock: &Clock) {
        let started = Instant::now();
        let mut eligible = Vec::new();
        for id in self.slots.keys().cloned().collect::<Vec<_>>() {
            let enabled = self
                .registry
                .get(&id)
                .map(|service| service.enabled)
                .unwrap_or(false);
            if !enabled {
                continue;
            }
            // A reload must not resurrect services the operator stopped or
            // bring a Failed service back behind their back.
            if self.slots[&id].machine.state != State::Stopped {
                continue;
            }
            let missing = self.registry.missing_dependencies(&id);
            if missing.is_empty() {
                eligible.push(id);
            } else {
                eprintln!(
                    "svcd: warning: skipping {}: missing dependencies: {}",
                    id,
                    missing.join(", ")
                );
            }
        }
        if eligible.is_empty() {
            log::note_slow("start_enabled", started);
            return;
        }
        let layers = match self.registry.start_layers(&eligible) {
            Ok(layers) => layers,
            Err(error) => {
                error!("start enabled: {}", error);
                log::note_slow("start_enabled", started);
                return;
            }
        };
        for layer in layers {
            let deadline = self.start_deadline(clock, layer.iter().map(String::as_str));
            if let Err(error) = self.start_layer(&layer, deadline, clock) {
                error!("start enabled: {}", error);
            }
        }
        log::note_slow("start_enabled", started);
    }

    /// One non-blocking supervisor pass: plugin stdio, the state machines and
    /// the periodic timers. Everything reads the same `now` snapshot, so a pass
    /// cannot mix two clock reads. The clock is only needed because a due rescan
    /// reconciles by blocking on start / stop.
    pub(crate) fn tick(&mut self, now: Now, clock: &Clock) {
        let started = Instant::now();
        self.pump(now);
        log::note_slow("pump", started);
        if self.rescan_interval_ms > 0 && now.mono_ms >= self.next_rescan_ms {
            self.next_rescan_ms = now.mono_ms.saturating_add(self.rescan_interval_ms);
            let started = Instant::now();
            self.rescan_if_changed(clock);
            log::note_slow("rescan", started);
        }
        if self.temp_cleanup_interval_ms > 0 && now.mono_ms >= self.next_temp_cleanup_ms {
            self.next_temp_cleanup_ms = now.mono_ms.saturating_add(self.temp_cleanup_interval_ms);
            let started = Instant::now();
            match crate::platform::storage::cleanup(&self.config.paths, self.temp_retention) {
                Ok(0) => {}
                Ok(removed) => {
                    info!("temp cleanup removed {} file(s)", removed)
                }
                Err(error) => warn!("temp cleanup failed: {}", error),
            }
            log::note_slow("temp cleanup", started);
        }
    }

    /// How long the loop may wait before the next timer is due, capped by the
    /// poll tick so shutdown and the reactor are serviced even with no timers.
    pub(crate) fn next_wait(&self, now: Now) -> Duration {
        let mut wait = POLL_WAIT;
        for (interval, next) in [
            (self.rescan_interval_ms, self.next_rescan_ms),
            (self.temp_cleanup_interval_ms, self.next_temp_cleanup_ms),
        ] {
            if interval > 0 {
                wait = wait.min(Duration::from_millis(next.saturating_sub(now.mono_ms)));
            }
        }
        wait
    }

    /// Bounded wait for the next event. With a reactor this wait *is* the
    /// plugin poll: the supervisor loop owns plugin stdio, and the shared wake
    /// latch writes to the poll's self-pipe so a management request returns at
    /// once. Without one (no reactor was installed) the condvar latch is the
    /// wake source.
    pub(crate) fn wait_for_work(&self, timeout: Duration) {
        if let Some(reactor) = &self.reactor {
            reactor.poll_once(timeout);
            return;
        }
        self.wake.wait_timeout(timeout);
    }

    pub(crate) fn statuses(&self, now: Now) -> Vec<Status> {
        self.registry
            .managed_values()
            .map(|service| self.status_for(service, now))
            .collect()
    }

    pub(crate) fn status_for_id(&self, id: &str, now: Now) -> Option<Status> {
        self.registry
            .get(id)
            .map(|service| self.status_for(service, now))
    }

    /// Liveness is a projection, not a second predicate: ask the same
    /// `Status::live` the list/status faces read, so "is this tool callable"
    /// and "what does status say" cannot drift apart (#230).
    pub(crate) fn is_live(&self, id: &str, now: Now) -> bool {
        self.status_for_id(id, now)
            .map(|status| status.summary.state == State::Running)
            .unwrap_or(false)
    }

    pub(crate) fn status_for(&self, service: &Service, now: Now) -> Status {
        if service.id == RESERVED_ID {
            return Status::builtin(service);
        }
        match self.slots.get(&service.id) {
            Some(slot) => Status::live(service, &slot.machine.live(), now),
            None => Status::of(service),
        }
    }

    pub(crate) fn pump(&mut self, now: Now) {
        // Plugin stdio belongs to this loop: one non-blocking pass before every
        // pump keeps plugin pipes moving inside a synchronous start/stop wait.
        // The reader-thread target needs no equivalent — its threads deliver
        // into the slot channels on their own.
        self.stdio.pump();
        let ids: Vec<String> = self.slots.keys().cloned().collect();
        let mut changed = false;
        for id in ids {
            let mut slot = self.slots.remove(&id).expect("slot");
            let before = slot.machine.state;
            slot.pump(now.mono_ms);
            if slot.machine.state != before {
                changed = true;
            }
            self.slots.insert(id, slot);
        }
        if changed {
            self.catalog_dirty = true;
        }
        self.persist_states(now);
    }
}
