//! `/api/v1` face: URL → capability projection.
//!
//! #113 moved `GET /api/v1/version` through the S4 seam, #116 added the first
//! `Blocking` route, and #117 moves the remaining management JSON routes here.
//! Blob/file downloads (#118) and uploads (#119) are claimed by their own
//! stream faces before the body is read; #120 moved export spk/spkb to
//! `face/export.rs`; #121 moved package import/install to `face/install.rs`.
//! The plugin UI streams keep their own path (#122). This face owns the
//! ordinary management surface, the console page and login/logout.

use std::io;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::kernel::schema::{MAX_PROTOCOL, MAX_SCHEMA};
use crate::manage as package;
use crate::manage as plugin;
use crate::platform::config::Config;
use crate::platform::log::{self, Level};
use crate::server::auth::lan::{self, AuthKind, Lan};
use crate::server::http::model as http;
use crate::server::http::{Completion, Exchange, Face, Request, Response, Route};

use super::reply::{
    cap_log, decode, download_response, error_response, method_not_allowed, problem_error_response,
    project_service, rpc_error, safe_filename, summary_response,
};
use super::routes::{self, Endpoint};

mod logs;
mod settings;
mod upgrade;

use super::blob;
use super::console;
use super::export;
use super::support::{self, CallBudget};

/// One ordinary API handler: the macro table stores a function pointer, so
/// dispatch is a table lookup instead of a `match` arm per endpoint.
#[derive(Clone, Copy)]
enum HandlerKind {
    Response(fn(&ApiFace, &ApiCall<'_>) -> Response),
}

/// Everything a handler needs from the matched request. This is the one place
/// that turns a path parameter, query string, body and budget into values, so
/// handlers do not repeat the closure plumbing the old per-endpoint dispatch carried.
pub(crate) struct ApiCall<'a> {
    request: &'a Request,
    matched: &'a routes::Matched<'a>,
    budget: CallBudget,
}

impl<'a> ApiCall<'a> {
    fn new(request: &'a Request, matched: &'a routes::Matched<'a>) -> ApiCall<'a> {
        ApiCall {
            request,
            matched,
            budget: CallBudget::from_request(request),
        }
    }

    pub(crate) fn param(&self, name: &str) -> String {
        self.matched.param(name).map(decode).unwrap_or_default()
    }

    pub(crate) fn query(&self) -> &'a str {
        routes::split_query(&self.request.path).1
    }

    pub(crate) fn body(&self) -> &'a [u8] {
        &self.request.body
    }

    pub(crate) fn budget(&self) -> &CallBudget {
        &self.budget
    }
}

/// The ordinary `/api/v1` rows. A method miss is a 405; historical 404 rows
/// use `api_quiet_routes!` below. Only method, path and handler are written.
macro_rules! api_routes {
    ($($method:ident $path:literal => $handler:ident),* $(,)?) => {
        static API_CLAIM_ROUTES: &[crate::face::routes::ApiRouteDef] = &[
            $(crate::face::routes::ApiRouteDef::api(
                stringify!($method),
                $path,
                stringify!($handler),
                false,
            ),)*
        ];
        static API_CLAIM_HANDLERS: &[(&str, HandlerKind)] = &[
            $((stringify!($handler), HandlerKind::Response(ApiFace::$handler)),)*
        ];
    };
}

/// The rare POST that reads a JSON body. Same line shape; the block name is
/// the body policy, so callers never pass a body flag.
macro_rules! api_json_routes {
    ($($method:ident $path:literal => $handler:ident),* $(,)?) => {
        static API_JSON_ROUTES: &[crate::face::routes::ApiRouteDef] = &[
            $(crate::face::routes::ApiRouteDef::json(
                stringify!($method),
                $path,
                stringify!($handler),
            ),)*
        ];
        static API_JSON_HANDLERS: &[(&str, HandlerKind)] = &[
            $((stringify!($handler), HandlerKind::Response(ApiFace::$handler)),)*
        ];
    };
}

api_json_routes! {
    POST   "/api/v1/trust"                    => trust_add,
}

/// The historical 404-on-method-miss rows. Same line shape as `api_routes!`.
macro_rules! api_quiet_routes {
    ($($method:ident $path:literal => $handler:ident),* $(,)?) => {
        static API_QUIET_ROUTES: &[crate::face::routes::ApiRouteDef] = &[
            $(crate::face::routes::ApiRouteDef::api(
                stringify!($method),
                $path,
                stringify!($handler),
                true,
            ),)*
        ];
        static API_QUIET_HANDLERS: &[(&str, HandlerKind)] = &[
            $((stringify!($handler), HandlerKind::Response(ApiFace::$handler)),)*
        ];
    };
}

