#![cfg(test)]

use super::*;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde_json::json;

use crate::kernel::contract::{Capability as KernelCapability, CapabilityName, Effect};
use crate::kernel::failure::Problem;
use crate::kernel::projection::Status;
use crate::wire::message::rpc;

use super::driver::status_response;
use super::shutdown::shutdown_reply_lost;
use crate::kernel::registry::{Exec, Kind, Ready, Service, State};
use crate::platform::clock::{Clock, Now};
use crate::supervise::model::runtime::{self, Record};

fn capability(name: &str) -> KernelCapability {
    KernelCapability {
        name: CapabilityName::new(name),
        desc: format!("{} description", name),
        input_schema: Some(json!({"type": "object"})),
        output_schema: None,
        effect: Effect::Write,
        idempotent: false,
    }
}

fn plugin(id: &str, caps: &[&str]) -> Service {
    let caps = caps.iter().map(|cap| capability(cap)).collect();
    Service {
        kind: Kind::Plugin,
        contract: crate::kernel::registry::contract_for(
            id,
            "1.0.0",
            caps,
            crate::kernel::registry::Carrier::Slot,
            &crate::platform::limits::test_default(),
        ),
        ..service(id, &[])
    }
}

fn service(id: &str, deps: &[&str]) -> Service {
    let contract = crate::kernel::registry::contract_for(
        id,
        "1.0.0",
        Vec::new(),
        crate::kernel::registry::Carrier::Slot,
        &crate::platform::limits::test_default(),
    );
    Service {
        id: id.into(),
        kind: Kind::Core,
        version: "1.0.0".into(),
        protocol: 1,
        desc: "test".into(),
        display_name: None,
        deps: deps.iter().map(|dep| dep.to_string()).collect(),
        contract,
        impl_ref: None,
        exec: Some(Exec {
            path: "/bin/true".into(),
            args: Vec::new(),
            env: Default::default(),
            ready: Ready::Exec,
            interpreter: None,
        }),
        lifecycle: None,
        capability_hooks: Default::default(),
        enabled: true,
        restart: crate::kernel::registry::Restart::OnFailure,
        remain_after_exit: false,
        compat: None,
        state: State::Stopped,
        pid: None,
        since: None,
        ready_since: None,
        next_at: None,
        failures: 0,
        last_exit: None,
        config: None,
        install_dir: None,
    }
}

fn registry(services: Vec<Service>) -> Registry {
    let mut registry = Registry::new();
    for service in services {
        registry.insert(service).expect("insert");
    }
    registry
}

#[cfg(target_os = "linux")]
#[test]
fn the_daemon_does_not_spawn_call_worker_threads() {
    let (_sender, receiver) = mpsc::channel();
    let _daemon = Daemon::new(
        Registry::with_builtin(&crate::platform::limits::test_default()),
        receiver,
        None,
    );
    let names: Vec<String> = std::fs::read_dir("/proc/self/task")
        .expect("task dir")
        .filter_map(|entry| {
            let path = entry.ok()?.path().join("comm");
            std::fs::read_to_string(path).ok()
        })
        .map(|name| name.trim().to_string())
        .collect();
    assert!(
        !names.iter().any(|name| name.starts_with("svcd-call")),
        "daemon must not spawn blocking call workers: {names:?}"
    );
}

fn tool_agent(root: &Path) -> Exec {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = root.join("agent.sh");
        std::fs::write(
                &script,
                r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -n1 | cut -d: -f2)
  case "$line" in
    *svcd.ping*) printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id" ;;
    *'"name":"echo"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}\n' "$id" ;;
    *'"name":"fail"'*) printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32000,"message":"boom"}}\n' "$id" ;;
    *'"name":"hang"'*) ;;
    *'"name":"crash"'*) exit 0 ;;
    *'"name":"progress"'*)
      meta=$(printf '%s' "$line" | sed -n 's/.*"_meta":{\([^}]*\)}.*/\1/p')
      token=$(printf '%s' "$meta" | sed -n 's/.*"progressToken":"\([^"]*\)".*/\1/p')
      if [ -z "$token" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32602,"message":"progressToken must be under _meta"}}\n' "$id"
      else
        printf '{"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1,"progressToken":"%s"}}\n' "$token"
        printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"done"}],"isError":false}}\n' "$id"
      fi
      ;;
  esac
done
"#,
            )
            .expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        Exec {
            path: "/bin/sh".into(),
            args: vec![script.to_string_lossy().to_string()],
            env: Default::default(),
            ready: Ready::Notify,
            interpreter: None,
        }
    }
    #[cfg(windows)]
    {
        let script = root.join("agent.ps1");
        std::fs::write(
                &script,
                r#"$reader = [Console]::In
while ($null -ne ($line = $reader.ReadLine())) {
  $msg = $line | ConvertFrom-Json
  if ($msg.method -eq 'svcd.ping') {
    [Console]::Out.WriteLine('{"jsonrpc":"2.0","id":' + $msg.id + ',"result":{"ok":true}}')
    [Console]::Out.Flush()
    continue
  }
  if ($msg.method -eq 'tools/call') {
    $name = $msg.params.name
    if ($name -eq 'crash') { exit 0 }
    if ($name -eq 'hang') { continue }
    if ($name -eq 'echo') {
      $out = '{"jsonrpc":"2.0","id":' + $msg.id + ',"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}'
    } elseif ($name -eq 'fail') {
      $out = '{"jsonrpc":"2.0","id":' + $msg.id + ',"error":{"code":-32000,"message":"boom"}}'
    } elseif ($name -eq 'progress') {
      $token = $msg.params._meta.progressToken
      if ($null -ne $token) {
        [Console]::Out.WriteLine('{"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1,"progressToken":"' + $token + '"}}')
        [Console]::Out.Flush()
      }
      $out = '{"jsonrpc":"2.0","id":' + $msg.id + ',"result":{"content":[{"type":"text","text":"done"}],"isError":false}}'
    } else {
      continue
    }
    [Console]::Out.WriteLine($out)
    [Console]::Out.Flush()
  }
}
"#,
            )
            .expect("script");
        let powershell = std::env::var_os("SystemRoot")
            .map(|root| Path::new(&root).join("System32/WindowsPowerShell/v1.0/powershell.exe"))
            .filter(|path| path.is_file())
            .unwrap_or_else(|| std::path::PathBuf::from("powershell.exe"));
        Exec {
            path: powershell.to_string_lossy().to_string(),
            args: vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                script.to_string_lossy().to_string(),
            ],
            env: Default::default(),
            ready: Ready::Notify,
            interpreter: None,
        }
    }
}

