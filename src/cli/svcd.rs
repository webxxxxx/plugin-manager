//! `svcd` host commands: start/stop/restart/status and the user-mode wrappers.
//!
//! The command projection lives at L5. Starting and stopping the daemon is
//! assembly work, so those two calls go through [`Host`], which the composition
//! root implements. Everything else (config discovery, service-manager
//! commands, the detached helper) is a downward call this layer may make.

use std::path::Path;

use crate::platform::config::Config;
use crate::platform::paths;

/// The assembly callbacks the CLI cannot own: only the composition root may
/// stop the daemon process it assembled.
pub(crate) trait Host {
    fn shutdown(&self) -> i32;
    fn shutdown_quiet(&self);
    /// `svcd user run`: the foreground loop lives in the composition root.
    fn run(&self, args: &[String]) -> i32;
}

pub(crate) fn start() -> i32 {
    if paths::user_mode() {
        match Config::bootstrap_or_report("svcd") {
            Some(config) => user_start(&config),
            None => 1,
        }
    } else {
        crate::selfinstall::service_start()
    }
}

pub(crate) fn stop(host: &dyn Host) -> i32 {
    if paths::user_mode() {
        host.shutdown()
    } else {
        crate::selfinstall::service_stop()
    }
}

pub(crate) fn status() -> i32 {
    if paths::user_mode() {
        match Config::bootstrap_or_report("svcd") {
            Some(config) => user_status(&config),
            None => 1,
        }
    } else {
        crate::selfinstall::service_status()
    }
}

pub(crate) fn user(host: &dyn Host, args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("start") => match Config::bootstrap_or_report("svcd") {
            Some(config) => user_start(&config),
            None => 1,
        },
        Some("status") => match Config::bootstrap_or_report("svcd") {
            Some(config) => user_status(&config),
            None => 1,
        },
        Some("stop") => host.shutdown(),
        Some("install") => user_install(),
        Some("run") => {
            std::env::set_var("SVCD_USER", "1");
            host.run(&args[1..])
        }
        _ => {
            user_usage();
            64
        }
    }
}

fn user_usage() {
    eprintln!("usage: svcd user start");
    eprintln!("       svcd user stop");
    eprintln!("       svcd user status");
    eprintln!("       svcd user install");
    eprintln!("       svcd user run --foreground [--conf|--config <file>]");
}

fn user_running(config: &Config) -> bool {
    crate::wire::control::channel::request(
        config.paths.control_dir(),
        "svc.list",
        serde_json::json!({}),
    )
    .is_ok()
}

fn user_start(config: &Config) -> i32 {
    if !paths::user_mode() {
        eprintln!(
            "svcd: user mode is only available to non-root users; set SVCD_USER=1 to force it"
        );
        return 1;
    }
    if user_running(config) {
        println!("svcd user daemon is already running");
        return 0;
    }
    let config_dir = &config.paths.config_dir;
    let logs = &config.paths.logs_dir;
    if let Err(error) = std::fs::create_dir_all(config_dir) {
        eprintln!("svcd: create {}: {}", config_dir.display(), error);
        return 1;
    }
    if let Err(error) = std::fs::create_dir_all(logs) {
        eprintln!("svcd: create {}: {}", logs.display(), error);
        return 1;
    }
    match spawn_foreground_daemon(config) {
        Ok(pid) => {
            println!("svcd user daemon started (pid {})", pid);
            println!("config: {}", config_dir.display());
            println!(
                "logs:   {}",
                config.paths.logs_dir.join("svcd.log").display()
            );
            0
        }
        Err(error) => {
            eprintln!("svcd: start user daemon: {}", error);
            1
        }
    }
}

