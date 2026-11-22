//! Windows completion backend: overlapped named-pipe I/O through one IOCP.
//!
//! The reactor owns no thread. `poll` is called by the supervisor loop and
//! blocks in `GetQueuedCompletionStatusEx`; a producer wakes it by posting a
//! packet with completion key 0. Every source has at most one read and one
//! write in flight, which is what turns a full consumer queue into real
//! backpressure: the next read is simply not issued until the queue drains.
//!
//! A slot is heap-stable and its address is the completion key, so a
//! completion packet finds its source without a lookup table. A slot is only
//! dropped after every issued operation completed; `CancelIoEx` starts that
//! drain when a plugin stops or is restarted.

use std::collections::HashSet;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::ptr;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_IO_PENDING, ERROR_OPERATION_ABORTED,
    ERROR_PIPE_NOT_CONNECTED, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::IO::{
    CancelIoEx, CreateIoCompletionPort, GetOverlappedResult, GetQueuedCompletionStatusEx,
    PostQueuedCompletionStatus, OVERLAPPED, OVERLAPPED_ENTRY,
};

use super::dispatch::{
    drain_source, on_raw_bytes, on_raw_eof, on_stream_bytes, on_stream_eof, on_stream_error,
};
use super::*;

/// Completion key of a wake packet. A slot pointer is never 0.
const WAKE_KEY: usize = 0;

pub(super) struct IocpSet {
    port: HANDLE,
    rx: Receiver<Command>,
    /// Boxed on purpose: the slot address is the IOCP completion key, so a
    /// `Vec` reallocation must not move a slot while operations are pending.
    #[allow(clippy::vec_box)]
    slots: Vec<Box<Slot>>,
}

struct Slot {
    source: Option<Source>,
    handle: HANDLE,
    read: Option<Box<ReadOp>>,
    write: Option<Box<WriteOp>>,
    /// Operations issued and not yet dequeued. The slot cannot be dropped while
    /// this is non-zero: the completion still points at its overlapped.
    pending: usize,
    retire: bool,
}

struct ReadOp {
    ov: OVERLAPPED,
    buf: Box<[u8]>,
}

struct WriteOp {
    ov: OVERLAPPED,
    chunk: Vec<u8>,
}

