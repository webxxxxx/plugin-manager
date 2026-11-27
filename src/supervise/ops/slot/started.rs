//! The post-ready action (`lifecycle.started`): it runs once the instance is
//! ready and before the service is reported running, so a failure is a failed
//! start (#335).
//!
//! The gate sits on the Ready event, not on the start request: every spawn path
//! (an explicit start, `svc restart`, a crash retry, a backoff retry) reaches
//! the machine through the same `Event::Ready`, so all of them run the action.
//! An adopted process never produced a Ready here and is not gated.

use super::*;

impl Slot {
    /// Consume a Ready event into the started action when this spawn owes one.
    /// `false` hands the event back to the caller, which applies it normally.
    pub(super) fn gate_ready(&mut self, pid: u32, now: u64) -> bool {
        if !crate::supervise::check::started::takes_ready(
            self.started_owed,
            self.machine.state,
            self.machine.pid,
            pid,
        ) {
            return false;
        }
        self.started_owed = false;
        match self.spawn_started(now) {
            Ok(()) => {
                self.ready_pending = Some(pid);
                // The start window now belongs to the action: its own deadline
                // bounds it, so the ready window cannot fire underneath it.
                self.start_deadline = None;
            }
            Err(error) => {
                error!(
                    "{}: started action failed to spawn: {}; the start fails",
                    self.id, error
                );
                self.start_failure = Some(format!("started action could not run: {error}"));
                let actions = self.machine.step(Event::StartTimeout, now);
                self.run_actions(actions, now);
            }
        }
        true
    }

    /// Poll the started action; a success settles the withheld Ready, anything
    /// else fails this start through the same window a start timeout uses.
    pub(super) fn check_started(&mut self, now: u64) {
        let Some(run) = self.started.as_mut() else {
            return;
        };
        let Some(end) = run.poll(now) else {
            return;
        };
        self.started = None;
        match end {
            ActionEnd::Exited(status) if status.success() => {
                let Some(pid) = self.ready_pending.take() else {
                    // The process exited while the action ran; `apply` already
                    // failed this start and killed the action.
                    return;
                };
                let actions = self.machine.step(Event::Ready { pid }, now);
                self.run_actions(actions, now);
            }
            ActionEnd::Exited(status) => {
                self.ready_pending = None;
                self.start_failure = Some(format!("started action exited {status}"));
                self.fail_started(now);
            }
            ActionEnd::TimedOut => {
                self.ready_pending = None;
                self.start_failure = Some("started action timed out".into());
                self.fail_started(now);
            }
        }
    }

    /// Kill and reap a running started action; a new attempt, a stop or an exit
    /// must never race it.
    pub(super) fn abort_started(&mut self) {
        self.ready_pending = None;
        self.started_owed = false;
        if let Some(mut run) = self.started.take() {
            warn!("{}: started action aborted", self.id);
            run.abort();
        }
    }

    fn fail_started(&mut self, now: u64) {
        if let Some(reason) = self.start_failure.as_deref() {
            error!("{}: {}; the start fails", self.id, reason);
        }
        // The instance is already up but unusable: stop it the way a start
        // timeout does (polite signal, kill window, then the stop action).
        let actions = self.machine.step(Event::StartTimeout, now);
        self.run_actions(actions, now);
    }

    fn spawn_started(&mut self, now: u64) -> Result<(), String> {
        let Some(hook) = self
            .lifecycle
            .as_ref()
            .and_then(|lifecycle| lifecycle.started.clone())
        else {
            return Ok(());
        };
        let command = hook_command(
            &self.id,
            "started",
            &hook,
            &self.paths,
            self.plugin_dir.as_deref(),
        );
        let (mut child, streams) =
            stdio::spawn(command, Plan::action()).map_err(|error| error.to_string())?;
        let _ = crate::platform::host::process::adopt(&child);
        // The action is not the service: it is not assigned to the service tree,
        // so the leader exiting underneath it still settles the instance.
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
                format!("svcd-{}-started-stderr", self.id),
                stderr,
                log,
                &self.limits,
            ) {
                // Without a reader the action's stderr pipe fills and blocks:
                // drop the attempt instead of leaking a process nobody holds.
                return abandon(&mut child, error.to_string()).map(|_| ());
            }
        }
        info!(
            "{}: started action running ({}); the start settles when it exits",
            self.id, hook.path
        );
        self.started = Some(ActionRun::new(child, now.saturating_add(hook.timeout_ms)));
        Ok(())
    }
}
