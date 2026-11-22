//! Unix readiness backend: one `poll(2)` set for every plugin pipe.
//!
//! The reactor owns no thread; `poll` is called by the supervisor loop. The
//! self-pipe is the interrupt: producers write one byte, the pass drains it and
//! returns to the caller.

use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::Receiver;

use super::dispatch::{drain_source, read_source, write_raw};
use super::*;
use crate::platform::host::poll;

/// The Unix wait set: the sources plus the interrupt pipe.
pub(super) struct PollSet {
    rx: Receiver<Command>,
    wake: UnixStream,
    sources: Vec<Source>,
}

/// Ask for a larger pipe so one poll pass can hand a whole raw frame to the
/// plugin instead of trickling it in 64 KiB slices. Best effort: the kernel
/// caps this at `pipe-max-size` and the reactor works either way. Only Linux
/// (and Android) expose `F_SETPIPE_SZ`; elsewhere the default 64 KiB stands.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) fn enlarge_pipe(file: &File) {
    const PIPE_BYTES: libc::c_int = 512 * 1024;
    // SAFETY: The descriptor belongs to a live `Source`; the buffer is local and correctly sized for this call.
    unsafe {
        libc::fcntl(file.as_raw_fd(), libc::F_SETPIPE_SZ, PIPE_BYTES);
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(super) fn enlarge_pipe(_file: &File) {}

impl PollSet {
    /// Build the wait set and its interrupt. No thread is started.
    pub(super) fn start(rx: Receiver<Command>) -> io::Result<(Self, Interrupt)> {
        let (wake_rx, wake) = UnixStream::pair()?;
        wake_rx.set_nonblocking(true)?;
        wake.set_nonblocking(true)?;
        let wake = Arc::new(wake);
        let interrupt: Interrupt = {
            let wake = wake.clone();
            Arc::new(move || {
                let mut handle = &*wake;
                let _ = handle.write(&[1]);
            })
        };
        Ok((
            PollSet {
                rx,
                wake: wake_rx,
                sources: Vec::new(),
            },
            interrupt,
        ))
    }

    /// Apply every registration change queued since the last pass.
    pub(super) fn apply_commands(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(Command::Register(mut incoming)) => {
                    if let Some(id) = incoming.first().map(|source| source.key.clone()) {
                        self.sources.retain(|source| source.key != id);
                    }
                    self.sources.append(&mut incoming);
                }
                Ok(Command::Unregister(id)) => self.sources.retain(|source| source.key != id),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // No producer can reach these pipes any more.
                    self.sources.clear();
                    break;
                }
            }
        }
    }

    /// One poll pass. The caller's loop owns this call, so the wait is the
    /// supervisor loop's only blocking point for plugin stdio.
    pub(super) fn poll(&mut self, timeout: Duration) {
        let sources = &mut self.sources;
        // A full per-plugin queue pauses that plugin's reader, not the whole
        // poll set. Retry blocked sources before waiting for more I/O.
        for source in sources.iter_mut() {
            if !source.blocked {
                continue;
            }
            source.blocked = false;
            resume(source);
        }
        retire(sources);

        let mut fds: Vec<poll::PollFd> = Vec::with_capacity(sources.len() + 1);
        fds.push(poll::PollFd::new(self.wake.as_raw_fd(), poll::READ));
        for source in sources.iter() {
            let events = poll_events(source);
            fds.push(poll::PollFd::new(
                if events == 0 {
                    poll::NONE
                } else {
                    source.file.as_raw_fd()
                },
                events,
            ));
        }
        let blocked_any = sources.iter().any(|source| source.blocked);
        let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        // A blocked source is retried on the next pass, so it must not wait for
        // the caller's full timeout. A caller that is already busy passes 0.
        let timeout_ms = if blocked_any {
            timeout_ms.min(super::BLOCKED_RETRY.as_millis() as i32)
        } else {
            timeout_ms
        };
        if let Err(error) = poll::poll(&mut fds, timeout_ms) {
            error!("reactor poll failed: {}", error);
            return;
        }
        if fds[0].revents & poll::READ != 0 {
            drain_wake(&self.wake);
        }

        // Feed plugin stdin before collecting plugin stdout: a plugin can only
        // produce output for input it already has, and a pass that greedily
        // drains every stdout pipe starves all the stdin pipes.
        for index in 0..sources.len() {
            if !matches!(sources[index].kind, SourceKind::RawStdin { .. }) {
                continue;
            }
            let revents = fds[index + 1].revents;
            if revents & (poll::WRITE | poll::ERROR | poll::HUP) != 0 {
                write_raw(&mut sources[index]);
            }
        }
        for index in 0..sources.len() {
            if matches!(sources[index].kind, SourceKind::RawStdin { .. }) {
                continue;
            }
            let revents = fds[index + 1].revents;
            if revents & (poll::READ | poll::ERROR | poll::HUP) != 0 {
                read_source(&mut sources[index]);
            }
        }
        retire(sources);
    }
}

fn drain_wake(wake: &UnixStream) {
    let mut handle = wake;
    let mut buffer = [0u8; 64];
    loop {
        match handle.read(&mut buffer) {
            Ok(0) => return,
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

fn poll_events(source: &Source) -> i16 {
    if source.blocked {
        return 0;
    }
    match &source.kind {
        SourceKind::RawStdin { queue } => {
            if queue.has_pending() {
                poll::WRITE
            } else {
                0
            }
        }
        _ => poll::READ,
    }
}

/// A source that was paused because its consumer queue was full.
fn resume(source: &mut Source) {
    if matches!(source.kind, SourceKind::RawStdin { .. }) {
        write_raw(source);
        return;
    }
    if !drain_source(source) {
        source.stop = Some(stop_for(source, RawFailure::FrameTooLarge));
    }
}