api_routes! {
    GET    "/api/v1/config/{id}"              => config_get,
    PUT    "/api/v1/config/{id}"              => config_put,
    POST   "/api/v1/config/{id}/reset"        => config_reset,
    GET    "/api/v1/export/{id}/config"       => export_config,
    GET    "/api/v1/trust"                    => trust_list,
    DELETE "/api/v1/trust/{key}"              => trust_remove,
    GET    "/api/v1/upgrade/check"            => upgrade_check,
    POST   "/api/v1/upgrade/apply"            => upgrade_apply,
    GET    "/api/v1/settings"                 => settings,
    PUT    "/api/v1/settings"                 => settings_put,
    GET    "/api/v1/logs"                     => daemon_logs,
    DELETE "/api/v1/logs"                     => daemon_logs_clear,
    GET    "/api/v1/logs/{id}"                => logs,
    DELETE "/api/v1/logs/{id}"                => logs_clear,
    POST   "/api/v1/plugins/{id}/ui-token"    => plugin_ui_token,
}

api_quiet_routes! {
    GET    "/api/v1/version"                  => version,
    GET    "/api/v1/compat"                   => compat,
    GET    "/api/v1/auth"                     => auth,
    GET    "/api/v1/services"                 => services,
    GET    "/api/v1/services/{id}"            => service_status,
    POST   "/api/v1/services/{id}/start"      => service_start,
    POST   "/api/v1/services/{id}/stop"       => service_stop,
    POST   "/api/v1/services/{id}/enable"     => service_enable,
    POST   "/api/v1/services/{id}/disable"    => service_disable,
    DELETE "/api/v1/plugins/{id}"             => plugin_uninstall,
}

/// Every macro-declared row, in table order.
pub(crate) fn route_defs() -> impl Iterator<Item = &'static crate::face::routes::ApiRouteDef> {
    API_CLAIM_ROUTES
        .iter()
        .chain(API_JSON_ROUTES)
        .chain(API_QUIET_ROUTES)
}

fn handler_for(name: &str) -> Option<HandlerKind> {
    API_CLAIM_HANDLERS
        .iter()
        .chain(API_JSON_HANDLERS)
        .chain(API_QUIET_HANDLERS)
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, handler)| *handler)
}

/// The console page and its tab icon share one embedded-asset table.
fn console_response(request: &Request) -> Response {
    match console::find(&request.path) {
        Some(asset) => console::response(asset, http::header(&request.headers, "if-none-match")),
        None => error_response(404, "not_found", "console asset not found"),
    }
}

/// The management API face.
pub struct ApiFace {
    config: Arc<Config>,
    lan: Arc<Mutex<Lan>>,
}

impl ApiFace {
    pub fn new(config: Arc<Config>, lan: Arc<Mutex<Lan>>) -> ApiFace {
        ApiFace { config, lan }
    }

    fn rpc(&self, budget: &CallBudget, method: &str, params: Value) -> Result<Value, Response> {
        support::rpc_result(self.config.paths.control_dir(), method, params, budget)
            .map_err(rpc_error)
    }

    fn require_plugin(&self, budget: &CallBudget, id: &str) -> Result<(), Response> {
        support::require_plugin_result(self.config.paths.control_dir(), id, budget)
            .map_err(|(status, code, message)| error_response(status, code, &message))
    }

    /// Project `svc.list` into the summaries the console consumes.
    fn list_services_rpc(&self, budget: &CallBudget) -> Response {
        match self.rpc(budget, "svc.list", json!({})) {
            Ok(value) => {
                let services: Vec<Value> = value
                    .get("services")
                    .and_then(Value::as_array)
                    .map(|services| {
                        services
                            .iter()
                            .filter_map(|service| project_service(service, &self.config.paths))
                            .collect()
                    })
                    .unwrap_or_default();
                Response::json(200, &json!({ "services": services }))
            }
            Err(response) => response,
        }
    }

    fn service_status_rpc(&self, budget: &CallBudget, id: &str) -> Response {
        match self.rpc(budget, "svc.status", json!({ "id": id })) {
            Ok(status) => Response::json(200, &status),
            Err(response) => response,
        }
    }

    fn service_action(&self, budget: &CallBudget, method: &str, id: &str) -> Response {
        match self.rpc(budget, method, json!({ "id": id })) {
            Ok(status) => summary_response(status),
            Err(response) => response,
        }
    }

