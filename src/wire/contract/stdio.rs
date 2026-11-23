//! Child-process stdio: which pipes open, and who reads the parent ends.
//!
//! S6b (#142) pulls the last platform fork out of [`crate::supervise::ops::slot`] and
//! [`crate::manage::ops::hook`]. A caller says what the streams *mean* — JSON-RPC frames, a
//! plugin log, a hook's output — and this module owns two things instead:
//!
//! - **Which pipes exist.** [`Plan`] answers that from `Ready` and the
//!   protocol; [`arm`] creates the pipe pairs and puts their child ends on
//!   the `Command` before the spawn. The parent ends come back as
//!   [`ReadEnd`] / [`WriteEnd`], and every stream a plan does not pipe is
//!   `/dev/null`, never an inherited daemon handle.
//! - **Who reads them.** [`Target`] is either the supervisor's reactor (both
//!   hosts: Unix `poll(2)`, Windows overlapped named pipes + IOCP) or one
//!   reader thread per pipe when the reactor could not start. Callers never
//!   choose by `cfg`.
//!
//! The host half is `stdio/platform/{unix,windows}.rs`: Unix creates an
//! anonymous pipe with `O_CLOEXEC` and can put an end in non-blocking mode;
//! Windows creates an overlapped named-pipe pair for the parent end and lets
//! `Command::spawn` mark exactly the child's end inheritable (see that file for
//! why the parent ends must not be).
//!
//! Non-blocking is for the poll set's raw transport only: it drains in bursts
//! and needs `WouldBlock` as "empty". The frame/log reader takes one read per
//! readiness event, so its end stays blocking — a `WouldBlock` there would tear
//! the transport down.

mod platform;

use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::thread;

use super::RAW_PROTOCOL;
use crate::platform::host::threads;
use crate::platform::host::wake::Wake;
use crate::platform::log::{Level, LogWriter};
use crate::wire::message::ipc::{self, FrameEvent};

/// Whether one standard stream is piped to the parent or opened on `/dev/null`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Open {
    Pipe,
    Null,
}

/// The stdio a spawn asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Plan {
    pub(crate) stdin: Open,
    pub(crate) stdout: Open,
    pub(crate) stderr: Open,
}

impl Plan {
    /// A notify-ready plugin: a JSON-RPC channel on stdin/stdout, a log on
    /// stderr.
    pub(crate) fn notify() -> Self {
        Self {
            stdin: Open::Pipe,
            stdout: Open::Pipe,
            stderr: Open::Pipe,
        }
    }

    /// An exec-ready service: protocol 3 carries raw frames on stdin, earlier
    /// protocols have no input channel at all.
    pub(crate) fn exec(protocol: u32) -> Self {
        Self {
            stdin: if protocol == RAW_PROTOCOL {
                Open::Pipe
            } else {
                Open::Null
            },
            stdout: Open::Pipe,
            stderr: Open::Pipe,
        }
    }

    /// An install/remove hook: output only, its stdin is closed.
    pub(crate) fn hook() -> Self {
        Self {
            stdin: Open::Null,
            stdout: Open::Pipe,
            stderr: Open::Pipe,
        }
    }

    /// The supervisor's stop action: only stderr is kept, stdout is dropped.
    pub(crate) fn stop_hook() -> Self {
        Self {
            stdin: Open::Null,
            stdout: Open::Null,
            stderr: Open::Pipe,
        }
    }
}

/// Parent ends of the pipes a plan opened.
#[derive(Default)]
pub(crate) struct Streams {
    pub(crate) stdin: Option<WriteEnd>,
    pub(crate) stdout: Option<ReadEnd>,
    pub(crate) stderr: Option<ReadEnd>,
}

/// The parent's read end of a pipe.
pub(crate) struct ReadEnd(File);

/// The parent's write end of a pipe.
pub(crate) struct WriteEnd(File);

impl Read for ReadEnd {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
}

