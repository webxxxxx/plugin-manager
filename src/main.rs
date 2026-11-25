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
mod wire;

fn main() {
    #[cfg(windows)]
    if let Some(pid) = std::env::var("SVCD_UNINSTALL_PARENT")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
    {
        selfinstall::wait_for_process(pid);
    }
    // A Windows self-replace renames the running image aside; the next start
    // removes the leftovers.
    crate::selfinstall::upgrade::cleanup_old_files();
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

/// The two assembly callbacks the CLI cannot own.
struct MainHost;

impl cli::svcd::Host for MainHost {
    fn shutdown(&self) -> i32 {
        daemon::shutdown()
    }

    fn shutdown_quiet(&self) {
        let _ = daemon::shutdown_quiet();
    }

    fn run(&self, args: &[String]) -> i32 {
        run_command(args)
    }
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
        Some("version") if args.len() == 1 => {
            println!("{}", crate::selfinstall::upgrade::version_line());
            0
        }
        Some("upgrade") => cli::upgrade::run(&args[1..], usage),
        Some("start") => cli::svcd::start(),
        Some("stop") => cli::svcd::stop(&MainHost),
        Some("restart") if args.len() == 1 => cli::svcd::restart(&MainHost),
        Some("status") => cli::svcd::status(),
        // Internal: spawned by `svcd restart` / a settings save. It waits for
        // its parent to exit (the control socket and HTTP port are still held),
        // then starts the daemon the same way a background user daemon starts.
        Some("restart-helper") => cli::svcd::restart_helper(&MainHost, &args[1..]),
        // Internal: the service-mode half of a settings save (#306). It stops
        // this daemon and lets the init system start it again.
        Some("service-restart-helper") => cli::svcd::service_restart_helper(&MainHost),
        Some("user") => cli::svcd::user(&MainHost, &args[1..]),
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

fn usage() {
    eprintln!("usage: svcd run --foreground [--conf|--config <file>]");
    eprintln!("       svcd version");
    eprintln!("       svcd upgrade [--check] [--file <archive>] [--sum <SHA256SUMS>] [--json]");
    eprintln!("       svcd start|stop|status|restart");
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
