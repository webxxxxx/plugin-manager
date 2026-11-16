//! `/api/v1` face: URL → capability projection.
//!
//! #113 moves only `GET /api/v1/version` through the S4 seam; the rest of the
//! management surface still lives in the legacy API module until #117.

use serde_json::json;

use crate::server::http::{BodyPolicy, Completion, Exchange, Face, Request, Response, Route};

/// The management API face.
pub struct ApiFace;

fn is_version(method: &str, path: &str) -> bool {
    let path = path.split_once('?').map(|(path, _)| path).unwrap_or(path);
    // Mirror the legacy router's `trim_matches('/')` so `/api/v1/version/`
    // keeps resolving exactly as it did before the face existed.
    method == "GET" && path.trim_matches('/') == "api/v1/version"
}

impl Face for ApiFace {
    fn route(&self, method: &str, path: &str) -> Option<Route> {
        is_version(method, path).then(|| Route::inline(BodyPolicy::Empty))
    }

    fn handle(&self, request: Request, done: Completion) {
        let response = if is_version(&request.method, request.path_only()) {
            Response::json(
                200,
                &json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "protocol": 1,
                    "platform": crate::package::target(),
                }),
            )
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
    use crate::server::http::dispatch_inline;

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
    fn only_get_version_is_claimed() {
        assert!(ApiFace.route("GET", "/api/v1/version").is_some());
        assert!(ApiFace.route("GET", "/api/v1/version?x=1").is_some());
        assert!(ApiFace.route("GET", "/api/v1/version/").is_some());
        assert!(ApiFace.route("POST", "/api/v1/version").is_none());
        assert!(ApiFace.route("GET", "/api/v1/services").is_none());
    }

    #[test]
    fn version_is_served_through_the_face_with_the_original_payload() {
        let route = ApiFace.route("GET", "/api/v1/version").expect("route");
        let exchange = dispatch_inline(&ApiFace, &route, request("GET", "/api/v1/version"))
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
}
