use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::OnceLock;

use crate::platform::clock::Clock;

/// Anything that holds the single supervisor loop longer than this is logged.
/// The loop is serial, so one slow phase delays every other caller; the 30 s
/// management deadline then shows up as a 504 far from the real cause.
const SLOW_LOOP_MS: u64 = 1_000;

/// One message's byte budget in a log line. A longer message is cut on a UTF-8
/// boundary and marked with the number of bytes dropped; the log source keeps
/// reading (ADR-0022).
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024;

/// Bound for one physical log line before the writer sees it. The reactor and
/// the thread reader both use this, so the two paths cannot disagree.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// Log a loop phase that ran past [`SLOW_LOOP_MS`]. Shared by the compose loop
/// and the supervisor so the threshold has one home.
pub fn note_slow(what: &str, started: std::time::Instant) {
    let elapsed = started.elapsed();
    if elapsed >= std::time::Duration::from_millis(SLOW_LOOP_MS) {
        warn!("{} held the loop for {:?}", what, elapsed);
    }
}

/// A log line's severity. Ordered from most to least severe, so the write side
/// keeps a record when `level <= threshold`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Level {
    /// Every level, most severe first. Used by the config parser's error text.
    pub const ALL: [Level; 5] = [
        Level::Error,
        Level::Warn,
        Level::Info,
        Level::Debug,
        Level::Trace,
    ];

    /// The fixed five-byte column: `ERROR` / `WARN ` / `INFO ` / `DEBUG` /
    /// `TRACE`. The trailing space is part of the contract, not cosmetic.
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }

    /// Parse a configured level. Case-insensitive, exact five names; `None`
    /// lets the settings parser report the key and line number.
    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_lowercase().as_str() {
            "error" => Some(Level::Error),
            "warn" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            "trace" => Some(Level::Trace),
            _ => None,
        }
    }
}

/// The fixed length of the timestamp column: `YYYY-MM-DDTHH:MM:SS.mmmZ`.
const STAMP_LEN: usize = 24;

/// Read the level column of a new-format line. Returns `None` for an old
/// (unlevelled) line, which the read side must pass through unchanged.
pub fn parse_level(line: &str) -> Option<Level> {
    let bytes = line.as_bytes();
    if bytes.len() < STAMP_LEN + 7 || !valid_stamp(&bytes[..STAMP_LEN]) {
        return None;
    }
    if bytes[STAMP_LEN] != b' ' || bytes[STAMP_LEN + 6] != b' ' {
        return None;
    }
    // Validate the level column as UTF-8 before slicing it: a legacy line may
    // put a multi-byte character across these byte offsets.
    let level = std::str::from_utf8(&bytes[STAMP_LEN + 1..STAMP_LEN + 6]).ok()?;
    Level::parse(level.trim())
}

/// The timestamp column of a new-format line, byte-comparable in time order
/// (RFC3339 UTC with a fixed width). `None` for an old-format line.
pub fn timestamp(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    if bytes.len() >= STAMP_LEN && valid_stamp(&bytes[..STAMP_LEN]) {
        // The validated prefix is ASCII, so byte STAMP_LEN is a char boundary.
        Some(&line[..STAMP_LEN])
    } else {
        None
    }
}

fn valid_stamp(bytes: &[u8]) -> bool {
    if bytes.len() != STAMP_LEN {
        return false;
    }
    for (index, byte) in bytes.iter().enumerate() {
        let expected = match index {
            4 | 7 => b'-',
            10 => b'T',
            13 | 16 => b':',
            19 => b'.',
            23 => b'Z',
            _ => {
                if !byte.is_ascii_digit() {
                    return false;
                }
                continue;
            }
        };
        if *byte != expected {
            return false;
        }
    }
    true
}

/// Whether a line passes a display threshold. The one place the read side
/// compares levels: an old-format line has no level and always passes
/// (ADR-0022).
pub fn keep(line: &str, threshold: Level) -> bool {
    parse_level(line).is_none_or(|level| level <= threshold)
}

