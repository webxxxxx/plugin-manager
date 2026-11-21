use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread;
use std::time::SystemTime;
use std::time::{Duration, Instant};

use crate::kernel::projection::Status;
use crate::kernel::registry::Hook;
use crate::platform::paths::Paths;
use crate::supervise::ops::host::Supervisor;
use crate::supervise::ops::slot::hook_command;

impl Supervisor {
    /// Run the optional status hook. This is only called when the caller asks
    /// for a probe (`svc status`), not by `svc list` or the HTTP projections.
    pub(crate) fn probe_status(&mut self, id: &str, status: &mut Status) {
        let Some(hook) = self
            .registry
            .get(id)
            .and_then(|service| service.lifecycle.as_ref())
            .and_then(|lifecycle| lifecycle.status.as_ref())
            .cloned()
        else {
            return;
        };
        let mut poll = self.hook_pump();
        status.details.probe = Some(run_status_hook(id, &hook, &self.config.paths, &mut poll));
    }

    /// A one-pass poll for a synchronous hook wait. The closure owns the
    /// reactor handle, so running the hook with a mutable borrow of self
    /// cannot re-borrow the poll set through a hidden thread-local.
    #[cfg(unix)]
    pub(crate) fn hook_pump(&self) -> impl FnMut() + 'static {
        let reactor = self.reactor.clone();
        move || {
            if let Some(reactor) = &reactor {
                reactor.poll_once(Duration::ZERO);
            }
        }
    }

    #[cfg(not(unix))]
    pub(crate) fn hook_pump(&self) -> impl FnMut() + 'static {
        || {}
    }
}

/// The status action is a detail supplement, not a health gate: its stdout
/// becomes the `probe` text (capped), and a non-zero exit only appends the
/// status word. Capture goes through a temp file so a hook that leaves a child
/// holding stdout cannot block the daemon's event loop.
fn run_status_hook(id: &str, hook: &Hook, paths: &Paths, poll: &mut dyn FnMut()) -> String {
    let output_path = status_output_path(id, paths);
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = match options.open(&output_path) {
        Ok(file) => file,
        Err(error) => return format!("cannot create status output: {}", error),
    };
    let mut command = hook_command(id, "status", hook, paths);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = fs::remove_file(&output_path);
            return format!("spawn failed: {}", error);
        }
    };
    let _ = crate::platform::host::process::adopt(&child);
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = read_status_output(&output_path);
                let _ = fs::remove_file(&output_path);
                return status_detail(&output, Some(status));
            }
            Ok(None) => {}
            Err(error) => {
                let output = read_status_output(&output_path);
                let _ = fs::remove_file(&output_path);
                let detail = status_detail(&output, None);
                return format!("{} (error: {})", detail, error);
            }
        }
        if started.elapsed() >= Duration::from_millis(hook.timeout_ms) {
            // The probe runs in its own process group: stop the whole tree, not
            // just the leader, or a status hook's children outlive the timeout.
            crate::supervise::ops::hook::kill(&mut child);
            let _ = child.wait();
            let output = read_status_output(&output_path);
            let _ = fs::remove_file(&output_path);
            let detail = status_detail(&output, None);
            return if detail.is_empty() {
                "timed out".into()
            } else {
                format!("{} (timed out)", detail)
            };
        }
        // The supervisor loop owns plugin stdio; a probe must not freeze it.
        poll();
        thread::sleep(Duration::from_millis(50));
    }
}

fn status_output_path(id: &str, paths: &Paths) -> PathBuf {
    let directory = paths.temp_dir.clone();
    let _ = fs::create_dir_all(&directory);
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    directory.join(format!(
        "status-{}-{}-{}.out",
        id,
        std::process::id(),
        nanos
    ))
}

fn read_status_output(path: &Path) -> String {
    use std::io::Read;

    let Ok(file) = fs::File::open(path) else {
        return String::new();
    };
    let mut bytes = Vec::new();
    let _ = file.take(8 * 1024).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).to_string()
}

fn status_detail(output: &str, status: Option<std::process::ExitStatus>) -> String {
    let text = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    match (text.is_empty(), status) {
        (true, Some(status)) if status.success() => "ok".into(),
        (true, Some(status)) => format!("failed ({})", status),
        (true, None) => String::new(),
        (false, Some(status)) if status.success() => text,
        (false, Some(status)) => format!("{} ({})", text, status),
        (false, None) => text,
    }
}