impl Write for WriteEnd {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl ReadEnd {
    /// The reactor owns its handles as plain files.
    pub(crate) fn into_file(self) -> File {
        self.0
    }
}

/// Spawn `command` with the plan's pipes.
///
/// The child ends go onto the command before the spawn; the parent ends come
/// back as [`Streams`]. Take the command by value: a `Command` keeps whatever
/// `Stdio` it was given, which is *this process's* handle to the child's end.
/// Dropping the command at the end of this function closes those copies, so the
/// caller's read ends can reach EOF and the child sees its stdin close.
pub(crate) fn spawn(
    mut command: Command,
    plan: Plan,
) -> io::Result<(std::process::Child, Streams)> {
    let streams = arm(plan, &mut command)?;
    let child = command.spawn()?;
    Ok((child, streams))
}

/// Create the plan's pipes and arm `command` with their child ends.
///
/// Every pipe is created before `command` is touched, so a failure leaves the
/// command unarmed and the caller free to report it. Prefer [`spawn`]; this
/// step alone leaves the child ends owned by the command until it is dropped.
fn arm(plan: Plan, command: &mut Command) -> io::Result<Streams> {
    let (stdin_child, stdin_parent) = match plan.stdin {
        Open::Null => (None, None),
        Open::Pipe => {
            let (read, write) = platform::parent_write_pipe()?;
            (Some(read), Some(WriteEnd(write)))
        }
    };
    let (stdout_child, stdout_parent) = match plan.stdout {
        Open::Null => (None, None),
        Open::Pipe => {
            let (read, write) = platform::parent_read_pipe()?;
            (Some(write), Some(ReadEnd(read)))
        }
    };
    let (stderr_child, stderr_parent) = match plan.stderr {
        Open::Null => (None, None),
        Open::Pipe => {
            let (read, write) = platform::parent_read_pipe()?;
            (Some(write), Some(ReadEnd(read)))
        }
    };

    command
        .stdin(stdin_child.map(Stdio::from).unwrap_or_else(Stdio::null))
        .stdout(stdout_child.map(Stdio::from).unwrap_or_else(Stdio::null))
        .stderr(stderr_child.map(Stdio::from).unwrap_or_else(Stdio::null));
    Ok(Streams {
        stdin: stdin_parent,
        stdout: stdout_parent,
        stderr: stderr_parent,
    })
}

/// A connected pair for tests that need the reactor without a child process.
#[cfg(all(test, windows))]
pub(crate) fn test_pipe() -> io::Result<(ReadEnd, WriteEnd)> {
    let (read, write) = platform::parent_read_pipe()?;
    Ok((ReadEnd(read), WriteEnd(write)))
}

/// The write-oriented pair for tests: (child_read, parent_write).
#[cfg(all(test, windows))]
pub(crate) fn test_write_pipe() -> io::Result<(File, File)> {
    platform::parent_write_pipe()
}

/// Hand a read end to the reactor. Unix needs `O_NONBLOCK`; Windows named
/// pipes are overlapped and need no mode change.
pub(crate) fn poll_read(end: ReadEnd) -> io::Result<File> {
    #[cfg(unix)]
    platform::nonblocking(&end.0)?;
    Ok(end.0)
}

/// Hand a write end to the reactor, same contract as [`poll_read`].
pub(crate) fn poll_write(end: WriteEnd) -> io::Result<File> {
    #[cfg(unix)]
    platform::nonblocking(&end.0)?;
    Ok(end.0)
}

/// Read `stream` line by line into `log` on a thread of its own.
///
/// This is the consumer for pipes that are not part of a protocol channel: a
/// hook's output, an exec-ready service's log, a raw plugin's stderr. One
/// physical line is bounded by `log::MAX_LINE_BYTES`; the writer truncates the
/// message to 8 KiB afterwards (ADR-0022).
pub(crate) fn log_lines(
    name: String,
    stream: ReadEnd,
    log: Option<Arc<Mutex<LogWriter>>>,
    limits: &crate::platform::limits::Limits,
) -> io::Result<()> {
    thread::Builder::new()
        .name(name)
        .stack_size(threads::io_stack(limits))
        .spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut pending = Vec::new();
            let mut discarding = false;
            let mut chunk = [0u8; 8 * 1024];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(count) => {
                        pending.extend_from_slice(&chunk[..count]);
                        for line in take_log_lines(
                            &mut pending,
                            &mut discarding,
                            crate::platform::log::MAX_LINE_BYTES,
                        ) {
                            log_line(&log, &line);
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            if !discarding && !pending.is_empty() {
                log_line(&log, &String::from_utf8_lossy(&pending));
            }
        })
        .map(|_| ())
}

/// Split one read chunk into complete lines. A line longer than `max` is
/// handed over once (the writer truncates and marks it) and the rest of that
/// physical line is discarded; the source keeps reading (ADR-0022).
fn take_log_lines(pending: &mut Vec<u8>, discarding: &mut bool, max: usize) -> Vec<String> {
    let mut lines = Vec::new();
    loop {
        if *discarding {
            match pending.iter().position(|byte| *byte == b'\n') {
                Some(position) => {
                    pending.drain(..=position);
                    *discarding = false;
                }
                None => {
                    pending.clear();
                    break;
                }
            }
            continue;
        }
        if let Some(position) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = pending.drain(..=position).collect();
            let line = &line[..line.len() - 1];
            // CRLF is one terminator; a bare CR is message content.
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            lines.push(String::from_utf8_lossy(line).into_owned());
            continue;
        }
        if pending.len() > max {
            lines.push(String::from_utf8_lossy(pending).into_owned());
            pending.clear();
            *discarding = true;
        }
        break;
    }
    lines
}

