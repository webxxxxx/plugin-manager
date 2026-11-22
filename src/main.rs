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
        Some("status") => status_command(),
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
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("svcd: current executable: {}", error);
            return 1;
        }
    };
    let log_path = logs.join("svcd.log");
    // `log = off` is a master switch: do not even create the redirect target.
    let (stdout, stderr) = if config.limits.log_enabled {
        let stdout = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            Ok(file) => file,
            Err(error) => {
                eprintln!("svcd: open {}: {}", log_path.display(), error);
                return 1;
            }
        };
        let stderr = match stdout.try_clone() {
            Ok(file) => file,
            Err(error) => {
                eprintln!("svcd: clone log handle: {}", error);
                return 1;
            }
        };
        (
            std::process::Stdio::from(stdout),
            std::process::Stdio::from(stderr),
        )
    } else {
        (std::process::Stdio::null(), std::process::Stdio::null())
    };
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
    match command.spawn() {
        Ok(child) => {
            println!("svcd user daemon started (pid {})", child.id());
            println!("config: {}", config_dir.display());
            println!("logs:   {}", log_path.display());
            0
        }
        Err(error) => {
            eprintln!("svcd: start user daemon: {}", error);
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
