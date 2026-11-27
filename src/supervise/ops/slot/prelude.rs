//! The start action (`lifecycle.start`): a preparation step that must exit
//! before the supervised process exists (#323). A definition whose start action
//! never exits — the pre-#323 shape where start *was* the process — fails here
//! with a message that names the fix.

use super::*;

impl Slot {
    /// Start one attempt: the start action first when the definition declares
    /// one, then the supervised process.
    pub(super) fn begin_start(&mut self, now: u64) {
        let Some(hook) = self.start_action() else {
            self.spawn_after_prelude(now);
            return;
        };
        let path = hook.path.clone();
        if let Err(error) = self.spawn_prelude(now) {
            error!("{}: start action failed to spawn: {}", self.id, error);
            // The action never ran, but a retry must still start from a clean
            // slate: the stop action owns that cleanup.
            self.ensure_stop_action(now);
            let actions = self.machine.step(Event::SpawnFailed, now);
            self.run_actions(actions, now);
            return;
        }
        info!("{}: start action running ({})", self.id, path);
        // The process window opens when the preparation finishes, not now.
        self.start_deadline = None;
    }

    /// A preparation step is not the service: it must exit before the process
    /// exists. A definition whose start action never exits (the pre-#323 shape)
    /// fails here with a message that names the fix.
    pub(super) fn check_prelude(&mut self, now: u64) {
        let Some(run) = self.prelude.as_mut() else {
            return;
        };
        let Some(end) = run.poll(now) else {
            return;
        };
        self.prelude = None;
        match end {
            ActionEnd::Exited(status) if status.success() => {
                self.spawn_after_prelude(now);
                if self.machine.state == State::Starting {
                    self.start_deadline = Some(now.saturating_add(supervisor::START_TIMEOUT_MS));
                }
            }
            ActionEnd::Exited(status) => {
                error!(
                    "{}: start action exited {}; the service was not started",
                    self.id, status
                );
                self.ensure_stop_action(now);
                let actions = self.machine.step(Event::SpawnFailed, now);
                self.run_actions(actions, now);
            }
            ActionEnd::TimedOut => {
                error!(
                    "{}: start action did not exit within its timeout; if it is the service process, put it in exec.args and drop lifecycle.start",
                    self.id
                );
                self.ensure_stop_action(now);
                let actions = self.machine.step(Event::SpawnFailed, now);
                self.run_actions(actions, now);
            }
        }
    }

    fn spawn_prelude(&mut self, now: u64) -> Result<(), String> {
        let Some(hook) = self.start_action().cloned() else {
            return Err("no start action".into());
        };
        let command = hook_command(
            &self.id,
            "start",
            &hook,
            &self.paths,
            self.plugin_dir.as_deref(),
        );
        let (mut child, streams) =
            stdio::spawn(command, Plan::action()).map_err(|error| error.to_string())?;
        let _ = crate::platform::host::process::adopt(&child);
        let log = LogWriter::open(
            &self.paths.logs_dir,
            &format!("{}-hooks", self.id),
            &format!("hook:{}", self.id),
            &self.limits,
            log::clock(),
            false,
        )
        .ok()
        .map(|sink| Arc::new(Mutex::new(sink)));
        if let Some(stderr) = streams.stderr {
            if let Err(error) = stdio::log_lines(
                format!("svcd-{}-start-stderr", self.id),
                stderr,
                log,
                &self.limits,
            ) {
                // Without a reader the action's stderr pipe fills and blocks:
                // drop the attempt instead of leaking a process nobody holds.
                return abandon(&mut child, error.to_string()).map(|_| ());
            }
        }
        self.prelude = Some(ActionRun::new(child, now.saturating_add(hook.timeout_ms)));
        Ok(())
    }

    pub(super) fn abort_prelude(&mut self) {
        if let Some(mut run) = self.prelude.take() {
            warn!("{}: start action aborted", self.id);
            run.abort();
        }
    }

    fn spawn_after_prelude(&mut self, now: u64) {
        match self.spawn_process() {
            Ok(pid) => {
                self.apply(Event::Spawned { pid }, now);
                match self.exec.ready {
                    // Through `apply`, so a declared started action gates this
                    // Ready exactly like a protocol peer's first response.
                    Ready::Exec => self.apply(Event::Ready { pid }, now),
                    // The connection is the readiness signal; `check_port_ready`
                    // settles it once the probe reports one.
                    Ready::Port(addr) => self.start_port_probe(pid, addr),
                    Ready::Notify => self.send_ping(),
                }
            }
            Err(error) => {
                error!("{}: spawn failed: {}", self.id, error);
                // The reason travels back to the start request; a port that is
                // already taken is otherwise only a log line.
                self.note_start_failure(format!("spawn failed: {}", error));
                // The start action may have created things the process never
                // used; the stop action cleans them up.
                self.ensure_stop_action(now);
                let actions = self.machine.step(Event::SpawnFailed, now);
                self.run_actions(actions, now);
            }
        }
    }
}
