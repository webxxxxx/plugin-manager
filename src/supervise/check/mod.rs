//! 监督的纯判定：重启退避、稳定窗口。
//!
//! `oneshot` / `reconcile` 仍在 `supervisor.rs` / `slot.rs` / `daemon.rs`，
//! 随内容拆分再建。

pub mod backoff;
pub mod restart;
