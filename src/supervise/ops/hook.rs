//! Install hooks declared by an `.spk` manifest: `postInstall` and `preRemove`.
//!
//! A hook is a package-signed executable under `hooks/`, run once at install or
//! remove time with the installer's privileges. The environment is cleared down
//! to `PATH` plus the `SVCD_*` contract, the working directory is the installed
//! version directory, output goes to the plugin log, and a hard timeout kills
//! the hook so a bad package cannot wedge the installer.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::kernel::failure::{Problem, ProblemError};
use crate::platform::log::{self, LogWriter};
use crate::platform::paths::Paths;

/// The process side of [`crate::kernel::hook::Hooks`]: the composition root
/// supplies the paths, limits and reactor pump once per install/remove.
pub struct Runner<'a> {
    paths: &'a Paths,
    limits: &'a crate::platform::limits::Limits,
    poll: &'a mut dyn FnMut(),
}

impl<'a> Runner<'a> {
    pub fn new(
        paths: &'a Paths,
        limits: &'a crate::platform::limits::Limits,
        poll: &'a mut dyn FnMut(),
    ) -> Self {
        Runner {
            paths,
            limits,
            poll,
        }
    }
}

impl crate::kernel::hook::Hooks for Runner<'_> {
    fn run(&mut self, spec: &crate::kernel::hook::HookSpec<'_>) -> Result<(), ProblemError> {
        run(spec, self.paths, self.limits, self.poll)
    }
}