/// Spawn `svcd run --foreground` detached, with stdout/stderr appended to
/// `svcd.log` (or discarded when the master log switch is off). Shared by
/// `svcd user start`, `svcd restart` and the restart helper so all three start
/// the daemon identically.
fn spawn_foreground_daemon(config: &Config) -> Result<u32, String> {
    // The daemon outlives its starter; it must not inherit the starter's pipes.
    crate::platform::host::process::detach_stdio();
    let log_path = config.paths.logs_dir.join("svcd.log");
    let (stdout, stderr) = if config.limits.log_enabled {
        let stdout = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|error| format!("open {}: {}", log_path.display(), error))?;
        let stderr = stdout
            .try_clone()
            .map_err(|error| format!("clone log handle: {}", error))?;
        (
            std::process::Stdio::from(stdout),
            std::process::Stdio::from(stderr),
        )
    } else {
        (std::process::Stdio::null(), std::process::Stdio::null())
    };
    let executable =
        std::env::current_exe().map_err(|error| format!("current executable: {}", error))?;
    let mut command = std::process::Command::new(&executable);
    command
        .args(["run", "--foreground"])
        .env("SVCD_USER", "1")
        .stdin(std::process::Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: The handle/pointer is produced by the immediately preceding OS call and is valid for this call.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
        .spawn()
        .map(|child| child.id())
        .map_err(|error| format!("spawn: {}", error))
}

/// Restart the daemon. A user-mode daemon is restarted here; a system service
/// belongs to its service manager, which owns its lifetime.
pub(crate) fn restart(host: &dyn Host) -> i32 {
    if !paths::user_mode() {
        eprintln!(
            "svcd: this daemon runs as a service; restart it with the service manager \
             (systemctl restart svcd / launchctl kickstart -k / sc stop svcd && sc start svcd)"
        );
        return 1;
    }
    let Some(config) = Config::bootstrap_or_report("svcd") else {
        return 1;
    };
    if user_running(&config) {
        if host.shutdown() != 0 {
            eprintln!("svcd: the running daemon did not accept the shutdown request");
            return 1;
        }
        for _ in 0..200 {
            if !user_running(&config) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
    // Start the replacement from the detached helper rather than from this
    // process: the helper runs with null stdio, so the daemon it starts cannot
    // inherit whatever pipes the caller of `svcd restart` holds.
    match crate::supervise::ops::restart::spawn_helper() {
        Ok(helper) => {
            println!("svcd restarting (helper pid {})", helper);
            0
        }
        Err(error) => {
            eprintln!("svcd: restart: {}", error);
            1
        }
    }
}

/// Wait for the old daemon to release the control channel, then start a new one.
/// This is the detached half of a settings save.
pub(crate) fn restart_helper(host: &dyn Host, args: &[String]) -> i32 {
    let Some(_pid) = args.first().and_then(|value| value.parse::<u32>().ok()) else {
        eprintln!("svcd: restart-helper expects a pid");
        return 64;
    };
    // Grace period: the save that spawned this helper still has an HTTP reply
    // in flight, and that response must reach the browser before the daemon
    // closes the listener.
    std::thread::sleep(std::time::Duration::from_millis(200));
    // Liveness is "does the control channel answer", not "does the pid exist":
    // process liveness has no portable Windows implementation, and the control
    // channel is exactly the resource the replacement must be able to rebind.
    let paths = crate::platform::config::ambient_paths(None);
    let control = paths.control_dir();
    let ping = |control: &Path| {
        crate::wire::control::channel::request(control, "svcd.ping", serde_json::json!({})).is_ok()
    };
    for _ in 0..40 {
        if !ping(control) {
            break;
        }
        host.shutdown_quiet();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    for _ in 0..200 {
        if !ping(control) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    // The ping fails as soon as the channel is gone; give the OS a moment to
    // release the socket/pipe name before the replacement binds it.
    std::thread::sleep(std::time::Duration::from_millis(100));
    let Some(config) = Config::bootstrap_or_report("svcd") else {
        return 1;
    };
    match spawn_foreground_daemon(&config) {
        Ok(child) => {
            println!("svcd restarted (pid {})", child);
            0
        }
        Err(error) => {
            eprintln!("svcd: restart helper: {}", error);
            1
        }
    }
}

fn user_status(config: &Config) -> i32 {
    println!("mode:   user");
    println!("config: {}", config.paths.config_dir.display());
    println!("data:   {}", config.paths.data_dir.display());
    println!("logs:   {}", config.paths.logs_dir.display());
    if user_running(config) {
        println!("state:  running");
        0
    } else {
        println!("state:  stopped");
        1
    }
}

fn user_install() -> i32 {
    crate::selfinstall::self_install(&[])
}
