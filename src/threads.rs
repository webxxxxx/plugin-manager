//! Explicit stack sizes for short-lived runtime threads. The OS default is
//! usually 2 MiB of reserve per thread; embedded targets care about both the
//! reserve and the pages touched, so keep request/control threads bounded.

/// HTTP/WS request handlers that parse JSON and serve a response.
#[cfg(feature = "http")]
pub(crate) const REQUEST_STACK: usize = 256 * 1024;
/// Control-channel, plugin stdout/stderr and other short I/O loops.
pub(crate) const IO_STACK: usize = 128 * 1024;