/// Keep the lines at or above `threshold`.
pub fn filter_text(text: &str, threshold: Level) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if keep(line, threshold) {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Rotation threshold used when a caller does not have a configured limit
/// (unit tests and the generated sample defaults).
#[cfg(test)]
pub const DEFAULT_LIMIT: u64 = 64 * 1024;

/// The low-level file writer. It stays "dumb": one call writes one line and
/// rotates when needed. Formatting, filtering and truncation live in
/// [`LogWriter`] above it (ADR-0022).
pub struct LogSink {
    path: PathBuf,
    file: Option<File>,
    size: u64,
    limit: u64,
    rotations: usize,
    enabled: bool,
}

impl LogSink {
    /// Open a sink using the limits derived once at the composition root.
    pub fn open(
        dir: &Path,
        id: &str,
        limits: &crate::platform::limits::Limits,
    ) -> io::Result<Self> {
        Self::open_with(
            dir,
            id,
            limits.log_max_bytes,
            limits.log_rotations,
            limits.log_enabled && limits.log_to_file,
        )
    }

    pub fn open_with(
        dir: &Path,
        id: &str,
        limit: u64,
        rotations: usize,
        enabled: bool,
    ) -> io::Result<Self> {
        if !enabled {
            return Ok(LogSink {
                path: dir.join(format!("{}.log", id)),
                file: None,
                size: 0,
                limit,
                rotations,
                enabled: false,
            });
        }
        fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.log", id));
        let mut sink = LogSink {
            path,
            file: None,
            size: 0,
            limit,
            rotations: rotations.max(1),
            enabled: true,
        };
        let existing = fs::metadata(&sink.path).map(|meta| meta.len()).unwrap_or(0);
        if existing > sink.limit {
            sink.rotate()?;
        }
        sink.file = Some(open_append(&sink.path)?);
        sink.size = fs::metadata(&sink.path).map(|meta| meta.len()).unwrap_or(0);
        Ok(sink)
    }

    pub fn line(&mut self, line: &str) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let mut bytes = line.as_bytes().to_vec();
        if !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        if self.size.saturating_add(bytes.len() as u64) > self.limit {
            self.rotate()?;
        }
        if let Some(file) = self.file.as_mut() {
            file.write_all(&bytes)?;
            file.flush()?;
        }
        self.size = self.size.saturating_add(bytes.len() as u64);
        Ok(())
    }

    /// Truncate this sink's file and drop every rotated sibling, then reopen so
    /// the next line lands in a fresh file. The handle is closed first: on
    /// Windows, deleting a file this process still holds open leaves the write
    /// side appending to an invisible file (#262).
    pub fn reset(&mut self) -> io::Result<usize> {
        self.file.take();
        let mut removed = 0;
        for number in 1..self.rotations {
            removed += remove_if_exists(&rotated(&self.path, number))?;
        }
        removed += remove_if_exists(&self.path)?;
        self.size = 0;
        if self.enabled {
            self.file = Some(open_append(&self.path)?);
        }
        Ok(removed)
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.take();
        let newest = self.path.clone();
        if self.rotations <= 1 {
            let _ = fs::remove_file(&newest);
            self.file = Some(open_append(&newest)?);
            self.size = 0;
            return Ok(());
        }
        let oldest = rotated(&newest, self.rotations - 1);
        match fs::remove_file(&oldest) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        for number in (1..self.rotations - 1).rev() {
            let from = rotated(&newest, number);
            if from.exists() {
                fs::rename(from, rotated(&newest, number + 1))?;
            }
        }
        if newest.exists() {
            fs::rename(&newest, rotated(&newest, 1))?;
        }
        self.file = Some(open_append(&newest)?);
        self.size = 0;
        Ok(())
    }
}

/// One writer per file: the sink plus the line contract (ADR-0022).
///
/// `source` is an opaque label the platform does not interpret (`svcd`,
/// `plugin:<id>`, `hook:<id>`, `stats`). `enabled` is the master switch; a
/// disabled writer writes neither the file nor stderr.
pub struct LogWriter {
    sink: LogSink,
    source: String,
    level: Level,
    enabled: bool,
    stderr: bool,
    clock: Clock,
}

