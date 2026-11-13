#[cfg(feature = "http")]
mod api;
mod bundle;
mod channel;
mod daemon;
mod definition;
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
mod slot;
mod status;
mod supervisor;
mod svc;
mod threads;
mod transfer;
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
        Some("run") if args.len() == 2 && args[1] == "--foreground" => run_foreground(),
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

/// `svcd run --foreground` serves both a console and the Windows SCM: the SCM
/// owns the process via its dispatcher, and every other caller falls through to
/// the plain daemon loop.
fn run_foreground() -> i32 {
    #[cfg(windows)]
    if let Some(code) = service_win::run_if_service() {
        return code;
    }
    daemon::run()
}

fn usage() {
    eprintln!("usage: svcd run --foreground");
    eprintln!("       svcd shutdown");
    eprintln!("       svcd --install [--prefix <dir>] [--no-service] [--dry-run]");
    eprintln!("       svcd --uninstall [--prefix <dir>] [--no-service] [--purge] [--dry-run]");
    eprintln!(
        "       svcd install-service [--target systemd|launchd|scm|busybox|procd] [--dry-run]"
    );
    eprintln!(
        "       svcd uninstall-service [--target systemd|launchd|scm|busybox|procd] [--purge] [--dry-run]"
    );
}