impl IocpSet {
    /// Build the completion port and its interrupt. No thread is started.
    pub(super) fn start(rx: Receiver<Command>) -> io::Result<(Self, Interrupt)> {
        // SAFETY: an invalid file handle creates a new port; zero concurrent
        // threads means one per processor, which is all a single caller needs.
        let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0, 0) };
        if port.is_null() {
            return Err(io::Error::last_os_error());
        }
        let bits = port as usize;
        let interrupt: Interrupt = Arc::new(move || {
            // SAFETY: the port is owned by this reactor and outlives the
            // interrupt; posting a null overlapped with key 0 is a wake packet.
            unsafe {
                PostQueuedCompletionStatus(bits as HANDLE, 0, WAKE_KEY, ptr::null_mut());
            }
        });
        Ok((
            IocpSet {
                port,
                rx,
                slots: Vec::new(),
            },
            interrupt,
        ))
    }

    pub(super) fn apply_commands(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(Command::Register(mut incoming)) => {
                    if let Some(id) = incoming.first().map(|source| source.key.clone()) {
                        self.retire_key(&id);
                    }
                    for source in incoming.drain(..) {
                        self.add_source(source);
                    }
                }
                Ok(Command::Unregister(id)) => self.retire_key(&id),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    for slot in &mut self.slots {
                        slot.retire = true;
                        cancel(&slot.handle);
                    }
                    self.retire_done();
                    break;
                }
            }
        }
    }

    pub(super) fn poll(&mut self, timeout: Duration) {
        self.retry_blocked();
        self.retire_stopped();
        self.pump();

        // A blocked source is retried on the next pass, so its wait must not
        // run the caller's full timeout; Unix caps the same wait with the same
        // constant in the readiness backend.
        let timeout = if self.blocked_any() {
            timeout.min(super::BLOCKED_RETRY)
        } else {
            timeout
        };
        let timeout_ms = if timeout.is_zero() {
            0
        } else {
            u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX)
        };
        // SAFETY: zeroed is a valid all-zero OVERLAPPED_ENTRY (integers and
        // pointers); the array is live for the call.
        let mut entries: [OVERLAPPED_ENTRY; 64] = unsafe { std::mem::zeroed() };
        let mut count = 0u32;
        // SAFETY: the port is live, the array holds 64 writable entries, and
        // the count pointer is a valid out-pointer.
        let ok = unsafe {
            GetQueuedCompletionStatusEx(
                self.port,
                entries.as_mut_ptr(),
                entries.len() as u32,
                &mut count,
                timeout_ms,
                0,
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(WAIT_TIMEOUT as i32) {
                error!("reactor IOCP wait failed: {}", error);
            }
            return;
        }
        for entry in entries.iter().take(count as usize) {
            self.complete(entry.lpCompletionKey, entry.lpOverlapped);
        }
        self.retire_done();
        self.pump();
    }

    fn add_source(&mut self, source: Source) {
        let handle = source.file.as_raw_handle() as HANDLE;
        let mut slot = Box::new(Slot {
            source: Some(source),
            handle,
            read: None,
            write: None,
            pending: 0,
            retire: false,
        });
        let key = (&*slot as *const Slot) as usize;
        // SAFETY: the handle is a live named-pipe end owned by this slot; the
        // key is the slot address, which stays valid while the slot is queued.
        let associated = unsafe { CreateIoCompletionPort(handle, self.port, key, 0) };
        if associated.is_null() {
            // The pipe cannot join the port: report it as a dead transport
            // instead of leaving the plugin waiting forever.
            if let Some(source) = slot.source.as_mut() {
                source.stop = Some(stop_for(source, RawFailure::PluginEof));
            }
            slot.retire = true;
        }
        self.slots.push(slot);
        let index = self.slots.len() - 1;
        if !self.slots[index].retire {
            self.issue_write(index);
            self.issue_read(index);
        }
        self.retire_stopped();
    }

    fn issue_read(&mut self, index: usize) {
        {
            let slot = &self.slots[index];
            if slot.retire || slot.read.is_some() || slot.write.is_some() {
                return;
            }
            let Some(source) = slot.source.as_ref() else {
                return;
            };
            if source.blocked || source.stop.is_some() {
                return;
            }
            if matches!(source.kind, SourceKind::RawStdin { .. }) {
                return;
            }
        }
        let size = match self.slots[index].source.as_ref().map(|s| &s.kind) {
            Some(SourceKind::RawStdout { .. }) => RAW_READ_CHUNK,
            _ => READ_CHUNK,
        };
        let mut op = Box::new(ReadOp {
            // SAFETY: an all-zero OVERLAPPED is the documented "no event, no
            // offset" form for pipe I/O.
            ov: unsafe { std::mem::zeroed() },
            buf: vec![0u8; size].into_boxed_slice(),
        });
        let handle = self.slots[index].handle;
        // SAFETY: the handle is live, the buffer is owned by `op` and stays
        // alive until the completion is dequeued, and `op` is boxed so the
        // OVERLAPPED address does not move.
        let ok = unsafe {
            ReadFile(
                handle,
                op.buf.as_mut_ptr().cast(),
                op.buf.len() as u32,
                ptr::null_mut(),
                &mut op.ov,
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                self.read_failed(index, error);
                return;
            }
        }
        self.slots[index].read = Some(op);
        self.slots[index].pending += 1;
    }

    fn issue_write(&mut self, index: usize) {
        let queue = {
            let slot = &self.slots[index];
            if slot.retire || slot.write.is_some() {
                return;
            }
            match slot.source.as_ref().map(|source| &source.kind) {
                Some(SourceKind::RawStdin { queue }) => queue.clone(),
                _ => return,
            }
        };
        let Some(chunk) = queue.begin_write() else {
            return;
        };
        let mut op = Box::new(WriteOp {
            // SAFETY: an all-zero OVERLAPPED is the documented form for pipe I/O.
            ov: unsafe { std::mem::zeroed() },
            chunk,
        });
        let handle = self.slots[index].handle;
        // SAFETY: the chunk is owned by `op` until the completion is dequeued
        // and `op` is boxed so the OVERLAPPED address does not move.
        let ok = unsafe {
            WriteFile(
                handle,
                op.chunk.as_ptr().cast(),
                op.chunk.len() as u32,
                ptr::null_mut(),
                &mut op.ov,
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                let chunk = std::mem::take(&mut op.chunk);
                queue.finish_write(chunk, 0);
                if let Some(source) = self.slots[index].source.as_mut() {
                    source.stop = Some(Stop::Plugin(RawFailure::PluginWriteError));
                }
                self.retire_stopped();
                return;
            }
        }
        self.slots[index].write = Some(op);
        self.slots[index].pending += 1;
    }

    fn complete(&mut self, key: usize, overlapped: *mut OVERLAPPED) {
        if key == WAKE_KEY {
            return;
        }
        let Some(index) = self
            .slots
            .iter()
            .position(|slot| (&**slot as *const Slot) as usize == key)
        else {
            // The slot was dropped only after its pending count reached zero,
            // so a packet always finds its slot.
            return;
        };
        let is_read = self.slots[index]
            .read
            .as_ref()
            .is_some_and(|op| ptr::eq(&op.ov, overlapped));
        if is_read {
            self.complete_read(index);
            return;
        }
        let is_write = self.slots[index]
            .write
            .as_ref()
            .is_some_and(|op| ptr::eq(&op.ov, overlapped));
        if is_write {
            self.complete_write(index);
        }
    }

    fn complete_read(&mut self, index: usize) {
        let Some(op) = self.slots[index].read.take() else {
            return;
        };
        self.slots[index].pending = self.slots[index].pending.saturating_sub(1);
        let (ok, transferred, error) = self.result(index, &op.ov);
        if self.slots[index].retire {
            self.retire_done();
            return;
        }
        let raw = matches!(
            self.slots[index].source.as_ref().map(|s| &s.kind),
            Some(SourceKind::RawStdout { .. })
        );
        let Some(source) = self.slots[index].source.as_mut() else {
            return;
        };
        if !ok {
            let error = error.unwrap_or_else(io::Error::last_os_error);
            match error.raw_os_error() {
                Some(code)
                    if code == ERROR_OPERATION_ABORTED as i32
                        || code == ERROR_BROKEN_PIPE as i32
                        || code == ERROR_HANDLE_EOF as i32
                        || code == ERROR_PIPE_NOT_CONNECTED as i32 =>
                {
                    if raw {
                        on_raw_eof(source);
                    } else {
                        on_stream_eof(source);
                    }
                }
                _ => {
                    if raw {
                        on_raw_eof(source);
                    } else {
                        on_stream_error(source, error);
                    }
                }
            }
        } else if transferred == 0 {
            if raw {
                on_raw_eof(source);
            } else {
                on_stream_eof(source);
            }
        } else if raw {
            on_raw_bytes(source, &op.buf[..transferred as usize]);
        } else {
            on_stream_bytes(source, &op.buf[..transferred as usize]);
        }
        if self.slots[index]
            .source
            .as_ref()
            .is_some_and(|s| s.stop.is_some())
        {
            self.retire_stopped();
        } else {
            self.issue_read(index);
        }
    }

    fn complete_write(&mut self, index: usize) {
        let Some(op) = self.slots[index].write.take() else {
            return;
        };
        self.slots[index].pending = self.slots[index].pending.saturating_sub(1);
        let (ok, transferred, error) = self.result(index, &op.ov);
        let queue = match self.slots[index].source.as_ref().map(|s| &s.kind) {
            Some(SourceKind::RawStdin { queue }) => queue.clone(),
            _ => return,
        };
        queue.finish_write(op.chunk, if ok { transferred as usize } else { 0 });
        if !ok && !self.slots[index].retire {
            let aborted = error
                .as_ref()
                .and_then(|error| error.raw_os_error())
                .is_some_and(|code| code == ERROR_OPERATION_ABORTED as i32);
            if let Some(source) = self.slots[index].source.as_mut() {
                source.stop = Some(if aborted {
                    Stop::Single
                } else {
                    Stop::Plugin(RawFailure::PluginWriteError)
                });
            }
        }
        if self.slots[index].retire
            || self.slots[index]
                .source
                .as_ref()
                .is_some_and(|s| s.stop.is_some())
        {
            self.retire_stopped();
        } else {
            self.issue_write(index);
        }
    }

    fn result(&self, index: usize, overlapped: &OVERLAPPED) -> (bool, u32, Option<io::Error>) {
        let mut transferred = 0u32;
        // SAFETY: the overlapped belongs to an operation on this live handle
        // and the completion was just dequeued; bWait=FALSE never blocks.
        let ok = unsafe {
            GetOverlappedResult(
                self.slots[index].handle,
                overlapped as *const OVERLAPPED as *mut OVERLAPPED,
                &mut transferred,
                0,
            )
        };
        if ok != 0 {
            (true, transferred, None)
        } else {
            (false, 0, Some(io::Error::last_os_error()))
        }
    }

    fn read_failed(&mut self, index: usize, error: io::Error) {
        let raw = matches!(
            self.slots[index].source.as_ref().map(|s| &s.kind),
            Some(SourceKind::RawStdout { .. })
        );
        if let Some(source) = self.slots[index].source.as_mut() {
            if error.raw_os_error() == Some(ERROR_OPERATION_ABORTED as i32) {
                source.stop = Some(Stop::Single);
            } else if raw {
                on_raw_eof(source);
            } else if matches!(
                error.raw_os_error(),
                Some(code)
                    if code == ERROR_BROKEN_PIPE as i32
                        || code == ERROR_HANDLE_EOF as i32
                        || code == ERROR_PIPE_NOT_CONNECTED as i32
            ) {
                on_stream_eof(source);
            } else {
                on_stream_error(source, error);
            }
        }
        self.retire_stopped();
    }

    fn retry_blocked(&mut self) {
        for index in 0..self.slots.len() {
            let slot = &mut self.slots[index];
            if slot.retire {
                continue;
            }
            let Some(source) = slot.source.as_mut() else {
                continue;
            };
            if !source.blocked {
                continue;
            }
            source.blocked = false;
            if matches!(source.kind, SourceKind::RawStdin { .. }) {
                continue;
            }
            if !drain_source(source) {
                source.stop = Some(stop_for(source, RawFailure::FrameTooLarge));
            }
        }
        self.retire_stopped();
    }

    fn pump(&mut self) {
        for index in 0..self.slots.len() {
            if self.slots[index].retire {
                continue;
            }
            let raw_stdin = matches!(
                self.slots[index].source.as_ref().map(|s| &s.kind),
                Some(SourceKind::RawStdin { .. })
            );
            if raw_stdin {
                self.issue_write(index);
            } else {
                self.issue_read(index);
            }
        }
        self.retire_stopped();
    }

    fn blocked_any(&self) -> bool {
        self.slots
            .iter()
            .any(|slot| !slot.retire && slot.source.as_ref().is_some_and(|source| source.blocked))
    }

    fn retire_stopped(&mut self) {
        // One stop can cover every pipe a plugin owns, so the report is
        // deduplicated within this pass only: a plugin that restarts with the
        // same key must report its next transport failure too.
        let mut reported = HashSet::new();
        for index in 0..self.slots.len() {
            if self.slots[index].retire {
                continue;
            }
            let stopped = self.slots[index]
                .source
                .as_ref()
                .is_some_and(|source| source.stop.is_some());
            if stopped {
                self.retire_slot(index, &mut reported);
            }
        }
        self.retire_done();
    }

    fn retire_slot(&mut self, index: usize, reported: &mut HashSet<String>) {
        let slot = &mut self.slots[index];
        if slot.retire {
            return;
        }
        slot.retire = true;
        cancel(&slot.handle);
        let closing = slot.source.as_ref().and_then(|source| match source.stop {
            Some(Stop::Plugin(reason)) => Some((source.key.clone(), source.hub.clone(), reason)),
            _ => None,
        });
        let Some((key, hub, reason)) = closing else {
            return;
        };
        if reported.insert(key.clone()) {
            report_closing(&key, &hub, reason);
        }
        // One plugin's transport covers every pipe it owns.
        for slot in self.slots.iter_mut() {
            if slot.source.as_ref().is_some_and(|source| source.key == key) {
                slot.retire = true;
                cancel(&slot.handle);
            }
        }
    }

    fn retire_key(&mut self, key: &str) {
        for slot in self.slots.iter_mut() {
            if slot.source.as_ref().is_some_and(|source| source.key == key) {
                slot.retire = true;
                cancel(&slot.handle);
            }
        }
        self.retire_done();
    }

    fn retire_done(&mut self) {
        let mut index = 0;
        while index < self.slots.len() {
            if self.slots[index].retire && self.slots[index].pending == 0 {
                self.slots.remove(index);
            } else {
                index += 1;
            }
        }
    }

    /// Dequeue every cancelled operation so no OVERLAPPED outlives its slot.
    ///
    /// Bounded on purpose: a driver that never completes a cancelled operation
    /// must not hang daemon shutdown. A timeout is logged; the caller closes
    /// the handles right after, which is the only remaining way to stop it.
    fn drain_pending(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.slots.iter().any(|slot| slot.pending > 0) {
            let now = Instant::now();
            if now >= deadline {
                let left: usize = self.slots.iter().map(|slot| slot.pending).sum();
                error!("reactor IOCP shutdown left {} operations pending", left);
                return;
            }
            let wait = (deadline - now).min(Duration::from_millis(100));
            let timeout_ms = u32::try_from(wait.as_millis()).unwrap_or(100);
            // SAFETY: an all-zero OVERLAPPED_ENTRY is valid (integers and
            // pointers) and the array is live for the call below.
            let mut entries: [OVERLAPPED_ENTRY; 64] = unsafe { std::mem::zeroed() };
            let mut count = 0u32;
            // SAFETY: the port is live, the array holds 64 writable entries and
            // the count pointer is a valid out-pointer.
            let ok = unsafe {
                GetQueuedCompletionStatusEx(
                    self.port,
                    entries.as_mut_ptr(),
                    entries.len() as u32,
                    &mut count,
                    timeout_ms,
                    0,
                )
            };
            if ok == 0 {
                continue;
            }
            for entry in entries.iter().take(count as usize) {
                self.complete(entry.lpCompletionKey, entry.lpOverlapped);
            }
            self.retire_done();
        }
    }
}

