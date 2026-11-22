//! Stop/flush handle for the server loop.
//!
//! The composition root (daemon) calls `shutdown` after its own loop returns.
//! The server loop then drains control replies, flushes them, and exits; the
//! daemon joins it before the process exits so a control reply is never lost.
//!
//! The handle only knows the loop's stop flag and its wake pipe, so it compiles
//! on every host: Unix drives `poll(2)`, Windows `WSAPoll`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::platform::host::wake_pipe::Writer;

pub(crate) struct LoopHandle {
    stop: Arc<AtomicBool>,
    notify: Arc<Writer>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl LoopHandle {
    pub(crate) fn new(notify: Arc<Writer>) -> LoopHandle {
        LoopHandle {
            stop: Arc::new(AtomicBool::new(false)),
            notify,
            join: Mutex::new(None),
        }
    }

    pub(crate) fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }

    pub(crate) fn set_join(&self, join: JoinHandle<()>) {
        *self.join.lock().unwrap_or_else(|error| error.into_inner()) = Some(join);
    }

    /// Ask the loop to drain control connections and return, then wait for it.
    pub(crate) fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the wait: the loop checks the stop flag at the top of each pass.
        self.notify.notify();
        if let Ok(mut join) = self.join.lock() {
            if let Some(join) = join.take() {
                let _ = join.join();
            }
        }
    }
}
