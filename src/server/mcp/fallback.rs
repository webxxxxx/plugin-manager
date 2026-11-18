//! MCP 在线程驱动上的回退 SSE：没有 stream driver 时，一条 SSE 在连接线程上
//! 写完；预算、断开探测与最终帧的丢弃顺序与 Unix 的 stream handler 一致。

use std::io::Write;
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::json;

use crate::server::http::threads::{client_gone, write_http};
use crate::server::mcp::transport;
use crate::wire::control::channel;
use crate::wire::message::rpc;

pub(crate) fn serve_mcp_sse(
    stream: &mut TcpStream,
    call: transport::StreamingCall,
    config: &Path,
    head_only: bool,
    limits: &crate::platform::limits::Limits,
) -> std::io::Result<()> {
    // The Windows fallback has no stream driver, but the long-lived budget is
    // still server policy: reserve it before the head and before the plugin
    // call starts. The permit lives until this blocking response ends, and a
    // client disconnect ends it below (#135).
    let _permit = match crate::server::http::budget::StreamBudget::global().acquire(
        crate::server::http::StreamClass::LongLived,
        limits.max_streams,
    ) {
        Ok(permit) => permit,
        Err(_) => {
            write_http(
                stream,
                &crate::server::http::stream_busy_response(),
                head_only,
            )?;
            return Ok(());
        }
    };
    stream.write_all(
        b"HTTP/1.1 200 OK\r\n\
          Content-Type: text/event-stream\r\n\
          Cache-Control: no-cache\r\n\
          Connection: close\r\n\
          Transfer-Encoding: chunked\r\n\r\n",
    )?;
    // The control read runs on this connection's thread, so it must be able to
    // see the client leave. A cloned handle gives the poll a read-only probe
    // while the response keeps writing to the original socket.
    let probe = match stream.try_clone() {
        Ok(probe) => probe,
        Err(_) => {
            let response = rpc::err(call.id, -32603, "cannot observe the client connection");
            let _ = write_sse_line(stream, &rpc::to_line(&response));
            let _ = stream.write_all(b"0\r\n\r\n");
            let _ = stream.flush();
            return Ok(());
        }
    };
    // `abandoned` means the SSE response can no longer be delivered: the
    // client left or a write already failed. The poll closure sets it and the
    // notification sink does the same, so neither path has to inspect an
    // `io::ErrorKind` to know the connection is gone.
    let abandoned = Arc::new(AtomicBool::new(false));
    let left = abandoned.clone();
    let polled = abandoned.clone();
    let response = channel::request_stream(
        config,
        "registry.call",
        json!({
            "name": call.name,
            "arguments": call.arguments,
            "progressToken": call.progress_token,
        }),
        // The same no-progress bound the Unix stream driver applies to an SSE
        // response: a hung supervisor cannot pin this thread forever.
        Some(limits.transfer_timeout),
        move || {
            let gone = left.load(Ordering::Acquire) || client_gone(&probe);
            if gone {
                polled.store(true, Ordering::Release);
            }
            gone
        },
        |notification| {
            if !abandoned.load(Ordering::Acquire) && write_sse_line(stream, notification).is_err() {
                abandoned.store(true, Ordering::Release);
            }
        },
    );
    let response = match response {
        Ok(response) => Some(crate::server::mcp::result::finish_modern_tool_call(
            call.id, response,
        )),
        // The client left, or a write failed: this response can no longer be
        // delivered. Dropping the control socket discards a late final frame
        // instead of pooling it for the next request.
        Err(_) if abandoned.load(Ordering::Acquire) => None,
        Err(error) => Some(rpc::err(
            call.id,
            -32603,
            &format!("svcd unreachable: {}", error),
        )),
    };
    if !abandoned.load(Ordering::Acquire) {
        if let Some(response) = &response {
            let _ = write_sse_line(stream, &rpc::to_line(response));
        }
        let _ = stream.write_all(b"0\r\n\r\n");
        let _ = stream.flush();
    }
    Ok(())
}

fn write_sse_line(stream: &mut TcpStream, line: &str) -> std::io::Result<()> {
    let payload = format!("event: message\ndata: {}\n\n", line);
    stream.write_all(format!("{:x}\r\n", payload.len()).as_bytes())?;
    stream.write_all(payload.as_bytes())?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}
