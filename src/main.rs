#[cfg(feature = "http")]
mod api;
mod bundle;
mod channel;
mod daemon;
mod definition;
mod hook;
mod http;
mod install;
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
mod registry;
mod rpc;
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
            "--conf" => {
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
                if let Some(value) = other.strip_prefix("--conf=") {
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

fn usage() {
    eprintln!("usage: svcd run --foreground [--conf <file>]");
    eprintln!("       svcd shutdown");
    eprintln!("       svcd --install [--prefix <dir>] [--conf <file>] [--no-service] [--dry-run]");
    eprintln!("       svcd --uninstall [--prefix <dir>] [--no-service] [--purge] [--dry-run]");
    eprintln!(
        "       svcd install-service [--target systemd|launchd|scm|busybox|procd] [--conf <file>] [--dry-run]"
    );
    eprintln!(
        "       svcd uninstall-service [--target systemd|launchd|scm|busybox|procd] [--purge] [--dry-run]"
    );
}
