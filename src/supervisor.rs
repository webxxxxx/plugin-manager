pub use crate::registry::State;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Start,
    Ready,
    Exited,
    Stop,
    StartTimeout,
    StopTimeout,
    KillTimeout,
    BackoffElapsed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Spawn,
    SignalPolite,
    SignalForce,
    ArmStart,
    ArmStop,
    ArmKill,
    ArmBackoff,
    Publish,
    Withdraw,
    GiveUp,
}

pub struct Service {
    pub state: State,
    pub pid: Option<u32>,
    pub failures: u32,
    pub ready_at: u64,
}

impl Service {
    pub fn new() -> Self {
        Service {
            state: State::Stopped,
            pid: None,
            failures: 0,
            ready_at: 0,
        }
    }

    pub fn step(&mut self, ev: Event, now: u64) -> Vec<Action> {
        use Action::*;
        use Event as E;
        use State as S;
        let mut out = Vec::new();
        match (self.state, ev) {
            (S::Stopped, E::Start) => {
                self.state = S::Starting;
                self.failures = 0;
                out.push(Spawn);
                out.push(ArmStart);
            }
            (S::Starting, E::Ready) => {
                self.state = S::Running;
                self.ready_at = now;
                out.push(Publish);
            }
            (S::Starting, E::Exited) => {
                self.failures += 1;
                if self.failures >= 5 {
                    self.state = S::Failed;
                    out.push(GiveUp);
                } else {
                    self.state = S::Backoff;
                    out.push(ArmBackoff);
                }
            }
            (S::Starting, E::StartTimeout) => {
                self.state = S::Stopping;
                out.push(SignalPolite);
                out.push(ArmStop);
            }
            (S::Running, E::Exited) => {
                self.state = S::Backoff;
                out.push(Withdraw);
                out.push(ArmBackoff);
            }
            (S::Running, E::Stop) => {
                self.state = S::Stopping;
                out.push(SignalPolite);
                out.push(ArmStop);
            }
            (S::Stopping, E::Exited) => {
                self.state = S::Stopped;
                out.push(Withdraw);
            }
            (S::Stopping, E::StopTimeout) => {
                out.push(SignalForce);
                out.push(ArmKill);
            }
            (S::Stopping, E::KillTimeout) => {
                self.state = S::Failed;
                out.push(GiveUp);
            }
            (S::Backoff, E::BackoffElapsed) => {
                self.state = S::Starting;
                out.push(Spawn);
                out.push(ArmStart);
            }
            (S::Failed, E::Start) => {
                self.state = S::Starting;
                self.failures = 0;
                out.push(Spawn);
                out.push(ArmStart);
            }
            _ => {}
        }
        out
    }
}
