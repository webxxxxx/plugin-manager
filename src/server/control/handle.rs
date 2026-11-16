//! Stop/flush handle for the server loop.
//!
//! The composition root (daemon) calls `shutdown` after its own loop returns.
//! The server loop then drains control replies, flushes them, and exits; the
//! daemon joins it before the process exits so a control reply is never lost.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

pub struct LoopHandle {
    stop: Arc<AtomicBool>,
    notify: Arc<UnixStream>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl LoopHandle {
    pub fn new(notify: Arc<UnixStream>) -> LoopHandle {
        LoopHandle {
            stop: Arc::new(AtomicBool::new(false)),
            notify,
            join: Mutex::new(None),
        }
    }

    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }

    pub fn set_join(&self, join: JoinHandle<()>) {
        *self.join.lock().unwrap_or_else(|error| error.into_inner()) = Some(join);
    }

    /// Ask the loop to drain control connections and return, then wait for it.
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the poll: the loop checks the stop flag at the top of each pass.
        let _ = (&*self.notify).write(&[1]);
        if let Ok(mut join) = self.join.lock() {
            if let Some(join) = join.take() {
                let _ = join.join();
            }
        }
    }
}
