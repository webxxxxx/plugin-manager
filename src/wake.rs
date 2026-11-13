//! A small condvar-based wake latch shared by the daemon, the control channel
//! and plugin stdout readers. Producers set the latch and notify; the daemon
//! waits with a timeout only to service timers/shutdown, so an idle service
//! does not spin on a fixed polling tick.

use std::sync::{Condvar, Mutex};
use std::time::Duration;

pub(crate) struct Wake {
    pending: Mutex<bool>,
    condvar: Condvar,
}

impl Wake {
    pub(crate) fn new() -> Self {
        Wake {
            pending: Mutex::new(false),
            condvar: Condvar::new(),
        }
    }

    pub(crate) fn notify(&self) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *pending = true;
        self.condvar.notify_one();
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