/// Run the hook, calling `poll` before every wait step. The supervisor loop
/// owns plugin stdio, so a hook it waits for must not stop the plugin pipes;
/// the composition root passes a closure over its reactor, and a caller with
/// no reactor passes a no-op.
pub fn run(
    spec: &crate::kernel::hook::HookSpec<'_>,
    paths: &Paths,
    limits: &crate::platform::limits::Limits,
    poll: &mut dyn FnMut(),
) -> Result<(), ProblemError> {
    crate::kernel::layout::check_path(spec.path)?;
    let program = spec.version_dir.join(spec.path);
    let metadata = fs::metadata(&program).map_err(|error| {
        Problem::Invalid.error(format!(
            "{} hook {}: {}",
            spec.action,
            program.display(),
            error
        ))
    })?;
    if !metadata.is_file() {
        return Err(Problem::Invalid.error(format!(
            "{} hook is not a regular file: {}",
            spec.action,
            program.display()
        )));
    }
    check_executable(spec.action, &program)?;

    let mut command = hook_command(&program);
    command
        .current_dir(spec.version_dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("SVCD_PLUGIN_ID", spec.id)
        .env("SVCD_PLUGIN_VERSION", spec.version)
        // The hook runs in the version directory; the plugin directory (which
        // holds `data/` and `config`) is its parent, the same rule `config`
        // follows (#307, #312).
        .env(
            "SVCD_PLUGIN_DIR",
            spec.version_dir.parent().unwrap_or(spec.version_dir),
        )
        .env("SVCD_ACTION", spec.action)
        .env("SVCD_DATA_DIR", &paths.data_dir)
        .env("SVCD_CONFIG_DIR", &paths.config_dir);
    #[cfg(windows)]
    {
        // cmd.exe needs these to start; neither reveals user-specific state.
        for key in ["SystemRoot", "WINDIR"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
    }

    // Per the log contract, hook output goes to `<logs>/<id>-hooks.log`, not the
    // service log; `svc logs` keeps showing only the supervised process.
    let log = match LogWriter::open(
        &paths.logs_dir,
        &format!("{}-hooks", spec.id),
        &format!("hook:{}", spec.id),
        limits,
        log::clock(),
        false,
    ) {
        Ok(sink) => Some(Arc::new(Mutex::new(sink))),
        Err(error) => {
            error!(
                "plugin {}: cannot open {} hook log: {}",
                spec.id, spec.action, error
            );
            None
        }
    };
    let tree = crate::platform::host::process::Tree::new();
    let (mut child, streams) =
        crate::wire::contract::stdio::spawn(command, crate::wire::contract::stdio::Plan::hook())
            .map_err(|error| {
                Problem::Internal.error(format!("spawn {} hook: {}", spec.action, error))
            })?;
    let _ = crate::platform::host::process::adopt(&child);
    if let Some(tree) = &tree {
        tree.assign(&child);
    }
    if let Some(stdout) = streams.stdout {
        if let Err(error) = crate::wire::contract::stdio::log_lines(
            format!("svcd-{}-hook-out", spec.id),
            stdout,
            log.clone(),
            limits,
        ) {
            error!(
                "plugin {}: {} hook stdout reader: {}",
                spec.id, spec.action, error
            );
        }
    }
    if let Some(stderr) = streams.stderr {
        if let Err(error) = crate::wire::contract::stdio::log_lines(
            format!("svcd-{}-hook-err", spec.id),
            stderr,
            log,
            limits,
        ) {
            error!(
                "plugin {}: {} hook stderr reader: {}",
                spec.id, spec.action, error
            );
        }
    }

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    info!("plugin {}: {} hook ok", spec.id, spec.action);
                    return Ok(());
                }
                return Err(
                    Problem::Internal.error(format!("{} hook exited {}", spec.action, status))
                );
            }
            Ok(None) => {}
            Err(error) => {
                return Err(Problem::Internal.error(format!("{} hook: {}", spec.action, error)))
            }
        }
        poll();
        if started.elapsed() >= Duration::from_millis(spec.timeout_ms) {
            if let Some(tree) = &tree {
                tree.terminate();
            }
            crate::platform::host::process::kill_now(&mut child);
            let _ = child.wait();
            return Err(Problem::Timeout.error(format!(
                "{} hook timed out after {}s",
                spec.action,
                spec.timeout_ms / 1000
            )));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn check_executable(action: &str, program: &Path) -> Result<(), ProblemError> {
    #[cfg(unix)]
    {
        use std::io::Read;
        use std::os::unix::fs::PermissionsExt;

        let mode = fs::metadata(program)
            .map_err(|error| Problem::Internal.error(format!("{} hook: {}", action, error)))?
            .permissions()
            .mode();
        if mode & 0o111 == 0 {
            return Err(Problem::Invalid.error(format!(
                "{} hook is not executable: {}",
                action,
                program.display()
            )));
        }
        let mut shebang = [0u8; 2];
        let mut file = fs::File::open(program)
            .map_err(|error| Problem::Internal.error(format!("{} hook: {}", action, error)))?;
        file.read_exact(&mut shebang).map_err(|_| {
            Problem::Invalid.error(format!(
                "{} hook must start with a shebang: {}",
                action,
                program.display()
            ))
        })?;
        if &shebang != b"#!" {
            return Err(Problem::Invalid.error(format!(
                "{} hook must start with a shebang: {}",
                action,
                program.display()
            )));
        }
    }
    #[cfg(windows)]
    {
        let extension = program
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase());
        if !matches!(extension.as_deref(), Some("exe" | "cmd" | "bat")) {
            return Err(Problem::Invalid.error(format!(
                "{} hook must end with .exe, .cmd or .bat on Windows: {}",
                action,
                program.display()
            )));
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
            crate::platform::host::process::configure(
                &mut command,
                crate::platform::host::process::Kind::Hook,
            );
            return command;
        }
    }
    let mut command = Command::new(program);
    crate::platform::host::process::configure(
        &mut command,
        crate::platform::host::process::Kind::Hook,
    );
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every hook test owns a temporary directory set; nothing reads the
    /// process environment.
    fn test_paths() -> Paths {
        crate::platform::test_support::paths("hook-unit")
    }

    fn run(
        spec: &crate::kernel::hook::HookSpec<'_>,
        poll: &mut dyn FnMut(),
    ) -> Result<(), ProblemError> {
        super::run(
            spec,
            &test_paths(),
            &crate::platform::limits::test_default(),
            poll,
        )
    }
    use crate::platform::test_support::TempDir;
    use std::fs;

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

    fn spec<'a>(
        dir: &'a TempDir,
        path: &'a str,
        timeout_ms: u64,
    ) -> crate::kernel::hook::HookSpec<'a> {
        crate::kernel::hook::HookSpec {
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

        run(
            &spec(&dir, &path, crate::kernel::hook::TIMEOUT_MS),
            &mut || {},
        )
        .expect("hook runs");

        let marker = fs::read_to_string(dir.0.join("marker.txt")).expect("marker");
        assert_eq!(marker.trim(), "demo 1.2.3 post-install");
        let cwd = fs::read_to_string(dir.0.join("cwd.txt")).expect("cwd");
        let name = dir.0.file_name().expect("temp name").to_string_lossy();
        assert!(cwd.contains(name.as_ref()), "cwd was {cwd:?}");
    }

    #[test]
    fn a_hook_wait_pumps_the_injected_poll() {
        // The hook waits for a marker only the poll closure writes, so it
        // cannot finish before `poll` has run at least once. Neither the
        // hook's body nor the test depends on how long an external program
        // takes to start: the budget below only fires when the pump is
        // genuinely missing, which is the failure this test is about.
        let dir = TempDir::new();
        #[cfg(windows)]
        let path = write_hook(
            &dir.0,
            "pump",
            "@echo off\n:wait\nif exist go goto done\ngoto wait\n:done\n",
        );
        #[cfg(unix)]
        let path = write_hook(&dir.0, "pump", "#!/bin/sh\nwhile [ ! -f go ]; do :; done\n");
        let marker = dir.0.join("go");

        let mut polls = 0u32;
        run(&spec(&dir, &path, 5_000), &mut || {
            polls += 1;
            let _ = fs::write(&marker, b"");
        })
        .expect("hook runs");
        assert!(
            polls >= 1,
            "the synchronous wait must call the poll closure it was given"
        );
    }

    #[test]
    fn a_failing_hook_reports_its_exit() {
        let dir = TempDir::new();
        #[cfg(windows)]
        let path = write_hook(&dir.0, "fail", "@echo off\nexit /b 3\n");
        #[cfg(unix)]
        let path = write_hook(&dir.0, "fail", "#!/bin/sh\nexit 3\n");

        let error = run(
            &spec(&dir, &path, crate::kernel::hook::TIMEOUT_MS),
            &mut || {},
        )
        .expect_err("hook fails");

        assert!(error.detail().contains("exited"), "{error}");
    }

    #[test]
    fn a_hook_that_outlives_its_timeout_is_killed() {
        let dir = TempDir::new();
        #[cfg(windows)]
        let path = write_hook(&dir.0, "slow", "@echo off\nping -n 6 127.0.0.1 >nul\n");
        #[cfg(unix)]
        let path = write_hook(&dir.0, "slow", "#!/bin/sh\nsleep 5\n");

        let started = Instant::now();
        let error = run(&spec(&dir, &path, 150), &mut || {}).expect_err("hook times out");

        assert!(error.detail().contains("timed out"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "timeout did not kill the hook: {:?}",
            started.elapsed()
        );
    }

    /// A timed-out hook takes its process group with it. Killing only the
    /// leader would leave a hook's background child running unsupervised.
    #[cfg(unix)]
    #[test]
    fn a_timed_out_hook_kills_its_process_group() {
        let dir = TempDir::new();
        let marker = dir.0.join("late");
        let body = format!(
            "#!/bin/sh\n( sleep 1; echo late > '{}' ) &\nsleep 5\n",
            marker.display()
        );
        let path = write_hook(&dir.0, "group", &body);

        let error = run(&spec(&dir, &path, 150), &mut || {}).expect_err("hook times out");
        assert!(error.detail().contains("timed out"), "{error}");

        // The background child would write after one second; the group kill
        // must have taken it down before that.
        thread::sleep(Duration::from_millis(1500));
        assert!(!marker.exists(), "a killed hook's child wrote {:?}", marker);
    }

    /// The supervisor loop owns plugin stdio: a hook it waits for must keep
    /// the loop polling instead of stalling every plugin pipe for up to a
    /// minute.
    #[cfg(unix)]
    #[test]
    fn a_waiting_hook_keeps_polling_its_loop() {
        use std::cell::Cell;

        let dir = TempDir::new();
        let path = write_hook(&dir.0, "slow-wait", "#!/bin/sh\nsleep 0.3\n");
        let polls = Cell::new(0);
        let mut poll = || polls.set(polls.get() + 1);

        run(
            &spec(&dir, &path, crate::kernel::hook::TIMEOUT_MS),
            &mut poll,
        )
        .expect("hook runs");

        assert!(
            polls.get() > 1,
            "a hook wait must keep the loop polling: {} poll(s)",
            polls.get()
        );
    }

    #[test]
    fn rejects_a_path_that_escapes_the_version_directory() {
        let dir = TempDir::new();

        let error = run(
            &spec(&dir, "../outside", crate::kernel::hook::TIMEOUT_MS),
            &mut || {},
        )
        .expect_err("escape");

        assert!(error.detail().contains("unsafe path"), "{error}");
    }

    #[test]
    fn rejects_a_missing_hook_file() {
        let dir = TempDir::new();

        let error = run(
            &spec(&dir, "hooks/missing", crate::kernel::hook::TIMEOUT_MS),
            &mut || {},
        )
        .expect_err("missing");

        assert!(error.detail().contains("post-install hook"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_hook_without_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new();
        let path = dir.0.join("not-executable");
        fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write hook");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");

        let error = run(
            &spec(&dir, "not-executable", crate::kernel::hook::TIMEOUT_MS),
            &mut || {},
        )
        .expect_err("mode");

        assert!(error.detail().contains("not executable"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_hook_without_a_shebang() {
        let dir = TempDir::new();
        let path = dir.0.join("no-shebang");
        fs::write(&path, "exit 0\n").expect("write hook");
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");

        let error = run(
            &spec(&dir, "no-shebang", crate::kernel::hook::TIMEOUT_MS),
            &mut || {},
        )
        .expect_err("shebang");

        assert!(error.detail().contains("shebang"), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn rejects_a_hook_that_is_not_a_windows_executable() {
        let dir = TempDir::new();
        fs::write(dir.0.join("hook.sh"), "@echo off\r\n").expect("write hook");

        let error = run(
            &spec(&dir, "hook.sh", crate::kernel::hook::TIMEOUT_MS),
            &mut || {},
        )
        .expect_err("extension");

        assert!(error.detail().contains(".exe, .cmd or .bat"), "{error}");
    }
}
