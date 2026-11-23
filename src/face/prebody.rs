//! 读 body 之前的路由：插件 UI、包上传、blob/file 上传。
//!
//! server 的 poll loop 持有 [`PreBodyRoutes`] trait object，所以 trait 由
//! `server/http` 定义、face 实现、compose 构造。

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use crate::face::routes::{self, Endpoint};
use crate::platform::config::Config;
use crate::server::auth::lan::Lan;
use crate::server::http::model as http;

/// Pre-body rows. Same line shape as the API macro: method, path, handler.
/// These routes own the request body before it is buffered, so their table
/// derives `Owner::PreBody`, `Body::Streamed`, inline dispatch and priority 2.
macro_rules! prebody {
    ($($method:ident $path:literal => $handler:ident),* $(,)?) => {
        static PREBODY_ROUTES: &[crate::face::routes::ApiRouteDef] = &[
            $(crate::face::routes::ApiRouteDef::prebody(
                stringify!($method),
                $path,
                stringify!($handler),
            ),)*
        ];
    };
}

prebody! {
    POST "/api/v1/plugins"                              => package_install,
    POST "/api/v1/import"                               => package_import,
    POST "/api/v1/blobs"                                => blob_stream,
    POST "/api/v1/file"                                 => file_stream,
    PUT  "/api/v1/blobs/uploads/{upload}/parts/{part}"  => blob_part,
    PUT  "/api/v1/file/uploads/{upload}/parts/{part}"   => file_part,
}

pub(crate) fn route_defs() -> impl Iterator<Item = &'static crate::face::routes::ApiRouteDef> {
    PREBODY_ROUTES.iter()
}

pub(crate) struct PreBodyFaces {
    /// Read through the trait object by both drivers: the poll loop on Unix and
    /// the thread-per-connection driver elsewhere (#205).
    pub(crate) config: Arc<Config>,
    /// The daemon-wide UI event bus, injected by the composition root.
    pub(crate) bus: Arc<crate::face::uichannel::EventBus>,
}

impl PreBodyFaces {
    /// The plugin UI claim. The stream keeps the connection's idle timeout.
    fn plugin_ui(
        &self,
        head: &http::Head,
        peer: IpAddr,
        lan: &Arc<Mutex<Lan>>,
        raw_hub: Arc<crate::wire::contract::rawhub::RawHub>,
    ) -> Option<Result<crate::server::http::Exchange, Vec<u8>>> {
        crate::face::plugin_ui::PluginUiFace::route_for(&head.method, &head.path)?;
        Some(crate::face::plugin_ui::PluginUiFace::exchange(
            head,
            peer,
            lan,
            self.config.clone(),
            Some(raw_hub),
            self.bus.clone(),
        ))
    }

    /// The static-site claim. Only a configured root is claimed; the face
    /// answers auth, the slash redirect, 404 and 405 itself.
    fn static_site(
        &self,
        head: &http::Head,
        lan: &Arc<Mutex<Lan>>,
    ) -> Option<Result<crate::server::http::Exchange, Vec<u8>>> {
        if self.config.settings.static_root.is_none()
            || !crate::face::static_site::is_path(&head.path)
        {
            return None;
        }
        Some(crate::face::static_site::exchange(&self.config, head, lan))
    }

    /// The package upload claim. The stream switches to the transfer timeout.
    fn package_upload(
        &self,
        head: &http::Head,
        lan: &Arc<Mutex<Lan>>,
    ) -> Option<Result<crate::server::http::Exchange, Vec<u8>>> {
        let matched = routes::resolve(&head.method, &head.path).found()?;
        if !matches!(
            matched.spec.endpoint,
            Endpoint::PreBody("package_install") | Endpoint::PreBody("package_import")
        ) {
            return None;
        }
        crate::face::install::target(head)?;
        Some(crate::face::install::exchange(
            head,
            lan,
            self.config.clone(),
        ))
    }

    /// The blob/file upload claim. The stream switches to the transfer timeout.
    fn blob_upload(
        &self,
        head: &http::Head,
        lan: &Arc<Mutex<Lan>>,
    ) -> Option<Result<crate::server::http::Exchange, Vec<u8>>> {
        let matched = routes::resolve(&head.method, &head.path).found()?;
        if !matches!(
            matched.spec.endpoint,
            Endpoint::PreBody("blob_stream")
                | Endpoint::PreBody("file_stream")
                | Endpoint::PreBody("blob_part")
                | Endpoint::PreBody("file_part")
        ) {
            return None;
        }
        crate::face::blob_upload::upload_target(head)?;
        Some(crate::face::blob_upload::exchange(
            head,
            lan,
            self.config.paths.clone(),
        ))
    }
}

