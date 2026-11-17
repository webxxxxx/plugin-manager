//! The host's anonymous-pipe implementation.
//!
//! `stdio.rs` works against the [`crate::wire::contract::stdio`] interface and never names a
//! platform. This module is the only place that creates a pipe or changes its
//! mode: `O_CLOEXEC` + `O_NONBLOCK` on Unix, `CreatePipe` + spawn-time handle
//! inheritance on Windows. Adding a third host means one more `mod`/`use` pair.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::{nonblocking, pipe};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::pipe;
