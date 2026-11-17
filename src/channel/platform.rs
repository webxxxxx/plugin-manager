//! The host's control-channel implementation.
//!
//! `channel.rs` works against the [`crate::channel::ControlChannel`] interface
//! and never names a platform. This module is the only place that chooses an
//! implementation: UDS on Unix, a named pipe elsewhere. The two modules export
//! the same names, so adding a third host means one more `mod`/`use` pair and
//! no change above the seam.

#[cfg(unix)]
mod unix;
/// The bound half is named directly by the channel's platform-neutral tests.
#[cfg(all(test, unix))]
pub(crate) use unix::Listener;
#[cfg(unix)]
pub(crate) use unix::{ControlServer, Notify, Platform, Stream};

#[cfg(windows)]
mod windows;
/// The bound half is named directly by the channel's platform-neutral tests.
#[cfg(all(test, windows))]
pub(crate) use windows::Listener;
#[cfg(windows)]
pub(crate) use windows::{ControlServer, Notify, Platform, Stream};
