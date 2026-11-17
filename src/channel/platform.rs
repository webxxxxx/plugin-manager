//! The host's control-channel implementation.
//!
//! `channel.rs` works against the [`crate::channel::ControlChannel`] interface
//! and never names a platform. This module is the only place that chooses an
//! implementation: UDS on Unix, a named pipe elsewhere. The two modules export
//! the same names, so adding a third host means one more `mod`/`use` pair and
//! no change above the seam.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::{ControlBinding, Notify, Platform, Stream};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::{ControlBinding, Notify, Platform, Stream};
