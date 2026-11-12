mod channel;
mod daemon;
mod definition;
mod http;
mod mcp;
mod package;
mod paths;
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
        "mcp" => mcp::run_stdio(),
        _ => svcd_run(std::env::args().skip(1).collect()),
    };
    std::process::exit(code);
}

fn svcd_run(args: Vec<String>) -> i32 {
    match args.as_slice() {
        [command, flag] if command == "run" && flag == "--foreground" => daemon::run(),
        [command] if command == "shutdown" => daemon::shutdown(),
        _ => {
            eprintln!("usage: svcd run --foreground");
            eprintln!("       svcd shutdown");
            64
        }
    }
}
