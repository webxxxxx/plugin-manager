//! 监督的纯判定：启动收敛、重启退避、稳定窗口。
//!

pub mod backoff;
pub mod oneshot;
pub mod peer;
pub mod reconcile;
pub mod restart;
pub mod start;
pub mod started;
pub mod stop_reason;
