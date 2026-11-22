//! The one place that decides which connection driver this host gets.
//!
//! The composition root builds a [`StartInput`] and calls [`start`]; nothing
//! above this module branches on the platform. Both Unix and Windows run the
//! poll loop (`server/http/conn/poll.rs`; Unix `poll(2)`, Windows `WSAPoll`,
//! ADR-0027). Adding another driver is one more `impl ConnectionDriver` plus the
//! alias below.

use std::io;

pub(crate) use crate::server::http::conn::{ServerHandle, StartInput};

/// The driver this host runs. Every host in the build matrix has a wait set;
/// ADR-0027 removed the per-connection thread driver.
type HostDriver = crate::server::http::conn::poll::Driver;

/// Start this host's driver.
pub(crate) fn start(input: StartInput) -> io::Result<ServerHandle> {
    use crate::server::http::conn::ConnectionDriver;
    HostDriver::start(input)
}