fn tool_service(id: &str, kind: Kind, caps: &[&str], root: &Path) -> Service {
    let caps = caps.iter().map(|cap| capability(cap)).collect();
    Service {
        kind,
        desc: "tools".into(),
        contract: crate::kernel::registry::contract_for(
            id,
            "1.0.0",
            caps,
            crate::kernel::registry::Carrier::Slot,
            &crate::platform::limits::test_default(),
        ),
        exec: Some(tool_agent(root)),
        ..service(id, &[])
    }
}

/// A notify-ready peer that answers the ready ping and then exits: exactly
/// the shape a channel peer must never have.
fn ping_once_agent(root: &Path) -> Exec {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = root.join("ping-once.sh");
        std::fs::write(
                &script,
                "#!/bin/sh\nread -r line\nprintf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\\n'\nexit 0\n",
            )
            .expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        Exec {
            path: "/bin/sh".into(),
            args: vec![script.to_string_lossy().to_string()],
            env: Default::default(),
            ready: Ready::Notify,
            interpreter: None,
        }
    }
    #[cfg(windows)]
    {
        let script = root.join("ping-once.ps1");
        std::fs::write(
            &script,
            r#"$null = [Console]::In.ReadLine()
[Console]::Out.WriteLine('{"jsonrpc":"2.0","id":1,"result":{"ok":true}}')
[Console]::Out.Flush()
"#,
        )
        .expect("script");
        let powershell = std::env::var_os("SystemRoot")
            .map(|root| Path::new(&root).join("System32/WindowsPowerShell/v1.0/powershell.exe"))
            .filter(|path| path.is_file())
            .unwrap_or_else(|| std::path::PathBuf::from("powershell.exe"));
        Exec {
            path: powershell.to_string_lossy().to_string(),
            args: vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                script.to_string_lossy().to_string(),
            ],
            env: Default::default(),
            ready: Ready::Notify,
            interpreter: None,
        }
    }
}

/// A notify-ready peer that answers the ready ping late, then stays alive:
/// the "other service" that keeps a start layer open long enough for a dead
/// peer to be observed.
fn slow_ping_agent(root: &Path) -> Exec {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = root.join("slow-ping.sh");
        std::fs::write(
                &script,
                "#!/bin/sh\nsleep 0.3\nread -r line\nprintf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\\n'\nwhile IFS= read -r line; do :; done\n",
            )
            .expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        Exec {
            path: "/bin/sh".into(),
            args: vec![script.to_string_lossy().to_string()],
            env: Default::default(),
            ready: Ready::Notify,
            interpreter: None,
        }
    }
    #[cfg(windows)]
    {
        let script = root.join("slow-ping.ps1");
        std::fs::write(
            &script,
            r#"Start-Sleep -Milliseconds 300
$null = [Console]::In.ReadLine()
[Console]::Out.WriteLine('{"jsonrpc":"2.0","id":1,"result":{"ok":true}}')
[Console]::Out.Flush()
while ($null -ne [Console]::In.ReadLine()) { }
"#,
        )
        .expect("script");
        let powershell = std::env::var_os("SystemRoot")
            .map(|root| Path::new(&root).join("System32/WindowsPowerShell/v1.0/powershell.exe"))
            .filter(|path| path.is_file())
            .unwrap_or_else(|| std::path::PathBuf::from("powershell.exe"));
        Exec {
            path: powershell.to_string_lossy().to_string(),
            args: vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                script.to_string_lossy().to_string(),
            ],
            env: Default::default(),
            ready: Ready::Notify,
            interpreter: None,
        }
    }
}

/// A fire-and-forget command that exits cleanly as fast as it can.
fn exit_zero_exec() -> Exec {
    #[cfg(windows)]
    {
        Exec {
            path: r"C:\Windows\System32\cmd.exe".into(),
            args: vec!["/c".into(), "exit".into(), "0".into()],
            env: Default::default(),
            ready: Ready::Exec,
            interpreter: None,
        }
    }
    #[cfg(not(windows))]
    {
        Exec {
            path: "/bin/true".into(),
            args: Vec::new(),
            env: Default::default(),
            ready: Ready::Exec,
            interpreter: None,
        }
    }
}

/// A plugin-shaped service with a chosen exec and protocol.
fn peer_plugin(id: &str, exec: Exec, protocol: u32) -> Service {
    let mut service = plugin(id, &[]);
    service.exec = Some(exec);
    service.protocol = protocol;
    service
}

fn send(daemon: &mut Daemon, method: &str, params: Value) -> Receiver<String> {
    let line = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    }))
    .expect("request");
    let (sender, receiver) = mpsc::channel();
    daemon.handle(
        &clock(),
        channel::PendingCall {
            endpoint: channel::Endpoint::Admin,
            line,
            reply: crate::wire::control::channel::Reply::plain(sender),
        },
    );
    receiver
}

/// Dispatch one request as if it arrived on `endpoint`.
fn send_on(
    daemon: &mut Daemon,
    endpoint: channel::Endpoint,
    method: &str,
    params: Value,
) -> serde_json::Value {
    let line = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    }))
    .expect("request");
    let (sender, receiver) = mpsc::channel();
    daemon.handle(
        &clock(),
        channel::PendingCall {
            endpoint,
            line,
            reply: crate::wire::control::channel::Reply::plain(sender),
        },
    );
    let line = receiver.recv().expect("reply");
    serde_json::from_str(&line).expect("response")
}

/// Assert one management error's JSON-RPC code and structured kind.
fn assert_error_kind(response: &Value, code: i32, kind: &str) {
    assert_eq!(response["error"]["code"], code, "{response}");
    assert_eq!(response["error"]["data"]["kind"], kind, "{response}");
}

