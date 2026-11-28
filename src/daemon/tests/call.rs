//! 调用号的测试：分配、盖章（`error.data.call`）与「成功响应不带号」。
//! 从 `daemon/tests.rs` 拆出来，保持那个文件在登记的 size baseline 内 (#343)。

use super::*;

use serde_json::json;

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
    let reply: Value = serde_json::from_str(&replies.recv().expect("reply")).expect("response");
    assert_eq!(reply["error"]["code"], -32700, "{reply}");
    assert_eq!(
        serde_json::to_string(&reply["error"]["data"]).expect("json"),
        r#"{"call":1}"#
    );
}

/// The MCP surface gets the number too: a `tools/call` the daemon refuses
/// answers with a JSON-RPC error that names the request (#347).
#[test]
fn a_failed_tool_call_names_the_request() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = builtin_daemon(receiver);
    let line = send(&mut daemon, "tools/call", json!({"name": "ghost.cap"}))
        .recv()
        .expect("reply");
    let reply: Value = serde_json::from_str(&line).expect("response");
    assert_eq!(reply["error"]["code"], -32602, "{reply}");
    assert_eq!(
        serde_json::to_string(&reply["error"]["data"]).expect("json"),
        r#"{"call":1}"#
    );
}

/// A successful tool call keeps the result shape it had before the number
/// existed (ADR-0033).
#[test]
fn a_successful_tool_call_carries_no_call_number() {
    let (_sender, receiver) = mpsc::channel();
    let mut daemon = builtin_daemon(receiver);
    let line = send(
        &mut daemon,
        "tools/call",
        json!({"name": "svc.version", "arguments": {}}),
    )
    .recv()
    .expect("reply");
    let reply: Value = serde_json::from_str(&line).expect("response");
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    assert!(reply["result"].get("_meta").is_none(), "{reply}");
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
