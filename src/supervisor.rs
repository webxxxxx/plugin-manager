pub use crate::registry::State;

/// Full §1.3 schedule. `MAX_FAILURES = 5` means one crash loop enters `Failed`
/// on the fifth failure, so only 1/2/4/8s are armed in practice; 16s/30s are
/// the policy ceiling kept for a raised cap.
pub const BACKOFF_MS: [u64; 6] = [1_000, 2_000, 4_000, 8_000, 16_000, 30_000];
pub const STABLE_MS: u64 = 10_000;
pub const MAX_FAILURES: u32 = 5;
pub const START_TIMEOUT_MS: u64 = 30_000;
pub const STOP_TIMEOUT_MS: u64 = 10_000;
pub const KILL_TIMEOUT_MS: u64 = 5_000;
/// Whole-daemon graceful shutdown budget.
pub const SHUTDOWN_BUDGET_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Start,
    Spawned { pid: u32 },
    SpawnFailed,
    Ready { pid: u32 },
    Exited { pid: u32, code: Option<i32> },
    Stop,
    StartTimeout,
    StopTimeout,
    KillTimeout,
    BackoffElapsed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Spawn,
    ArmStart,
    SignalPolite,
    SignalForce,
    ArmStop,
    ArmKill,
    ArmBackoff { delay_ms: u64 },
    Publish,
    Withdraw,
    GiveUp,
}

pub fn backoff_delay(failures: u32) -> u64 {
    let index = failures.saturating_sub(1) as usize;
    BACKOFF_MS[index.min(BACKOFF_MS.len() - 1)]
}

/// Pure six-state supervisor core. The shell owns processes, pipes and timers;
/// this type owns only state, failure bookkeeping and the sequence of actions.
#[derive(Clone, Debug)]
pub struct Machine {
    pub state: State,
    pub pid: Option<u32>,
    pub failures: u32,
    pub since_ms: u64,
    pub ready_since_ms: Option<u64>,
    pub next_at_ms: Option<u64>,
    pub last_exit: Option<i32>,
    pub unkillable: bool,
    pub enabled: bool,
    /// A start timed out, so the following exit is a failed start, not a user stop.
    fail_after_stop: bool,
}

impl Machine {
    pub fn new(enabled: bool) -> Self {
        Machine {
            state: State::Stopped,
            pid: None,
            failures: 0,
            since_ms: 0,
            ready_since_ms: None,
            next_at_ms: None,
            last_exit: None,
            unkillable: false,
            enabled,
            fail_after_stop: false,
        }
    }

