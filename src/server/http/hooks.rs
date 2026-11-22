//! Hooks the composition root injects into a connection driver.
//!
//! They live here rather than in `server.rs` (#139) because the composition
//! root builds them before any loop exists, and the unix control channel uses
//! the same hooks.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use crate::server::auth::lan::Lan;
use crate::server::http::engine::HttpRequest;
use crate::server::http::model as http;
use crate::server::http::{BodyPolicy, Exchange};

/// The remaining server-owned routes (buffered MCP, plugin UI rpc and the blob
/// session metadata). Composition injects this closure so the server stays
/// below the face layer; it runs on the blocking pool, never on the loop.
pub(crate) type FallbackHandler =
    Arc<dyn Fn(&HttpRequest, &Arc<Mutex<Lan>>) -> Vec<u8> + Send + Sync>;

/// One pre-body claim: the face's answer, so the loop can run it without
/// learning which face answered.
pub(crate) struct PreBodyClaim {
    pub(crate) exchange: Result<Exchange, Vec<u8>>,
}

/// The pre-body stream hooks. Composition owns the concrete faces; the server
/// only asks the one ordered claim hook and, if it says no, the remaining
/// server-owned routes.
pub(crate) trait PreBodyRoutes: Send + Sync {
    /// The first pre-body face that claims this head.
    ///
    /// The order (plugin UI, package upload, blob upload) is endpoint
    /// precedence owned by the face layer; a driver must not re-spell it.
    fn claim(
        &self,
        head: &http::Head,
        peer: IpAddr,
        lan: &Arc<Mutex<Lan>>,
        raw_hub: Arc<crate::wire::contract::rawhub::RawHub>,
    ) -> Option<PreBodyClaim>;

    /// The `/mcp` stream plan, if this request is a `tools/call` that asked
    /// for SSE. Both the poll loop and the thread loop ask this one hook, so
    /// the path, origin, bearer and `_meta` checks are not written twice.
    fn mcp(
        &self,
        request: &HttpRequest,
        lan: &Arc<Mutex<Lan>>,
    ) -> Option<Result<crate::server::mcp::transport::StreamingCall, Vec<u8>>>;

    /// Body policy for the server-owned routes the face leaves alone (MCP,
    /// blob/file session metadata). The composition root owns this last legacy
    /// table; a new endpoint declares its policy in `Face::route` instead.
    fn body_policy(&self, head: &http::Head) -> BodyPolicy;
}
