//! `ready: port`: the host connects to the declared address instead of trusting
//! the spawn (`exec`) or waiting for a message (`notify`).
//!
//! The supervisor loop is single threaded and owns every plugin pipe, so the
//! blocking connects happen on one short-lived worker per spawn: it sets a flag
//! and wakes the loop, which then settles the Ready through the same gate a
//! protocol peer's first response uses.

use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::*;

/// One spawn's port probe.
pub(super) struct PortProbe {
    ready: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    pid: u32,
    addr: SocketAddr,
}

impl Drop for PortProbe {
    fn drop(&mut self) {
        // The worker may be inside a connect; it checks the flag between
        // attempts, so a dropped probe stops within one attempt (~250ms).
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl PortProbe {
    fn start(addr: SocketAddr, pid: u32, wake: Option<Arc<Wake>>) -> PortProbe {
        let ready = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_ready = ready.clone();
        let worker_stop = stop.clone();
        std::thread::spawn(move || {
            while !worker_stop.load(Ordering::SeqCst) {
                if TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok() {
                    worker_ready.store(true, Ordering::SeqCst);
                    // Without a wake the loop still notices on its next wait
                    // timeout; with one the Ready settles immediately.
                    if let Some(wake) = wake {
                        wake.notify();
                    }
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        PortProbe {
            ready,
            stop,
            pid,
            addr,
        }
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    fn pid(&self) -> u32 {
        self.pid
    }

    pub(super) fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Slot {
    /// Start probing for the instance that was just spawned.
    pub(super) fn start_port_probe(&mut self, pid: u32, addr: SocketAddr) {
        info!("{}: waiting for pid {} to accept on {}", self.id, pid, addr);
        self.port = Some(PortProbe::start(addr, pid, self.wake.clone()));
    }

    /// Settle a Ready event once the probe reports a connection.
    pub(super) fn check_port_ready(&mut self, now: u64) {
        let Some(probe) = self.port.take() else {
            return;
        };
        // A probe belongs to one spawn: after an exit, a stop or a new attempt
        // its verdict says nothing about the current instance.
        if self.machine.state != State::Starting || self.machine.pid != Some(probe.pid()) {
            return;
        }
        if !probe.is_ready() {
            // The window ran out: name the address so the start request can
            // report something better than "timed out". The timeout itself is
            // applied by `check_timers` in this same tick.
            if self.start_deadline.is_some_and(|deadline| now >= deadline) {
                // A window that ran out is a 504 at the API, like every other
                // start timeout (#341).
                self.note_start_timeout(format!(
                    "port {} never accepted a connection",
                    probe.addr()
                ));
            }
            self.port = Some(probe);
            return;
        }
        let (pid, addr) = (probe.pid(), probe.addr());
        info!("{}: port {} accepts; the instance is ready", self.id, addr);
        self.apply(Event::Ready { pid }, now);
    }

    /// Drop a pending probe: a new attempt owns the port now. A stop or an exit
    /// drops it through `check_port_ready`, whose state guard discards a verdict
    /// that belongs to an older instance.
    pub(super) fn abort_port(&mut self) {
        self.port = None;
    }
}