fn builtin_daemon(receiver: mpsc::Receiver<channel::PendingCall>) -> Daemon {
    Daemon::new(
        Registry::with_builtin(&crate::platform::limits::test_default()),
        receiver,
        None,
    )
}

/// #343: every management request gets its own number, and the error payload
/// carries it next to the category. The serialized form is pinned because it is
/// the control-channel contract the CLI reads.
#[test]
fn error_payloads_carry_the_requests_call_number() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = builtin_daemon(receiver);

    let first = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "svc.status",
        json!({"id": "ghost"}),
    );
    let second = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "svc.status",
        json!({"id": "ghost"}),
    );

    assert_eq!(
        serde_json::to_string(&first["error"]["data"]).expect("json"),
        r#"{"call":1,"kind":"unknown_service"}"#
    );
    assert_eq!(
        serde_json::to_string(&second["error"]["data"]).expect("json"),
        r#"{"call":2,"kind":"unknown_service"}"#
    );
}

/// A malformed frame is answered before dispatch; it is still one call, so it
/// gets a number too.
#[test]
fn a_malformed_frame_is_still_a_numbered_call() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = builtin_daemon(receiver);
    let (sender, replies) = mpsc::channel();
    daemon.handle(
        &clock(),
        channel::PendingCall {
            endpoint: channel::Endpoint::Admin,
            line: "{not json".into(),
            reply: crate::wire::control::channel::Reply::plain(sender),
        },
    );
    let reply: Value =
        serde_json::from_str(&replies.recv().expect("reply")).expect("response");
    assert_eq!(reply["error"]["code"], -32700, "{reply}");
    assert_eq!(
        serde_json::to_string(&reply["error"]["data"]).expect("json"),
        r#"{"call":1}"#
    );
}

/// A successful response has no request metadata: the number exists to name a
/// failure, and adding keys to results would break every consumer's shape.
#[test]
fn a_successful_response_has_no_call_number() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = builtin_daemon(receiver);
    let line = send(&mut daemon, "svcd.ping", json!({}))
        .recv()
        .expect("reply");
    assert_eq!(line, r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#);
}

/// §6: the read endpoint may drive lifecycle, never a write method.
#[test]
fn the_read_endpoint_cannot_run_a_write_method() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(
        Registry::with_builtin(&crate::platform::limits::test_default()),
        receiver,
        None,
    );

    let refused = send_on(
        &mut daemon,
        channel::Endpoint::User,
        "plugin.uninstall",
        json!({ "id": "nope" }),
    );
    assert_eq!(
        refused["error"]["message"], "unauthorized",
        "the read endpoint must not reach a write method: {refused}"
    );
    assert_eq!(
        refused["error"]["data"]["kind"], "internal",
        "the unauthorized gate is a control-plane failure: {refused}"
    );

    let allowed = send_on(&mut daemon, channel::Endpoint::User, "svc.list", json!({}));
    assert!(
        allowed.get("error").is_none(),
        "the read endpoint must still list services: {allowed}"
    );
}

/// The same write method is not refused on the admin endpoint; it fails on
/// the missing plugin instead of on the tier.
#[test]
fn the_admin_endpoint_reaches_a_write_method() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(
        Registry::with_builtin(&crate::platform::limits::test_default()),
        receiver,
        None,
    );

    let response = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "plugin.uninstall",
        json!({ "id": "nope" }),
    );
    assert_ne!(
        response["error"]["message"], "unauthorized",
        "the admin endpoint must get past the tier check: {response}"
    );
}

/// A real clock for the tests that drive blocking start / stop paths.
fn clock() -> Clock {
    Clock::boot()
}

/// A fixed pair for the tests that only read a projection.
fn now() -> Now {
    Now::at(0, 0)
}

/// The loop waits for the nearest timer and never longer than one poll
/// tick: that bound is what keeps shutdown and plugin stdio serviced.
#[test]
fn next_wait_shrinks_for_the_nearest_timer_and_never_exceeds_the_poll_tick() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(
        Registry::with_builtin(&crate::platform::limits::test_default()),
        receiver,
        None,
    );
    daemon.host.rescan_interval_ms = 5_000;
    daemon.host.next_rescan_ms = 5_000;
    daemon.host.temp_cleanup_interval_ms = 60_000;
    daemon.host.next_temp_cleanup_ms = 60_000;

    assert_eq!(
        daemon.host.next_wait(now()),
        crate::supervise::ops::host::POLL_WAIT,
        "no timer inside the tick"
    );

    daemon.host.next_rescan_ms = 20;
    assert_eq!(daemon.host.next_wait(now()), Duration::from_millis(20));

    daemon.host.rescan_interval_ms = 0;
    daemon.host.next_rescan_ms = 0;
    assert_eq!(
        daemon.host.next_wait(now()),
        crate::supervise::ops::host::POLL_WAIT,
        "0 disables the timer"
    );
}

/// A due timer is re-armed exactly one interval ahead, so a late tick does
/// not fire the same timer twice.
#[test]
fn tick_rearms_a_due_rescan_timer_without_double_firing() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(
        Registry::with_builtin(&crate::platform::limits::test_default()),
        receiver,
        None,
    );
    daemon.host.rescan_interval_ms = 5_000;
    daemon.host.next_rescan_ms = 5_000;
    daemon.host.temp_cleanup_interval_ms = 0;
    daemon.host.next_temp_cleanup_ms = 0;

    daemon.host.tick(Now::at(4_999, 0), &clock());
    assert_eq!(daemon.host.next_rescan_ms, 5_000, "not due yet");

    daemon.host.tick(Now::at(5_000, 0), &clock());
    assert_eq!(
        daemon.host.next_rescan_ms, 10_000,
        "due: re-armed one interval ahead"
    );
}

/// The temp cleanup cadence is the loop's other timer; a tick past its
/// deadline re-arms it from the tick's own `now`.
#[test]
fn tick_rearms_the_temp_cleanup_timer_from_its_own_now() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(
        Registry::with_builtin(&crate::platform::limits::test_default()),
        receiver,
        None,
    );
    daemon.host.rescan_interval_ms = 0;
    daemon.host.next_rescan_ms = 0;
    daemon.host.temp_cleanup_interval_ms = 1_000;
    daemon.host.next_temp_cleanup_ms = 0;

    daemon.host.tick(Now::at(500, 0), &clock());
    assert_eq!(
        daemon.host.next_temp_cleanup_ms, 1_500,
        "re-armed from the tick's now, not from a second clock read"
    );
}

