use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

use crate::log::{self, LogSink};
use crate::paths;
use crate::registry::{Exec, Hook, Lifecycle, Ready, State};
use crate::rpc;
use crate::supervisor::{self, Action, Event, Machine};

/// One supervised service: the pure state machine plus the OS handles and
/// deadlines the machine's actions drive.
pub(crate) struct Slot {
    id: String,
    pub(crate) machine: Machine,
    exec: Exec,
    lifecycle: Option<Lifecycle>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    lines: Option<Receiver<String>>,
    start_deadline: Option<u64>,
    stop_deadline: Option<u64>,
    kill_deadline: Option<u64>,
    stop_hook: Option<HookRun>,
    stop_hook_started: bool,
    ping_id: u64,
}

struct HookRun {
    child: Child,
    deadline: u64,
}

impl Slot {
    pub(crate) fn new(id: String, exec: Exec, lifecycle: Option<Lifecycle>, enabled: bool) -> Self {
        Slot {
            id,
            machine: Machine::new(enabled),
            exec,
            lifecycle,
            child: None,
            stdin: None,
            lines: None,
            start_deadline: None,
            stop_deadline: None,
            kill_deadline: None,
            stop_hook: None,
            stop_hook_started: false,
            ping_id: 1,
        }
    }

    pub(crate) fn set_exec(&mut self, exec: Exec) {
        self.exec = exec;
    }

    pub(crate) fn set_lifecycle(&mut self, lifecycle: Option<Lifecycle>) {
        self.lifecycle = lifecycle;
    }

    /// Start the stop hook before the state machine begins stopping. The hook
    /// runs in the background; `SignalPolite` waits for it (or its timeout)
    /// before touching the supervised process.
    pub(crate) fn request_stop(&mut self, now: u64) {
        self.ensure_stop_hook(now);
        self.apply(Event::Stop, now);
    }

    pub(crate) fn pump(&mut self, now: u64) {
        self.drain_lines(now);
        self.check_exit(now);
        self.check_stop_hook(now);
        self.check_timers(now);
    }

    pub(crate) fn apply(&mut self, event: Event, now: u64) {
        let actions = self.machine.step(event, now);
        self.run_actions(actions, now);
    }

    fn run_actions(&mut self, actions: Vec<Action>, now: u64) {
        for action in actions {
            match action {
                Action::Spawn => {
                    self.abort_stop_hook();
                    self.stop_hook_started = false;
                    match self.spawn() {
                        Ok(pid) => {
                            let actions = self.machine.step(Event::Spawned { pid }, now);
                            self.run_actions(actions, now);
                            if self.exec.ready == Ready::Exec {
                                let actions = self.machine.step(Event::Ready { pid }, now);
                                self.run_actions(actions, now);
                            } else {
                                self.send_ping();
                            }
                        }
                        Err(error) => {
                            log::svcd(&format!("svcd: {}: spawn failed: {}", self.id, error));
                            let actions = self.machine.step(Event::SpawnFailed, now);
                            self.run_actions(actions, now);
                        }
                    }
                }
                Action::ArmStart => {
                    self.start_deadline = Some(now.saturating_add(supervisor::START_TIMEOUT_MS));
                }
                Action::SignalPolite => {
                    if !self.ensure_stop_hook(now) {
                        self.signal_polite();
                    }
                }
                Action::SignalForce => self.signal_force(),
                Action::ArmStop => {
                    self.stop_deadline = Some(now.saturating_add(supervisor::STOP_TIMEOUT_MS));
                }
                Action::ArmKill => {
                    self.kill_deadline = Some(now.saturating_add(supervisor::KILL_TIMEOUT_MS));
                }
                Action::ArmBackoff { .. } => {}
                Action::Publish | Action::Withdraw => {}
                Action::GiveUp => {
                    self.abort_stop_hook();
                    self.give_up();
                }
            }
        }
    }