    fn plugin_ui_token_rpc(&self, id: &str) -> Response {
        if crate::face::plugin_ui::web_assets_for(&self.config.paths, id).is_none() {
            if package::valid_id(id) {
                return error_response(404, "not_found", &format!("plugin {} has no web UI", id));
            }
            return error_response(404, "not_found", &format!("unknown plugin UI: {}", id));
        }
        let token = match self.lan.lock() {
            Ok(mut state) => state.issue_plugin_token(id),
            Err(_) => return error_response(500, "internal", "auth state unavailable"),
        };
        Response::json(
            200,
            &json!({
                "token": token,
                "url": format!("/plugin-ui/{}/{}/", id, token),
                "expiresIn": lan::plugin_token_ttl_secs(),
            }),
        )
    }

    fn uninstall_plugin_rpc(&self, budget: &CallBudget, id: &str, purge: bool) -> Response {
        match self.rpc(
            budget,
            "plugin.uninstall",
            json!({ "id": id, "purge": purge }),
        ) {
            Ok(_) => Response::json(200, &json!({ "ok": true })),
            Err(response) => response,
        }
    }

    fn get_config_rpc(&self, budget: &CallBudget, id: &str) -> Response {
        if let Err(response) = self.require_plugin(budget, id) {
            return response;
        }
        let Some(path) = plugin::active_config(&self.config.paths, id) else {
            return error_response(404, "not_found", &format!("plugin {} has no config", id));
        };
        match std::fs::read(&path) {
            Ok(data) => Response::new(200, "text/plain", data),
            Err(cause) => error_response(500, "internal", &format!("read config: {}", cause)),
        }
    }

    fn put_config_rpc(&self, budget: &CallBudget, id: &str, body: &[u8]) -> Response {
        if let Err(response) = self.require_plugin(budget, id) {
            return response;
        }
        let _guard = plugin::config_lock();
        let _lock = match plugin::lock_config(&self.config.paths, id) {
            Ok(lock) => lock,
            Err(error) => return problem_error_response(&error),
        };
        match plugin::replace_config(&self.config.paths, id, body, true) {
            Ok(()) => Response::json(200, &json!({ "ok": true })),
            Err(error) => problem_error_response(&error),
        }
    }

    fn reset_config_rpc(&self, budget: &CallBudget, id: &str) -> Response {
        if let Err(response) = self.require_plugin(budget, id) {
            return response;
        }
        let Some((template, _)) = plugin::config_template(&self.config.paths, id) else {
            return error_response(
                404,
                "not_found",
                &format!("plugin {} has no config template", id),
            );
        };
        let _guard = plugin::config_lock();
        let _lock = match plugin::lock_config(&self.config.paths, id) {
            Ok(lock) => lock,
            Err(error) => return problem_error_response(&error),
        };
        match plugin::replace_config(&self.config.paths, id, &template, true) {
            Ok(()) => Response::json(200, &json!({ "ok": true })),
            Err(error) => problem_error_response(&error),
        }
    }

    fn export_config_rpc(&self, budget: &CallBudget, id: &str) -> Response {
        if let Err(response) = self.require_plugin(budget, id) {
            return response;
        }
        match plugin::export_config(&self.config.paths, id) {
            Ok((version, data)) => download_response(
                &format!("{}-{}.config", safe_filename(id), safe_filename(&version)),
                &data,
            ),
            Err(error) => problem_error_response(&error),
        }
    }

    fn trust_list(&self, _call: &ApiCall<'_>) -> Response {
        match crate::manage::check::trust::keys(&self.config.paths) {
            Ok(keys) => Response::json(
                200,
                &json!({
                    "keys": keys,
                    "directory": self.config.paths.trust_dir.to_string_lossy(),
                }),
            ),
            Err(error) => problem_error_response(&error),
        }
    }

    fn trust_add_rpc(&self, body: &[u8], query: &str) -> Response {
        let Some(key_id) = support::query_value(query, "key_id") else {
            return error_response(400, "bad_request", "key_id is required");
        };
        if body.is_empty() {
            return error_response(400, "bad_request", "empty public key");
        }
        match crate::manage::check::trust::add(&self.config.paths, &key_id, body) {
            Ok(path) => Response::json(
                200,
                &json!({
                    "keyId": key_id,
                    "path": path.to_string_lossy(),
                }),
            ),
            Err(error) => problem_error_response(&error),
        }
    }

    fn trust_remove_rpc(&self, key_id: &str) -> Response {
        match crate::manage::check::trust::remove(&self.config.paths, key_id) {
            Ok(true) => Response::json(200, &json!({ "keyId": key_id, "removed": true })),
            Ok(false) => error_response(
                404,
                "not_found",
                &format!("unknown trusted key: {}", key_id),
            ),
            Err(error) => problem_error_response(&error),
        }
    }

    fn compat(&self, _call: &ApiCall<'_>) -> Response {
        Response::json(
            200,
            &serde_json::to_value(crate::platform::compat::detect(
                &self.config.paths,
                MAX_SCHEMA,
                MAX_PROTOCOL,
            ))
            .unwrap_or(Value::Null),
        )
    }