fn log_line(log: &Option<Arc<Mutex<LogWriter>>>, line: &str) {
    if let Some(writer) = log {
        if let Ok(mut writer) = writer.lock() {
            let _ = writer.record(Level::Info, line);
        }
    }
}

/// Who consumes the parent read ends.
///
/// Cloning is cheap: it is an `Arc` on Unix and nothing on Windows. Slots hold
/// one so a slot created after the reactor started inherits the same target.
#[derive(Clone, Default)]
pub(crate) struct Target {
    reactor: Option<Arc<crate::wire::contract::reactor::Reactor>>,
}

impl Target {
    /// One reader thread per pipe: only when no reactor was installed (it
    /// failed to start, or this build has none).
    pub(crate) fn threaded() -> Self {
        Self::default()
    }

    /// The supervisor loop's wait set: no per-plugin threads.
    pub(crate) fn polled(reactor: Arc<crate::wire::contract::reactor::Reactor>) -> Self {
        Self {
            reactor: Some(reactor),
        }
    }

    /// Deliver `stdout` to `sender` as JSON-RPC frames (length-prefixed from
    /// protocol 2 on, line-delimited before) and `stderr` to the service log.
    #[allow(clippy::too_many_arguments)] // The arguments are the wiring seam; a struct would only rename them.
    pub(crate) fn frames(
        &self,
        id: String,
        protocol: u32,
        stdout: ReadEnd,
        stderr: Option<ReadEnd>,
        sender: SyncSender<FrameEvent>,
        wake: Option<Arc<Wake>>,
        log: Option<Arc<Mutex<LogWriter>>>,
        limits: &crate::platform::limits::Limits,
    ) -> io::Result<()> {
        if let Some(reactor) = &self.reactor {
            let framing = if protocol >= 2 {
                crate::wire::contract::reactor::Framing::Length
            } else {
                crate::wire::contract::reactor::Framing::Lines
            };
            return reactor.register(
                id,
                stdout.into_file(),
                stderr.map(ReadEnd::into_file),
                sender,
                wake,
                log,
                ipc::CALL_FRAME_MAX,
                framing,
            );
        }
        if protocol >= 2 {
            ipc::spawn_length_reader_limit(
                format!("svcd-{}-stdout", id),
                stdout,
                sender,
                wake,
                ipc::CALL_FRAME_MAX,
                limits,
            )?;
        } else {
            ipc::spawn_reader_limit(
                format!("svcd-{}-stdout", id),
                stdout,
                sender,
                wake,
                ipc::CALL_FRAME_MAX,
                limits,
            )?;
        }
        if let Some(stderr) = stderr {
            log_lines(format!("svcd-{}-stderr", id), stderr, log, limits)?;
        }
        Ok(())
    }

