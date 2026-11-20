//! systemd's `Type=notify` handshake.
//!
//! `READY=1` means the management channel can accept commands; services may
//! still be starting. A no-op without `NOTIFY_SOCKET` and on non-Unix hosts.

#[cfg(unix)]
pub fn systemd(state: &str) {
    use std::os::unix::net::UnixDatagram;

    let Some(socket_path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    if socket_path.is_empty() {
        return;
    }
    if let Ok(socket) = UnixDatagram::unbound() {
        let _ = socket.send_to(state.as_bytes(), socket_path);
    }
}

#[cfg(not(unix))]
pub fn systemd(_state: &str) {}
