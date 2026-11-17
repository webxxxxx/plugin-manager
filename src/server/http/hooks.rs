//! Hooks the composition root injects into a connection driver.
//!
//! They live here rather than in `server.rs` (#139) because the types have to
//! exist on every host: the thread driver is built by the composition root on
//! hosts that have no poll loop, and it uses the same hooks.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use crate::http;
use crate::lan::Lan;
use crate::server::http::engine::HttpRequest;
use crate::server::http::{BodyPolicy, Exchange};

/// The remaining server-owned routes (buffered MCP, plugin UI rpc and the blob
/// session metadata). Composition injects this closure so the server stays
/// below the face layer; it runs on the blocking pool, never on the loop.
pub(crate) type FallbackHandler =
    Arc<dyn Fn(&HttpRequest, &Arc<Mutex<Lan>>) -> Vec<u8> + Send + Sync>;

/// The pre-body stream hooks. Composition owns the concrete faces; the server
/// only asks each hook whether it claims the head and, if so, for the handler.
pub(crate) trait PreBodyRoutes: Send + Sync {
    fn plugin_ui(
        &self,
        head: &http::Head,
        peer: IpAddr,
        lan: &Arc<Mutex<Lan>>,
        config: &std::path::Path,
        raw_hub: Arc<crate::rawhub::RawHub>,
    ) -> Option<Result<Exchange, Vec<u8>>>;
    fn package_upload(
        &self,
        head: &http::Head,
        lan: &Arc<Mutex<Lan>>,
        config: &std::path::Path,
    ) -> Option<Result<Exchange, Vec<u8>>>;
    fn blob_upload(
        &self,
        head: &http::Head,
        lan: &Arc<Mutex<Lan>>,
    ) -> Option<Result<Exchange, Vec<u8>>>;
    /// Body policy for the server-owned routes the face leaves alone (MCP,
    /// blob/file session metadata). The composition root owns this last legacy
    /// table; a new endpoint declares its policy in `Face::route` instead.
    fn body_policy(&self, head: &http::Head) -> BodyPolicy;
}
