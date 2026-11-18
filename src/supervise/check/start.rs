//! 启动收敛：一个 pending 服务的当前状态该怎么处置。
//!
//! `Stopped` 有两个来源：一次性服务干完退出（完成），和通道对端在启动窗口内
//! 自己退掉（失去通道）。两者在状态上同形，所以判定还要知道服务欠不欠一条活通道。

use crate::kernel::registry::State;

/// 启动等待对一个 pending 服务的下一步。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// 服务已经 ready：start 可以返回。
    Settled,
    /// 非通道对端的一次性服务干净退出了：启动完成（#159）。
    Finished,
    /// 通道对端在启动窗口内停掉了：通道没了，这次启动没有产出可用的服务。
    ChannelLost,
    /// 服务起不来。
    Failed,
    /// 仍在启动：继续等。
    Waiting,
}

/// 按状态与「是否通道对端」决定这一步怎么走。
///
/// `Running` 恒算完成；`Stopped` 在通道对端上是 `ChannelLost`，在其余服务上
/// 是 `Finished`——`ready: exec` 的一次性服务 spawn 完就退是 #159 的既有结论。
pub(crate) fn start_step(state: State, channel_peer: bool) -> Step {
    match state {
        State::Running => Step::Settled,
        State::Stopped if channel_peer => Step::ChannelLost,
        State::Stopped => Step::Finished,
        State::Failed => Step::Failed,
        State::Starting | State::Stopping | State::Backoff => Step::Waiting,
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
        assert_eq!(start_step(State::Stopped, false), Step::Finished);
        assert_eq!(start_step(State::Running, false), Step::Settled);
    }

    #[test]
    fn a_channel_peer_that_stops_during_start_loses_the_channel() {
        // #183: a notify / raw peer that answered ready and then exited left no
        // channel behind, so reporting a successful start would be a lie.
        assert_eq!(start_step(State::Stopped, true), Step::ChannelLost);
        assert_eq!(start_step(State::Running, true), Step::Settled);
    }

    #[test]
    fn the_whole_state_matrix_is_decided() {
        for channel_peer in [false, true] {
            let stopped = if channel_peer {
                Step::ChannelLost
            } else {
                Step::Finished
            };
            for (state, expected) in [
                (State::Running, Step::Settled),
                (State::Stopped, stopped),
                (State::Failed, Step::Failed),
                (State::Starting, Step::Waiting),
                (State::Stopping, Step::Waiting),
                (State::Backoff, Step::Waiting),
            ] {
                assert_eq!(
                    start_step(state, channel_peer),
                    expected,
                    "{state:?} channel_peer={channel_peer}"
                );
            }
        }
    }
}
