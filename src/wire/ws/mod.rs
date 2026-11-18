//! WebSocket: the RFC 6455 shape of the bytes, with no transport.
//!
//! - `frame.rs` — parse one client frame, encode one server frame;
//! - `accept.rs` — validate the opening handshake and compute
//!   `Sec-WebSocket-Accept`.
//!
//! Moved out of `server/http` and `face/uichannel` (#206): the layers that
//! carry a socket or a stream handler must not own the frame format (§5).

pub(crate) mod accept;
pub(crate) mod frame;
