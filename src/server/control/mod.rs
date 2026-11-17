//! L4 control sessions: the CLI channel and the outbound management stream.
//!
//! Both used to run on their own reactor threads. The server loop now owns the
//! control listener, every control connection and every outbound streaming
//! connection; the supervisor loop only receives `PendingCall` values through
//! its bounded channel and replies through `Reply`. It never writes a client
//! socket.
//!
//! The same loop is used by the minimal build: the engine is started without
//! an HTTP listener and only the control channel is polled.

#[cfg_attr(not(feature = "http"), allow(dead_code))]
pub mod client;
pub mod handle;
pub mod listener;

use std::os::unix::io::RawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::thread;

use crate::channel::PendingCall;
use crate::server::http::engine::{self, Connection, HttpRequest, LegacyBridge, PreRequest, Table};
use crate::wake::Wake;

use self::handle::LoopHandle;
use self::listener::{ControlServer, Source};

/// Start the server loop for the minimal (no HTTP) build: control channel only.
#[cfg_attr(feature = "http", allow(dead_code))]
pub fn spawn_control_only(
    listener: UnixListener,
    tx: SyncSender<PendingCall>,
    wake: Arc<Wake>,
) -> std::io::Result<LoopHandle> {
    let (notify_rx, notify_tx) = UnixStream::pair()?;
    notify_rx.set_nonblocking(true)?;
    notify_tx.set_nonblocking(true)?;
    let notify_tx = Arc::new(notify_tx);
    let handle = LoopHandle::new(notify_tx.clone());
    let stop = handle.stop_flag();
    let control = ControlServer::new(listener, tx, wake, notify_tx)?;
    let join = thread::Builder::new()
        .name("svcd-server-loop".into())
        .stack_size(crate::threads::transfer_stack())
        .spawn(move || {
            let mut handler = ControlOnly {
                control,
                stop,
                notify_rx,
                keys: Vec::new(),
            };
            engine::run(None, &mut handler, crate::limits::current());
        })?;
    handle.set_join(join);
    Ok(handle)
}

#[cfg_attr(feature = "http", allow(dead_code))]
struct ControlOnly {
    control: ControlServer,
    stop: Arc<AtomicBool>,
    notify_rx: UnixStream,
    keys: Vec<Source>,
}

impl LegacyBridge for ControlOnly {
    type State = ();

    fn new_state(&mut self, _id: u64, _peer: std::net::IpAddr) {}
    fn on_accept(&mut self, _connection: &mut Connection<()>) {}
    fn body_limit(&self, _path: &str) -> u64 {
        0
    }
    fn reads_request(&self, _connection: &Connection<()>) -> bool {
        false
    }
    fn pre_request(&mut self, _connection: &mut Connection<()>) -> PreRequest {
        PreRequest::Continue
    }
    fn on_request(&mut self, _connection: &mut Connection<()>, _request: HttpRequest) {}
    fn on_error(&mut self, _connection: &mut Connection<()>, _response: Vec<u8>) {}
    fn on_readable(&mut self, _connection: &mut Connection<()>) -> bool {
        true
    }
    fn on_writable(&mut self, _connection: &mut Connection<()>) -> bool {
        true
    }
    fn on_tick(&mut self, _connections: &mut Table<()>) {
        let mut drain = [0u8; 64];
        while std::io::Read::read(&mut self.notify_rx, &mut drain).is_ok() {}
        self.control.drain_replies();
        self.control.flush();
    }
    fn interest(&self, _connection: &Connection<()>) -> i16 {
        0
    }
    fn is_idle(
        &self,
        _connection: &Connection<()>,
        _now: std::time::Instant,
        _idle: std::time::Duration,
    ) -> bool {
        false
    }
    fn on_close(&mut self, _connection: &mut Connection<()>) {}
    fn wake_fd(&self) -> Option<RawFd> {
        Some(std::os::unix::io::AsRawFd::as_raw_fd(&self.notify_rx))
    }
    fn prepare_extra(&mut self, out: &mut Vec<(RawFd, i16)>) {
        self.keys.clear();
        self.control.poll_sources(out, &mut self.keys);
    }
    fn on_extra_ready(&mut self, index: usize, revents: i16) {
        if let Some(source) = self.keys.get(index).copied() {
            self.control.ready(source, revents);
        }
    }
    fn should_stop(&self) -> bool {
        self.stop.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn on_stop(&mut self) {
        self.control.drain_for_shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc;
    use std::io::{BufRead, BufReader, Write};
    use std::sync::mpsc;
    use std::time::Duration;

    /// The minimal build runs the same loop without an HTTP listener. A CLI
    /// request must still be answered and the loop must stop on the handle.
    #[test]
    fn the_control_only_loop_answers_a_request_and_stops() {
        let root = crate::test_support::temp_dir().join(format!(
            "svcd-control-only-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("root");
        let path = root.join("svcd.sock");
        let listener = UnixListener::bind(&path).expect("listener");
        let (tx, rx) = mpsc::sync_channel::<PendingCall>(8);
        std::thread::spawn(move || {
            for call in rx {
                let value: serde_json::Value = serde_json::from_str(&call.line).expect("request");
                let id = value.get("id").cloned();
                let _ = call.reply.send(rpc::to_line(&rpc::ok(
                    id,
                    serde_json::json!({ "ok": true }),
                )));
            }
        });
        let handle = spawn_control_only(listener, tx, Arc::new(Wake::new())).expect("loop");

        let mut stream = UnixStream::connect(&path).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("timeout");
        stream
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"svcd.ping\"}\n")
            .expect("write");
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).expect("reply");
        let value: serde_json::Value = serde_json::from_str(line.trim()).expect("reply");
        assert_eq!(value["id"], 5, "{line}");
        handle.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }
}