    fn spawn(&mut self) -> Result<u32, String> {
        let start = self
            .lifecycle
            .as_ref()
            .and_then(|lifecycle| lifecycle.start.as_ref());
        let mut command = match start {
            Some(hook) => hook_command(&self.id, "start", hook),
            None => command_for(&self.exec),
        };
        match self.exec.ready {
            Ready::Notify => {
                command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
            }
            Ready::Exec => {
                command
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let mut child = command.spawn().map_err(|error| error.to_string())?;
        let pid = child.id();
        let log = match LogSink::open(&paths::logs_dir(), &self.id) {
            Ok(sink) => Some(Arc::new(Mutex::new(sink))),
            Err(error) => {
                log::svcd(&format!(
                    "svcd: {}: cannot open service log: {}",
                    self.id, error
                ));
                None
            }
        };

        if self.exec.ready == Ready::Notify {
            let stdin = child.stdin.take();
            let stdout = match child.stdout.take() {
                Some(stdout) => stdout,
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("piped stdout is missing".into());
                }
            };
            let stderr = match child.stderr.take() {
                Some(stderr) => stderr,
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("piped stderr is missing".into());
                }
            };

            let (sender, receiver) = mpsc::channel();
            let stdout_log = log.clone();
            if let Err(error) = thread::Builder::new()
                .name(format!("svcd-{}-stdout", self.id))
                .spawn(move || {
                    let reader = BufReader::new(stdout);
                    for line in reader.lines() {
                        match line {
                            Ok(line) => {
                                log_line(&stdout_log, &line);
                                if sender.send(line).is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                })
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }

            let stderr_log = log.clone();
            if let Err(error) = thread::Builder::new()
                .name(format!("svcd-{}-stderr", self.id))
                .spawn(move || {
                    let reader = BufReader::new(stderr);
                    for line in reader.lines().map_while(Result::ok) {
                        log_line(&stderr_log, &line);
                    }
                })
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }

            self.stdin = stdin;
            self.lines = Some(receiver);
            self.ping_id = 1;
        } else {
            self.stdin = None;
            let stdout = match child.stdout.take() {
                Some(stdout) => stdout,
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("piped stdout is missing".into());
                }
            };
            let stderr = match child.stderr.take() {
                Some(stderr) => stderr,
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("piped stderr is missing".into());
                }
            };

            let stdout_log = log.clone();
            if let Err(error) = thread::Builder::new()
                .name(format!("svcd-{}-stdout", self.id))
                .spawn(move || {
                    let reader = BufReader::new(stdout);
                    for line in reader.lines().map_while(Result::ok) {
                        log_line(&stdout_log, &line);
                    }
                })
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }

            let stderr_log = log.clone();
            if let Err(error) = thread::Builder::new()
                .name(format!("svcd-{}-stderr", self.id))
                .spawn(move || {
                    let reader = BufReader::new(stderr);
                    for line in reader.lines().map_while(Result::ok) {
                        log_line(&stderr_log, &line);
                    }
                })
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }
            self.lines = None;
        }

        self.child = Some(child);
        Ok(pid)
    }

    fn send_ping(&mut self) {
        if let Some(stdin) = self.stdin.as_mut() {
            let line = json!({
                "jsonrpc": "2.0",
                "id": self.ping_id,
                "method": "svcd.ping",
                "params": {},
            })
            .to_string();
            let _ = writeln!(stdin, "{}", line);
            let _ = stdin.flush();
        }
    }

    fn drain_lines(&mut self, now: u64) {
        let mut received = Vec::new();
        if let Some(lines) = &self.lines {
            while let Ok(line) = lines.try_recv() {
                received.push(line);
            }
        }
        for line in received {
            if self.machine.state != State::Starting {
                continue;
            }
            if let Ok(response) = serde_json::from_str::<rpc::Response>(&line) {
                if response.id.as_ref().and_then(Value::as_u64) == Some(self.ping_id) {
                    if let Some(pid) = self.machine.pid {
                        self.apply(Event::Ready { pid }, now);
                    }
                }
            }
        }
    }

    fn check_exit(&mut self, now: u64) {
        let status = self
            .child
            .as_mut()
            .and_then(|child| match child.try_wait() {
                Ok(Some(status)) => Some(status),
                _ => None,
            });
        if let Some(status) = status {
            let pid = self.machine.pid;
            self.clear_process();
            if let Some(pid) = pid {
                self.apply(
                    Event::Exited {
                        pid,
                        code: status.code(),
                    },
                    now,
                );
            }
        }
    }

    fn check_timers(&mut self, now: u64) {
        if self.machine.state == State::Starting {
            if let Some(deadline) = self.start_deadline {
                if now >= deadline {
                    self.start_deadline = None;
                    self.apply(Event::StartTimeout, now);
                }
            }
        }
        if self.machine.state == State::Stopping {
            if let Some(deadline) = self.stop_deadline {
                if now >= deadline {
                    self.stop_deadline = None;
                    self.apply(Event::StopTimeout, now);
                }
            }
            if let Some(deadline) = self.kill_deadline {
                if now >= deadline {
                    self.kill_deadline = None;
                    self.apply(Event::KillTimeout, now);
                }
            }
        }
        if self.machine.state == State::Backoff {
            if let Some(next_at) = self.machine.next_at_ms {
                if now >= next_at {
                    self.apply(Event::BackoffElapsed, now);
                }
            }
        }
    }

    fn ensure_stop_hook(&mut self, now: u64) -> bool {
        if self.stop_hook.is_some() {
            return true;
        }
        if self.stop_hook_started {
            return false;
        }
        self.stop_hook_started = true;
        let Some(hook) = self
            .lifecycle
            .as_ref()
            .and_then(|lifecycle| lifecycle.stop.as_ref())
        else {
            return false;
        };
        let mut command = hook_command(&self.id, "stop", hook);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        match command.spawn() {
            Ok(mut child) => {
                let log = LogSink::open(&paths::logs_dir(), &self.id)
                    .ok()
                    .map(|sink| Arc::new(Mutex::new(sink)));
                if let Some(stderr) = child.stderr.take() {
                    let log = log.clone();
                    let id = self.id.clone();
                    let _ = thread::Builder::new()
                        .name(format!("svcd-{}-stop-stderr", id))
                        .spawn(move || {
                            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                                log_line(&log, &line);
                            }
                        });
                }
                log::svcd(&format!(
                    "svcd: {}: stop hook started ({})",
                    self.id, hook.path
                ));
                self.stop_hook = Some(HookRun {
                    child,
                    deadline: now.saturating_add(hook.timeout_ms),
                });
                true
            }
            Err(error) => {
                log::svcd(&format!(
                    "svcd: {}: stop hook spawn failed: {}",
                    self.id, error
                ));
                false
            }
        }
    }

    fn check_stop_hook(&mut self, now: u64) {
        let Some(run) = self.stop_hook.as_mut() else {
            return;
        };
        let deadline = run.deadline;
        let outcome = match run.child.try_wait() {
            Ok(Some(status)) => Some(Ok(status)),
            Ok(None) if now >= deadline => {
                let _ = run.child.kill();
                let _ = run.child.wait();
                Some(Err("timed out"))
            }
            _ => None,
        };
        let Some(outcome) = outcome else {
            return;
        };
        self.stop_hook = None;
        match outcome {
            Ok(status) => log::svcd(&format!("svcd: {}: stop hook exited {}", self.id, status)),
            Err(reason) => log::svcd(&format!("svcd: {}: stop hook {}", self.id, reason)),
        }
        if self.machine.state == State::Stopping {
            self.signal_polite();
        }
    }

    fn abort_stop_hook(&mut self) {
        if let Some(mut run) = self.stop_hook.take() {
            let _ = run.child.kill();
            let _ = run.child.wait();
            log::svcd(&format!("svcd: {}: stop hook aborted", self.id));
        }
    }

    fn signal_polite(&mut self) {
        if let Some(stdin) = self.stdin.as_mut() {
            let line = rpc::notification("svcd.shutdown", json!({}));
            let _ = writeln!(stdin, "{}", line);
            let _ = stdin.flush();
        }
        self.stdin = None;
        #[cfg(unix)]
        {
            if let Some(pid) = self.machine.pid {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGTERM);
                }
            }
        }
    }

    pub(crate) fn signal_force(&mut self) {
        self.abort_stop_hook();
        #[cfg(windows)]
        {
            if let Some(child) = self.child.as_mut() {
                let _ = child.kill();
            }
        }
        #[cfg(unix)]
        {
            if let Some(pid) = self.machine.pid {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
        }
    }

    fn give_up(&mut self) {
        if self.machine.unkillable {
            log::svcd(&format!("svcd: {}: unkillable after stop timeout", self.id));
        } else {
            log::svcd(&format!(
                "svcd: {}: failed after {} consecutive failures",
                self.id, self.machine.failures
            ));
        }
        self.signal_force();
        self.clear_process();
    }

    fn clear_process(&mut self) {
        self.child = None;
        self.stdin = None;
        self.lines = None;
        self.start_deadline = None;
        self.stop_deadline = None;
        self.kill_deadline = None;
    }
}