fn next_line(daemon: &mut Daemon, receiver: &Receiver<String>) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    let clock = clock();
    loop {
        if let Ok(line) = receiver.try_recv() {
            return serde_json::from_str(&line).expect("line json");
        }
        daemon.host.pump(clock.now());
        assert!(Instant::now() < deadline, "no line");
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait(daemon: &mut Daemon, receiver: &Receiver<String>) -> rpc::Response {
    let deadline = Instant::now() + Duration::from_secs(5);
    let clock = clock();
    loop {
        if let Ok(line) = receiver.try_recv() {
            let value: Value = serde_json::from_str(&line).expect("response json");
            // Progress notifications share the caller's channel; skip
            // them until the final JSON-RPC response arrives. Any other
            // notification is unexpected and must fail the test.
            if let Some(method) = value.get("method").and_then(Value::as_str) {
                if method == "notifications/progress" {
                    continue;
                }
                panic!("unexpected notification {method}: {value}");
            }
            return serde_json::from_value(value).expect("response");
        }
        daemon.host.pump(clock.now());
        assert!(Instant::now() < deadline, "no response");
        thread::sleep(Duration::from_millis(10));
    }
}

fn assert_tool_error(daemon: &mut Daemon, name: &str, expected: &str) {
    let call = send(daemon, "tools/call", json!({"name": name}));
    let result = wait(daemon, &call).result.expect("failure result");
    assert_eq!(result["isError"], true);
    assert_eq!(result["content"][0]["text"], expected);
}

#[test]
fn a_live_plugin_routes_tool_calls_and_fails_them_on_a_crash() {
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = crate::platform::test_support::temp_dir().join(format!(
        "svcd-tools-unit-{}-{}",
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(root.join("logs")).expect("logs");

    let plugin = tool_service(
        "plug",
        Kind::Plugin,
        &["echo", "fail", "hang", "crash"],
        &root,
    );
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(registry(vec![plugin]), receiver, None);
    daemon.host.start_service("plug", &clock()).expect("start");
    assert!(daemon.host.is_live("plug", now()));

    let echo = send(
        &mut daemon,
        "tools/call",
        json!({"name": "plug.echo", "arguments": {}}),
    );
    let response = wait(&mut daemon, &echo);
    assert_eq!(
        response.result.expect("echo result")["content"][0]["text"],
        "pong"
    );

    assert_tool_error(&mut daemon, "plug.fail", "boom");

    let hung = send(&mut daemon, "tools/call", json!({"name": "plug.hang"}));
    let _crashed = send(&mut daemon, "tools/call", json!({"name": "plug.crash"}));
    let result = wait(&mut daemon, &hung).result.expect("crash result");
    assert_eq!(result["isError"], true);
    assert!(!daemon.host.is_live("plug", now()));

    let tools = daemon.tool_list(now());
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "plug.start");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_core_service_with_capabilities_is_callable_through_the_slot_path() {
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = crate::platform::test_support::temp_dir().join(format!(
        "svcd-tools-unit-{}-{}",
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(root.join("logs")).expect("logs");

    let core = tool_service("core", Kind::Core, &["echo"], &root);
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(registry(vec![core]), receiver, None);
    daemon.host.start_service("core", &clock()).expect("start");
    assert!(daemon.host.is_live("core", now()));

    let tools = daemon.tool_list(now());
    assert!(
        tools.iter().any(|tool| tool["name"] == "core.echo"),
        "{tools:?}"
    );
    let echo = send(
        &mut daemon,
        "tools/call",
        json!({"name": "core.echo", "arguments": {}}),
    );
    let response = wait(&mut daemon, &echo);
    assert_eq!(
        response.result.expect("echo result")["content"][0]["text"],
        "pong"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// Covers progress forwarding, failure projection, and child cleanup
// through the same tools/call seam. Timeout/cancellation live in slot.rs:
// tools/call has no per-call timeout or cancel entry.
#[test]
fn an_exec_core_service_forwards_progress_and_cleans_up_its_child() {
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = crate::platform::test_support::temp_dir().join(format!(
        "svcd-tools-unit-{}-{}",
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(root.join("logs")).expect("logs");

    let service = tool_service("proc", Kind::Core, &["progress", "fail"], &root);
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(registry(vec![service]), receiver, None);
    daemon.host.start_service("proc", &clock()).expect("start");
    assert!(daemon.host.is_live("proc", now()));
    let pid = daemon
        .host
        .status_for_id("proc", now())
        .expect("status")
        .summary
        .pid
        .expect("the slot owns a child pid");

    let call = send(
        &mut daemon,
        "tools/call",
        json!({
            "name": "proc.progress",
            "arguments": {},
            "progressToken": "t1"
        }),
    );
    let notification = next_line(&mut daemon, &call);
    assert_eq!(notification["method"], "notifications/progress");
    assert_eq!(notification["params"]["progressToken"], "t1");
    assert_eq!(notification["params"]["progress"], 1);

    let response = wait(&mut daemon, &call);
    assert_eq!(
        response.result.expect("progress result")["content"][0]["text"],
        "done"
    );

    // The same tools/call seam must project a capability failure as a
    // tool error, not only a successful value.
    assert_tool_error(&mut daemon, "proc.fail", "boom");

    daemon.host.stop_service("proc", &clock()).expect("stop");
    let status = daemon.host.status_for_id("proc", now()).expect("status");
    assert_eq!(status.summary.state, State::Stopped);
    assert!(status.summary.pid.is_none());
    #[cfg(unix)]
    assert!(
        !crate::platform::host::process::alive(pid),
        "the child process is still alive after stop"
    );
    // Windows has no portable liveness probe here; the per-service Job
    // Object terminates the tree, and status.pid is the observable seam.
    #[cfg(not(unix))]
    let _ = pid;

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn tool_projection_follows_the_live_state() {
    let registry = registry(vec![plugin("plug", &["echo"])]);
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(registry, receiver, None);

    let tools = daemon.tool_list(now());
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "plug.start");

    daemon
        .host
        .slots
        .get_mut("plug")
        .expect("slot")
        .machine
        .state = State::Running;
    let tools = daemon.tool_list(now());
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(names, vec!["plug.echo", "plug.start"]);

    daemon
        .host
        .slots
        .get_mut("plug")
        .expect("slot")
        .machine
        .state = State::Backoff;
    assert_eq!(daemon.tool_list(now()).len(), 1);
}

#[test]
fn an_eighty_plugin_catalog_is_returned_in_bounded_pages() {
    let services: Vec<Service> = (0..80)
        .map(|index| {
            let mut service = plugin(&format!("p{index}"), &[]);
            let caps = (0..20)
                .map(|cap| capability(&format!("cap{cap}")))
                .collect();
            service.contract = crate::kernel::registry::contract_for(
                &service.id,
                "1.0.0",
                caps,
                crate::kernel::registry::Carrier::Slot,
                &crate::platform::limits::test_default(),
            );
            service
        })
        .collect();
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(registry(services), receiver, None);
    for slot in daemon.host.slots.values_mut() {
        slot.machine.state = State::Running;
    }

    let mut cursor = None;
    let mut total = 0usize;
    loop {
        let page = daemon
            .tools_page(cursor.as_deref(), Some(100), now())
            .expect("page");
        let tools = page["tools"].as_array().expect("tools");
        assert!(!tools.is_empty());
        assert!(
            serde_json::to_vec(tools).expect("serialize").len()
                <= crate::wire::message::ipc::CATALOG_PAGE_MAX
        );
        total += tools.len();
        cursor = page["next"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(total, 80 * 21);
}

#[test]
fn tool_pages_are_stable_and_reject_stale_cursors() {
    let registry = registry(vec![
        plugin("a", &["x"]),
        plugin("b", &["y"]),
        plugin("c", &["z"]),
    ]);
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(registry, receiver, None);

    let first = daemon.tools_page(None, Some(2), now()).expect("first page");
    assert_eq!(first["tools"].as_array().expect("tools").len(), 2);
    assert!(
        first.get("nextCursor").is_none(),
        "the control result must not use the MCP field name: {first}"
    );
    let cursor = first["next"].as_str().expect("next cursor").to_string();

    let second = daemon
        .tools_page(Some(&cursor), Some(2), now())
        .expect("second page");
    assert_eq!(second["tools"].as_array().expect("tools").len(), 1);
    assert!(second.get("next").is_none());

    let stale = daemon
        .tools_page(Some("bm90LWEtY3Vyc29y"), Some(2), now())
        .expect_err("invalid cursor");
    assert_eq!(stale.problem, Problem::Invalid);
    assert!(stale.detail().contains("cursor"), "{stale}");

    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let wrong_version = URL_SAFE_NO_PAD.encode("999:0");
    let stale = daemon
        .tools_page(Some(&wrong_version), Some(2), now())
        .expect_err("version mismatch");
    assert_eq!(stale.problem, Problem::Internal);
    let past_end = URL_SAFE_NO_PAD.encode(format!("{}:99", daemon.catalog.version()));
    let stale = daemon
        .tools_page(Some(&past_end), Some(2), now())
        .expect_err("past end");
    assert_eq!(stale.problem, Problem::Internal);
}

#[test]
fn tools_list_and_unknown_tool_use_the_documented_codes() {
    let registry = registry(vec![plugin("plug", &["echo"])]);
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(registry, receiver, None);

    let listed = send(&mut daemon, "tools/list", json!({}));
    let listed = wait(&mut daemon, &listed);
    assert_eq!(
        listed.result.expect("result")["tools"]
            .as_array()
            .expect("array")
            .len(),
        1
    );

    let unknown = send(&mut daemon, "tools/call", json!({"name": "plug.nope"}));
    let unknown = wait(&mut daemon, &unknown);
    assert_eq!(unknown.error.expect("error").code, -32602);

    let missing = send(&mut daemon, "tools/call", json!({}));
    let missing = wait(&mut daemon, &missing);
    assert_eq!(missing.error.expect("error").code, -32602);
}

#[test]
fn a_known_tool_on_a_dead_plugin_is_a_tool_error() {
    let registry = registry(vec![plugin("plug", &["echo"])]);
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(registry, receiver, None);

    let response = send(&mut daemon, "tools/call", json!({"name": "plug.echo"}));
    let response = wait(&mut daemon, &response);
    let result = response.result.expect("tool result");
    assert_eq!(result["isError"], true);
    assert!(result["content"][0]["text"]
        .as_str()
        .expect("text")
        .contains("plug.start"));
}

#[test]
fn a_capability_ending_in_start_is_not_shadowed_by_the_lifecycle_tool() {
    let registry = registry(vec![plugin("plug", &["foo.start"])]);
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(registry, receiver, None);

    let response = send(&mut daemon, "tools/call", json!({"name": "plug.foo.start"}));
    let response = wait(&mut daemon, &response);

    let result = response.result.expect("tool result");
    assert_eq!(result["isError"], true);
    assert!(result["content"][0]["text"]
        .as_str()
        .expect("text")
        .contains("plug.start"));
}

#[test]
fn core_services_are_not_tools() {
    let registry = registry(vec![service("core", &[])]);
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(registry, receiver, None);

    let response = send(&mut daemon, "tools/call", json!({"name": "core.start"}));
    let response = wait(&mut daemon, &response);
    assert_eq!(response.error.expect("error").code, -32602);
}

#[test]
fn start_refuses_a_failed_dependency() {
    let registry = registry(vec![service("app", &["base"]), service("base", &[])]);
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(registry, receiver, None);
    daemon
        .host
        .slots
        .get_mut("base")
        .expect("base")
        .machine
        .state = State::Failed;

    let error = daemon
        .host
        .start_service("app", &clock())
        .expect_err("failed dependency");

    assert_eq!(error.problem, Problem::Internal);
    assert!(
        error.detail().contains("dependency base is failed"),
        "{}",
        error
    );
}

fn two_owners(id: &str) -> Service {
    Service {
        impl_ref: Some(crate::kernel::contract::ImplRef {
            tag: "builtin".into(),
            binding: json!({}),
        }),
        ..service(id, &[])
    }
}

#[test]
fn a_reload_rejects_two_owners_and_leaves_the_live_service_and_slot_alone() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(registry(vec![service("live", &[])]), receiver, None);

    let mut desired = BTreeMap::new();
    desired.insert("bad".to_string(), two_owners("bad"));
    let mut conflicting_live = two_owners("live");
    conflicting_live.version = "9.9.9".into();
    conflicting_live.contract = crate::kernel::registry::contract_for(
        "live",
        "9.9.9",
        conflicting_live.contract.surface().to_vec(),
        crate::kernel::registry::Carrier::Launcher,
        &crate::platform::limits::test_default(),
    );
    desired.insert("live".to_string(), conflicting_live);

    let (added, updated) = daemon.host.apply_definitions(desired);

    assert!(
        added.is_empty(),
        "a rejected new service must not be reported as added"
    );
    assert!(
        updated.is_empty(),
        "a rejected update must not be reported as updated"
    );
    assert!(daemon.host.registry.get("bad").is_none());
    assert!(
        !daemon.host.slots.contains_key("bad"),
        "a rejected new service must not leave a slot behind"
    );
    let live = daemon.host.registry.get("live").expect("live service");
    assert_eq!(
        live.version, "1.0.0",
        "a rejected update must not reach the registry"
    );
    assert_eq!(live.carrier(), Ok(crate::kernel::registry::Carrier::Slot));
    assert!(
        daemon.host.slots.contains_key("live"),
        "the live slot must survive a rejected update"
    );
}

#[test]
fn the_assembly_point_keeps_one_carrier_per_service() {
    let mut registry = Registry::with_builtin(&crate::platform::limits::test_default());
    registry.insert(service("core", &[])).expect("core");
    let (_sender, receiver) = mpsc::channel();
    let daemon = Daemon::new(registry, receiver, None);

    assert!(
        daemon.host.slots.contains_key("core"),
        "an exec service is owned by the supervised slot"
    );
    assert!(
        !daemon.host.slots.contains_key("svc"),
        "the builtin is launcher-backed and must never get a slot"
    );
    assert_eq!(
        daemon.host.registry.get("svc").map(Service::carrier),
        Some(Ok(crate::kernel::registry::Carrier::Launcher))
    );
    assert_eq!(
        daemon.host.registry.get("core").map(Service::carrier),
        Some(Ok(crate::kernel::registry::Carrier::Slot))
    );
}

/// One fact, one `since`: the runtime record anchors the same [`Live`]
/// facet, with the same guarded conversion, as `Status::live`. A slot that
/// never started has `since_ms == 0`, which must stay absent on disk
/// instead of becoming a plausible-looking but wrong epoch second.
///
/// [`Live`]: crate::kernel::projection::Live
#[test]
fn the_runtime_record_anchors_since_like_the_status_projection() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(
        registry(vec![service("never", &[]), service("started", &[])]),
        receiver,
        None,
    );
    let now = Now::at(1_700_000_010_000, 1_700_000_010);
    daemon
        .host
        .slots
        .get_mut("never")
        .expect("never slot")
        .machine
        .state = State::Running;
    let started = &mut daemon
        .host
        .slots
        .get_mut("started")
        .expect("started slot")
        .machine;
    started.state = State::Running;
    started.since_ms = 1_700_000_000_000;

    daemon.host.persist_states(now);

    let never = runtime::load("never", &daemon.config.paths).expect("never record");
    assert_eq!(never.since, None, "a never-started slot must not anchor 0");
    // `load` proves the semantic absence; the raw file proves the key is
    // omitted instead of being written as `"since": null`.
    let raw = std::fs::read(daemon.config.paths.run_dir.join("never.json")).expect("never record");
    let raw: Value = serde_json::from_slice(&raw).expect("never record json");
    assert!(
        raw.get("since").is_none(),
        "a never-started slot must omit the since key: {raw}"
    );

    let recorded = runtime::load("started", &daemon.config.paths).expect("started record");
    let service = daemon
        .host
        .registry
        .get("started")
        .expect("started service");
    let status = Status::live(service, &daemon.host.slots["started"].machine.live(), now);
    assert_eq!(
        recorded.since, status.summary.since,
        "the record and the status projection must agree"
    );
    assert_eq!(recorded.since, Some(1_700_000_000));
}

/// Adoption must not invent a start. A record without `since` keeps
/// `since_ms == 0`, which `Status::live` projects as `since: null`; a
/// record with a concrete `since` still anchors back to the same second.
#[test]
fn adopting_a_record_without_since_reports_no_since() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(
        registry(vec![service("never", &[]), service("started", &[])]),
        receiver,
        None,
    );
    let now = Now::at(1_700_000_010_000, 1_700_000_010);
    for id in ["never", "started"] {
        daemon
            .host
            .slots
            .get_mut(id)
            .expect("slot")
            .set_remain_after_exit(true);
        let since = (id == "started").then_some(1_700_000_000);
        runtime::save(
            id,
            &Record {
                boot: None,
                state: State::Running,
                pid: None,
                starttime: None,
                since,
                ready_since: None,
                failures: 0,
                last_exit: Some(0),
            },
            &daemon.config.paths,
        )
        .expect("save record");
    }

    daemon.host.adopt_runtime(now);

    let never = Status::live(
        daemon.host.registry.get("never").expect("never service"),
        &daemon.host.slots["never"].machine.live(),
        now,
    );
    assert_eq!(
        never.summary.since, None,
        "a record without since must stay absent"
    );
    let started = Status::live(
        daemon
            .host
            .registry
            .get("started")
            .expect("started service"),
        &daemon.host.slots["started"].machine.live(),
        now,
    );
    assert_eq!(started.summary.since, Some(1_700_000_000));
    assert_eq!(
        daemon.host.slots["started"].machine.since_ms,
        1_700_000_000_000
    );
}

#[test]
fn the_builtin_svc_is_projected_and_called_through_launchers() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(
        Registry::with_builtin(&crate::platform::limits::test_default()),
        receiver,
        None,
    );

    let listed = send(&mut daemon, "tools/list", json!({}));
    let listed = wait(&mut daemon, &listed);
    let names: Vec<&str> = listed.result.as_ref().expect("list result")["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"svc.version"), "{names:?}");
    assert!(names.contains(&"svc.health"), "{names:?}");
    assert!(!names.contains(&"svc.start"), "{names:?}");

    let version = send(
        &mut daemon,
        "tools/call",
        json!({"name": "svc.version", "arguments": {}}),
    );
    let version = wait(&mut daemon, &version);
    let result = version.result.expect("version result");
    assert_eq!(result["isError"], false);
    let text = result["content"][0]["text"].as_str().expect("version text");
    let value: Value = serde_json::from_str(text).expect("version json");
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));

    let health = send(
        &mut daemon,
        "tools/call",
        json!({"name": "svc.health", "arguments": {}}),
    );
    let health = wait(&mut daemon, &health);
    let result = health.result.expect("health result");
    assert_eq!(result["isError"], false);
    let text = result["content"][0]["text"].as_str().expect("health text");
    let value: Value = serde_json::from_str(text).expect("health json");
    assert_eq!(value["state"], "ready");
}

#[test]
fn a_closed_channel_after_shutdown_is_not_reported_as_unreachable() {
    for kind in [
        std::io::ErrorKind::UnexpectedEof,
        std::io::ErrorKind::ConnectionReset,
        std::io::ErrorKind::ConnectionAborted,
        std::io::ErrorKind::BrokenPipe,
    ] {
        assert!(shutdown_reply_lost(&std::io::Error::from(kind)), "{kind:?}");
    }
    // A pipe that was never available is a real "not running" failure.
    assert!(!shutdown_reply_lost(&std::io::Error::from(
        std::io::ErrorKind::TimedOut
    )));
    assert!(!shutdown_reply_lost(&std::io::Error::from(
        std::io::ErrorKind::NotFound
    )));
}

#[test]
fn lifecycle_management_errors_carry_their_problem_kind() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(
        Registry::with_builtin(&crate::platform::limits::test_default()),
        receiver,
        None,
    );

    let started = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "svc.start",
        json!({"id": "nope"}),
    );
    assert_error_kind(&started, -32003, "unknown_service");

    let stopped = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "svc.stop",
        json!({"id": "nope"}),
    );
    assert_error_kind(&stopped, -32004, "unknown_service");

    let restarted = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "svc.restart",
        json!({"id": "nope"}),
    );
    assert_error_kind(&restarted, -32010, "unknown_service");

    let status = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "svc.status",
        json!({"id": "nope"}),
    );
    assert_error_kind(&status, -32001, "unknown_service");

    let enabled = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "svc.enable",
        json!({"id": "svc"}),
    );
    assert_error_kind(&enabled, -32005, "conflict");

    let disabled = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "svc.disable",
        json!({"id": "svc"}),
    );
    assert_error_kind(&disabled, -32005, "conflict");
}

