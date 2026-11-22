//! Windows 的 MCP 回退 SSE：命名管道控制流不能并进 wait set，所以一条 SSE
//! 在交接后的线程上写完；预算、断开探测与最终帧的丢弃顺序与 Unix 的 stream
//! handler 一致。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::json;

use crate::server::mcp::transport;
use crate::wire::control::channel;
use crate::wire::message::rpc;

/// The SSE body, for a caller that already holds the long-lived permit.
///
/// The poll loop reserves the slot before it decides where the response runs,
/// so the Windows hand-off must not acquire a second permit.
pub(crate) fn serve_mcp_sse_admitted(
    stream: &mut TcpStream,
    call: transport::StreamingCall,
    config: &Path,
    limits: &crate::platform::limits::Limits,
    _permit: Option<crate::server::http::budget::StreamPermit>,
) -> std::io::Result<()> {
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

/// Run the admitted SSE body on its own thread after the poll loop hands the
/// socket over. Windows has no wait-set integration for the named-pipe control
/// stream, so the blocking response cannot run on the loop thread; one
/// dedicated thread is bounded by `max_streams`.
#[cfg(windows)]
pub(crate) fn spawn_mcp_sse(
    stream: &TcpStream,
    call: transport::StreamingCall,
    config: Arc<crate::platform::config::Config>,
    permit: Option<crate::server::http::budget::StreamPermit>,
) -> std::io::Result<()> {
    let mut owned = stream.try_clone()?;
    let limits = config.limits.clone();
    std::thread::Builder::new()
        .name("svcd-mcp-sse".into())
        .stack_size(crate::platform::host::threads::transfer_stack(&limits))
        .spawn(move || {
            // The loop's socket is non-blocking and the SSE writer needs a
            // blocking one. Switch the shared socket only after the thread
            // exists: a failed spawn would otherwise leave the loop's socket
            // blocking while the loop still polls it.
            let _ = owned.set_nonblocking(false);
            let _ = serve_mcp_sse_admitted(
                &mut owned,
                call,
                config.paths.control_dir(),
                &limits,
                permit,
            );
        })?;
    Ok(())
}

fn write_sse_line(stream: &mut TcpStream, line: &str) -> std::io::Result<()> {
    stream.write_all(&crate::wire::sse::SseEvent::new("message", line).chunked())?;
    stream.flush()
}

/// Whether the SSE client has gone away (#135).
///
/// `peek` on a blocking socket waits for data, so a readiness check with a
/// zero timeout must come first; only a readable socket is peeked. A graceful
/// FIN is `Ok(0)`, and an error means the peer reset the connection: both mean
/// the SSE response can no longer be delivered.
fn client_gone(stream: &TcpStream) -> bool {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{select, FD_SET, SOCKET, SOCKET_ERROR, TIMEVAL};

    // SAFETY: `readable` is a local, fully initialized-by-zero FD_SET; the
    // socket comes from the live connection and is only read by `select`.
    let mut readable: FD_SET = unsafe { std::mem::zeroed() };
    readable.fd_count = 1;
    readable.fd_array[0] = stream.as_raw_socket() as SOCKET;
    let timeout = TIMEVAL {
        tv_sec: 0,
        tv_usec: 0,
    };
    // SAFETY: Both the FD_SET and timeout pointers are valid for the call;
    // `select` only writes into the local FD_SET.
    let ready = unsafe {
        select(
            0,
            &mut readable,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &timeout,
        )
    };
    if ready == SOCKET_ERROR {
        // A transient winsock failure is not proof that the client left;
        // cancelling on it would truncate a live SSE body. A real reset still
        // surfaces as readable + peek error, and a write failure flips the
        // caller's abandonment flag.
        return false;
    }
    if ready == 0 {
        return false;
    }
    // Readable now, so this peek cannot block. A FIN shows up as `Ok(0)`.
    let mut byte = [0u8; 1];
    match stream.peek(&mut byte) {
        Ok(0) => true,
        Ok(_) => {
            // Pipelined or stray bytes on a `Connection: close` response can
            // never be served; drain them so a FIN queued behind them becomes
            // visible instead of leaving the socket readable forever.
            let mut junk = [0u8; 64];
            let mut socket = stream;
            let _ = socket.read(&mut junk);
            false
        }
        Err(_) => true,
    }
}