impl LogWriter {
    /// Open the log for one source. A disabled master switch or
    /// `log_to_file = off` means no file is created; an open failure is
    /// returned so the caller can explain it once.
    pub fn open(
        dir: &Path,
        id: &str,
        source: &str,
        limits: &crate::platform::limits::Limits,
        clock: Clock,
        stderr: bool,
    ) -> io::Result<Self> {
        Ok(LogWriter {
            sink: LogSink::open(dir, id, limits)?,
            source: source.to_string(),
            level: limits.log_level,
            enabled: limits.log_enabled,
            stderr,
            clock,
        })
    }

    /// Clear this writer's files (current plus rotations). The writer object
    /// stays the same, so a thread that already holds it keeps writing into the
    /// new file.
    pub fn reset(&mut self) -> io::Result<usize> {
        self.sink.reset()
    }

    /// Format, filter and truncate one record, then hand it to the sink.
    pub fn record(&mut self, level: Level, message: &str) -> io::Result<()> {
        if !self.enabled || level > self.level {
            return Ok(());
        }
        let line = format_line(self.clock.epoch_ms(), level, &self.source, message);
        if self.stderr {
            eprintln!("{}", line);
        }
        self.sink.line(&line)
    }
}

fn rotated(path: &Path, number: usize) -> PathBuf {
    PathBuf::from(format!("{}.{}", path.display(), number))
}

fn open_append(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o640);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o640))?;
        set_group(path);
    }
    Ok(file)
}

#[cfg(unix)]
fn set_group(path: &Path) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    // Best effort: the service account/group is created by the installer; a
    // development tree may not have it. The mode is already 0640.
    let Ok(name) = CString::new("svcd") else {
        return;
    };
    // SAFETY: The handle comes from the process-wide log sink and is only used while that sink is alive.
    let group = unsafe { libc::getgrnam(name.as_ptr()) };
    if group.is_null() {
        return;
    }
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    // SAFETY: The handle comes from the process-wide log sink and is only used while that sink is alive.
    unsafe {
        libc::chown(path.as_ptr(), 0, (*group).gr_gid);
    }
}

/// `<RFC3339 UTC milliseconds> <LEVEL> <source> <message>`.
///
/// CR/LF become a literal backslash-n so one line is always one record; the
/// message is cut at [`MAX_MESSAGE_BYTES`] on a UTF-8 boundary with the dropped
/// byte count appended.
pub fn format_line(epoch_ms: u64, level: Level, source: &str, message: &str) -> String {
    let escaped = message.replace(['\r', '\n'], "\\n");
    let body = truncate_message(&escaped);
    format!("{} {} {} {}", stamp(epoch_ms), level.as_str(), source, body)
}

