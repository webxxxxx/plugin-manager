use super::*;

#[cfg(unix)]
pub(super) fn write_raw(source: &mut Source) {
    let SourceKind::RawStdin { queue } = &source.kind else {
        return;
    };
    let queue = queue.clone();
    if queue.flush(&mut source.file).is_err() {
        source.stop = Some(Stop::Plugin(RawFailure::PluginWriteError));
    }
}

/// Returns false when the source hit EOF or a protocol violation and must be
/// removed. The reactor owns the file; dropping it closes the descriptor.
#[cfg(unix)]
pub(super) fn read_source(source: &mut Source) {
    if matches!(source.kind, SourceKind::RawStdout { .. }) {
        read_raw(source);
        return;
    }
    read_stream(source);
}

/// Drain a non-blocking raw stdout until the kernel reports it empty. The
/// budget bounds how much one plugin can move per poll pass.
#[cfg(unix)]
fn read_raw(source: &mut Source) {
    let mut chunk = vec![0u8; RAW_READ_CHUNK];
    let mut budget = RAW_READ_BUDGET;
    loop {
        match source.file.read(&mut chunk) {
            Ok(0) => {
                on_raw_eof(source);
                return;
            }
            Ok(read) => {
                on_raw_bytes(source, &chunk[..read]);
                if source.stop.is_some() {
                    return;
                }
                budget = budget.saturating_sub(read);
                if budget == 0 {
                    return;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                on_raw_eof(source);
                return;
            }
        }
    }
}

#[cfg(unix)]
fn read_stream(source: &mut Source) {
    let mut chunk = [0u8; READ_CHUNK];
    match source.file.read(&mut chunk) {
        Ok(0) => on_stream_eof(source),
        Ok(read) => on_stream_bytes(source, &chunk[..read]),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
        Err(error) => on_stream_error(source, error),
    }
}

/// Bytes arrived for a framed or log source. Shared with the completion
/// backend: both hosts only differ in how the bytes were read.
pub(super) fn on_stream_bytes(source: &mut Source, chunk: &[u8]) {
    source.buffer.extend_from_slice(chunk);
    if !drain_source(source) {
        source.stop = Some(stop_for(source, RawFailure::FrameTooLarge));
    }
}

pub(super) fn on_stream_eof(source: &mut Source) {
    if let SourceKind::Frames { sender, .. } = &source.kind {
        let _ = sender.try_send(FrameEvent::Closed);
    }
    source.stop = Some(stop_for(source, RawFailure::PluginEof));
}

pub(super) fn on_stream_error(source: &mut Source, error: io::Error) {
    if let SourceKind::Frames { sender, .. } = &source.kind {
        let _ = sender.try_send(FrameEvent::Frame(Err(FrameError::Io(error))));
    }
    source.stop = Some(stop_for(source, RawFailure::PluginEof));
}

/// Bytes arrived for a protocol-3 command's stdout.
pub(super) fn on_raw_bytes(source: &mut Source, chunk: &[u8]) {
    source.buffer.extend_from_slice(chunk);
    if !drain_raw_frames(source) {
        source.stop = Some(Stop::Plugin(RawFailure::FrameTooLarge));
    }
}

pub(super) fn on_raw_eof(source: &mut Source) {
    source.stop = Some(Stop::Plugin(RawFailure::PluginEof));
}

pub(super) fn drain_source(source: &mut Source) -> bool {
    match &source.kind {
        SourceKind::RawStdout { .. } => drain_raw_frames(source),
        SourceKind::RawStdin { .. } => true,
        _ => drain_lines(source),
    }
}

fn drain_raw_frames(source: &mut Source) -> bool {
    let SourceKind::RawStdout { max_frame } = &source.kind else {
        return false;
    };
    let max_frame = *max_frame;
    loop {
        if source.buffer.len() < RAW_HEADER {
            break;
        }
        let length = u32::from_be_bytes([
            source.buffer[0],
            source.buffer[1],
            source.buffer[2],
            source.buffer[3],
        ]) as usize;
        let stream = u32::from_be_bytes([
            source.buffer[4],
            source.buffer[5],
            source.buffer[6],
            source.buffer[7],
        ]);
        if length > max_frame {
            return false;
        }
        if source.buffer.len() < RAW_HEADER + length {
            break;
        }
        // The common case is one frame per pass: hand the buffer over instead
        // of allocating and copying the payload out of it.
        if source.buffer.len() == RAW_HEADER + length {
            let mut frame = std::mem::take(&mut source.buffer);
            frame.drain(..RAW_HEADER);
            if let Some(hub) = source.hub.as_ref().and_then(|(hub, _)| hub.upgrade()) {
                hub.deliver(&source.key, stream, frame);
            }
            continue;
        }
        let payload: Vec<u8> = source
            .buffer
            .drain(..RAW_HEADER + length)
            .skip(RAW_HEADER)
            .collect();
        if let Some(hub) = source.hub.as_ref().and_then(|(hub, _)| hub.upgrade()) {
            hub.deliver(&source.key, stream, payload);
        }
    }
    if source.buffer.is_empty() && source.buffer.capacity() > RAW_BUFFER_SHRINK {
        source.buffer.shrink_to(RAW_BUFFER_SHRINK);
    }
    true
}

fn drain_lines(source: &mut Source) -> bool {
    if let SourceKind::Frames { framing, .. } = &source.kind {
        if matches!(framing, Framing::Length) {
            return drain_length_frames(source);
        }
    }
    if matches!(source.kind, SourceKind::Log) && source.discarding {
        match source.buffer.iter().position(|byte| *byte == b'\n') {
            Some(position) => {
                source.buffer.drain(..=position);
                source.discarding = false;
            }
            None => {
                source.buffer.clear();
                return true;
            }
        }
    }
    loop {
        let Some(position) = source.buffer.iter().position(|byte| *byte == b'\n') else {
            let max = match &source.kind {
                SourceKind::Frames { max, .. } => *max,
                SourceKind::Log => crate::platform::log::MAX_LINE_BYTES,
                SourceKind::RawStdin { .. } | SourceKind::RawStdout { .. } => {
                    crate::platform::log::MAX_LINE_BYTES
                }
            };
            if source.buffer.len() > max {
                if matches!(source.kind, SourceKind::Log) {
                    // The write side truncates and marks this record; the
                    // source keeps reading instead of being dropped (#174).
                    let line = String::from_utf8_lossy(&source.buffer).into_owned();
                    log_line(&source.log, &line);
                    source.buffer.clear();
                    source.discarding = true;
                    return true;
                }
                if let SourceKind::Frames { sender, .. } = &source.kind {
                    let _ = sender.try_send(FrameEvent::Frame(Err(FrameError::TooLarge { max })));
                }
                return false;
            }
            return true;
        };
        match &source.kind {
            SourceKind::Frames {
                sender, max, wake, ..
            } => {
                let line = &source.buffer[..position];
                if line.len() > *max {
                    let _ =
                        sender.try_send(FrameEvent::Frame(Err(FrameError::TooLarge { max: *max })));
                    return false;
                }
                let text = match std::str::from_utf8(line) {
                    Ok(text) => text.to_string(),
                    Err(_) => {
                        let _ = sender.try_send(FrameEvent::Frame(Err(FrameError::InvalidUtf8)));
                        return false;
                    }
                };
                match sender.try_send(FrameEvent::Frame(Ok(text))) {
                    Ok(()) => {
                        source.buffer.drain(..=position);
                        if let Some(wake) = wake {
                            wake.notify();
                        }
                    }
                    Err(TrySendError::Full(_)) => {
                        // Keep the line in the buffer; the receiver will drain
                        // it and the reactor retries this source.
                        source.blocked = true;
                        return true;
                    }
                    Err(TrySendError::Disconnected(_)) => return false,
                }
            }
            SourceKind::Log => {
                let line: Vec<u8> = source.buffer.drain(..=position).collect();
                let line = &line[..line.len().saturating_sub(1)];
                // A CRLF terminator is still one line; only a bare CR is
                // message content and gets escaped by the writer.
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                log_line(&source.log, &String::from_utf8_lossy(line));
            }
            SourceKind::RawStdin { .. } | SourceKind::RawStdout { .. } => return true,
        }
    }
}

fn drain_length_frames(source: &mut Source) -> bool {
    loop {
        if source.buffer.len() < 4 {
            return true;
        }
        let length = u32::from_be_bytes([
            source.buffer[0],
            source.buffer[1],
            source.buffer[2],
            source.buffer[3],
        ]) as usize;
        let SourceKind::Frames {
            sender, max, wake, ..
        } = &source.kind
        else {
            return false;
        };
        if length > *max {
            let _ = sender.try_send(FrameEvent::Frame(Err(FrameError::TooLarge { max: *max })));
            return false;
        }
        if source.buffer.len() < 4 + length {
            if source.buffer.len() > *max + 4 {
                let _ = sender.try_send(FrameEvent::Frame(Err(FrameError::TooLarge { max: *max })));
                return false;
            }
            return true;
        }
        let text = match std::str::from_utf8(&source.buffer[4..4 + length]) {
            Ok(text) => text.to_string(),
            Err(_) => {
                let _ = sender.try_send(FrameEvent::Frame(Err(FrameError::InvalidUtf8)));
                return false;
            }
        };
        match sender.try_send(FrameEvent::Frame(Ok(text))) {
            Ok(()) => {
                source.buffer.drain(..4 + length);
                if let Some(wake) = wake {
                    wake.notify();
                }
            }
            Err(TrySendError::Full(_)) => {
                source.blocked = true;
                return true;
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

fn log_line(log: &Option<Arc<Mutex<LogWriter>>>, line: &str) {
    if let Some(writer) = log {
        if let Ok(mut writer) = writer.lock() {
            let _ = writer.record(crate::platform::log::Level::Info, line);
        }
    }
}
