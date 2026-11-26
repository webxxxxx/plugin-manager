//! Tests for the JSON API face: the route table, the auth/CSRF policy and the
//! projections the console reads. Kept beside `api.rs` (external test module)
//! so the file itself stays inside the production-line budget.

use super::*;

use std::io::Read;

use crate::platform::paths::Paths;
use crate::platform::test_support::config as test_config;
use crate::server::http::{dispatch_blocking_now, dispatch_inline};
use crate::server::http::{BodyPolicy, Dispatch};
use crate::server::http::{CompletionSlot, Exchange};

fn lan() -> Arc<Mutex<Lan>> {
    let bind = "127.0.0.1:0".parse().expect("bind");
    Arc::new(Mutex::new(Lan::new(bind, bind, false, None)))
}

fn face() -> ApiFace {
    ApiFace::new(test_config("face-unit"), lan())
}

fn face_with_password() -> ApiFace {
    let bind = "127.0.0.1:0".parse().expect("bind");
    let hash = crate::platform::auth::hash_password("secret").expect("hash");
    ApiFace::new(
        test_config("face-unit"),
        Arc::new(Mutex::new(Lan::new(bind, bind, true, Some(hash)))),
    )
}

fn request(method: &str, path: &str) -> Request {
    Request {
        method: method.to_string(),
        path: path.to_string(),
        headers: Vec::new(),
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
            console::find("/").expect("console page"),
            include_str!("../../assets/static/index.html"),
        ),
        (
            console::find("/assets/favicon.svg").expect("tab icon"),
            include_str!("../../assets/static/favicon.svg"),
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
    assert!(
        console::find("/assets/index.html").is_none(),
        "the page has one URL only"
    );
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
        decoded.contains("<link rel=\"icon\" type=\"image/svg+xml\" href=\"/assets/favicon.svg\">"),
        "the console must point the tab at /assets/favicon.svg"
    );
}

#[test]
fn console_lookup_ignores_the_query_string() {
    let face = face();
    for (path, expected) in [
        ("/?next=%2Fstatic%2Findex.html", 200),
        ("/assets/favicon.svg?v=1", 200),
        ("/assets/index.html", 404),
    ] {
        let (slot, done) = CompletionSlot::new(path);
        face.handle(request("GET", path), done);
        let Some(Exchange::Response(response)) = slot.take() else {
            panic!("{path} must answer");
        };
        assert_eq!(response.status, expected, "{path}");
    }
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
        format!("\"{}\"", crate::manage::hex_sha256(asset.body)),
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
        "summary": {
            "id": "unit-test-missing",
            "kind": "plugin",
            "installDir": "/tmp/plugins/unit-test-missing"
        },
        "details": { "config": "/tmp/config" }
    });
    let summary = project_service(&with_config, &test_config("face-unit").paths).expect("summary");
    assert_eq!(summary["hasConfig"], true);
    assert_eq!(summary["hasUI"], false);
    assert_eq!(
        summary["installDir"], "/tmp/plugins/unit-test-missing",
        "the install directory reaches the console list unchanged"
    );

    let without_config = json!({
        "summary": { "id": "unit-test-missing", "kind": "core" }
    });
    let summary =
        project_service(&without_config, &test_config("face-unit").paths).expect("summary");
    assert_eq!(summary["hasConfig"], false);
    assert_eq!(summary["hasUI"], false);
}

#[test]
fn a_plugin_summary_names_its_root_by_position() {
    let mut paths = crate::platform::paths::Paths::under(std::path::Path::new("/svcd-unit"));
    paths.plugin_roots = vec![
        std::path::PathBuf::from("/svcd-unit/data/plugins"),
        std::path::PathBuf::from("/svcd-unit/card/plugins"),
    ];
    let summary = |install_dir: &str| {
        project_service(
            &json!({
                "summary": { "id": "demo", "kind": "plugin", "installDir": install_dir }
            }),
            &paths,
        )
        .expect("summary")
    };

    assert_eq!(
        summary("/svcd-unit/data/plugins/demo")["installRootIndex"],
        1
    );
    assert_eq!(
        summary("/svcd-unit/card/plugins/demo")["installRootIndex"],
        2
    );
    // A directory no configured root holds carries no position: the row falls
    // back to the id alone.
    assert!(summary("/elsewhere/demo").get("installRootIndex").is_none());
    // A core service has no install directory at all.
    let core = project_service(
        &json!({ "summary": { "id": "svcd", "kind": "core" } }),
        &paths,
    )
    .expect("summary");
    assert!(core.get("installRootIndex").is_none());

    // Nested roots: the specific one wins even though the parent is listed
    // first, so `/data` cannot claim every plugin under `/data/plugins`.
    paths.plugin_roots = vec![
        std::path::PathBuf::from("/svcd-unit/data"),
        std::path::PathBuf::from("/svcd-unit/data/plugins"),
    ];
    let nested = project_service(
        &json!({
            "summary": {
                "id": "demo",
                "kind": "plugin",
                "installDir": "/svcd-unit/data/plugins/demo"
            }
        }),
        &paths,
    )
    .expect("summary");
    assert_eq!(nested["installRootIndex"], 2);
}

