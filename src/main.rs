mod api;
mod channel;
mod daemon;
mod definition;
mod http;
mod install;
mod log;
mod mcp;
mod package;
mod paths;
mod plugin;
mod plugin_cli;
mod registry;
mod rpc;
mod slot;
mod status;
mod supervisor;
mod svc;

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
        Some("run") if args.len() == 2 && args[1] == "--foreground" => daemon::run(),
        Some("shutdown") if args.len() == 1 => daemon::shutdown(),
        Some("install-service") => install::install_service(&args[1..]),
        Some("uninstall-service") => install::uninstall_service(&args[1..]),
        _ => {
            usage();
            64
        }
    }
}

fn usage() {
    eprintln!("usage: svcd run --foreground");
    eprintln!("       svcd shutdown");
    eprintln!(
        "       svcd install-service [--target systemd|launchd|scm|busybox|procd] [--dry-run]"
    );
    eprintln!(
        "       svcd uninstall-service [--target systemd|launchd|scm|busybox|procd] [--purge] [--dry-run]"
    );
}