    fn auth(&self, _call: &ApiCall<'_>) -> Response {
        match self.lan.lock() {
            Ok(state) => {
                // One face-neutral value, two renderers: auth.status and this
                // route both read LanStatus, so password_set has a single
                // derivation. The renderer still spells this protocol's keys;
                // the cross-surface test keeps the two shapes honest.
                let status = state.status();
                Response::json(
                    200,
                    &json!({
                        "bind": status.bind.to_string(),
                        "requestedBind": status.requested_bind.to_string(),
                        "authRequired": status.require_auth,
                        "passwordSet": status.password_set,
                    }),
                )
            }
            Err(_) => error_response(500, "internal", "auth state unavailable"),
        }
    }

    fn version(&self, _call: &ApiCall<'_>) -> Response {
        Response::json(
            200,
            &json!({
                "version": env!("CARGO_PKG_VERSION"),
                "protocol": 1,
                "platform": crate::platform::target(),
            }),
        )
    }

    fn services(&self, call: &ApiCall<'_>) -> Response {
        self.list_services_rpc(call.budget())
    }

    fn service_status(&self, call: &ApiCall<'_>) -> Response {
        self.service_status_rpc(call.budget(), &call.param("id"))
    }

    fn service_start(&self, call: &ApiCall<'_>) -> Response {
        self.service_action(call.budget(), "svc.start", &call.param("id"))
    }

    fn service_stop(&self, call: &ApiCall<'_>) -> Response {
        self.service_action(call.budget(), "svc.stop", &call.param("id"))
    }

    fn service_enable(&self, call: &ApiCall<'_>) -> Response {
        self.service_action(call.budget(), "svc.enable", &call.param("id"))
    }

    fn service_disable(&self, call: &ApiCall<'_>) -> Response {
        self.service_action(call.budget(), "svc.disable", &call.param("id"))
    }

    fn plugin_ui_token(&self, call: &ApiCall<'_>) -> Response {
        self.plugin_ui_token_rpc(&call.param("id"))
    }

    fn plugin_uninstall(&self, call: &ApiCall<'_>) -> Response {
        let purge = support::query_bool(call.query(), "purge").unwrap_or(false);
        self.uninstall_plugin_rpc(call.budget(), &call.param("id"), purge)
    }

    fn config_reset(&self, call: &ApiCall<'_>) -> Response {
        self.reset_config_rpc(call.budget(), &call.param("id"))
    }

    fn config_get(&self, call: &ApiCall<'_>) -> Response {
        self.get_config_rpc(call.budget(), &call.param("id"))
    }

    fn config_put(&self, call: &ApiCall<'_>) -> Response {
        self.put_config_rpc(call.budget(), &call.param("id"), call.body())
    }

    fn export_config(&self, call: &ApiCall<'_>) -> Response {
        self.export_config_rpc(call.budget(), &call.param("id"))
    }

    fn trust_add(&self, call: &ApiCall<'_>) -> Response {
        self.trust_add_rpc(call.body(), call.query())
    }

    fn trust_remove(&self, call: &ApiCall<'_>) -> Response {
        self.trust_remove_rpc(&call.param("key"))
    }

    fn login(&self, request: &Request) -> Response {
        let password = http::header(&request.headers, "authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim)
            .map(str::to_string)
            .or_else(|| {
                serde_json::from_slice::<Value>(&request.body)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("password")
                            .or_else(|| value.get("token"))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
            });
        let Some(password) = password else {
            return error_response(401, "unauthorized", "password required");
        };
        let session = match self.lan.lock() {
            Ok(mut state) => match state.login(&password) {
                Ok(session) => session,
                Err(message) => return error_response(401, "unauthorized", &message),
            },
            Err(_) => return error_response(500, "internal", "auth state unavailable"),
        };
        let mut response = Response::json(200, &json!({ "ok": true }));
        for (name, value) in lan::cookie_headers(&session) {
            response = response.with_header(name, value);
        }
        response
    }

    fn logout(&self, request: &Request) -> Response {
        if let Ok(mut state) = self.lan.lock() {
            state.logout(&request.headers);
        }
        let mut response = Response::json(200, &json!({ "ok": true }));
        for (name, value) in lan::clear_cookie_headers() {
            response = response.with_header(name, value);
        }
        response
    }
}

impl Face for ApiFace {
    fn route(&self, method: &str, path: &str) -> Option<Route> {
        match routes::resolve(method, path) {
            routes::Match::Found(matched) if matched.spec.owner == routes::Owner::Api => {
                Some(matched.spec.route(&self.config.limits))
            }
            routes::Match::MethodNotAllowed { owners, spec, .. } if owners.api_claims() => {
                Some(spec.route_for_405())
            }
            _ => None,
        }
    }

