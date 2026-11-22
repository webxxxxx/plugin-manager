// The five-entry family is the call-site contract for internal lines. Their
// source column is always `svcd` (ADR-0022); a module path is not a column in
// the file contract, so the macros do not fabricate a `target`.
#[allow(unused_macros)]
macro_rules! error {
    ($($arg:tt)*) => {
        $crate::platform::log::record(
            $crate::platform::log::Level::Error,
            &format!($($arg)*),
        )
    };
}

#[allow(unused_macros)]
macro_rules! warn {
    ($($arg:tt)*) => {
        $crate::platform::log::record(
            $crate::platform::log::Level::Warn,
            &format!($($arg)*),
        )
    };
}

#[allow(unused_macros)]
macro_rules! info {
    ($($arg:tt)*) => {
        $crate::platform::log::record(
            $crate::platform::log::Level::Info,
            &format!($($arg)*),
        )
    };
}

#[allow(unused_macros)]
macro_rules! debug {
    ($($arg:tt)*) => {
        $crate::platform::log::record(
            $crate::platform::log::Level::Debug,
            &format!($($arg)*),
        )
    };
}

#[allow(unused_macros)]
macro_rules! trace {
    ($($arg:tt)*) => {
        $crate::platform::log::record(
            $crate::platform::log::Level::Trace,
            &format!($($arg)*),
        )
    };
}

mod cli;
mod daemon;
mod face;
mod kernel;
mod manage;
mod platform;
mod selfinstall;
mod server;
mod supervise;
#[cfg(test)]
mod test_support;
mod wire;

fn main() {
    #[cfg(windows)]
    if let Some(pid) = std::env::var("SVCD_UNINSTALL_PARENT")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
    {
        selfinstall::wait_for_process(pid);
    }
    let arg0 = std::env::args().next().unwrap_or_default();
    // Installed entries are hardlinks/symlinks named `svc`, `plugin`, `mcp`
    // (and `.exe` variants on Windows); dispatch on the stem, not the suffix.
    let name = std::path::Path::new(&arg0)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("svcd");

    let code = match name {
        "svc" => cli::svc::run(std::env::args().skip(1).collect()),
        "plugin" => cli::plugin::run(std::env::args().skip(1).collect()),
        "mcp" => face::mcp::run_stdio(),
        "uninstall" => {
            let mut args: Vec<String> = std::env::args().skip(1).collect();
            if args.first().map(String::as_str) != Some("--uninstall") {
                args.insert(0, "--uninstall".into());
            }
            // The first element is the dispatch word, not part of the flags.
            selfinstall::self_uninstall(&args[1..], &stop_daemon)
        }
        _ => svcd_run(std::env::args().skip(1).collect()),
    };
    std::process::exit(code);
}

fn svcd_run(args: Vec<String>) -> i32 {
    match args.first().map(String::as_str) {
        Some("run") => run_command(&args[1..]),
        Some("shutdown") if args.len() == 1 => daemon::shutdown(),
        Some("--install") | Some("install") => selfinstall::self_install(&args[1..]),
        Some("--uninstall") | Some("uninstall") => {
            selfinstall::self_uninstall(&args[1..], &stop_daemon)
        }
        Some("install-service") => selfinstall::install_service(&args[1..]),
        Some("uninstall-service") => selfinstall::uninstall_service(&args[1..]),
        Some("start") => start_command(),
        Some("stop") => stop_command(),
        Some("restart") if args.len() == 1 => restart_command(),
        Some("status") => status_command(),
        // Internal: spawned by `svcd restart` / a settings save. It waits for
        // its parent to exit (the control socket and HTTP port are still held),
        // then starts the daemon the same way a background user daemon starts.
        Some("restart-helper") => restart_helper(&args[1..]),
        Some("user") => user_command(&args[1..]),
        _ => {
            usage();
            64
        }
    }
}

fn run_command(args: &[String]) -> i32 {
    let mut foreground = false;
    let mut conf = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--foreground" => foreground = true,
            "--conf" | "--config" => {
                index += 1;
                match args.get(index) {
                    Some(value) if !value.is_empty() => {
                        conf = Some(std::path::PathBuf::from(value))
                    }
                    _ => {
                        usage();
                        return 64;
                    }
                }
            }
            other => {
                if let Some(value) = other
                    .strip_prefix("--conf=")
                    .or_else(|| other.strip_prefix("--config="))
                {
                    if value.is_empty() {
                        usage();
                        return 64;
                    }
                    conf = Some(std::path::PathBuf::from(value));
                } else {
                    usage();
                    return 64;
                }
            }
        }
        index += 1;
    }
    if !foreground {
        usage();
        return 64;
    }
    run_foreground(conf)
}