    pub fn step(&mut self, event: Event, now_ms: u64) -> Vec<Action> {
        use Action::*;
        use Event as E;
        use State as S;

        let mut out = Vec::new();
        match event {
            E::Start => {
                if matches!(self.state, S::Running | S::Starting | S::Stopping) {
                    return out;
                }
                self.failures = 0;
                self.pid = None;
                self.ready_since_ms = None;
                self.next_at_ms = None;
                self.unkillable = false;
                self.fail_after_stop = false;
                self.state = S::Starting;
                self.since_ms = now_ms;
                out.push(Spawn);
                out.push(ArmStart);
            }
            E::Spawned { pid } => {
                if self.state == S::Starting && self.pid.is_none() {
                    self.pid = Some(pid);
                }
            }
            E::SpawnFailed => {
                if self.state == S::Starting {
                    self.pid = None;
                    self.fail(now_ms, &mut out);
                }
            }
            E::Ready { pid } => {
                if self.state == S::Starting && self.pid == Some(pid) {
                    self.state = S::Running;
                    self.ready_since_ms = Some(now_ms);
                    self.since_ms = now_ms;
                    out.push(Publish);
                }
            }
            E::Exited { pid, code } => {
                // A late exit from an older instance must not touch the current one.
                if self.pid != Some(pid) {
                    return out;
                }
                self.pid = None;
                self.last_exit = code;
                self.next_at_ms = None;
                match self.state {
                    S::Starting => {
                        self.fail(now_ms, &mut out);
                    }
                    S::Running => {
                        out.push(Withdraw);
                        let stable = self
                            .ready_since_ms
                            .map(|ready| now_ms.saturating_sub(ready) >= STABLE_MS)
                            .unwrap_or(false);
                        if stable {
                            self.failures = 0;
                            self.state = S::Starting;
                            self.since_ms = now_ms;
                            self.ready_since_ms = None;
                            out.push(Spawn);
                            out.push(ArmStart);
                        } else {
                            self.fail(now_ms, &mut out);
                        }
                    }
                    S::Stopping => {
                        if self.fail_after_stop {
                            self.fail_after_stop = false;
                            self.fail(now_ms, &mut out);
                        } else {
                            self.state = S::Stopped;
                            self.since_ms = now_ms;
                            self.ready_since_ms = None;
                        }
                    }
                    S::Failed => {
                        self.ready_since_ms = None;
                    }
                    _ => {
                        self.state = S::Stopped;
                        self.since_ms = now_ms;
                        self.ready_since_ms = None;
                    }
                }
            }
            E::Stop => match self.state {
                S::Running | S::Starting => {
                    out.push(Withdraw);
                    self.state = S::Stopping;
                    self.since_ms = now_ms;
                    self.ready_since_ms = None;
                    self.next_at_ms = None;
                    self.fail_after_stop = false;
                    out.push(SignalPolite);
                    out.push(ArmStop);
                }
                S::Backoff => {
                    self.state = S::Stopped;
                    self.since_ms = now_ms;
                    self.next_at_ms = None;
                }
                _ => {}
            },
            E::StartTimeout => {
                if self.state == S::Starting {
                    self.fail_after_stop = true;
                    self.state = S::Stopping;
                    self.since_ms = now_ms;
                    self.ready_since_ms = None;
                    out.push(Withdraw);
                    out.push(SignalPolite);
                    out.push(ArmStop);
                }
            }
            E::StopTimeout => {
                if self.state == S::Stopping {
                    out.push(SignalForce);
                    out.push(ArmKill);
                }
            }
            E::KillTimeout => {
                if self.state == S::Stopping {
                    self.state = S::Failed;
                    self.unkillable = true;
                    self.since_ms = now_ms;
                    out.push(GiveUp);
                }
            }
            E::BackoffElapsed => {
                if self.state == S::Backoff {
                    self.next_at_ms = None;
                    if self.enabled {
                        self.state = S::Starting;
                        self.since_ms = now_ms;
                        out.push(Spawn);
                        out.push(ArmStart);
                    } else {
                        self.state = S::Stopped;
                        self.since_ms = now_ms;
                    }
                }
            }
        }
        out
    }

