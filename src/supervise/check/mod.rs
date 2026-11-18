//! 监督的纯判定：重启退避、稳定窗口。
//!
//! `reconcile` 仍在 `daemon.rs`，随内容拆分再建。

pub mod backoff;
pub mod oneshot;
pub mod restart;