fn command_for(exec: &Exec) -> Command {
    command_for_parts(&exec.path, &exec.args, &exec.env)
}

pub(crate) fn hook_command(id: &str, action: &str, hook: &Hook) -> Command {
    let mut command = command_for_parts(&hook.path, &hook.args, &hook.env);
    command
        .env("SVCD_SERVICE_ID", id)
        .env("SVCD_ACTION", action)
        .env("SVCD_DATA_DIR", paths::data_dir())
        .env("SVCD_CONFIG_DIR", paths::config_dir())
        .env("SVCD_LOG_DIR", paths::logs_dir());
    command
}

fn command_for_parts(path: &str, args: &[String], env: &BTreeMap<String, String>) -> Command {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if extension == "ps1" {
        #[cfg(windows)]
        {
            let mut command = Command::new("powershell");
            command
                .arg("-NoProfile")
                .arg("-ExecutionPolicy")
                .arg("Bypass")
                .arg("-File")
                .arg(path)
                .args(args)
                .envs(env);
            return command;
        }
        #[cfg(not(windows))]
        {
            let mut command = Command::new("pwsh");
            command
                .arg("-NoProfile")
                .arg("-File")
                .arg(path)
                .args(args)
                .envs(env);
            return command;
        }
    }
    #[cfg(windows)]
    if extension == "cmd" || extension == "bat" {
        let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
        let mut command = Command::new(shell);
        command.arg("/C").arg(path).args(args).envs(env);
        return command;
    }
    let mut command = Command::new(path);
    command.args(args).envs(env);
    command
}

fn log_line(log: &Option<Arc<Mutex<LogSink>>>, line: &str) {
    if let Some(sink) = log {
        if let Ok(mut sink) = sink.lock() {
            let _ = sink.line(line);
        }
    }
}
