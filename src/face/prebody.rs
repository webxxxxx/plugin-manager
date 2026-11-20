//! 读 body 之前的路由：插件 UI、包上传、blob/file 上传。
//!
//! server 的 poll loop 持有 [`PreBodyRoutes`] trait object，所以 trait 由
//! `server/http` 定义、face 实现、compose 构造。

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use crate::platform::config::Config;
use crate::server::auth::lan::Lan;
use crate::server::http::model as http;

pub(crate) struct PreBodyFaces {
    /// Read through the trait object by both drivers: the poll loop on Unix and
    /// the thread-per-connection driver elsewhere (#205).
    pub(crate) config: Arc<Config>,
}

impl crate::server::http::hooks::PreBodyRoutes for PreBodyFaces {
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
            crate::wire::contract::rawhub::RawHub::ui_hub(raw_hub),
        ))
    }

    fn package_upload(
        &self,
        head: &http::Head,
        lan: &Arc<Mutex<Lan>>,
    ) -> Option<Result<crate::server::http::Exchange, Vec<u8>>> {
        crate::face::install::target(head)?;
        Some(crate::face::install::exchange(
            head,
            lan,
            self.config.clone(),
        ))
    }

    fn blob_upload(
        &self,
        head: &http::Head,
        lan: &Arc<Mutex<Lan>>,
    ) -> Option<Result<crate::server::http::Exchange, Vec<u8>>> {
        crate::face::blob_upload::upload_target(head)?;
        Some(crate::face::blob_upload::exchange(
            head,
            lan,
            self.config.paths.clone(),
        ))
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
        ) {
            Ok(Some(call)) => Some(Ok(call)),
            Ok(None) => None,
            Err(response) => Some(Err(response)),
        }
    }

    fn body_policy(&self, head: &http::Head) -> crate::server::http::BodyPolicy {
        crate::server::http::server_body_policy(head, &self.config.limits)
    }
}
