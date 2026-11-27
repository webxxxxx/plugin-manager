use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use crate::kernel::projection::Status;
use crate::kernel::registry::Hook;
use crate::platform::paths::Paths;
use crate::supervise::ops::hook_proc::{self, Capture, Unfinished};
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
        let plugin_dir = self
            .registry
            .get(id)
            .and_then(|service| service.install_dir.clone());
        let mut poll = self.hook_pump();
        status.details.probe = Some(run_status_hook(
            id,
            &hook,
            &self.config.paths,
            plugin_dir.as_deref().map(Path::new),
            &mut poll,
        ));
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
fn run_status_hook(
    id: &str,
    hook: &Hook,
    paths: &Paths,
    plugin_dir: Option<&Path>,
    poll: &mut dyn FnMut(),
) -> String {
    let output = match Capture::new(id, "status", paths) {
        Ok(capture) => capture,
        Err(error) => return format!("cannot create status output: {}", error),
    };
    let file = match output.open() {
        Ok(file) => file,
        Err(error) => return format!("cannot create status output: {}", error),
    };
    let mut command = hook_command(id, "status", hook, paths, plugin_dir);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return format!("spawn failed: {}", error),
    };
    let _ = crate::platform::host::process::adopt(&child);
    let outcome = hook_proc::wait(
        &mut child,
        Duration::from_millis(hook.timeout_ms),
        None,
        poll,
        Duration::from_millis(50),
    );
    let (text, _) = output.read(8 * 1024);
    match outcome {
        Ok(status) => status_detail(&text, Some(status)),
        // The probe never passes a cancel token, so a cancelled wait cannot
        // happen here; report it like a stop, not like a health verdict.
        Err(Unfinished::TimedOut) | Err(Unfinished::Cancelled) => timed_out(&text),
        Err(Unfinished::Failed(error)) => {
            format!("{} (error: {})", status_detail(&text, None), error)
        }
    }
}

fn timed_out(text: &str) -> String {
    let detail = status_detail(text, None);
    if detail.is_empty() {
        "timed out".into()
    } else {
        format!("{} (timed out)", detail)
    }
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
