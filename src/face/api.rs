//! `/api/v1` face: URL → capability projection.
//!
//! #113 moved only `GET /api/v1/version` through the S4 seam. #116 adds the
//! first blocking route: `GET /api/v1/services` round-trips the control
//! channel, so the server runs it on the blocking pool. The rest of the
//! management surface still lives in the legacy API module until #117.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::server::http::{BodyPolicy, Completion, Exchange, Face, Request, Response, Route};

use super::blob;

/// The management API face.
pub struct ApiFace {
    config: PathBuf,
}

impl ApiFace {
    pub fn new(config: impl Into<PathBuf>) -> ApiFace {
        ApiFace {
            config: config.into(),
        }
    }

    /// Project `svc.list` into the summaries the console consumes. The daemon
    /// RPC runs on the blocking pool; this function must not be called from the
    /// server loop.
    fn list_services(&self) -> Response {
        match crate::api::rpc_result(&self.config, "svc.list", json!({})) {
            Ok(value) => {
                let services: Vec<Value> = value
                    .get("services")
                    .and_then(Value::as_array)
                    .map(|services| services.iter().filter_map(project_service).collect())
                    .unwrap_or_default();
                Response::json(200, &json!({ "services": services }))
            }
            Err((status, code, message)) => Response::json(
                status,
                &json!({ "error": { "code": code, "message": message } }),
            ),
        }
    }
}

fn path_only(path: &str) -> &str {
    path.split_once('?').map(|(path, _)| path).unwrap_or(path)
}

fn is_version(method: &str, path: &str) -> bool {
    // Mirror the legacy router's `trim_matches('/')` so `/api/v1/version/`
    // keeps resolving exactly as it did before the face existed.
    method == "GET" && path_only(path).trim_matches('/') == "api/v1/version"
}

fn is_services(method: &str, path: &str) -> bool {
    method == "GET" && path_only(path).trim_matches('/') == "api/v1/services"
}

/// Add the two flags the console uses to decide whether to show a config link
/// and a UI button. The daemon's summary is the source of truth for the rest.
fn project_service(status: &Value) -> Option<Value> {
    let has_config = status
        .pointer("/details/config")
        .and_then(Value::as_str)
        .is_some();
    let has_ui = status
        .get("summary")
        .and_then(|summary| summary.get("id"))
        .and_then(Value::as_str)
        .map(|id| crate::plugin::web_assets(id).is_some())
        .unwrap_or(false);
    let mut summary = status.get("summary").cloned()?;
    if let Some(object) = summary.as_object_mut() {
        object.insert("hasConfig".into(), Value::Bool(has_config));
        object.insert("hasUI".into(), Value::Bool(has_ui));
    }
    Some(summary)
}

impl Face for ApiFace {
    fn route(&self, method: &str, path: &str) -> Option<Route> {
        if is_version(method, path) {
            Some(Route::inline(BodyPolicy::Empty))
        } else if is_services(method, path) {
            Some(Route::blocking(BodyPolicy::Empty))
        } else if blob::download_target(method, path).is_some() {
            // A download has no request body, but its result is a stream
            // handler: the route must say so, or the dispatch check rejects it.
            Some(Route::inline(BodyPolicy::Streamed))
        } else {
            None
        }
    }