#[test]
fn plugin_management_errors_carry_their_problem_kind() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(
        Registry::with_builtin(&crate::platform::limits::test_default()),
        receiver,
        None,
    );

    let missing_path = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "plugin.install",
        json!({}),
    );
    assert_error_kind(&missing_path, -32006, "internal");

    let missing_staged_path = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "plugin.install_staged",
        json!({}),
    );
    assert_error_kind(&missing_staged_path, -32006, "internal");

    let unknown_strategy = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "plugin.install",
        json!({"path": "ignored", "strategy": "bogus"}),
    );
    assert_error_kind(&unknown_strategy, -32006, "unknown_strategy");

    let unknown_plugin = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "plugin.uninstall",
        json!({"id": "nope"}),
    );
    assert_error_kind(&unknown_plugin, -32007, "unknown_plugin");

    let missing_id = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "plugin.uninstall",
        json!({}),
    );
    assert_error_kind(&missing_id, -32007, "internal");
}

#[test]
fn auth_and_parameter_errors_carry_their_problem_kind() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(
        Registry::with_builtin(&crate::platform::limits::test_default()),
        receiver,
        None,
    );

    let auth = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "auth.status",
        json!({}),
    );
    assert_error_kind(&auth, -32008, "internal");

    let password = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "auth.passwd",
        json!({"password": ""}),
    );
    assert_error_kind(&password, -32008, "internal");

    let missing_id = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "svc.start",
        json!({}),
    );
    assert_error_kind(&missing_id, -32602, "invalid");

    let cursor = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "tools/list",
        json!({"cursor": "bm90LWEtY3Vyc29y"}),
    );
    assert_error_kind(&cursor, -32602, "invalid");

    let unknown_method = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "no.such.method",
        json!({}),
    );
    assert_error_kind(&unknown_method, -32601, "invalid");
}

