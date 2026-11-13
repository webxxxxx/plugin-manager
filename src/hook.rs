//! Install hooks declared by an `.spk` manifest: `postInstall` and `preRemove`.
//!
//! A hook is a package-signed executable under `hooks/`, run once at install or
//! remove time with the installer's privileges. The environment is cleared down
//! to `PATH` plus the `SVCD_*` contract, the working directory is the installed
//! version directory, output goes to the plugin log, and a hard timeout kills
//! the hook so a bad package cannot wedge the installer.

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::log::{self, LogSink};
use crate::package;
use crate::paths;
use crate::threads;

/// Both hook actions share the same 60-second budget.
pub const TIMEOUT_MS: u64 = 60_000;

pub struct Invocation<'a> {
    pub id: &'a str,
    pub version: &'a str,
    /// `post-install` or `pre-remove`; also exported as `SVCD_ACTION`.
    pub action: &'a str,
    pub version_dir: &'a Path,
    /// Package-relative hook path taken from the manifest.
    pub path: &'a str,
    pub timeout_ms: u64,
}

pub fn run(spec: &Invocation<'_>) -> Result<(), String> {
    package::check_path(spec.path)?;
    let program = spec.version_dir.join(spec.path);
    let metadata = fs::metadata(&program)
        .map_err(|error| format!("{} hook {}: {}", spec.action, program.display(), error))?;
    if !metadata.is_file() {
        return Err(format!(
            "{} hook is not a regular file: {}",
            spec.action,
            program.display()
        ));
    }
    check_executable(spec.action, &program)?;

    let mut command = hook_command(&program);
    command
        .current_dir(spec.version_dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("SVCD_PLUGIN_ID", spec.id)
        .env("SVCD_PLUGIN_VERSION", spec.version)
        .env("SVCD_ACTION", spec.action)
        .env("SVCD_DATA_DIR", paths::data_dir())
        .env("SVCD_CONFIG_DIR", paths::config_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        // cmd.exe needs these to start; neither reveals user-specific state.
        for key in ["SystemRoot", "WINDIR"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    // Per the log contract, hook output goes to `<logs>/<id>-hooks.log`, not the
    // service log; `svc logs` keeps showing only the supervised process.
    let log = match LogSink::open(&paths::logs_dir(), &format!("{}-hooks", spec.id)) {
        Ok(sink) => Some(Arc::new(Mutex::new(sink))),
        Err(error) => {
            log::svcd(&format!(
                "svcd: plugin {}: cannot open {} hook log: {}",
                spec.id, spec.action, error
            ));
            None
        }
    };
    #[cfg(windows)]
    let job = crate::proc::Job::new();
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn {} hook: {}", spec.action, error))?;
    let _ = crate::proc::assign(&child);
    #[cfg(windows)]
    if let Some(job) = &job {
        job.assign(&child);
    }
    spawn_reader(
        format!("svcd-{}-hook-out", spec.id),
        child.stdout.take(),
        log.clone(),
    );
    spawn_reader(
        format!("svcd-{}-hook-err", spec.id),
        child.stderr.take(),
        log,
    );

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    log::svcd(&format!(
                        "svcd: plugin {}: {} hook ok",
                        spec.id, spec.action
                    ));
                    return Ok(());
                }
                return Err(format!("{} hook exited {}", spec.action, status));
            }
            Ok(None) => {}
            Err(error) => return Err(format!("{} hook: {}", spec.action, error)),
        }
        if started.elapsed() >= Duration::from_millis(spec.timeout_ms) {
            #[cfg(windows)]
            if let Some(job) = &job {
                job.terminate();
            }
            kill(&mut child);
            let _ = child.wait();
            return Err(format!(
                "{} hook timed out after {}s",
                spec.action,
                spec.timeout_ms / 1000
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn spawn_reader<R>(name: String, stream: Option<R>, log: Option<Arc<Mutex<LogSink>>>)
where
    R: Read + Send + 'static,
{
    let Some(stream) = stream else { return };
    let _ = thread::Builder::new()
        .name(name)
        .stack_size(threads::IO_STACK)
        .spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                if let Some(log) = &log {
                    if let Ok(mut sink) = log.lock() {
                        let _ = sink.line(&line);
                    }
                }
            }
        });
}

fn kill(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        // Every hook gets its own process group, so the whole tree goes down.
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = child.kill();
    }
}

fn check_executable(action: &str, program: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = fs::metadata(program)
            .map_err(|error| format!("{} hook: {}", action, error))?
            .permissions()
            .mode();
        if mode & 0o111 == 0 {
            return Err(format!(
                "{} hook is not executable: {}",
                action,
                program.display()
            ));
        }
        let mut shebang = [0u8; 2];
        let mut file =
            fs::File::open(program).map_err(|error| format!("{} hook: {}", action, error))?;
        file.read_exact(&mut shebang).map_err(|_| {
            format!(
                "{} hook must start with a shebang: {}",
                action,
                program.display()
            )
        })?;
        if &shebang != b"#!" {
            return Err(format!(
                "{} hook must start with a shebang: {}",
                action,
                program.display()
            ));
        }
    }
    #[cfg(windows)]
    {
        let extension = program
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase());
        if !matches!(extension.as_deref(), Some("exe" | "cmd" | "bat")) {
            return Err(format!(
                "{} hook must end with .exe, .cmd or .bat on Windows: {}",
                action,
                program.display()
            ));
        }
    }
    Ok(())
}

