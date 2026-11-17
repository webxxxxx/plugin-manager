//! Explicit stack sizes for short-lived runtime threads. The OS default is
//! usually 2 MiB of reserve per thread; embedded targets care about both the
//! reserve and the pages touched, so keep request/control threads bounded.
//!
//! The values come from `limits` (compile-time `MEM_LEVEL` defaults plus
//! optional `svcd.conf` overrides). They are read when a thread is created;
//! changing the config requires a daemon restart.

/// HTTP/WS request handlers that parse JSON and serve a response.
pub(crate) fn request_stack(limits: &crate::platform::limits::Limits) -> usize {
    limits.request_stack_bytes
}

/// Control-channel, plugin stdout/stderr and other short I/O loops.
pub(crate) fn io_stack(limits: &crate::platform::limits::Limits) -> usize {
    limits.io_stack_bytes
}

/// Streaming package/asset handlers need more headroom than a plain I/O loop:
/// they parse tar/JSON and write through the HTTP layer. Keep a floor even in
/// the low profile, where `request_stack` can be 128 KiB.
pub(crate) fn transfer_stack(limits: &crate::platform::limits::Limits) -> usize {
    request_stack(limits).max(256 * 1024)
}
