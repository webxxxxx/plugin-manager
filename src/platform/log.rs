use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::OnceLock;

/// Rotation threshold used when a caller does not have a configured limit
/// (unit tests and the generated sample defaults).
#[cfg(test)]
pub const DEFAULT_LIMIT: u64 = 64 * 1024;

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
            limits.log_to_file,
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

static SVCD_LOG: OnceLock<Mutex<LogSink>> = OnceLock::new();

/// Open the daemon's own `svcd.log`. Failure is reported by the caller; the
/// rest of the daemon still works with stderr only.
pub fn init_svcd(limits: &crate::platform::limits::Limits, dir: &Path) -> io::Result<()> {
    let sink = LogSink::open(dir, "svcd", limits)?;
    let _ = SVCD_LOG.set(Mutex::new(sink));
    Ok(())
}

/// Write a daemon log line to stderr and, when available, `svcd.log`.
pub fn svcd(text: &str) {
    eprintln!("{}", text);
    if let Some(sink) = SVCD_LOG.get() {
        if let Ok(mut sink) = sink.lock() {
            let _ = sink.line(text);
        }
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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = crate::test_support::temp_dir().join(format!(
                "svcd-log-{}-{}-{}",
                std::process::id(),
                nanos,
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("temp dir");
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
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
