use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Rotation limit: 1 MiB per file, `.log` + `.log.1` + `.log.2`.
pub const LIMIT: u64 = 1024 * 1024;

pub struct LogSink {
    path: PathBuf,
    file: Option<File>,
    size: u64,
}

impl LogSink {
    pub fn open(dir: &Path, id: &str) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.log", id));
        let file = open_append(&path)?;
        let size = file.metadata()?.len();
        Ok(LogSink {
            path,
            file: Some(file),
            size,
        })
    }

    pub fn line(&mut self, line: &str) -> io::Result<()> {
        let mut bytes = line.as_bytes().to_vec();
        if !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        if self.size.saturating_add(bytes.len() as u64) > LIMIT {
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
        let one = newest.with_extension("log.1");
        let two = newest.with_extension("log.2");
        match fs::remove_file(&two) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if one.exists() {
            fs::rename(&one, &two)?;
        }
        fs::rename(&newest, &one)?;
        self.file = Some(open_append(&newest)?);
        self.size = 0;
        Ok(())
    }
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
    let group = unsafe { libc::getgrnam(name.as_ptr()) };
    if group.is_null() {
        return;
    }
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    unsafe {
        libc::chown(path.as_ptr(), 0, (*group).gr_gid);
    }
}

static SVCD_LOG: OnceLock<Mutex<LogSink>> = OnceLock::new();

/// Open the daemon's own `svcd.log`. Failure is reported by the caller; the
/// rest of the daemon still works with stderr only.
pub fn init_svcd(dir: &Path) -> io::Result<()> {
    let sink = LogSink::open(dir, "svcd")?;
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
            let path = std::env::temp_dir().join(format!(
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
        let mut sink = LogSink::open(&dir.0, "demo").expect("open");
        sink.line("hello").expect("write");
        sink.line("world").expect("write");

        let text = fs::read_to_string(dir.0.join("demo.log")).expect("read");
        assert_eq!(text, "hello\nworld\n");
    }

    #[test]
    fn rotates_at_one_mib_and_keeps_two_old_files() {
        let dir = TempDir::new();
        let mut sink = LogSink::open(&dir.0, "demo").expect("open");
        let chunk = "x".repeat(600 * 1024);

        for _ in 0..4 {
            sink.line(&chunk).expect("write");
        }

        assert!(dir.0.join("demo.log").is_file());
        assert!(dir.0.join("demo.log.1").is_file());
        assert!(dir.0.join("demo.log.2").is_file());
        assert!(!dir.0.join("demo.log.3").is_file());
        let current = fs::metadata(dir.0.join("demo.log")).expect("meta").len();
        assert!(current > 0);
        assert!(current <= LIMIT);
    }

    #[cfg(unix)]
    #[test]
    fn log_is_group_readable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new();
        let mut sink = LogSink::open(&dir.0, "demo").expect("open");
        sink.line("hello").expect("write");

        let mode = fs::metadata(dir.0.join("demo.log"))
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o640);
    }
}