fn truncate_message(message: &str) -> std::borrow::Cow<'_, str> {
    if message.len() <= MAX_MESSAGE_BYTES {
        return std::borrow::Cow::Borrowed(message);
    }
    let mut end = MAX_MESSAGE_BYTES;
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = message.len() - end;
    std::borrow::Cow::Owned(format!("{}…(truncated {} bytes)", &message[..end], dropped))
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` from epoch milliseconds.
fn stamp(epoch_ms: u64) -> String {
    let seconds = epoch_ms / 1000;
    let millis = epoch_ms % 1000;
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year,
        month,
        day,
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60,
        millis
    )
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to Y/M/D.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month as u32, day as u32)
}

/// The composition root's clock, kept here for writers created after
/// `init_svcd` (service and hook pipes). Tests install a fixed clock through
/// [`init_svcd`] too.
static LOG_CLOCK: OnceLock<Clock> = OnceLock::new();
static SVCD_LOG: OnceLock<Mutex<LogWriter>> = OnceLock::new();

/// The clock every log line uses. Before the composition root installs one,
/// this is a live clock; that only happens on early error paths.
pub fn clock() -> Clock {
    LOG_CLOCK.get().copied().unwrap_or_else(Clock::boot)
}

/// Open the daemon's own `svcd.log`. Failure is reported by the caller; the
/// rest of the daemon still works with stderr only.
pub fn init_svcd(
    limits: &crate::platform::limits::Limits,
    dir: &Path,
    clock: Clock,
) -> io::Result<()> {
    let _ = LOG_CLOCK.set(clock);
    let writer = match LogWriter::open(dir, "svcd", "svcd", limits, clock, true) {
        Ok(writer) => writer,
        Err(error) => {
            // A broken log directory must not silence stderr: install a
            // file-disabled writer, then report the open failure once.
            let sink = LogSink::open_with(
                dir,
                "svcd",
                limits.log_max_bytes,
                limits.log_rotations,
                false,
            )?;
            let writer = LogWriter {
                sink,
                source: "svcd".to_string(),
                level: limits.log_level,
                enabled: limits.log_enabled,
                stderr: true,
                clock,
            };
            let _ = SVCD_LOG.set(Mutex::new(writer));
            return Err(error);
        }
    };
    let _ = SVCD_LOG.set(Mutex::new(writer));
    Ok(())
}

/// Record one daemon line. Before `init_svcd` this falls back to stderr only,
/// with the default level; that is an early-error path, not a second writer.
pub fn record(level: Level, message: &str) {
    if let Some(log) = SVCD_LOG.get() {
        if let Ok(mut log) = log.lock() {
            let _ = log.record(level, message);
        }
        return;
    }
    eprintln!(
        "{}",
        format_line(clock().epoch_ms(), level, "svcd", message)
    );
}

/// Truncate the daemon's own `svcd.log` (and its rotations) through the writer
/// that holds the handle. Returns how many files were removed.
pub fn reset_svcd() -> io::Result<usize> {
    match SVCD_LOG.get().and_then(|log| log.lock().ok()) {
        Some(mut writer) => writer.reset(),
        // No writer yet (an early error path) or a poisoned lock: there is
        // nothing this process is writing to.
        None => Ok(0),
    }
}

/// Append one already-formatted line to a plugin log (#262). Used for the
/// "logs cleared" audit line, which has to land in the file the clear just
/// recreated; the running slot keeps its own writer and both append.
pub fn append_plugin_line(
    dir: &Path,
    id: &str,
    limits: &crate::platform::limits::Limits,
    level: Level,
    message: &str,
) -> io::Result<()> {
    let mut sink = LogSink::open(dir, id, limits)?;
    sink.line(&format_line(clock().epoch_ms(), level, "svcd", message))
}

/// Delete `<base>.log` and its rotated siblings where no writer is reachable:
/// a stopped plugin, or the hooks log whose writer lives only for the duration
/// of one hook run.
pub fn remove_log_files(dir: &Path, base: &str, rotations: usize) -> io::Result<usize> {
    let path = dir.join(format!("{}.log", base));
    let mut removed = 0;
    for number in 1..rotations.max(1) {
        removed += remove_if_exists(&rotated(&path, number))?;
    }
    removed += remove_if_exists(&path)?;
    Ok(removed)
}

fn remove_if_exists(path: &Path) -> io::Result<usize> {
    match fs::remove_file(path) {
        Ok(()) => Ok(1),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

/// Return the last `lines` lines of `path`, each terminated by a newline.
///
/// Both `svc logs` and `GET /api/v1/logs/{id}` project the file this way, so a
/// tail is defined once instead of being re-implemented by each caller. The
/// file is read backwards in chunks; a 64 KiB log is never copied wholesale.
pub fn tail(path: &Path, lines: usize) -> io::Result<String> {
    if lines == 0 {
        return Ok(String::new());
    }
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(String::new());
    }
    let mut buffer = Vec::new();
    let mut position = length;
    let mut newlines = 0usize;
    const CHUNK: u64 = 4096;
    while position > 0 && newlines <= lines {
        let start = position.saturating_sub(CHUNK);
        file.seek(SeekFrom::Start(start))?;
        let mut chunk = vec![0u8; (position - start) as usize];
        file.read_exact(&mut chunk)?;
        newlines += chunk.iter().filter(|byte| **byte == b'\n').count();
        chunk.extend_from_slice(&buffer);
        buffer = chunk;
        position = start;
    }
    let text = String::from_utf8_lossy(&buffer);
    let mut selected: Vec<&str> = text.lines().rev().take(lines).collect();
    selected.reverse();
    let mut out = String::new();
    for line in selected {
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::limits;
    use crate::platform::test_support::TempDir;

    fn test_limits(level: Level) -> limits::Limits {
        let mut limits = limits::test_default();
        limits.log_level = level;
        limits.log_max_bytes = DEFAULT_LIMIT;
        limits.log_rotations = 3;
        limits.log_to_file = true;
        limits.log_enabled = true;
        limits
    }

    fn writer(dir: &TempDir, level: Level, source: &str) -> LogWriter {
        LogWriter::open(
            &dir.0,
            "demo",
            source,
            &test_limits(level),
            Clock::fixed(1_700_000_000_123),
            false,
        )
        .expect("open")
    }

    #[test]
    fn writes_a_line_with_a_newline() {
        let dir = TempDir::new();
        let mut sink = LogSink::open_with(&dir.0, "demo", DEFAULT_LIMIT, 3, true).expect("open");
        sink.line("hello").expect("write");
        sink.line("world").expect("write");

        let text = fs::read_to_string(dir.0.join("demo.log")).expect("read");
        assert_eq!(text, "hello\nworld\n");
    }

    #[test]
    fn formats_the_stamp_level_source_and_message() {
        let line = format_line(1_700_000_000_123, Level::Warn, "plugin:demo", "hello");
        assert_eq!(line, "2023-11-14T22:13:20.123Z WARN  plugin:demo hello");
        assert!(format_line(0, Level::Info, "svcd", "x").starts_with("1970-01-01T00:00:00.000Z"));
        assert_eq!(Level::Trace.as_str(), "TRACE");
        for level in Level::ALL {
            assert_eq!(level.as_str().len(), 5);
        }
    }

    #[test]
    fn cr_and_lf_become_one_literal_backslash_n() {
        let line = format_line(0, Level::Info, "svcd", "a\nb\r\nc");
        assert_eq!(line.lines().count(), 1);
        assert!(line.ends_with("a\\nb\\n\\nc"), "{line:?}");
    }

    #[test]
    fn parses_level_names_case_insensitively() {
        assert_eq!(Level::parse("wArN"), Some(Level::Warn));
        assert_eq!(Level::parse("TRACE"), Some(Level::Trace));
        assert_eq!(Level::parse("verbose"), None);
    }

    #[test]
    fn threshold_drops_lower_severities_at_the_write_side() {
        let dir = TempDir::new();
        let mut log = writer(&dir, Level::Warn, "svcd");
        log.record(Level::Info, "quiet").expect("record");
        log.record(Level::Warn, "kept").expect("record");
        log.record(Level::Error, "also kept").expect("record");
        let text = fs::read_to_string(dir.0.join("demo.log")).expect("read");
        assert!(!text.contains("quiet"));
        assert_eq!(text.lines().count(), 2);
        assert!(text
            .lines()
            .next()
            .expect("line")
            .contains(" WARN  svcd kept"));
    }

    #[test]
    fn a_long_message_is_cut_on_a_utf8_boundary_and_marked() {
        let dir = TempDir::new();
        let mut log = writer(&dir, Level::Trace, "plugin:demo");
        log.record(Level::Info, &"日".repeat(3000)).expect("record");
        let text = fs::read_to_string(dir.0.join("demo.log")).expect("read");
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("(truncated 810 bytes)"), "{text:?}");
        assert!(text.ends_with("bytes)\n"));
    }

    #[test]
    fn the_read_side_tolerates_a_multibyte_legacy_line() {
        // Byte 24 falls inside the last 3-byte character; slicing before a
        // boundary check used to panic here.
        let legacy = format!("a{}", "日".repeat(8));
        assert!(legacy.len() > STAMP_LEN);
        assert_eq!(parse_level(&legacy), None);
        assert_eq!(timestamp(&legacy), None);
        assert!(keep(&legacy, Level::Error));
    }

    #[test]
    fn the_read_side_keeps_old_lines_and_filters_by_level() {
        assert!(keep("old format line", Level::Error));
        assert!(keep(
            "2023-11-14T22:13:22.000Z ERROR svcd boom",
            Level::Error
        ));
        assert!(!keep(
            "2023-11-14T22:13:20.000Z INFO  svcd quiet",
            Level::Error
        ));
        assert!(keep(
            "2023-11-14T22:13:20.000Z INFO  svcd quiet",
            Level::Info
        ));
    }

    #[test]
    fn the_message_budget_is_inclusive() {
        let exact = "x".repeat(MAX_MESSAGE_BYTES);
        let line = format_line(0, Level::Info, "svcd", &exact);
        assert!(!line.contains("truncated"));

        let over = format!("{}y", exact);
        let line = format_line(0, Level::Info, "svcd", &over);
        assert!(line.contains("(truncated 1 bytes)"), "{line:?}");
    }

    #[test]
    fn a_disabled_master_switch_writes_no_file_even_above_threshold() {
        let dir = TempDir::new();
        let mut limits = test_limits(Level::Trace);
        limits.log_enabled = false;
        let mut log =
            LogWriter::open(&dir.0, "demo", "svcd", &limits, Clock::fixed(1), true).expect("open");
        log.record(Level::Error, "nothing").expect("record");
        assert!(!dir.0.join("demo.log").exists());
    }

    #[test]
    fn log_to_file_off_still_formats_but_creates_no_file() {
        let dir = TempDir::new();
        let mut limits = test_limits(Level::Info);
        limits.log_to_file = false;
        let mut log =
            LogWriter::open(&dir.0, "demo", "svcd", &limits, Clock::fixed(1), false).expect("open");
        log.record(Level::Info, "stderr only").expect("record");
        assert!(!dir.0.join("demo.log").exists());
    }

    #[test]
    fn tail_returns_the_requested_lines_with_newlines() {
        let dir = TempDir::new();
        let path = dir.0.join("demo.log");
        fs::write(&path, "one\ntwo\nthree\n").expect("write log");

        assert_eq!(tail(&path, 2).expect("tail"), "two\nthree\n");
        assert_eq!(tail(&path, 0).expect("tail"), "");
        assert!(tail(&dir.0.join("missing.log"), 2).is_err());
    }

    #[test]
    fn rotates_at_the_configured_limit_and_keeps_the_configured_number() {
        let dir = TempDir::new();
        let limit = 1024;
        let mut sink = LogSink::open_with(&dir.0, "demo", limit, 3, true).expect("open");
        let chunk = "x".repeat(600);

        for _ in 0..4 {
            sink.line(&chunk).expect("write");
        }

        assert!(dir.0.join("demo.log").is_file());
        assert!(dir.0.join("demo.log.1").is_file());
        assert!(dir.0.join("demo.log.2").is_file());
        assert!(!dir.0.join("demo.log.3").is_file());
        let current = fs::metadata(dir.0.join("demo.log")).expect("meta").len();
        assert!(current > 0);
        assert!(current <= limit);
    }

    #[test]
    fn reset_drops_rotations_and_reopens_the_same_path() {
        let dir = TempDir::new();
        let mut sink = LogSink::open_with(&dir.0, "demo", 8, 3, true).expect("open");
        sink.line("one").expect("line");
        sink.line("two").expect("line");
        sink.line("three").expect("line");
        assert!(
            dir.0.join("demo.log.1").exists(),
            "the limit forces a rotation"
        );

        let removed = sink.reset().expect("reset");

        assert_eq!(removed, 2, "the current file and its one rotation");
        assert!(!dir.0.join("demo.log.1").exists());
        assert!(dir.0.join("demo.log").is_file(), "reset reopens the path");
        assert_eq!(fs::read_to_string(dir.0.join("demo.log")).unwrap(), "");
        sink.line("after").expect("line");
        assert_eq!(
            fs::read_to_string(dir.0.join("demo.log")).unwrap(),
            "after\n"
        );
    }

    #[test]
    fn disabled_sink_writes_nothing() {
        let dir = TempDir::new();
        let mut sink = LogSink::open_with(&dir.0, "demo", DEFAULT_LIMIT, 3, false).expect("open");
        sink.line("hello").expect("write");
        assert!(!dir.0.join("demo.log").exists());
    }

    #[cfg(unix)]
    #[test]
    fn log_is_group_readable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new();
        let mut sink = LogSink::open_with(&dir.0, "demo", DEFAULT_LIMIT, 3, true).expect("open");
        sink.line("hello").expect("write");

        let mode = fs::metadata(dir.0.join("demo.log"))
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o640);
    }
}
