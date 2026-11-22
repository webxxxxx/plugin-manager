//! A small condvar-based wake latch shared by the daemon, the control channel
//! and plugin stdout readers. Producers set the latch and notify; the daemon
//! waits with a timeout only to service timers/shutdown, so an idle service
//! does not spin on a fixed polling tick.
//!
//! The supervisor loop may instead wait inside the reactor (it owns plugin
//! stdio, #126). `set_interrupt` attaches the reactor's interrupt, so `notify`
//! also ends that wait and a management request returns at once instead of at
//! the timeout. The latch stays for the paths without a reactor.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub(crate) struct Wake {
    pending: Mutex<bool>,
    condvar: Condvar,
    /// Poke for a wait set that owns this latch: the Unix self-pipe write or
    /// the Windows IOCP completion. `None` keeps the condvar-only behaviour.
    interrupt: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Wake {
    pub(crate) fn new() -> Self {
        Wake {
            pending: Mutex::new(false),
            condvar: Condvar::new(),
            interrupt: Mutex::new(None),
        }
    }

    /// Attach the wait set's interrupt to this latch.
    pub(crate) fn set_interrupt(&self, interrupt: Arc<dyn Fn() + Send + Sync>) {
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
        // One poke per notify, never coalesced: the wait set drains its own
        // interrupt before returning, so coalescing here could swallow the only
        // wake a later producer needed.
        let slot = self
            .interrupt
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(interrupt) = slot.as_ref() {
            interrupt();
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