    fn handle(&self, request: Request, done: Completion) {
        if let Some(target) = blob::download_target(&request.method, &request.path) {
            done.complete(Exchange::Stream(Box::new(blob::DownloadStream::open(
                &request, target,
            ))));
            return;
        }
        let response = if is_version(&request.method, request.path_only()) {
            Response::json(
                200,
                &json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "protocol": 1,
                    "platform": crate::package::target(),
                }),
            )
        } else if is_services(&request.method, request.path_only()) {
            self.list_services()
        } else {
            // Routing guarantees this is unreachable; answering with a 500 is
            // safer than panicking on a miswired dispatcher.
            Response::json(
                500,
                &json!({
                    "error": {
                        "code": "face_contract",
                        "message": "face handled a route it did not claim",
                    }
                }),
            )
        };
        done.complete(Exchange::Response(response));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lan::AuthKind;
    use crate::server::http::dispatch::DispatchError;
    use crate::server::http::{dispatch_inline, Dispatch};

    fn face() -> ApiFace {
        ApiFace::new(".")
    }

    fn request(method: &str, path: &str) -> Request {
        Request {
            method: method.to_string(),
            path: path.to_string(),
            headers: Vec::new(),
            peer: "127.0.0.1".parse().expect("peer"),
            principal: AuthKind::None,
            body: Vec::new(),
        }
    }

    #[test]
    fn version_is_inline_and_services_is_blocking() {
        let face = face();
        let version = face.route("GET", "/api/v1/version").expect("version route");
        assert_eq!(version.dispatch, Dispatch::Inline);
        assert!(face.route("GET", "/api/v1/version?x=1").is_some());
        assert!(face.route("GET", "/api/v1/version/").is_some());
        assert!(face.route("POST", "/api/v1/version").is_none());

        let services = face
            .route("GET", "/api/v1/services")
            .expect("services route");
        assert_eq!(services.dispatch, Dispatch::Blocking);
        assert!(face.route("GET", "/api/v1/services?x=1").is_some());
        assert!(face.route("GET", "/api/v1/services/").is_some());
        assert!(face.route("POST", "/api/v1/services").is_none());
        assert!(face.route("GET", "/api/v1/services/demo").is_none());
    }

    #[test]
    fn version_is_served_through_the_face_with_the_original_payload() {
        let face = face();
        let route = face.route("GET", "/api/v1/version").expect("route");
        let exchange = dispatch_inline(&face, &route, request("GET", "/api/v1/version"))
            .expect("version must complete");
        let Exchange::Response(response) = exchange else {
            panic!("version returns a memory response");
        };
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "application/json");
        let body: serde_json::Value = serde_json::from_slice(&response.body).expect("json");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(body["protocol"], 1);
        assert_eq!(body["platform"], crate::package::target());
    }

    #[test]
    fn a_blocking_route_is_not_dispatched_inline() {
        let face = face();
        let route = face.route("GET", "/api/v1/services").expect("route");
        let error = match dispatch_inline(&face, &route, request("GET", "/api/v1/services")) {
            Ok(_) => panic!("a blocking route must not run on the server loop"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            DispatchError::UnsupportedDispatch(Dispatch::Blocking)
        );
    }

    #[test]
    fn blob_downloads_are_inline_streamed_routes() {
        let face = face();
        for (method, path) in [
            ("GET", "/api/v1/blobs/sha256:abc"),
            ("HEAD", "/api/v1/file/sha256:abc"),
            ("GET", "/api/v1/files/sha256:abc"),
        ] {
            let route = face.route(method, path).expect("download route");
            assert_eq!(route.dispatch, Dispatch::Inline);
            assert_eq!(route.body, BodyPolicy::Streamed);
        }
        assert!(face.route("POST", "/api/v1/blobs/sha256:abc").is_none());
        assert!(face.route("GET", "/api/v1/blobs").is_none());
        assert!(face
            .route("PUT", "/api/v1/blobs/uploads/x/parts/1")
            .is_none());
        assert!(face.route("GET", "/api/v1/file/uploads/x").is_none());
    }

    #[test]
    fn a_download_route_returns_a_stream_even_when_the_blob_is_missing() {
        let face = face();
        let path = "/api/v1/blobs/sha256:deadbeef";
        let route = face.route("GET", path).expect("download route");
        let exchange = dispatch_inline(&face, &route, request("GET", path))
            .expect("a missing blob is a streamed 404, not a dispatch error");
        assert!(matches!(exchange, Exchange::Stream(_)));
    }

    #[test]
    fn service_summaries_carry_has_config_and_has_ui() {
        let with_config = json!({
            "summary": { "id": "unit-test-missing", "kind": "plugin" },
            "details": { "config": "/tmp/config" }
        });
        let summary = project_service(&with_config).expect("summary");
        assert_eq!(summary["hasConfig"], true);
        assert_eq!(summary["hasUI"], false);

        let without_config = json!({
            "summary": { "id": "unit-test-missing", "kind": "core" }
        });
        let summary = project_service(&without_config).expect("summary");
        assert_eq!(summary["hasConfig"], false);
        assert_eq!(summary["hasUI"], false);
    }
}