    /// Deliver both pipes to the service log; nothing here speaks a protocol.
    pub(crate) fn logs(
        &self,
        id: String,
        stdout: ReadEnd,
        stderr: Option<ReadEnd>,
        log: Option<Arc<Mutex<LogWriter>>>,
        limits: &crate::platform::limits::Limits,
    ) -> io::Result<()> {
        if let Some(reactor) = &self.reactor {
            return reactor.register_log(
                id,
                stdout.into_file(),
                stderr.map(ReadEnd::into_file),
                log,
            );
        }
        log_lines(format!("svcd-{}-stdout", id), stdout, log.clone(), limits)?;
        if let Some(stderr) = stderr {
            log_lines(format!("svcd-{}-stderr", id), stderr, log, limits)?;
        }
        Ok(())
    }

    /// Drop every source of `id` from the poll set (a no-op for reader
    /// threads, which end on their own when the pipes close).
    pub(crate) fn unregister(&self, id: &str) {
        if let Some(reactor) = &self.reactor {
            reactor.unregister(id);
        }
    }

    /// One non-blocking pass. The supervisor loop calls this while it is busy
    /// in a synchronous wait (an install hook) so plugin pipes keep moving;
    /// reader threads need no equivalent.
    pub(crate) fn pump(&self) {
        if let Some(reactor) = &self.reactor {
            reactor.poll_once(std::time::Duration::ZERO);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Run `command` to completion, but never hang a test run on a child that
    /// is waiting for a stdin we forgot to close.
    fn wait(child: &mut std::process::Child) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match child.try_wait().expect("try_wait") {
                Some(status) => return status,
                None if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                None => {
                    let _ = child.kill();
                    panic!("child did not exit");
                }
            }
        }
    }

    /// A command that writes one line to each of stdout and stderr.
    fn talker() -> Command {
        #[cfg(windows)]
        {
            let mut command = Command::new("cmd.exe");
            command.args(["/C", "echo out & echo err 1>&2"]);
            command
        }
        #[cfg(unix)]
        {
            let mut command = Command::new("sh");
            command.args(["-c", "echo out; echo err >&2"]);
            command
        }
    }

    /// A command that waits for one stdin line and exits successfully.
    fn waits_for_a_line() -> Command {
        #[cfg(windows)]
        {
            let mut command = Command::new("cmd.exe");
            command.args(["/C", "set /p line="]);
            command
        }
        #[cfg(unix)]
        {
            let mut command = Command::new("sh");
            command.args(["-c", "read line"]);
            command
        }
    }

    fn read_all(end: ReadEnd) -> String {
        let mut text = String::new();
        BufReader::new(end)
            .read_to_string(&mut text)
            .expect("read to EOF");
        text
    }

    #[test]
    fn notify_pipes_every_stream_and_both_ends_work() {
        let (mut child, streams) = spawn(talker(), Plan::notify()).expect("spawn");
        let stdout = streams.stdout.expect("stdout pipe");
        let stderr = streams.stderr.expect("stderr pipe");
        assert!(streams.stdin.is_some(), "notify needs a stdin channel");

        let out = read_all(stdout);
        let err = read_all(stderr);
        drop(streams.stdin);
        assert!(wait(&mut child).success(), "talker exits 0");
        assert!(out.contains("out"), "stdout was {:?}", out);
        assert!(err.contains("err"), "stderr was {:?}", err);
    }

    #[test]
    fn protocol_3_is_the_only_exec_plan_with_stdin() {
        assert_eq!(Plan::exec(3).stdin, Open::Pipe);
        assert_eq!(Plan::exec(2).stdin, Open::Null);
        assert_eq!(Plan::exec(1).stdin, Open::Null);
        assert_eq!(Plan::exec(3).stdout, Open::Pipe);
        assert_eq!(Plan::exec(3).stderr, Open::Pipe);
    }