    fn handle(&self, request: Request, done: Completion) {
        let matched = match routes::resolve(&request.method, &request.path) {
            routes::Match::Found(matched) => matched,
            routes::Match::MethodNotAllowed { allow, .. } => {
                done.complete(Exchange::Response(method_not_allowed(&allow)));
                return;
            }
            routes::Match::NotFound => {
                done.complete(Exchange::Response(error_response(
                    500,
                    "internal",
                    "face handled a route it did not claim",
                )));
                return;
            }
        };
        match matched.spec.endpoint {
            Endpoint::ExportSpk | Endpoint::ExportBundle => {
                let Some(target) = export::export_target(&request.method, &request.path) else {
                    done.complete(Exchange::Response(error_response(
                        500,
                        "internal",
                        "export route mismatch",
                    )));
                    return;
                };
                done.complete(Exchange::Stream(Box::new(export::ExportStream::open(
                    target,
                    self.config.paths.clone(),
                ))));
            }
            Endpoint::BlobDownload | Endpoint::FileDownload => {
                let Some(target) = blob::download_target(&request.method, &request.path) else {
                    done.complete(Exchange::Response(error_response(
                        500,
                        "internal",
                        "download route mismatch",
                    )));
                    return;
                };
                done.complete(Exchange::Stream(Box::new(blob::DownloadStream::open(
                    &request,
                    target,
                    self.config.paths.clone(),
                ))));
            }
            Endpoint::Login => {
                done.complete(Exchange::Response(self.login(&request)));
            }
            Endpoint::Logout => {
                done.complete(Exchange::Response(self.logout(&request)));
            }
            Endpoint::Console | Endpoint::ConsoleAsset => {
                done.complete(Exchange::Response(console_response(&request)));
            }
            Endpoint::UnknownApi => {
                done.complete(Exchange::Response(error_response(
                    404,
                    "not_found",
                    "unknown endpoint",
                )));
            }
            Endpoint::Api(name) => {
                if request.principal == AuthKind::Cookie
                    && http::is_write_method(&request.method)
                    && !http::csrf_ok(&request.headers)
                {
                    done.complete(Exchange::Response(error_response(
                        403,
                        "forbidden",
                        "cookie writes require Origin and X-Svcd-Csrf",
                    )));
                    return;
                }
                let Some(handler) = handler_for(name) else {
                    done.complete(Exchange::Response(error_response(
                        500,
                        "internal",
                        "no handler for route",
                    )));
                    return;
                };
                let call = ApiCall::new(&request, &matched);
                match handler {
                    HandlerKind::Response(handle) => {
                        done.complete(Exchange::Response(handle(self, &call)));
                    }
                }
            }
            _ => {
                done.complete(Exchange::Response(error_response(
                    500,
                    "internal",
                    "face handled a route it did not claim",
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;
    use crate::platform::paths::Paths;
    use crate::server::http::{dispatch_blocking_now, dispatch_inline};
    use crate::server::http::{BodyPolicy, Dispatch};
    use crate::server::http::{CompletionSlot, Exchange};

    fn lan() -> Arc<Mutex<Lan>> {
        let bind = "127.0.0.1:0".parse().expect("bind");
        Arc::new(Mutex::new(Lan::new(bind, bind, false, None)))
    }

    fn test_config() -> Arc<Config> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = crate::test_support::temp_dir().join(format!(
            "svcd-face-unit-{}-{}",
            std::process::id(),
            nanos
        ));
        Arc::new(Config::with_paths(
            crate::platform::settings::Settings::default(),
            None,
            Paths::under(&root),
        ))
    }

    fn face() -> ApiFace {
        ApiFace::new(test_config(), lan())
    }

    fn face_with_password() -> ApiFace {
        let bind = "127.0.0.1:0".parse().expect("bind");
        let hash = crate::platform::auth::hash_password("secret").expect("hash");
        ApiFace::new(
            test_config(),
            Arc::new(Mutex::new(Lan::new(bind, bind, true, Some(hash)))),
        )
    }

    fn request(method: &str, path: &str) -> Request {
        Request {
            method: method.to_string(),
            path: path.to_string(),
            headers: Vec::new(),
            peer: "127.0.0.1".parse().expect("peer"),
            principal: AuthKind::None,
            body: Vec::new(),
            deadline: None,
            cancel: None,
        }
    }

    #[test]
    fn embedded_console_gzip_is_deterministic_and_matches_the_source() {
        for (asset, source) in [
            (
                console::find("/assets/index.html").expect("console page"),
                include_str!("../assets/static/index.html"),
            ),
            (
                console::find("/assets/favicon.svg").expect("tab icon"),
                include_str!("../assets/static/favicon.svg"),
            ),
        ] {
            assert_eq!(asset.encoding, console::Encoding::Gzip);
            assert_eq!(&asset.body[..2], &[0x1f, 0x8b][..], "gzip magic");
            assert!(
                asset.body[4..8].iter().all(|byte| *byte == 0),
                "fixed mtime"
            );
            assert_eq!(asset.body[9], 255, "fixed OS byte");
            let mut decoded = String::new();
            flate2::read::GzDecoder::new(asset.body)
                .read_to_string(&mut decoded)
                .expect("gunzip embedded console asset");
            assert_eq!(decoded, source, "{}", asset.path);
        }
    }

    #[test]
    fn console_asset_paths_are_unique_and_root_shares_the_page() {
        for (index, asset) in console::assets().iter().enumerate() {
            assert!(
                !console::assets()[..index]
                    .iter()
                    .any(|other| other.path == asset.path),
                "duplicate console asset path {}",
                asset.path
            );
        }
        assert_eq!(
            console::find("/").map(|asset| asset.path),
            Some("index.html")
        );
        assert!(console::find("/assets/index.html").is_some());
        assert!(
            console::find("/index.html").is_none(),
            "the page has no /index.html alias"
        );
        assert!(
            console::find("/favicon.svg").is_none(),
            "assets only live under /assets/"
        );
        assert!(console::find("/assets/missing").is_none());
    }

    #[test]
    fn management_routes_have_the_expected_body_and_dispatch_policy() {
        let face = face();
        let login = face.route("POST", "/login").expect("login route");
        assert!(login.public);
        assert_eq!(login.dispatch, Dispatch::Blocking);
        assert!(matches!(login.body, BodyPolicy::Buffered { .. }));

        let logout = face.route("POST", "/logout").expect("logout route");
        assert!(logout.public);
        assert_eq!(logout.dispatch, Dispatch::Blocking);
        assert_eq!(logout.body, BodyPolicy::Empty);

        for path in ["/", "/assets/app.css"] {
            let route = face.route("GET", path).expect("console route");
            assert!(route.public, "{path}");
            assert_eq!(route.dispatch, Dispatch::Inline, "{path}");
        }
        assert!(
            face.route("GET", "/index.html").is_none(),
            "the page has no /index.html alias"
        );
        assert!(
            face.route("GET", "/favicon.svg").is_none(),
            "console assets only live under /assets/"
        );

        let auth = face.route("GET", "/api/v1/auth").expect("auth route");
        assert!(!auth.public);
        assert_eq!(auth.dispatch, Dispatch::Blocking);

        for (method, path) in [
            ("GET", "/api/v1/compat"),
            ("GET", "/api/v1/services/demo"),
            ("POST", "/api/v1/services/demo/start"),
            ("POST", "/api/v1/plugins/demo/ui-token"),
            ("DELETE", "/api/v1/plugins/demo"),
            ("GET", "/api/v1/config/demo"),
            ("GET", "/api/v1/export/demo/config"),
            ("GET", "/api/v1/logs/demo"),
            ("GET", "/api/v1/nope"),
        ] {
            let route = face
                .route(method, path)
                .unwrap_or_else(|| panic!("{method} {path}"));
            assert!(!route.public, "{method} {path}");
            assert_eq!(route.dispatch, Dispatch::Blocking, "{method} {path}");
        }

        let put = face
            .route("PUT", "/api/v1/config/demo")
            .expect("config write");
        assert!(matches!(put.body, BodyPolicy::Buffered { .. }));
        let trust = face.route("POST", "/api/v1/trust").expect("trust write");
        assert!(matches!(trust.body, BodyPolicy::Buffered { .. }));
    }

    #[test]
    fn downloads_and_exports_use_the_stream_face_and_uploads_use_the_prebody_face() {
        let face = face();
        for (method, path) in [
            ("GET", "/api/v1/blobs/abc"),
            ("HEAD", "/api/v1/file/abc"),
            ("GET", "/api/v1/export/demo"),
            ("GET", "/api/v1/export/demo/spk"),
        ] {
            let route = face
                .route(method, path)
                .unwrap_or_else(|| panic!("{method} {path} must be a face route"));
            assert!(
                matches!(route.body, BodyPolicy::Streamed),
                "{method} {path} must be a streamed response"
            );
            assert_eq!(route.dispatch, Dispatch::Inline);
        }
        // Uploads are claimed by the pre-body upload face, not by ApiFace.
        for (method, path) in [
            ("POST", "/api/v1/blobs"),
            ("PUT", "/api/v1/blobs/uploads/abc/parts/1"),
            ("PUT", "/api/v1/file/uploads/abc/parts/2"),
        ] {
            assert!(
                crate::face::blob_upload::upload_target(&crate::server::http::model::Head {
                    method: method.to_string(),
                    path: path.to_string(),
                    version: "HTTP/1.1".to_string(),
                    headers: Vec::new(),
                })
                .is_some(),
                "{method} {path} must use the pre-body upload face"
            );
        }
        for (method, path) in [
            ("POST", "/api/v1/plugins"),
            ("POST", "/api/v1/import"),
            ("GET", "/mcp"),
            ("GET", "/plugin-ui/demo/token/events"),
        ] {
            assert!(
                face.route(method, path).is_none(),
                "{method} {path} must stay legacy"
            );
        }
        // The other export shape is a management route: only the archive
        // downloads are streams.
        let config = face
            .route("GET", "/api/v1/export/demo/config")
            .expect("config export route");
        assert_eq!(config.dispatch, Dispatch::Blocking);
        let post = face
            .route("POST", "/api/v1/export/demo")
            .expect("405 route");
        assert_eq!(post.dispatch, Dispatch::Blocking);
    }

    #[test]
    fn an_export_route_returns_a_stream_handler() {
        let face = face();
        let path = "/api/v1/export/missing-plugin";
        let route = face.route("GET", path).expect("export route");
        let exchange = dispatch_inline(&face, &route, request("GET", path))
            .expect("an unknown plugin is a streamed 404, not a dispatch error");
        assert!(matches!(exchange, Exchange::Stream(_)));
    }

    #[test]
    fn cookie_writes_without_csrf_are_rejected_before_the_projection() {
        let face = face();
        let mut req = request("POST", "/api/v1/trust");
        req.principal = AuthKind::Cookie;
        req.headers = vec![("Host".into(), "127.0.0.1:8787".into())];
        let (slot, done) = CompletionSlot::new("POST /api/v1/trust");
        face.handle(req, done);
        let Some(Exchange::Response(response)) = slot.take() else {
            panic!("the face must answer");
        };
        assert_eq!(response.status, 403);
        let body: Value = serde_json::from_slice(&response.body).expect("json");
        assert_eq!(body["error"]["code"], "forbidden");

        // A bearer write is not the cookie CSRF surface.
        let mut req = request("POST", "/api/v1/trust");
        req.principal = AuthKind::Bearer;
        let (slot, done) = CompletionSlot::new("POST /api/v1/trust");
        face.handle(req, done);
        let Some(Exchange::Response(response)) = slot.take() else {
            panic!("the face must answer");
        };
        assert_eq!(response.status, 400);
    }

    #[test]
    fn login_is_public_and_sets_a_session_cookie() {
        let face = face_with_password();
        let mut req = request("POST", "/login");
        req.body = br#"{"password":"secret"}"#.to_vec();
        let (slot, done) = CompletionSlot::new("POST /login");
        face.handle(req, done);
        let Some(Exchange::Response(response)) = slot.take() else {
            panic!("login must answer");
        };
        assert_eq!(response.status, 200);
        assert!(
            response.headers.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("set-cookie")
                    && value.contains(lan::COOKIE_SESSION)
                    && value.contains("Max-Age=86400")
            }),
            "login must set the session cookie: {:?}",
            response.headers
        );
        assert!(slot.take().is_none(), "the completion is exactly once");
    }

    #[test]
    fn logout_is_public_and_clears_the_cookie() {
        let face = face_with_password();
        let req = request("POST", "/logout");
        let (slot, done) = CompletionSlot::new("POST /logout");
        face.handle(req, done);
        let Some(Exchange::Response(response)) = slot.take() else {
            panic!("logout must answer");
        };
        assert_eq!(response.status, 200);
        assert!(
            response.headers.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("set-cookie")
                    && value.contains(lan::COOKIE_SESSION)
                    && value.contains("Max-Age=0")
            }),
            "logout must clear the session cookie"
        );
    }

    #[test]
    fn console_page_is_public_html() {
        let face = face();
        let (slot, done) = CompletionSlot::new("GET /");
        face.handle(request("GET", "/"), done);
        let Some(Exchange::Response(response)) = slot.take() else {
            panic!("the console must answer");
        };
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "text/html; charset=utf-8");
        assert!(
            response.headers.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("content-encoding") && value == "gzip"
            }),
            "the console is served in its precompressed form"
        );
        let mut decoded = String::new();
        flate2::read::GzDecoder::new(&response.body[..])
            .read_to_string(&mut decoded)
            .expect("gunzip console");
        assert!(
            decoded.contains("id=\"rows\""),
            "the console table is missing"
        );
        assert!(
            decoded
                .contains("<link rel=\"icon\" type=\"image/svg+xml\" href=\"assets/favicon.svg\">"),
            "the console must point the tab at assets/favicon.svg"
        );
    }

    #[test]
    fn console_asset_is_public_and_answers_304_for_a_matching_etag() {
        let face = face();
        let route = face
            .route("GET", "/assets/favicon.svg")
            .expect("console asset route");
        assert!(route.public, "the icon must load before login");
        assert_eq!(route.dispatch, Dispatch::Inline);
        assert!(
            face.route("HEAD", "/assets/favicon.svg").is_some(),
            "a HEAD probe must reach the icon"
        );

        let asset = console::find("/assets/favicon.svg").expect("icon asset");
        let (slot, done) = CompletionSlot::new("GET /assets/favicon.svg");
        face.handle(request("GET", "/assets/favicon.svg"), done);
        let Some(Exchange::Response(first)) = slot.take() else {
            panic!("the icon must answer");
        };
        assert_eq!(first.status, 200);
        assert_eq!(first.content_type, "image/svg+xml");
        assert_eq!(first.body, asset.body);
        let etag = first
            .headers
            .iter()
            .find(|(name, _)| name == "ETag")
            .map(|(_, value)| value.clone())
            .expect("the icon needs a validator");
        assert_eq!(
            etag,
            format!("\"{}\"", package::hex_sha256(asset.body)),
            "the validator is the content hash, so an upgrade invalidates it"
        );
        for (name, value) in [
            ("Content-Encoding", "gzip"),
            ("Cache-Control", "no-cache"),
            ("X-Content-Type-Options", "nosniff"),
        ] {
            assert!(
                first
                    .headers
                    .iter()
                    .any(|(header, actual)| header == name && actual == value),
                "missing {name}: {value}"
            );
        }

        let mut revalidated = request("GET", "/assets/favicon.svg");
        revalidated
            .headers
            .push(("If-None-Match".to_string(), etag));
        let (slot, done) = CompletionSlot::new("GET /assets/favicon.svg");
        face.handle(revalidated, done);
        let Some(Exchange::Response(second)) = slot.take() else {
            panic!("the revalidation must answer");
        };
        assert_eq!(second.status, 304);
        assert!(second.body.is_empty(), "a 304 carries no body");
    }

    #[test]
    fn version_is_served_through_the_face_with_the_original_payload() {
        let face = face();
        let route = face.route("GET", "/api/v1/version").expect("route");
        let exchange = dispatch_blocking_now(
            &face,
            &route,
            request("GET", "/api/v1/version"),
            face.config.limits.control_timeout,
        )
        .expect("version must complete");
        let Exchange::Response(response) = exchange else {
            panic!("version returns a memory response");
        };
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "application/json");
        let body: Value = serde_json::from_slice(&response.body).expect("json");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(body["protocol"], 1);
        assert_eq!(body["platform"], crate::platform::target());
    }

    #[test]
    fn every_macro_route_has_a_handler() {
        for def in route_defs() {
            assert!(
                handler_for(def.handler).is_some(),
                "{} has no handler",
                def.handler
            );
        }
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
            crate::server::http::DispatchError::UnsupportedDispatch(Dispatch::Blocking)
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
        let summary = project_service(&with_config, &test_config().paths).expect("summary");
        assert_eq!(summary["hasConfig"], true);
        assert_eq!(summary["hasUI"], false);

        let without_config = json!({
            "summary": { "id": "unit-test-missing", "kind": "core" }
        });
        let summary = project_service(&without_config, &test_config().paths).expect("summary");
        assert_eq!(summary["hasConfig"], false);
        assert_eq!(summary["hasUI"], false);
    }

    #[test]
    fn a_public_face_route_skips_transport_auth() {
        use crate::server::auth::lan::AuthKind;
        use crate::server::http::{route_face_request, Face};

        let bind = "127.0.0.1:0".parse().expect("bind");
        let lan = Arc::new(Mutex::new(Lan::new(bind, bind, true, Some("hash".into()))));
        let face = ApiFace::new(test_config(), lan.clone());
        let peer = "127.0.0.1".parse().expect("peer");

        assert!(face.route("POST", "/login").expect("route").public);
        let login = http::Head {
            method: "POST".into(),
            path: "/login".into(),
            version: "HTTP/1.1".into(),
            headers: Vec::new(),
        };
        let prepared = route_face_request(&face, &login, peer, &lan)
            .expect("a public route must not fail authentication")
            .expect("the face claims login");
        assert_eq!(prepared.request.principal, AuthKind::None);

        let protected = http::Head {
            method: "GET".into(),
            path: "/api/v1/auth".into(),
            version: "HTTP/1.1".into(),
            headers: Vec::new(),
        };
        assert!(
            route_face_request(&face, &protected, peer, &lan).is_err(),
            "a protected route still requires a principal"
        );
    }
}
