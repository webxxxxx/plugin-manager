#[cfg(feature = "http")]
mod api;
mod auth;
mod blob;
mod bundle;
mod catalog;
mod channel;
#[cfg(unix)]
mod channel_client_reactor;
#[cfg(unix)]
mod channel_reactor;
mod daemon;
mod definition;
mod hook;
mod http;
mod install;
mod interpreter;
mod ipc;
#[cfg_attr(not(feature = "http"), allow(dead_code))]
mod lan;
mod log;
mod mcp;
#[cfg(feature = "http")]
mod mcp_http;
mod package;
mod paths;
mod plugin;
mod plugin_cli;
mod proc;
#[cfg(unix)]
mod reactor;
mod registry;
mod rpc;
mod script;
#[cfg(windows)]
mod service_win;
mod settings;
mod slot;
mod status;
mod supervisor;
mod svc;
mod threads;
mod transfer;
mod trust;
#[cfg(feature = "http")]
mod uichannel;
mod wake;
#[cfg(feature = "http")]
mod webui;

fn main() {
    let arg0 = std::env::args().next().unwrap_or_default();
    // Installed entries are hardlinks/symlinks named `svc`, `plugin`, `mcp`
    // (and `.exe` variants on Windows); dispatch on the stem, not the suffix.
    let name = std::path::Path::new(&arg0)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("svcd");

    let code = match name {
        "svc" => svc::run(std::env::args().skip(1).collect()),
        "plugin" => plugin_cli::run(std::env::args().skip(1).collect()),
        "mcp" => mcp::run_stdio(),
        _ => svcd_run(std::env::args().skip(1).collect()),
    };
    std::process::exit(code);
}

fn svcd_run(args: Vec<String>) -> i32 {
    match args.first().map(String::as_str) {
        Some("run") => run_command(&args[1..]),
        Some("shutdown") if args.len() == 1 => daemon::shutdown(),
        Some("--install") | Some("install") => install::self_install(&args[1..]),
        Some("--uninstall") | Some("uninstall") => install::self_uninstall(&args[1..]),
        Some("install-service") => install::install_service(&args[1..]),
        Some("uninstall-service") => install::uninstall_service(&args[1..]),
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
    if let Some(code) = service_win::run_if_service(conf.clone()) {
        return code;
    }
    daemon::run(conf)
}

fn start_command() -> i32 {
    if crate::paths::user_mode() {
        user_start()
    } else {
        crate::install::service_start()
    }
}

fn stop_command() -> i32 {
    if crate::paths::user_mode() {
        crate::daemon::shutdown()
    } else {
        crate::install::service_stop()
    }
}

fn status_command() -> i32 {
    if crate::paths::user_mode() {
        user_status()
    } else {
        crate::install::service_status()
    }
}

fn user_command(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("start") => user_start(),
        Some("stop") => crate::daemon::shutdown(),
        Some("status") => user_status(),
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

fn user_running() -> bool {
    crate::channel::request(
        &crate::paths::config_dir(),
        "svc.list",
        serde_json::json!({}),
    )
    .is_ok()
}

fn user_start() -> i32 {
    if !crate::paths::user_mode() {
        eprintln!(
            "svcd: user mode is only available to non-root users; set SVCD_USER=1 to force it"
        );
        return 1;
    }
    if user_running() {
        println!("svcd user daemon is already running");
        return 0;
    }
    let config = crate::paths::config_dir();
    let logs = crate::paths::logs_dir();
    if let Err(error) = std::fs::create_dir_all(&config) {
        eprintln!("svcd: create {}: {}", config.display(), error);
        return 1;
    }
    if let Err(error) = std::fs::create_dir_all(&logs) {
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
    let mut command = std::process::Command::new(&executable);
    command
        .args(["run", "--foreground"])
        .env("SVCD_USER", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(stdout))
        .stderr(std::process::Stdio::from(stderr));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
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
            println!("config: {}", config.display());
            println!("logs:   {}", log_path.display());
            0
        }
        Err(error) => {
            eprintln!("svcd: start user daemon: {}", error);
            1
        }
    }
}

fn user_status() -> i32 {
    println!("mode:   user");
    println!("config: {}", crate::paths::config_dir().display());
    println!("data:   {}", crate::paths::data_dir().display());
    println!("logs:   {}", crate::paths::logs_dir().display());
    if user_running() {
        println!("state:  running");
        0
    } else {
        println!("state:  stopped");
        1
    }
}

fn user_install() -> i32 {
    crate::install::self_install(&[])
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
