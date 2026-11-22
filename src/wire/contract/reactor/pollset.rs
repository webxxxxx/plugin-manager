use super::dispatch::{drain_source, read_source, write_raw};
use super::*;

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

        let mut fds: Vec<libc::pollfd> = Vec::with_capacity(sources.len() + 1);
        fds.push(libc::pollfd {
            fd: self.wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        for source in sources.iter() {
            let events = poll_events(source);
            fds.push(libc::pollfd {
                fd: if events == 0 {
                    -1
                } else {
                    source.file.as_raw_fd()
                },
                events,
                revents: 0,
            });
        }
        let blocked_any = sources.iter().any(|source| source.blocked);
        let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        // A blocked source is retried on the next pass, so it must not wait for
        // the caller's full timeout. A caller that is already busy passes 0.
        let timeout_ms = if blocked_any {
            timeout_ms.min(10)
        } else {
            timeout_ms
        };
        if let Err(error) = poll::poll(&mut fds, timeout_ms) {
            error!("reactor poll failed: {}", error);
            return;
        }
        if fds[0].revents & libc::POLLIN != 0 {
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
            if revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) != 0 {
                write_raw(&mut sources[index]);
            }
        }
        for index in 0..sources.len() {
            if matches!(sources[index].kind, SourceKind::RawStdin { .. }) {
                continue;
            }
            let revents = fds[index + 1].revents;
            if revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
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
                libc::POLLOUT
            } else {
                0
            }
        }
        _ => libc::POLLIN,
    }
}

/// One raw plugin being told why its transport closed: id, hub handle, reason.
type ClosingEntry = (String, Option<(Weak<dyn RawStopSink>, u64)>, RawFailure);

/// Drop every source marked dead and tell the raw hub which plugins lost
/// their transport.
fn retire(sources: &mut Vec<Source>) {
    if !sources.iter().any(|source| source.stop.is_some()) {
        return;
    }
    let mut kept = Vec::with_capacity(sources.len());
    let mut closing: Vec<ClosingEntry> = Vec::new();
    let mut closed = HashSet::new();
    for source in sources.drain(..) {
        match source.stop {
            None => kept.push(source),
            Some(Stop::Single) => {}
            Some(Stop::Plugin(reason)) => {
                if closed.insert(source.key.clone()) {
                    closing.push((source.key.clone(), source.hub.clone(), reason));
                }
            }
        }
    }
    kept.retain(|source| !closed.contains(&source.key));
    *sources = kept;
    for (key, hub, reason) in closing {
        let Some((hub, generation)) = hub else {
            continue;
        };
        let Some(hub) = hub.upgrade() else { continue };
        hub.note_transport_failure(reason, &key);
        hub.unregister_generation(&key, generation);
    }
}

pub(super) fn stop_for(source: &Source, reason: RawFailure) -> Stop {
    match &source.kind {
        SourceKind::RawStdin { .. } | SourceKind::RawStdout { .. } => Stop::Plugin(reason),
        _ => Stop::Single,
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