#[test]
fn uninstalling_a_core_service_is_not_a_plugin() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(registry(vec![service("core", &[])]), receiver, None);

    let response = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "plugin.uninstall",
        json!({"id": "core"}),
    );
    assert_error_kind(&response, -32007, "not_a_plugin");
}

#[test]
fn a_start_with_a_missing_dependency_reports_missing_dependency() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(registry(vec![service("app", &["missing"])]), receiver, None);

    let response = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "svc.start",
        json!({"id": "app"}),
    );
    assert_error_kind(&response, -32003, "missing_dependency");
}

#[test]
fn installing_a_package_with_the_reserved_id_is_invalid() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(
        Registry::with_builtin(&crate::platform::limits::test_default()),
        receiver,
        None,
    );
    let dir = crate::platform::test_support::TempDir::new();
    let path = dir.path().join("svc.spk");
    std::fs::write(
        &path,
        crate::manage::test_support::test_signed_package("svc", crate::platform::target()),
    )
    .expect("write package");

    let response = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "plugin.install",
        json!({"path": path.to_string_lossy(), "strategy": "replace"}),
    );
    assert_error_kind(&response, -32006, "invalid");
}

#[test]
fn installing_a_package_for_another_platform_reports_unsupported_platform() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(
        Registry::with_builtin(&crate::platform::limits::test_default()),
        receiver,
        None,
    );
    let dir = crate::platform::test_support::TempDir::new();
    let path = dir.path().join("other.spk");
    std::fs::write(
        &path,
        crate::manage::test_support::test_signed_package("other", "other-triple"),
    )
    .expect("write package");

    let response = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "plugin.install",
        json!({"path": path.to_string_lossy(), "strategy": "replace"}),
    );
    assert_error_kind(&response, -32006, "unsupported_platform");
}