impl crate::server::http::hooks::PreBodyRoutes for PreBodyFaces {
    fn claim(
        &self,
        head: &http::Head,
        peer: IpAddr,
        lan: &Arc<Mutex<Lan>>,
        raw_hub: Arc<crate::wire::contract::rawhub::RawHub>,
    ) -> Option<crate::server::http::hooks::PreBodyClaim> {
        // Endpoint precedence lives here, in the face layer. The loop calls this
        // one method; the first Some wins.
        if let Some(exchange) = self.static_site(head, lan) {
            return Some(crate::server::http::hooks::PreBodyClaim { exchange });
        }
        if let Some(exchange) = self.plugin_ui(head, peer, lan, raw_hub) {
            return Some(crate::server::http::hooks::PreBodyClaim { exchange });
        }
        if let Some(exchange) = self.package_upload(head, lan) {
            return Some(crate::server::http::hooks::PreBodyClaim { exchange });
        }
        if let Some(exchange) = self.blob_upload(head, lan) {
            return Some(crate::server::http::hooks::PreBodyClaim { exchange });
        }
        None
    }

    fn mcp(
        &self,
        request: &crate::server::http::engine::HttpRequest,
        lan: &Arc<Mutex<Lan>>,
    ) -> Option<Result<crate::server::mcp::transport::StreamingCall, Vec<u8>>> {
        let path = request
            .path
            .split_once('?')
            .map(|(path, _)| path)
            .unwrap_or(request.path.as_str());
        if path != "/mcp" {
            return None;
        }
        let password_hash = lan
            .lock()
            .ok()
            .and_then(|state| state.password_hash().map(str::to_string));
        match crate::server::mcp::transport::prepare_streaming(
            &crate::server::mcp::transport::Request {
                method: &request.method,
                headers: &request.headers,
                body: &request.body,
            },
            password_hash.as_deref(),
            self.config.settings.mcp_streaming.unwrap_or(false),
        ) {
            Ok(Some(call)) => Some(Ok(call)),
            Ok(None) => None,
            Err(response) => Some(Err(response)),
        }
    }

    fn body_policy(&self, head: &http::Head) -> crate::server::http::BodyPolicy {
        routes::body_policy(head, &self.config.limits)
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::sync::{Arc, Mutex};

    use crate::face::uichannel::EventBus;
    use crate::platform::config::Config;
    use crate::platform::paths::Paths;
    use crate::platform::settings::Settings;
    use crate::server::auth::lan::Lan;
    use crate::server::http::hooks::PreBodyRoutes;
    use crate::server::http::model as http;
    use crate::wire::contract::rawhub::RawHub;

    use super::*;

    fn faces() -> PreBodyFaces {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = crate::platform::test_support::temp_dir().join(format!(
            "svcd-prebody-unit-{}-{}",
            std::process::id(),
            nanos
        ));
        PreBodyFaces {
            config: Arc::new(Config::with_paths(
                Settings::default(),
                None,
                Paths::under(&root),
            )),
            bus: Arc::new(EventBus::new()),
        }
    }

    fn lan() -> Arc<Mutex<Lan>> {
        let bind = "127.0.0.1:0".parse().expect("bind");
        Arc::new(Mutex::new(Lan::new(bind, bind, false, None)))
    }

    fn head(method: &str, path: &str) -> http::Head {
        http::Head {
            method: method.to_string(),
            path: path.to_string(),
            version: "HTTP/1.1".to_string(),
            headers: Vec::new(),
        }
    }

    #[test]
    fn claim_owns_the_three_pre_body_families() {
        let faces = faces();
        let lan = lan();
        let raw = RawHub::new(faces.config.limits.clone());
        let peer: IpAddr = "127.0.0.1".parse().expect("peer");

        for (method, path) in [
            ("GET", "/plugin-ui/demo/"),
            ("POST", "/api/v1/plugins"),
            ("POST", "/api/v1/import"),
            ("POST", "/api/v1/blobs"),
        ] {
            faces
                .claim(&head(method, path), peer, &lan, raw.clone())
                .unwrap_or_else(|| panic!("{method} {path} must be claimed"));
        }

        assert!(faces
            .claim(&head("GET", "/api/v1/version"), peer, &lan, raw)
            .is_none());
    }
}
