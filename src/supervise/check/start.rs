//! 启动收敛：一个 pending 服务的当前状态该怎么处置。
//!
//! `Stopped` 有两个来源：一次性服务干完退出（完成），和通道对端在启动窗口内
//! 自己退掉（死亡）。两者在状态上同形，所以判定还要知道服务欠不欠一条活通道。

use crate::kernel::registry::State;

/// 启动等待对一个 pending 服务的下一步。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// 启动完成：服务已 ready，或非通道对端的一次性服务干净退出了。
    Settled,
    /// 通道对端在启动窗口内停掉了：通道没了，这次启动没有产出可用的服务。
    Died,
    /// 服务起不来。
    Failed,
    /// 仍在启动：继续等。
    Waiting,
}

/// 按状态与「是否通道对端」决定这一步怎么走。
///
/// `Running` 恒算完成；`Stopped` 只在**不是通道对端**时算完成——`ready: exec`
/// 的一次性服务 spawn 完就退是 #159 的既有结论；通道对端（通知型插件、raw
/// 传输）落到 `Stopped` 时通道已经没了，调用方应当拿到失败。
pub(crate) fn start_step(state: State, channel_peer: bool) -> Step {
    match state {
        State::Running => Step::Settled,
        State::Stopped if channel_peer => Step::Died,
        State::Stopped => Step::Settled,
        State::Failed => Step::Failed,
        _ => Step::Waiting,
    }
}

#[cfg(test)]
mod tests {
    use super::{start_step, Step};
    use crate::kernel::registry::State;

    #[test]
    fn a_clean_one_shot_exit_settles_a_start() {
        // Regression for #159: a `ready: exec` one-shot that exits right after
        // spawn is a finished start, not a timeout.
        assert_eq!(start_step(State::Stopped, false), Step::Settled);
        assert_eq!(start_step(State::Running, false), Step::Settled);
    }

    #[test]
    fn a_channel_peer_that_stops_during_start_is_a_death() {
        // #183: a notify / raw peer that answered ready and then exited left no
        // channel behind, so reporting a successful start would be a lie.
        assert_eq!(start_step(State::Stopped, true), Step::Died);
        assert_eq!(start_step(State::Running, true), Step::Settled);
    }

    #[test]
    fn an_in_flight_start_keeps_waiting() {
        for state in [State::Starting, State::Stopping, State::Backoff] {
            assert_eq!(start_step(state, false), Step::Waiting, "{state:?}");
            assert_eq!(start_step(state, true), Step::Waiting, "{state:?}");
        }
    }

    #[test]
    fn a_failed_start_is_reported_as_failed() {
        assert_eq!(start_step(State::Failed, false), Step::Failed);
        assert_eq!(start_step(State::Failed, true), Step::Failed);
    }
}