/// `svcd run --foreground` serves both a console and the Windows SCM: the SCM
/// owns the process via its dispatcher, and every other caller falls through to
/// the plain daemon loop.
fn run_foreground(conf: Option<std::path::PathBuf>) -> i32 {
    #[cfg(windows)]
    if let Some(code) =
        selfinstall::service_win::run_if_service(conf.clone(), daemon::run, request_shutdown)
    {
        return code;
    }
    daemon::run(conf)
}

/// Let the Windows SCM control handler stop the daemon without the SCM module
/// reaching into the composition root itself (§7 rule 1).
#[cfg(windows)]
fn request_shutdown() {
    daemon::request_shutdown();
}

/// Stop the running daemon before a self-uninstall replaces or removes the
/// binary. A self-install action reports that it needs the daemon stopped; the
/// composition root performs it instead of handing the domain a way back up.
fn stop_daemon() {
    let _ = daemon::shutdown_quiet();
}

fn start_command() -> i32 {
    if crate::platform::paths::user_mode() {
        match crate::platform::config::Config::bootstrap_or_report("svcd") {
            Some(config) => user_start(&config),
            None => 1,
        }
    } else {
        crate::selfinstall::service_start()
    }
}

fn stop_command() -> i32 {
    if crate::platform::paths::user_mode() {
        crate::daemon::shutdown()
    } else {
        crate::selfinstall::service_stop()
    }
}

fn status_command() -> i32 {
    if crate::platform::paths::user_mode() {
        match crate::platform::config::Config::bootstrap_or_report("svcd") {
            Some(config) => user_status(&config),
            None => 1,
        }
    } else {
        crate::selfinstall::service_status()
    }
}

fn user_command(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("start") => match crate::platform::config::Config::bootstrap_or_report("svcd") {
            Some(config) => user_start(&config),
            None => 1,
        },
        Some("status") => match crate::platform::config::Config::bootstrap_or_report("svcd") {
            Some(config) => user_status(&config),
            None => 1,
        },
        Some("stop") => crate::daemon::shutdown(),
        Some("install") => user_install(),
        Some("run") => {
            std::env::set_var("SVCD_USER", "1");
            run_command(&args[1..])
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

fn user_running(config: &crate::platform::config::Config) -> bool {
    crate::wire::control::channel::request(
        config.paths.control_dir(),
        "svc.list",
        serde_json::json!({}),
    )
    .is_ok()
}

fn user_start(config: &crate::platform::config::Config) -> i32 {
    if !crate::platform::paths::user_mode() {
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
fn spawn_foreground_daemon(config: &crate::platform::config::Config) -> Result<u32, String> {
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
fn restart_command() -> i32 {
    if !crate::platform::paths::user_mode() {
        eprintln!(
            "svcd: this daemon runs as a service; restart it with the service manager \
             (systemctl restart svcd / launchctl kickstart -k / sc stop svcd && sc start svcd)"
        );
        return 1;
    }
    let Some(config) = crate::platform::config::Config::bootstrap_or_report("svcd") else {
        return 1;
    };
    if user_running(&config) {
        if crate::daemon::shutdown() != 0 {
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
    match crate::daemon::restart::spawn_helper() {
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

/// Wait for the process named in `pid` to exit, then start the daemon again.
/// This is the detached half of a settings save: the daemon cannot rebind its
/// control socket or HTTP port while it is still running.
fn restart_helper(args: &[String]) -> i32 {
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
    let ping = |control: &std::path::Path| {
        crate::wire::control::channel::request(control, "svcd.ping", serde_json::json!({})).is_ok()
    };
    for _ in 0..40 {
        if !ping(control) {
            break;
        }
        let _ = crate::daemon::shutdown_quiet();
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
    let Some(config) = crate::platform::config::Config::bootstrap_or_report("svcd") else {
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

fn user_status(config: &crate::platform::config::Config) -> i32 {
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

fn usage() {
    eprintln!("usage: svcd run --foreground [--conf|--config <file>]");
    eprintln!("       svcd start|stop|status");
    eprintln!("       svcd shutdown");
    eprintln!("       svcd user start|stop|status|install|run --foreground   (legacy aliases)");
    eprintln!("       svcd --install [--prefix <dir>] [--conf|--config <file>] [--manual] [--no-service] [--dry-run]");
    eprintln!("       svcd --uninstall [--prefix <dir>] [--no-service] [--purge] [--dry-run]");
    eprintln!(
        "       svcd install-service [--target systemd|launchd|scm|busybox|procd] [--conf <file>] [--dry-run]"
    );
    eprintln!(
        "       svcd uninstall-service [--target systemd|launchd|scm|busybox|procd] [--purge] [--dry-run]"
    );
}
