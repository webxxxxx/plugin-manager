use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use serde_json::{json, Value};

use crate::registry::{Exec, Ready, State};
use crate::rpc;
use crate::supervisor::{self, Action, Event, Machine};

/// One supervised service: the pure state machine plus the OS handles and
/// deadlines the machine's actions drive.
pub(crate) struct Slot {
    id: String,
    pub(crate) machine: Machine,
    exec: Exec,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    lines: Option<Receiver<String>>,
    start_deadline: Option<u64>,
    stop_deadline: Option<u64>,
    kill_deadline: Option<u64>,
    ping_id: u64,
}

impl Slot {
    pub(crate) fn new(id: String, exec: Exec, enabled: bool) -> Self {
        Slot {
            id,
            machine: Machine::new(enabled),
            exec,
            child: None,
            stdin: None,
            lines: None,
            start_deadline: None,
            stop_deadline: None,
            kill_deadline: None,
            ping_id: 1,
        }
    }

    pub(crate) fn pump(&mut self, now: u64) {
        self.drain_lines(now);
        self.check_exit(now);
        self.check_timers(now);
    }

    pub(crate) fn apply(&mut self, event: Event, now: u64) {
        let actions = self.machine.step(event, now);
        self.run_actions(actions, now);
    }

    fn run_actions(&mut self, actions: Vec<Action>, now: u64) {
        for action in actions {
            match action {
                Action::Spawn => match self.spawn() {
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
                        eprintln!("svcd: {}: spawn failed: {}", self.id, error);
                        let actions = self.machine.step(Event::SpawnFailed, now);
                        self.run_actions(actions, now);
                    }
                },
                Action::ArmStart => {
                    self.start_deadline = Some(now.saturating_add(supervisor::START_TIMEOUT_MS));
                }
                Action::SignalPolite => self.signal_polite(),
                Action::SignalForce => self.signal_force(),
                Action::ArmStop => {
                    self.stop_deadline = Some(now.saturating_add(supervisor::STOP_TIMEOUT_MS));
                }
                Action::ArmKill => {
                    self.kill_deadline = Some(now.saturating_add(supervisor::KILL_TIMEOUT_MS));
                }
                Action::ArmBackoff { .. } => {}
                Action::Publish | Action::Withdraw => {}
                Action::GiveUp => self.give_up(),
            }
        }
    }

    fn spawn(&mut self) -> Result<u32, String> {
        let mut command = Command::new(&self.exec.path);
        command.args(&self.exec.args).envs(&self.exec.env);
        match self.exec.ready {
            Ready::Notify => {
                command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit());
            }
            Ready::Exec => {
                command
                    .stdin(Stdio::null())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit());
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let mut child = command.spawn().map_err(|error| error.to_string())?;
        let pid = child.id();
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
            let (sender, receiver) = mpsc::channel();
            if let Err(error) = thread::Builder::new()
                .name(format!("svcd-{}-stdout", self.id))
                .spawn(move || {
                    let reader = BufReader::new(stdout);
                    for line in reader.lines() {
                        match line {
                            Ok(line) => {
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
            self.stdin = stdin;
            self.lines = Some(receiver);
            self.ping_id = 1;
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
            eprintln!("svcd: {}: unkillable after stop timeout", self.id);
        } else {
            eprintln!(
                "svcd: {}: failed after {} consecutive failures",
                self.id, self.machine.failures
            );
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