// SAFETY: every field is only touched while the reactor's mutex is held (or at
// drop, after the loop is gone). The port and the pipe handles are thread-safe
// kernel objects; the boxed slot addresses are keys, never dereferenced by
// another thread.
unsafe impl Send for IocpSet {}

fn cancel(handle: &HANDLE) {
    // SAFETY: the handle is live; a null overlapped cancels every operation on
    // it, and the completions still arrive so the slot can be freed.
    unsafe {
        CancelIoEx(*handle, ptr::null());
    }
}

impl Drop for IocpSet {
    fn drop(&mut self) {
        for slot in &mut self.slots {
            slot.retire = true;
            cancel(&slot.handle);
        }
        // Cancellation is asynchronous: the completion packet and the
        // OVERLAPPED it carries still belong to the kernel until it is
        // dequeued. Closing the port first would free the slot and its buffer
        // underneath an operation that is still writing into them.
        self.drain_pending();
        // SAFETY: the port is owned by this backend, the loop is gone and every
        // issued operation has completed, so no slot is referenced any more.
        unsafe {
            let _ = windows_sys::Win32::Foundation::CloseHandle(self.port);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::Instant;

    use super::*;

    #[test]
    fn a_frame_arrives_through_the_completion_port() {
        let (read, write) = crate::wire::contract::stdio::test_pipe().expect("pipe pair");
        let (sender, receiver) = mpsc::sync_channel(4);
        let reactor = Reactor::start().expect("reactor");
        reactor
            .register(
                "demo".into(),
                read.into_file(),
                None,
                sender,
                None,
                None,
                4096,
                Framing::Lines,
            )
            .expect("register");
        let mut write = write;
        write.write_all(b"hello\n").expect("write");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut frame = None;
        while frame.is_none() {
            assert!(Instant::now() < deadline, "frame never arrived");
            reactor.poll_once(Duration::from_millis(50));
            if let Ok(FrameEvent::Frame(result)) = receiver.try_recv() {
                frame = Some(result.expect("frame"));
            }
        }
        assert_eq!(frame.as_deref(), Some("hello"));
    }
    #[derive(Default)]
    struct TestSink {
        delivered: Mutex<Vec<(String, u32, Vec<u8>)>>,
        failures: Mutex<Vec<(RawFailure, String)>>,
    }

    impl RawStopSink for TestSink {
        fn note_transport_failure(&self, failure: RawFailure, plugin: &str) {
            self.failures
                .lock()
                .expect("sink")
                .push((failure, plugin.to_string()));
        }
        fn unregister_generation(&self, _id: &str, _generation: u64) {}
        fn deliver(&self, id: &str, stream: u32, payload: Vec<u8>) {
            self.delivered
                .lock()
                .expect("sink")
                .push((id.to_string(), stream, payload));
        }
    }

    /// Protocol-3 carries raw frames on both directions. Push a frame into the
    /// stdin queue and read it from the child end; write a frame into the
    /// stdout pair and see it delivered by the hub sink. This is the write
    /// completion path, which the line-framing test above does not touch.
    #[test]
    fn raw_frames_cross_both_directions_through_the_completion_port() {
        use std::io::{Read, Write};

        let (child_read, parent_write) =
            crate::wire::contract::stdio::test_write_pipe().expect("stdin pair");
        let (parent_read, child_write) =
            crate::wire::contract::stdio::test_pipe().expect("stdout pair");
        let sink = Arc::new(TestSink::default());
        let dyn_sink: Arc<dyn RawStopSink> = sink.clone();
        let weak = Arc::downgrade(&dyn_sink);
        let reactor = Reactor::start().expect("reactor");
        let queue = reactor
            .register_raw(
                "demo".into(),
                parent_write,
                parent_read.into_file(),
                None,
                None,
                weak,
                1,
                1024,
                4,
            )
            .expect("register");

        let frame = b"\x00\x00\x00\x05\x00\x00\x00\x07hello".to_vec();
        queue.push(frame.clone()).expect("push");
        let (sent, received) = mpsc::channel();
        let expected = frame.clone();
        std::thread::spawn(move || {
            let mut child = child_read;
            let mut got = vec![0u8; expected.len()];
            child.read_exact(&mut got).expect("child read");
            let _ = sent.send(got);
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            reactor.poll_once(Duration::from_millis(50));
            if let Ok(got) = received.try_recv() {
                assert_eq!(got, frame);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "stdin frame never reached the child"
            );
        }

        let mut child_write = child_write;
        let out = b"\x00\x00\x00\x03\x00\x00\x00\x09abc".to_vec();
        child_write.write_all(&out).expect("child write");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            reactor.poll_once(Duration::from_millis(50));
            let delivered = sink.delivered.lock().expect("sink");
            if let Some((id, stream, payload)) = delivered.first() {
                assert_eq!(id, "demo");
                assert_eq!(*stream, 9);
                assert_eq!(payload, b"abc");
                break;
            }
            drop(delivered);
            assert!(Instant::now() < deadline, "stdout frame never arrived");
        }
    }

    /// A stopped plugin still owns every operation the kernel has not finished.
    /// The slot (and the pipe handle inside it) must survive until the cancel
    /// completion is dequeued, so nothing frees a buffer mid-write.
    #[test]
    fn a_stopped_source_keeps_its_slot_until_the_cancelled_io_completes() {
        use std::io::Read;

        let (mut child_read, parent_write) =
            crate::wire::contract::stdio::test_write_pipe().expect("stdin pair");
        let (parent_read, _child_write) =
            crate::wire::contract::stdio::test_pipe().expect("stdout pair");
        let sink = Arc::new(TestSink::default());
        let dyn_sink: Arc<dyn RawStopSink> = sink;
        let weak = Arc::downgrade(&dyn_sink);
        let reactor = Reactor::start().expect("reactor");
        let queue = reactor
            .register_raw(
                "demo".into(),
                parent_write,
                parent_read.into_file(),
                None,
                None,
                weak,
                1,
                1024,
                4,
            )
            .expect("register");
        queue.push(vec![1, 2, 3]).expect("push");
        reactor.poll_once(Duration::ZERO);
        {
            let backend = reactor.backend.lock().expect("backend");
            assert_eq!(backend.slots.len(), 2, "both pipes are registered");
            let stdout = backend
                .slots
                .iter()
                .find(|slot| {
                    matches!(
                        slot.source.as_ref().map(|source| &source.kind),
                        Some(SourceKind::RawStdout { .. })
                    )
                })
                .expect("stdout slot");
            assert!(
                stdout.read.is_some() && stdout.pending > 0,
                "the child wrote nothing, so the stdout read must still be pending"
            );
        }

        reactor.unregister("demo");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            reactor.poll_once(Duration::from_millis(20));
            if reactor.backend.lock().expect("backend").slots.is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "cancelled slots never drained");
        }

        // The parent end lived in the slot: once the slot is gone its handle is
        // closed, so the child side reaches EOF instead of staying open.
        let mut total = 0usize;
        let mut buffer = [0u8; 8];
        loop {
            match child_read.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => total += read,
                Err(error) => panic!("child read: {}", error),
            }
        }
        assert!(total <= 3, "the cancelled write cannot duplicate bytes");
    }

    /// One stop reports once, but a plugin that restarts under the same key
    /// must report its next failure: the report set is per stop, not per life.
    #[test]
    fn a_restarted_plugin_reports_its_next_transport_failure() {
        let sink = Arc::new(TestSink::default());
        let dyn_sink: Arc<dyn RawStopSink> = sink.clone();
        let reactor = Reactor::start().expect("reactor");

        for round in 0..2usize {
            let (_child_read, parent_write) =
                crate::wire::contract::stdio::test_write_pipe().expect("stdin pair");
            let (parent_read, child_write) =
                crate::wire::contract::stdio::test_pipe().expect("stdout pair");
            let _queue = reactor
                .register_raw(
                    "demo".into(),
                    parent_write,
                    parent_read.into_file(),
                    None,
                    None,
                    Arc::downgrade(&dyn_sink),
                    1,
                    1024,
                    4,
                )
                .expect("register");
            // The child exits: its stdout end closes and the parent read sees EOF.
            drop(child_write);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                reactor.poll_once(Duration::from_millis(20));
                if sink.failures.lock().expect("sink").len() > round {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "failure {} was never reported",
                    round + 1
                );
            }
        }

        let failures = sink.failures.lock().expect("sink");
        assert_eq!(failures.len(), 2, "each stop reports exactly once");
        for (failure, plugin) in failures.iter() {
            assert_eq!(*failure, RawFailure::PluginEof);
            assert_eq!(plugin, "demo");
        }
    }
}
