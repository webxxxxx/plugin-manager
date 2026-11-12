mod definition;
mod http;
mod mcp;
mod package;
mod paths;
mod registry;
mod rpc;
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
        _ => svcd_run(),
    };
    std::process::exit(code);
}

fn svcd_run() -> i32 {
    // The skeleton exercises every layer so the linker keeps it. The registry is
    // real now: built-in `svc` plus services.d, the same view `svc list` uses.
    let loaded = definition::load_services(&paths::services_dir());
    for issue in &loaded.issues {
        eprintln!("svcd: warning: {}", issue);
    }
    let reg = loaded.registry;

    let order = reg.topo().unwrap_or_default();
    let mut proc = supervisor::Service::new();
    let actions = proc.step(supervisor::Event::Start, 0);
    let req = rpc::parse("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"svcd.ping\"}");
    let line = match req {
        Ok(r) => rpc::to_line(&rpc::ok(r.id, serde_json::json!({"ok": true}))),
        Err(e) => rpc::to_line(&e),
    };
    let http_req = http::parse(b"GET /plugin-ui/wireguard/ HTTP/1.1\r\nHost: x\r\n\r\n");
    let http_res = http::response(200, "text/plain", b"ok");
    let manifest = package::parse_manifest(MANIFEST.as_bytes()).map(|m| m.name.clone());
    let tools = mcp::tools(&reg);

    println!(
        "services={} order={:?} actions={:?} line={} http={:?} manifest={:?} tools={}",
        reg.len(),
        order,
        actions,
        line,
        http_req.map(|r| (r.method.to_string(), r.path.to_string())),
        manifest,
        tools.len()
    );
    let _ = http_res;
    0
}

const MANIFEST: &str = r#"{"schema":1,"name":"wireguard","version":"1.0.0","desc":"d","platform":"x86_64-unknown-linux-musl","protocol":1,"exec":{"path":"payload/bin/wg-agent"},"capabilities":[{"name":"status","desc":"s","inputSchema":{"type":"object"}}],"files":[]}"#;
