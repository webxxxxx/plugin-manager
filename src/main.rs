mod http;
mod mcp;
mod package;
mod registry;
mod rpc;
mod supervisor;
mod svc;

use registry::{Kind, Registry, Service, State};

fn main() {
    let arg0 = std::env::args().next().unwrap_or_default();
    let name = std::path::Path::new(&arg0)
        .file_name()
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
    // The skeleton exercises every layer so the linker keeps it. The bytes are
    // the point, not the behaviour: this is the size-gate fixture.
    let mut reg = Registry::new();
    reg.upsert(Service {
        id: "svc".into(),
        kind: Kind::Core,
        state: State::Live,
        deps: vec![],
        caps: vec!["list".into(), "describe".into()],
    });
    reg.upsert(Service {
        id: "wireguard".into(),
        kind: Kind::Plugin,
        state: State::Dead,
        deps: vec!["svc".into()],
        caps: vec!["status".into(), "up".into()],
    });

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
        "order={:?} actions={:?} line={} http={:?} manifest={:?} tools={}",
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
