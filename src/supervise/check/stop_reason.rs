//! 停止原因：`lifecycle.stop` 跑的时候，这次实例是怎么结束的。
//!
//! `stop` 钩子在每条路径上都跑（主动停、崩溃、崩溃重启前、启动失败），过去
//! 它不知道自己为什么被调用。原因只在这里拼一次，写进 `SVCD_STOP_REASON`；
//! 一个钩子按原因分支，胜过一个状态一个回调。

use crate::supervise::model::supervisor::Cause;

/// 停止动作的触发原因；`as_str` 是 `SVCD_STOP_REASON` 的拼写。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopReason {
    /// 显式 stop（`svc stop`，以及 `svc restart` 的停止半程）。
    Stop,
    /// 守护进程整体退出（`stop_all`）。
    Shutdown,
    /// 进程自己结束且不正常，会按退避重试。
    Crash,
    /// 进程正常结束，不再回来。
    Exit,
    /// 这次实例从没可用过：start 动作、spawn、started 动作或就绪超时。
    StartFailed,
}

impl StopReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            StopReason::Stop => "stop",
            StopReason::Shutdown => "shutdown",
            StopReason::Crash => "crash",
            StopReason::Exit => "exit",
            StopReason::StartFailed => "start-failed",
        }
    }

    /// 实例自己结束时，原因由结束方式决定。
    pub(crate) fn from_cause(cause: Cause) -> StopReason {
        match cause {
            Cause::Crash => StopReason::Crash,
            Cause::Exit => StopReason::Exit,
            Cause::StartFailed => StopReason::StartFailed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::StopReason;
    use crate::supervise::model::supervisor::Cause;

    #[test]
    fn every_cause_maps_to_one_spelling() {
        assert_eq!(StopReason::from_cause(Cause::Crash).as_str(), "crash");
        assert_eq!(StopReason::from_cause(Cause::Exit).as_str(), "exit");
        assert_eq!(
            StopReason::from_cause(Cause::StartFailed).as_str(),
            "start-failed"
        );
        assert_eq!(StopReason::Stop.as_str(), "stop");
        assert_eq!(StopReason::Shutdown.as_str(), "shutdown");
    }
}
