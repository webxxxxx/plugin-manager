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

use crate::platform::host::wake::Wake;
use crate::wire::control::channel::{
    ControlChannel, ControlListener, ControlStream, Endpoint, PendingCall,
};

pub(crate) struct Platform;

pub(crate) struct Listener(UnixListener);

pub(crate) struct Stream(UnixStream);

/// The bound server half. Unix hands the listener to the server loop; nothing
/// is accepted off-thread.
pub(crate) struct ControlBinding {
    user: Listener,
    admin: Listener,
    pending: SyncSender<PendingCall>,
    wake: Arc<Wake>,
}

impl ControlBinding {
    /// Split the server into both listeners and the plumbing its loop owns.
    /// Called by `server::control` (Unix-only), so the raw socket type does not
    /// leak into `daemon.rs`.
    pub(crate) fn into_parts(
        self,
    ) -> (
        UnixListener,
        UnixListener,
        SyncSender<PendingCall>,
        Arc<Wake>,
    ) {
        (self.user.0, self.admin.0, self.pending, self.wake)
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
    pub(crate) fn bind(config: &Path, endpoint: Endpoint) -> io::Result<Self> {
        let path = config.join(endpoint.socket_name());
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
        // The socket's mode is the authorization: the `svcd` group may drive the
        // read endpoint, only root may open the admin one.
        fs::set_permissions(&path, fs::Permissions::from_mode(endpoint.mode()))?;
        Ok(Listener(listener))
    }
}

impl Endpoint {
    fn socket_name(self) -> &'static str {
        match self {
            Endpoint::User => "svcd.sock",
            Endpoint::Admin => "svcd-admin.sock",
        }
    }

    /// 0660 lets the `svcd` group drive lifecycle; the admin endpoint stays
    /// root-only at 0600.
    fn mode(self) -> u32 {
        match self {
            Endpoint::User => 0o660,
            Endpoint::Admin => 0o600,
        }
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
        // SAFETY: The pipe handle is owned by the listener or stream and stays valid for the duration of the call.
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
    type Server = ControlBinding;

    fn bind(config: &Path, endpoint: Endpoint) -> io::Result<Self::Listener> {
        Listener::bind(config, endpoint)
    }

    fn into_server(
        user: Self::Listener,
        admin: Self::Listener,
        pending: SyncSender<PendingCall>,
        wake: Arc<Wake>,
        _io_stack: usize,
    ) -> io::Result<Self::Server> {
        Ok(ControlBinding {
            user,
            admin,
            pending,
            wake,
        })
    }

    fn connect(config: &Path, endpoint: Endpoint) -> io::Result<Self::Stream> {
        UnixStream::connect(config.join(endpoint.socket_name())).map(Stream)
    }

    fn remove_socket(config: &Path) {
        let _ = fs::remove_file(config.join(Endpoint::User.socket_name()));
        let _ = fs::remove_file(config.join(Endpoint::Admin.socket_name()));
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

    /// The socket mode is the authorization: the group drives the read
    /// endpoint, only root opens the admin one.
    #[test]
    fn each_endpoint_gets_its_own_mode() {
        let config = config_dir("socket");
        let user = Platform::bind(&config, Endpoint::User).expect("bind user");
        let admin = Platform::bind(&config, Endpoint::Admin).expect("bind admin");
        for (name, expected) in [("svcd.sock", 0o660), ("svcd-admin.sock", 0o600)] {
            let path = config.join(name);
            let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, expected, "{name} mode: {mode:o}");
        }
        drop(user);
        drop(admin);
        let _ = fs::remove_dir_all(&config);
    }
}