#[test]
fn a_start_layer_past_its_deadline_projects_timeout_kind() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = Daemon::new(registry(vec![service("stuck", &[])]), receiver, None);
    // Force a start that is in flight but never becomes ready; the deadline
    // is already past, so the layer must report a timeout without sleeping.
    daemon
        .host
        .slots
        .get_mut("stuck")
        .expect("slot")
        .machine
        .state = State::Starting;

    let error = daemon
        .host
        .start_layer(&["stuck".into()], 0, &clock())
        .expect_err("past deadline");
    assert_eq!(error.problem, Problem::Timeout);

    let response = status_response(Some(json!(7)), -32003, Err(error));
    let error = response.error.expect("error");
    assert_eq!(error.code, -32003);
    assert_eq!(error.data.expect("data")["kind"], "timeout");
}

/// Let the supervisor observe an exit that already happened: the unit
/// harness drives `pump` itself instead of running the event loop.
fn wait_for_stopped(daemon: &mut Daemon, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        daemon.host.pump(clock().now());
        if daemon.host.slots[id].machine.state == State::Stopped {
            return;
        }
        assert!(Instant::now() < deadline, "{id} never stopped");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A remain-after-exit one-shot ends as `running` with no pid; wait for the
/// supervisor to settle that state.
fn wait_until_remaining(daemon: &mut Daemon, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        daemon.host.pump(clock().now());
        let machine = &daemon.host.slots[id].machine;
        if machine.state == State::Running && machine.pid.is_none() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{id} never reached the remain state: {:?} pid {:?}",
            machine.state,
            machine.pid
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_peer_that_dies_while_the_layer_is_still_starting_fails_the_layer() {
    // #183 in its deterministic shape: the dead peer reaches ready and
    // exits while a second service keeps the wait open, so a later pass
    // observes `Stopped` and must refuse the start instead of reporting
    // success. Without the slow peer the layer would settle on `Running`
    // in the same millisecond the peer left.
    let (_sender, receiver) = mpsc::channel();
    let dir = crate::platform::test_support::TempDir::new();
    let dead = peer_plugin("dead", ping_once_agent(dir.path()), 1);
    let slow = peer_plugin("slow", slow_ping_agent(dir.path()), 1);
    let mut daemon = Daemon::new(registry(vec![dead, slow]), receiver, None);

    let error = daemon
        .host
        .start_layer(
            &["dead".into(), "slow".into()],
            clock().now().mono_ms + 5_000,
            &clock(),
        )
        .expect_err("the dead peer must fail the layer");
    assert_eq!(error.problem, Problem::Internal);
    assert!(error.detail().contains("exited during start"), "{error}");
    assert!(error.detail().contains("(exit code 0)"), "{error}");

    let status = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "svc.status",
        json!({"id": "dead"}),
    );
    assert_eq!(status["result"]["summary"]["state"], "stopped", "{status}");
}

#[test]
fn a_raw_peer_that_dies_while_the_layer_is_still_starting_loses_the_channel() {
    // protocol 3 forces `exec.ready=exec`, so a single-service start settles
    // on `Running` at spawn. The slow peer is what lets a later pass observe
    // the raw peer's clean exit, exactly like the notify case.
    let (_sender, receiver) = mpsc::channel();
    let dir = crate::platform::test_support::TempDir::new();
    let raw = peer_plugin("raw", exit_zero_exec(), 3);
    let slow = peer_plugin("slow", slow_ping_agent(dir.path()), 1);
    let mut daemon = Daemon::new(registry(vec![raw, slow]), receiver, None);
    let hub = daemon.host.factory.raw_hub();
    daemon.host.set_raw_hub(hub);

    let error = daemon
        .host
        .start_layer(
            &["raw".into(), "slow".into()],
            clock().now().mono_ms + 5_000,
            &clock(),
        )
        .expect_err("the raw peer must fail the layer");
    assert_eq!(error.problem, Problem::Internal);
    assert!(error.detail().contains("exited during start"), "{error}");
    assert!(error.detail().contains("(exit code 0)"), "{error}");
}

#[test]
fn a_fire_and_forget_one_shot_still_settles_a_start() {
    // Regression for #159: an exec one-shot owes no channel, so a clean
    // exit right after spawn is a finished start.
    let (_sender, receiver) = mpsc::channel();
    let mut service = service("once", &[]);
    service.exec = Some(exit_zero_exec());
    let mut daemon = Daemon::new(registry(vec![service]), receiver, None);

    let response = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "svc.start",
        json!({"id": "once"}),
    );
    assert!(response.get("error").is_none(), "{response}");

    wait_for_stopped(&mut daemon, "once");
    let status = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "svc.status",
        json!({"id": "once"}),
    );
    assert_eq!(status["result"]["summary"]["state"], "stopped", "{status}");
}

#[test]
fn a_remain_after_exit_one_shot_keeps_running_after_start() {
    // The other axis: "the effect is still there". The process exits, the
    // service stays running with no pid, and the start still succeeds.
    let (_sender, receiver) = mpsc::channel();
    let mut service = service("keep", &[]);
    service.exec = Some(exit_zero_exec());
    service.remain_after_exit = true;
    let mut daemon = Daemon::new(registry(vec![service]), receiver, None);

    let response = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "svc.start",
        json!({"id": "keep"}),
    );
    assert!(response.get("error").is_none(), "{response}");

    wait_until_remaining(&mut daemon, "keep");
    let status = send_on(
        &mut daemon,
        channel::Endpoint::Admin,
        "svc.status",
        json!({"id": "keep"}),
    );
    assert_eq!(status["result"]["summary"]["state"], "running", "{status}");
    assert!(status["result"]["summary"]["pid"].is_null(), "{status}");
}

mod plugins;
