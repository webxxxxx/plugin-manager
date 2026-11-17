//! A small condvar-based wake latch shared by the daemon, the control channel
//! and plugin stdout readers. Producers set the latch and notify; the daemon
//! waits with a timeout only to service timers/shutdown, so an idle service
//! does not spin on a fixed polling tick.
//!
//! The supervisor loop may instead wait inside a poll set (it owns plugin
//! stdio, #126). `set_interrupt` attaches the poll's self-pipe, so `notify`
//! also pokes that pipe and a management request returns at once instead of at
//! the poll timeout. The latch stays for the paths without a poll set (Windows).

use std::sync::{Condvar, Mutex};
use std::time::Duration;

#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::net::UnixStream;

pub(crate) struct Wake {
    pending: Mutex<bool>,
    condvar: Condvar,
    /// Write end of a poll loop's self-pipe. `None` keeps the condvar-only
    /// behaviour of the thread-per-connection paths.
    #[cfg(unix)]
    interrupt: Mutex<Option<UnixStream>>,
}

impl Wake {
    pub(crate) fn new() -> Self {
        Wake {
            pending: Mutex::new(false),
            condvar: Condvar::new(),
            #[cfg(unix)]
            interrupt: Mutex::new(None),
        }
    }

    /// Attach the self-pipe of the poll set that now owns this latch.
    #[cfg(unix)]
    pub(crate) fn set_interrupt(&self, interrupt: UnixStream) {
        let mut slot = self
            .interrupt
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *slot = Some(interrupt);
    }

    pub(crate) fn notify(&self) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *pending = true;
        self.condvar.notify_one();
        #[cfg(unix)]
        {
            // One byte per notify, never coalesced: a poll pass drains the pipe
            // before waking, so coalescing here could swallow the only byte a
            // later producer needed and leave its work waiting for the timeout.
            let mut slot = self
                .interrupt
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(interrupt) = slot.as_mut() {
                let _ = interrupt.write(&[1]);
            }
        }
    }

    /// Wait until notified or the timeout expires, then clear the latch. The
    /// caller drains its channels after every return, so a notification that
    /// arrives just before the wait is not lost.
    pub(crate) fn wait_timeout(&self, timeout: Duration) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !*pending {
            let (guard, _) = self
                .condvar
                .wait_timeout(pending, timeout)
                .unwrap_or_else(|error| error.into_inner());
            pending = guard;
        }
        *pending = false;
    }
}