#[test]
fn a_public_face_route_skips_transport_auth() {
    use crate::server::auth::lan::AuthKind;
    use crate::server::http::{route_face_request, Face};

    let bind = "127.0.0.1:0".parse().expect("bind");
    let lan = Arc::new(Mutex::new(Lan::new(bind, bind, true, Some("hash".into()))));
    let face = ApiFace::new(test_config("face-unit"), lan.clone());

    assert!(face.route("POST", "/login").expect("route").public);
    let login = http::Head {
        method: "POST".into(),
        path: "/login".into(),
        version: "HTTP/1.1".into(),
        headers: Vec::new(),
    };
    let prepared = route_face_request(&face, &login, &lan)
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
        route_face_request(&face, &protected, &lan).is_err(),
        "a protected route still requires a principal"
    );
}
#[test]
fn a_probe_row_reports_the_measured_value_unless_the_document_states_one() {
    assert_eq!(
        super::settings::probe_reported(None, Some(512)),
        serde_json::json!(512),
        "an unstated probe row shows what this process measured"
    );
    assert_eq!(
        super::settings::probe_reported(Some(64), Some(512)),
        serde_json::json!(64),
        "a stated value wins over the probe"
    );
    assert_eq!(
        super::settings::probe_reported(None, None),
        serde_json::Value::Null,
        "an unreadable probe stays unknown, not zero"
    );
}

/// #298: the editor's first row is the default install root and only that row
/// is marked default. With `plugins_dir` unset the list is exactly
/// `<data_dir>/plugins`, which is what the console shows.
#[test]
fn the_plugin_root_editor_lists_the_default_root_first() {
    let temp = crate::platform::test_support::TempDir::new();
    let unset = Paths::under(&temp.0);
    let rows = super::settings::plugin_root_rows(&unset);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["path"],
        serde_json::json!(unset.data_dir.join("plugins").to_string_lossy()),
        "an unset list is the single <data_dir>/plugins root"
    );
    assert_eq!(rows[0]["default"], serde_json::json!(true));

    let mut paths = Paths::under(&temp.0);
    let card = temp.0.join("card/plugins");
    paths.plugin_roots = vec![paths.data_dir.join("plugins"), card.clone()];
    let rows = super::settings::plugin_root_rows(&paths);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["default"], serde_json::json!(true));
    assert_eq!(rows[1]["path"], serde_json::json!(card.to_string_lossy()));
    assert_eq!(rows[1]["default"], serde_json::json!(false));
}

/// A root is occupied when a plugin is on it now *or* when the location index
/// remembers one there; only the second case survives a pulled card, which is
/// exactly the case the settings screen must not let an operator delete.
#[test]
fn a_root_that_still_holds_plugins_cannot_be_dropped() {
    let temp = crate::platform::test_support::TempDir::new();
    let internal = temp.0.join("data/plugins");
    let card = temp.0.join("card/plugins");
    let empty = temp.0.join("spare/plugins");
    for root in [&internal, &card, &empty] {
        std::fs::create_dir_all(root).expect("root");
    }
    let demo = internal.join("demo");
    std::fs::create_dir_all(demo.join("1.0.0")).expect("version dir");
    std::fs::write(demo.join("current"), "1.0.0").expect("current");
    let mut paths = Paths::under(&temp.0);
    paths.plugin_roots = vec![internal.clone(), card.clone(), empty.clone()];
    crate::platform::locations::record(&paths, "weather", &card).expect("record");

    let rows = super::settings::plugin_root_rows(&paths);
    assert_eq!(rows[0]["pluginIds"], serde_json::json!(["demo"]));
    assert_eq!(rows[1]["pluginIds"], serde_json::json!(["weather"]));

    let all: Vec<std::path::PathBuf> = vec![internal.clone(), card.clone(), empty.clone()];
    assert!(
        super::settings::removed_with_plugins(&paths, &all).is_none(),
        "keeping every root is never refused"
    );

    let on_disk = super::settings::removed_with_plugins(&paths, &[card.clone(), empty.clone()])
        .expect("demo lives on the internal root");
    assert!(on_disk.contains("demo"), "{on_disk}");
    assert!(on_disk.contains("still holds 1 plugin"), "{on_disk}");

    let index_only =
        super::settings::removed_with_plugins(&paths, &[internal.clone(), empty.clone()])
            .expect("weather is recorded on the card");
    assert!(
        index_only.contains("weather"),
        "an unmounted root keeps its plugins: {index_only}"
    );

    assert!(
        super::settings::removed_with_plugins(&paths, &[internal.clone(), card.clone()]).is_none(),
        "an empty root may be dropped"
    );
}
