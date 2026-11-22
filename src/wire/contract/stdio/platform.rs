//! The host's child-pipe implementation.
//!
//! `stdio.rs` works against the [`crate::wire::contract::stdio`] interface and
//! never names a platform. This module is the only place that creates a pipe or
//! changes its mode.
//!
//! The parent ends are the reactor's: on Unix they are anonymous pipes and the
//! reactor makes them non-blocking; on Windows they are overlapped named-pipe
//! ends so the IOCP can complete a read or a write. The child ends stay plain
//! inherited handles on both hosts.

#[cfg(unix)]
mod unix;
#[cfg(all(unix, test))]
pub(crate) use unix::pipe;
#[cfg(unix)]
pub(crate) use unix::{nonblocking, parent_read_pipe, parent_write_pipe};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::{parent_read_pipe, parent_write_pipe};