fn hook_command(program: &Path) -> Command {
    #[cfg(windows)]
    {
        let extension = program
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if extension == "cmd" || extension == "bat" {
            let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
            let mut command = Command::new(shell);
            command.arg("/C").arg(program);
            crate::proc::configure(&mut command);
            return command;
        }
    }
    let mut command = Command::new(program);
    crate::proc::configure(&mut command);
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
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
                "svcd-hook-{}-{}-{}",
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

    #[cfg(windows)]
    fn write_hook(dir: &Path, name: &str, body: &str) -> String {
        let path = dir.join(format!("{}.cmd", name));
        fs::write(&path, body.replace('\n', "\r\n")).expect("write hook");
        format!("{}.cmd", name)
    }

    #[cfg(unix)]
    fn write_hook(dir: &Path, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join(name);
        fs::write(&path, body).expect("write hook");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod hook");
        name.to_string()
    }

    fn spec<'a>(dir: &'a TempDir, path: &'a str, timeout_ms: u64) -> Invocation<'a> {
        Invocation {
            id: "demo",
            version: "1.2.3",
            action: "post-install",
            version_dir: &dir.0,
            path,
            timeout_ms,
        }
    }

    #[test]
    fn the_hook_runs_with_the_documented_environment_and_working_directory() {
        let dir = TempDir::new();
        #[cfg(windows)]
        let body = "@echo off\necho %SVCD_PLUGIN_ID% %SVCD_PLUGIN_VERSION% %SVCD_ACTION%>marker.txt\necho %CD%>cwd.txt\n";
        #[cfg(unix)]
        let body = "#!/bin/sh\nprintf '%s %s %s\\n' \"$SVCD_PLUGIN_ID\" \"$SVCD_PLUGIN_VERSION\" \"$SVCD_ACTION\" > marker.txt\npwd > cwd.txt\n";
        let path = write_hook(&dir.0, "env", body);

        run(&spec(&dir, &path, TIMEOUT_MS)).expect("hook runs");

        let marker = fs::read_to_string(dir.0.join("marker.txt")).expect("marker");
        assert_eq!(marker.trim(), "demo 1.2.3 post-install");
        let cwd = fs::read_to_string(dir.0.join("cwd.txt")).expect("cwd");
        let name = dir.0.file_name().expect("temp name").to_string_lossy();
        assert!(cwd.contains(name.as_ref()), "cwd was {cwd:?}");
    }

    #[test]
    fn a_failing_hook_reports_its_exit() {
        let dir = TempDir::new();
        #[cfg(windows)]
        let path = write_hook(&dir.0, "fail", "@echo off\nexit /b 3\n");
        #[cfg(unix)]
        let path = write_hook(&dir.0, "fail", "#!/bin/sh\nexit 3\n");

        let error = run(&spec(&dir, &path, TIMEOUT_MS)).expect_err("hook fails");

        assert!(error.contains("exited"), "{error}");
    }

    #[test]
    fn a_hook_that_outlives_its_timeout_is_killed() {
        let dir = TempDir::new();
        #[cfg(windows)]
        let path = write_hook(&dir.0, "slow", "@echo off\nping -n 6 127.0.0.1 >nul\n");
        #[cfg(unix)]
        let path = write_hook(&dir.0, "slow", "#!/bin/sh\nsleep 5\n");

        let started = Instant::now();
        let error = run(&spec(&dir, &path, 150)).expect_err("hook times out");

        assert!(error.contains("timed out"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "timeout did not kill the hook: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn rejects_a_path_that_escapes_the_version_directory() {
        let dir = TempDir::new();

        let error = run(&spec(&dir, "../outside", TIMEOUT_MS)).expect_err("escape");

        assert!(error.contains("unsafe path"), "{error}");
    }

    #[test]
    fn rejects_a_missing_hook_file() {
        let dir = TempDir::new();

        let error = run(&spec(&dir, "hooks/missing", TIMEOUT_MS)).expect_err("missing");

        assert!(error.contains("post-install hook"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_hook_without_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new();
        let path = dir.0.join("not-executable");
        fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write hook");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");

        let error = run(&spec(&dir, "not-executable", TIMEOUT_MS)).expect_err("mode");

        assert!(error.contains("not executable"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_hook_without_a_shebang() {
        let dir = TempDir::new();
        let path = dir.0.join("no-shebang");
        fs::write(&path, "exit 0\n").expect("write hook");
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");

        let error = run(&spec(&dir, "no-shebang", TIMEOUT_MS)).expect_err("shebang");

        assert!(error.contains("shebang"), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn rejects_a_hook_that_is_not_a_windows_executable() {
        let dir = TempDir::new();
        fs::write(dir.0.join("hook.sh"), "@echo off\r\n").expect("write hook");

        let error = run(&spec(&dir, "hook.sh", TIMEOUT_MS)).expect_err("extension");

        assert!(error.contains(".exe, .cmd or .bat"), "{error}");
    }
}
