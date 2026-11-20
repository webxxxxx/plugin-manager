//! Blocking socket helpers shared by the thread loop and the MCP SSE fallback.
//!
//! These are the transport primitives the thread-per-connection driver and the
//! blocking MCP fallback both need; keeping them here breaks the old
//! `thread_loop` ↔ `mcp::fallback` cycle.

use std::io::{Read, Write};
use std::net::TcpStream;

use crate::server::http::model as http;

/// Write one rendered response. HEAD keeps the head (including GET's
/// `Content-Length`) and gets no body, so a keep-alive connection cannot read
/// the omitted body as the start of the next response.
pub(crate) fn write_http(
    stream: &mut TcpStream,
    response: &[u8],
    head_only: bool,
) -> std::io::Result<()> {
    if head_only {
        stream.write_all(&http::strip_body(response.to_vec()))
    } else {
        stream.write_all(response)
    }
}

/// Whether the SSE client has gone away (#135).
///
/// `peek` on a blocking socket waits for data, so a readiness check with a
/// zero timeout must come first; only a readable socket is peeked. A graceful
/// FIN is `Ok(0)`, and an error means the peer reset the connection: both mean
/// the SSE response can no longer be delivered. This is the Windows equivalent
/// of the Unix stream driver's cancel token, not a permanent exemption.
pub(crate) fn client_gone(stream: &TcpStream) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{
            select, FD_SET, SOCKET, SOCKET_ERROR, TIMEVAL,
        };

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
    #[cfg(not(windows))]
    {
        // The Unix server loop owns disconnect detection; this body only
        // exists so the shared driver compiles. A FIN shows up as a
        // zero-length peek.
        let mut byte = [0u8; 1];
        matches!(stream.peek(&mut byte), Ok(0))
    }
}