    #[test]
    fn hooks_pipe_output_only_and_keep_stdin_closed() {
        assert_eq!(Plan::hook().stdin, Open::Null);
        assert_eq!(Plan::hook().stdout, Open::Pipe);
        assert_eq!(Plan::hook().stderr, Open::Pipe);
        assert_eq!(Plan::stop_hook().stdout, Open::Null);
        assert_eq!(Plan::stop_hook().stderr, Open::Pipe);
    }

    /// The stdin pipe must be the child's real stdin: a line reaches it, and
    /// closing the parent end is EOF.
    #[test]
    fn closing_the_parent_stdin_is_eof_for_the_child() {
        let (mut child, streams) = spawn(waits_for_a_line(), Plan::notify()).expect("spawn");
        let mut stdin = streams.stdin.expect("stdin pipe");
        stdin.write_all(b"line\n").expect("write stdin");
        stdin.flush().expect("flush stdin");
        drop(stdin);
        assert!(wait(&mut child).success(), "child read the line and exited");
    }

    /// The thread target is the Windows production path and the Unix fallback;
    /// it must turn a JSON-RPC stdout line into a frame.
    #[test]
    fn threaded_target_delivers_frames() {
        let (mut child, streams) = spawn(talker(), Plan::notify()).expect("spawn");
        let (sender, receiver) = std::sync::mpsc::sync_channel(8);
        Target::threaded()
            .frames(
                "t01".into(),
                1,
                streams.stdout.expect("stdout"),
                None,
                sender,
                None,
                None,
                &crate::platform::limits::test_default(),
            )
            .expect("reader threads");
        drop(streams.stdin);
        let frame = receiver
            .recv_timeout(Duration::from_secs(20))
            .expect("a frame");
        match frame {
            FrameEvent::Frame(Ok(line)) => assert_eq!(line.trim(), "out"),
            other => panic!("expected the first line, got {:?}", other),
        }
        assert!(wait(&mut child).success());
    }

    #[cfg(unix)]
    #[test]
    fn the_poll_consumer_gets_a_non_blocking_read_end() {
        // The raw transport drains in bursts and treats WouldBlock as "empty";
        // without O_NONBLOCK that read would block the supervisor loop.
        let (read, write) = platform::pipe().expect("pipe");
        let mut end = poll_read(ReadEnd(read)).expect("non-blocking read end");
        let mut buffer = [0u8; 8];
        let error = end.read(&mut buffer).expect_err("empty poll end");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        drop(write);
    }

    #[cfg(unix)]
    #[test]
    fn a_plain_read_end_still_reports_eof() {
        // Reader threads take the end as it is: the pipe layer must not have
        // touched its mode, or a `WouldBlock` would look like end-of-file.
        let (read, write) = platform::pipe().expect("pipe");
        let mut end = ReadEnd(read);
        drop(write);
        let mut buffer = [0u8; 8];
        assert_eq!(end.read(&mut buffer).expect("EOF"), 0);
    }

    #[test]
    fn an_overlong_line_is_handed_over_once_and_the_source_keeps_reading() {
        let mut pending = vec![b'x'; 70 * 1024];
        let mut discarding = false;
        let lines = take_log_lines(&mut pending, &mut discarding, 64 * 1024);
        assert_eq!(lines.len(), 1, "the overlong line must still be recorded");
        assert_eq!(lines[0].len(), 70 * 1024);
        assert!(discarding);
        assert!(pending.is_empty());

        // The rest of the same physical line is dropped, not turned into a
        // second record.
        pending.extend_from_slice(b"tail-without-newline");
        assert!(take_log_lines(&mut pending, &mut discarding, 64 * 1024).is_empty());
        assert!(pending.is_empty());
        assert!(discarding);

        // Its newline ends the discard; the next line is read normally.
        pending.extend_from_slice(b"\nafter\nnext");
        assert_eq!(
            take_log_lines(&mut pending, &mut discarding, 64 * 1024),
            vec!["after".to_string()]
        );
        assert!(!discarding);
        assert_eq!(pending, b"next");
        // The trailing partial line arrives with the next chunk.
        pending.push(b'\n');
        assert_eq!(
            take_log_lines(&mut pending, &mut discarding, 64 * 1024),
            vec!["next".to_string()]
        );
        assert!(pending.is_empty());
    }
}
