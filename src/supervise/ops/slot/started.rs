//! The post-ready action (`lifecycle.started`): it runs once the instance is
//! ready and before the service is reported running, so a failure is a failed
//! start (#335).
//!
//! The gate sits on the Ready event, not on the start request: every spawn path
//! (an explicit start, `svc restart`, a crash retry, a backoff retry) reaches
//! the machine through the same `Event::Ready`, so all of them run the action.
//! An adopted process never produced a Ready here and is not gated.

use super::*;

/// Why the last attempt failed to start. `timeout` picks the API's 504 instead
/// of a generic 500 (#341).
pub(crate) struct StartFailure {
    pub(crate) reason: String,
    pub(crate) timeout: bool,
}

/// One spawn's post-ready phase: the action, whether this spawn still owes it,
/// the pid whose Ready is withheld, and why the attempt failed.
pub(super) struct StartedFlow {
    run: Option<ActionRun>,
    owed: bool,
    pending: Option<u32>,
    failure: Option<StartFailure>,
}

impl StartedFlow {
    pub(super) fn new() -> StartedFlow {
        StartedFlow {
            run: None,
            owed: false,
            pending: None,
            failure: None,
        }
    }

    /// Whether the action is running: the start window is suspended while it
    /// is, because its own timeout is the bound for this phase (#341).
    pub(super) fn running(&self) -> bool {
        self.run.is_some()
    }
}

impl Slot {
    /// The gate in front of every event: `true` when the post-ready phase
    /// consumed it.
    pub(super) fn intercept(&mut self, event: Event, now: u64) -> bool {
        if let Event::Ready { pid } = event {
            if self.gate_ready(pid, now) {
                return true;
            }
        }
        // Anything else that reaches the machine while the started action runs
        // must not race it: a stop, an exit, a start timeout, a new attempt all
        // kill the action first. The settle path clears the run before applying
        // its own Ready, so a good run is never aborted here.
        if self.started.run.is_some() {
            if let Event::Exited { code, .. } = event {
                if self.machine.state == State::Starting {
                    self.note_start_failure(format!(
                        "service exited (code {}) while the started action ran",
                        code.map(|code| code.to_string())
                            .unwrap_or_else(|| "none".into())
                    ));
                }
            }
            self.abort_started();
        }
        // A port-ready instance that dies before accepting leaves the start
        // request waiting for a window it can no longer use: name the address
        // so the failure is not a generic timeout (#341).
        if let Event::Exited { .. } = event {
            if self.machine.state == State::Starting {
                if let Some(addr) = self.port.as_ref().map(|probe| probe.addr()) {
                    self.note_start_failure(format!("port {} never accepted a connection", addr));
                }
            }
        }
        false
    }

    /// A new attempt: kill the previous action, drop its outcome, and owe a new
    /// one when the definition declares it.
    pub(super) fn reset_started(&mut self) {
        self.abort_started();
        self.started.failure = None;
        self.started.owed = self
            .lifecycle
            .as_ref()
            .is_some_and(|lifecycle| lifecycle.started.is_some());
    }

    /// Why the last attempt failed to start; the start request reads it once so
    /// it fails fast instead of waiting out the window.
    pub(crate) fn take_start_failure(&mut self) -> Option<StartFailure> {
        self.started.failure.take()
    }

    /// Record why this attempt is failing before the state machine settles it
    /// (a spawn that found the port taken, an action that exited non-zero).
    pub(super) fn note_start_failure(&mut self, reason: String) {
        self.started.failure = Some(StartFailure {
            reason,
            timeout: false,
        });
    }

    /// A start window that ran out: the caller maps it to a 504, not a 500.
    pub(super) fn note_start_timeout(&mut self, reason: String) {
        self.started.failure = Some(StartFailure {
            reason,
            timeout: true,
        });
    }

    /// Consume a Ready event into the started action when this spawn owes one.
    fn gate_ready(&mut self, pid: u32, now: u64) -> bool {
        if !crate::supervise::check::started::takes_ready(
            self.started.owed,
            self.machine.state,
            self.machine.pid,
            pid,
        ) {
            return false;
        }
        self.started.owed = false;
        match self.spawn_started(now) {
            Ok(()) => {
                self.started.pending = Some(pid);
                // The start window is suspended while the action runs
                // (`check_timers` skips it), so only the action's own deadline
                // bounds this phase (#341).
                self.start_deadline = None;
            }
            Err(error) => {
                error!(
                    "{}: started action failed to spawn: {}; the start fails",
                    self.id, error
                );
                self.note_start_failure(format!("started action could not run: {error}"));
                let actions = self.machine.step(Event::StartTimeout, now);
                self.run_actions(actions, now);
            }
        }
        true
    }

    /// Poll the started action; a success settles the withheld Ready, anything
    /// else fails this start through the window a start timeout already uses.
    pub(super) fn check_started(&mut self, now: u64) {
        let Some(run) = self.started.run.as_mut() else {
            return;
        };
        let Some(end) = run.poll(now) else {
            return;
        };
        self.started.run = None;
        match end {
            ActionEnd::Exited(status) if status.success() => {
                let Some(pid) = self.started.pending.take() else {
                    // The process exited while the action ran; `intercept`
                    // already failed this start and killed the action.
                    return;
                };
                let actions = self.machine.step(Event::Ready { pid }, now);
                self.run_actions(actions, now);
            }
            ActionEnd::Exited(status) => {
                self.started.pending = None;
                self.note_start_failure(format!("started action exited {status}"));
                self.fail_started(now);
            }
            ActionEnd::TimedOut => {
                self.started.pending = None;
                self.note_start_timeout("started action timed out".into());
                self.fail_started(now);
            }
        }
    }

    /// Kill and reap a running action; a new attempt, a stop or an exit must
    /// never race it.
    pub(super) fn abort_started(&mut self) {
        self.started.pending = None;
        self.started.owed = false;
        if let Some(mut run) = self.started.run.take() {
            warn!("{}: started action aborted", self.id);
            run.abort();
        }
    }

    fn fail_started(&mut self, now: u64) {
        if let Some(failure) = self.started.failure.as_ref() {
            error!("{}: {}; the start fails", self.id, failure.reason);
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
            // `owed` only opens this gate when the definition declares the
            // action, so a missing hook is a bug in that predicate, not a
            // silent success: failing loudly beats a start with no deadline.
            return Err("no started action".into());
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
        self.started.run = Some(ActionRun::new(child, now.saturating_add(hook.timeout_ms)));
        Ok(())
    }
}