    fn fail(&mut self, now_ms: u64, out: &mut Vec<Action>) {
        self.failures = self.failures.saturating_add(1);
        self.ready_since_ms = None;
        self.since_ms = now_ms;
        if self.failures >= MAX_FAILURES {
            self.state = State::Failed;
            out.push(Action::GiveUp);
        } else {
            self.state = State::Backoff;
            let delay = backoff_delay(self.failures);
            self.next_at_ms = Some(now_ms.saturating_add(delay));
            out.push(Action::ArmBackoff { delay_ms: delay });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(machine: &mut Machine, pid: u32, now: u64) -> Vec<Action> {
        let actions = machine.step(Event::Start, now);
        assert!(actions.contains(&Action::Spawn));
        machine.step(Event::Spawned { pid }, now)
    }

    fn ready(machine: &mut Machine, pid: u32, now: u64) -> Vec<Action> {
        machine.step(Event::Ready { pid }, now)
    }

    #[test]
    fn backoff_schedule_is_one_two_four_eight_sixteen_thirty() {
        assert_eq!(
            [1, 2, 3, 4, 5, 6].map(backoff_delay),
            [1_000, 2_000, 4_000, 8_000, 16_000, 30_000]
        );
    }

    #[test]
    fn start_spawns_and_arms_the_start_timeout() {
        let mut machine = Machine::new(true);
        let actions = machine.step(Event::Start, 100);

        assert_eq!(machine.state, State::Starting);
        assert_eq!(machine.failures, 0);
        assert_eq!(actions, vec![Action::Spawn, Action::ArmStart]);
    }

    #[test]
    fn ready_moves_to_running_and_publishes() {
        let mut machine = Machine::new(true);
        start(&mut machine, 7, 100);
        let actions = ready(&mut machine, 7, 150);

        assert_eq!(machine.state, State::Running);
        assert_eq!(machine.pid, Some(7));
        assert_eq!(machine.ready_since_ms, Some(150));
        assert_eq!(actions, vec![Action::Publish]);
    }

    #[test]
    fn a_quick_crash_backs_off_and_gives_up_after_five_failures() {
        let mut machine = Machine::new(true);
        start(&mut machine, 1, 0);
        let mut now = 10;
        let mut delays = Vec::new();

        for _ in 0..4 {
            let actions = machine.step(
                Event::Exited {
                    pid: machine.pid.expect("pid"),
                    code: Some(1),
                },
                now,
            );
            assert_eq!(machine.state, State::Backoff);
            let delay = actions
                .iter()
                .find_map(|action| match action {
                    Action::ArmBackoff { delay_ms } => Some(*delay_ms),
                    _ => None,
                })
                .expect("arm backoff");
            delays.push(delay);
            assert_eq!(machine.next_at_ms, Some(now + delay));

            now += delay;
            let actions = machine.step(Event::BackoffElapsed, now);
            assert_eq!(machine.state, State::Starting);
            assert!(actions.contains(&Action::Spawn));
            machine.step(
                Event::Spawned {
                    pid: 10 + machine.failures,
                },
                now,
            );
        }

        assert_eq!(delays, vec![1_000, 2_000, 4_000, 8_000]);

        let actions = machine.step(
            Event::Exited {
                pid: machine.pid.expect("pid"),
                code: Some(1),
            },
            now,
        );
        assert_eq!(machine.state, State::Failed);
        assert_eq!(machine.failures, 5);
        assert_eq!(actions, vec![Action::GiveUp]);
    }

    #[test]
    fn a_run_of_ten_seconds_resets_failures_and_restarts_immediately() {
        let mut machine = Machine::new(true);
        start(&mut machine, 3, 0);
        ready(&mut machine, 3, 100);
        machine.failures = 3;

        let actions = machine.step(
            Event::Exited {
                pid: 3,
                code: Some(0),
            },
            10_100,
        );

        assert_eq!(machine.state, State::Starting);
        assert_eq!(machine.failures, 0);
        assert!(actions.contains(&Action::Withdraw));
        assert!(actions.contains(&Action::Spawn));
        assert!(actions.contains(&Action::ArmStart));
    }

    #[test]
    fn a_crash_before_ten_seconds_keeps_the_failure_count() {
        let mut machine = Machine::new(true);
        start(&mut machine, 3, 0);
        ready(&mut machine, 3, 100);

        let actions = machine.step(
            Event::Exited {
                pid: 3,
                code: Some(0),
            },
            9_000,
        );

        assert_eq!(machine.state, State::Backoff);
        assert_eq!(machine.failures, 1);
        assert!(actions.contains(&Action::Withdraw));
        assert!(actions.contains(&Action::ArmBackoff { delay_ms: 1_000 }));
    }

    #[test]
    fn a_late_exit_from_an_old_pid_is_ignored() {
        let mut machine = Machine::new(true);
        start(&mut machine, 3, 0);
        ready(&mut machine, 3, 10);
        let before = machine.clone();

        let actions = machine.step(
            Event::Exited {
                pid: 99,
                code: Some(1),
            },
            20,
        );

        assert!(actions.is_empty());
        assert_eq!(machine.state, before.state);
        assert_eq!(machine.pid, before.pid);
        assert_eq!(machine.failures, before.failures);
    }

    #[test]
    fn a_late_ready_from_an_old_pid_is_ignored() {
        let mut machine = Machine::new(true);
        start(&mut machine, 3, 0);

        let actions = machine.step(Event::Ready { pid: 99 }, 10);

        assert!(actions.is_empty());
        assert_eq!(machine.state, State::Starting);
    }

    #[test]
    fn stop_moves_through_polite_force_and_unkillable() {
        let mut machine = Machine::new(true);
        start(&mut machine, 5, 0);
        ready(&mut machine, 5, 10);

        assert_eq!(
            machine.step(Event::Stop, 20),
            vec![Action::Withdraw, Action::SignalPolite, Action::ArmStop]
        );
        assert_eq!(machine.state, State::Stopping);

        assert_eq!(
            machine.step(Event::StopTimeout, 10_020),
            vec![Action::SignalForce, Action::ArmKill]
        );

        assert_eq!(
            machine.step(Event::KillTimeout, 15_020),
            vec![Action::GiveUp]
        );
        assert_eq!(machine.state, State::Failed);
        assert!(machine.unkillable);
    }

    #[test]
    fn a_normal_stop_exits_to_stopped() {
        let mut machine = Machine::new(true);
        start(&mut machine, 5, 0);
        ready(&mut machine, 5, 10);
        machine.step(Event::Stop, 20);

        let actions = machine.step(
            Event::Exited {
                pid: 5,
                code: Some(0),
            },
            30,
        );

        assert!(actions.is_empty());
        assert_eq!(machine.state, State::Stopped);
        assert_eq!(machine.last_exit, Some(0));
    }

    #[test]
    fn stopping_from_backoff_needs_no_process() {
        let mut machine = Machine::new(true);
        start(&mut machine, 1, 0);
        machine.step(
            Event::Exited {
                pid: 1,
                code: Some(1),
            },
            10,
        );
        assert_eq!(machine.state, State::Backoff);

        let actions = machine.step(Event::Stop, 20);

        assert!(actions.is_empty());
        assert_eq!(machine.state, State::Stopped);
        assert_eq!(machine.next_at_ms, None);
    }

    #[test]
    fn a_start_timeout_counts_the_following_exit_as_a_failure() {
        let mut machine = Machine::new(true);
        start(&mut machine, 4, 0);

        let actions = machine.step(Event::StartTimeout, 30_000);
        assert_eq!(machine.state, State::Stopping);
        assert!(actions.contains(&Action::SignalPolite));

        let actions = machine.step(Event::Exited { pid: 4, code: None }, 30_010);

        assert_eq!(machine.state, State::Backoff);
        assert_eq!(machine.failures, 1);
        assert!(actions.contains(&Action::ArmBackoff { delay_ms: 1_000 }));
    }

    #[test]
    fn a_spawn_failure_backs_off() {
        let mut machine = Machine::new(true);
        machine.step(Event::Start, 0);

        let actions = machine.step(Event::SpawnFailed, 10);

        assert_eq!(machine.state, State::Backoff);
        assert_eq!(machine.failures, 1);
        assert!(actions.contains(&Action::ArmBackoff { delay_ms: 1_000 }));
    }

    #[test]
    fn disabled_services_stop_after_backoff_instead_of_respawning() {
        let mut machine = Machine::new(false);
        start(&mut machine, 1, 0);
        machine.step(
            Event::Exited {
                pid: 1,
                code: Some(1),
            },
            10,
        );
        assert_eq!(machine.state, State::Backoff);

        let actions = machine.step(Event::BackoffElapsed, 1_010);

        assert!(actions.is_empty());
        assert_eq!(machine.state, State::Stopped);
    }

    #[test]
    fn an_explicit_start_resets_failed() {
        let mut machine = Machine::new(true);
        machine.state = State::Failed;
        machine.failures = 5;
        machine.unkillable = true;

        let actions = machine.step(Event::Start, 100);

        assert_eq!(machine.state, State::Starting);
        assert_eq!(machine.failures, 0);
        assert!(!machine.unkillable);
        assert_eq!(actions, vec![Action::Spawn, Action::ArmStart]);
    }
}
