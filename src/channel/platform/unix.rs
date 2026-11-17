//! Unix control channel: a Unix domain socket plus POSIX file modes.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::time::Duration;

use crate::channel::{ControlChannel, ControlListener, ControlStream, PendingCall};
use crate::wake::Wake;

pub(crate) struct Platform;

pub(crate) struct Listener(UnixListener);

pub(crate) struct Stream(UnixStream);

/// The bound server half. Unix hands the listener to the server loop; nothing
/// is accepted off-thread.
pub(crate) struct ControlServer {
    listener: Listener,
    pending: SyncSender<PendingCall>,
    wake: Arc<Wake>,
}

impl ControlServer {
    /// Split the server into the listener and the plumbing its loop owns.
    /// Called by `server::control` (Unix-only), so the raw socket type does not
    /// leak into `daemon.rs`.
    pub(crate) fn into_parts(self) -> (UnixListener, SyncSender<PendingCall>, Arc<Wake>) {
        (self.listener.0, self.pending, self.wake)
    }
}

/// Wake pipe shared with `Reply::with_notify`: the supervisor writes one byte
/// after queueing a reply so the server loop can write it immediately.
#[derive(Clone)]
pub(crate) struct Notify(Arc<UnixStream>);

impl Notify {
    pub(crate) fn new(stream: Arc<UnixStream>) -> Self {
        Notify(stream)
    }

    pub(crate) fn wake(&self) {
        let _ = (&*self.0).write(&[1]);
    }
}

impl Listener {
    pub(crate) fn bind(config: &Path) -> io::Result<Self> {
        let path = config.join("svcd.sock");
        if path.exists() {
            if UnixStream::connect(&path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "svcd control socket is already in use",
                ));
            }
            let _ = fs::remove_file(&path);
        }
        let listener = UnixListener::bind(&path)?;
        // The svcd group may talk to the socket; local management is
        // group/root only.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o660))?;
        Ok(Listener(listener))
    }
}

impl ControlListener for Listener {
    type Stream = Stream;

    fn incoming(&self) -> impl Iterator<Item = io::Result<Stream>> + '_ {
        self.0.incoming().map(|stream| stream.map(Stream))
    }
}

impl ControlStream for Stream {
    /// Wait until the socket is readable (or the timeout expires).
    ///
    /// The pooled stream keeps its blocking mode: poll decides when a read is
    /// safe, so a deadline never has to mutate the socket that the next request
    /// will reuse.
    fn wait_readable(&self, timeout: Duration) -> io::Result<bool> {
        let mut descriptor = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
        let ready = unsafe { libc::poll(&mut descriptor, 1, millis) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(error);
        }
        Ok(ready > 0)
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl ControlChannel for Platform {
    type Stream = Stream;
    type Listener = Listener;
    type Server = ControlServer;

    fn bind(config: &Path) -> io::Result<Self::Listener> {
        Listener::bind(config)
    }

    fn serve(
        listener: Self::Listener,
        pending: SyncSender<PendingCall>,
        wake: Arc<Wake>,
    ) -> io::Result<Self::Server> {
        Ok(ControlServer {
            listener,
            pending,
            wake,
        })
    }

    fn connect(config: &Path) -> io::Result<Self::Stream> {
        UnixStream::connect(config.join("svcd.sock")).map(Stream)
    }

    fn remove_socket(config: &Path) {
        let _ = fs::remove_file(config.join("svcd.sock"));
    }

    fn set_private(path: &Path) -> io::Result<()> {
        // The svcd group may read the token; local management is group/root
        // only.
        fs::set_permissions(path, fs::Permissions::from_mode(0o640))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn config_dir(tag: &str) -> std::path::PathBuf {
        let directory = crate::test_support::temp_dir().join(format!(
            "svcd-channel-mode-{}-{}-{}",
            tag,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).expect("config dir");
        directory
    }

    /// The token stays group-readable (the `svc` CLI runs as a group member)
    /// and is not exposed to other users. This is the POSIX half of the
    /// permission seam; Windows keeps its own ACL semantics.
    #[test]
    fn the_token_is_private_to_the_owner_and_group() {
        let config = config_dir("token");
        let token = config.join("svcd.token");
        fs::write(&token, "token").expect("token");
        Platform::set_private(&token).expect("set private");
        let mode = fs::metadata(&token).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "token mode: {mode:o}");
        let _ = fs::remove_dir_all(&config);
    }
}
