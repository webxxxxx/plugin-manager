//! 厂商后台会话换 svcd 会话：路由器后台（同一台设备的 :80）已经登录时，
//! 注入到厂商页面的菜单脚本用它让 :8787 的控制台免登录。
//!
//! # 为什么要有这个端点
//!
//! 设备上厂商后台在 `http://<设备>/`（80 端口），注入的菜单把 svcd 控制台
//! 放进 `http://<设备>:8787/` 的 iframe。端口不同、主机相同：控制台自己看不到
//! 厂商的 `SID`，浏览器也只会在响应允许携带凭据时才把 :8787 的 cookie 用上。
//!
//! 厂商会话绑客户端 IP，守护进程代查不可行（从设备本机带同一个 SID 请求
//! `/?_type=vueData&…` 返回 404，只有浏览器里才是 SUCC）。所以“是否已登录”
//! 只能由浏览器里的页面判断；那个页面就是我们注入的菜单脚本，它需要 svcd 的
//! 只有这一条入口。
//!
//! # 信任模型
//!
//! `POST /vendor-login` 是认证入口（和 `/login` 同级），跑在准入之前，所以它
//! 必须自己证明来源：
//!
//! - `Origin` 必须与 `Host` 的**主机名相同**，且端口为空 / 80 / 443 —— 也就是
//!   设备自己的 web 根，不接受其它应用端口（插件页面、代理面板都不行）；
//! - 必须带 `X-Vendor-OK: 1`：自定义头让调用变成 CORS 预检，只有可信 `Origin`
//!   能通过预检，别的源上的页面到不了处理器；
//! - 浏览器侧脚本只在厂商自己的 `loginStatus` 用用户会话答 SUCC 之后才发。
//!
//! 本端点**不**去问厂商 httpd：它信任的是“厂商同源页面在浏览器里验证过”。
//! 能伪造这条断言的前提是已经能在厂商后台跑 JS —— 那已经等于拿到了后台。
//!
//! 换到的会话与密码登录同形：`svcd_session`（Path=`/`）与 `svcd_api`
//! （Path=`/api/v1/`），TTL 24h、使用续期；`POST /logout` 同样撤销。

use std::sync::{Arc, Mutex};

use crate::server::auth::lan::{self, Lan};
use crate::server::http::engine::HttpRequest;
use crate::server::http::model as http;

/// `/vendor-login`：`POST` 换会话，`OPTIONS` 是浏览器预检。
pub(crate) fn respond(request: &HttpRequest, lan: &Arc<Mutex<Lan>>) -> Vec<u8> {
    match request.method.as_str() {
        "OPTIONS" => preflight(request),
        "POST" => exchange(request, lan),
        _ => http::method_not_allowed("OPTIONS, POST"),
    }
}

fn preflight(request: &HttpRequest) -> Vec<u8> {
    let Some(origin) = trusted_origin(&request.headers) else {
        return http::json_error(403, "forbidden", "vendor origin required");
    };
    http::response_with_headers(
        204,
        "text/plain; charset=utf-8",
        &cors_headers(&origin),
        b"",
    )
}

fn exchange(request: &HttpRequest, lan: &Arc<Mutex<Lan>>) -> Vec<u8> {
    let Some(origin) = trusted_origin(&request.headers) else {
        return http::json_error(403, "forbidden", "vendor origin required");
    };
    let asserted = http::header(&request.headers, "x-vendor-ok")
        .map(str::trim)
        .unwrap_or("");
    if asserted != "1" {
        return json_with_cors(
            403,
            &origin,
            br#"{"ok":false,"err":"vendor session not asserted"}"#,
        );
    }
    let session = match lan.lock() {
        Ok(mut state) => state.issue_session(),
        Err(_) => {
            return json_with_cors(
                500,
                &origin,
                br#"{"ok":false,"err":"auth state unavailable"}"#,
            )
        }
    };
    info!("vendor handoff: session issued for origin {}", origin);
    let mut headers = cors_headers(&origin);
    headers.extend(lan::cookie_headers(&session));
    http::response_with_headers(200, "application/json", &headers, br#"{"ok":true}"#)
}

fn json_with_cors(status: u16, origin: &str, body: &[u8]) -> Vec<u8> {
    http::response_with_headers(status, "application/json", &cors_headers(origin), body)
}

/// The vendor origin: same host as this request, on the device's web root port
/// (absent, 80 or 443), over http or https. Returns the origin to echo back in
/// `Access-Control-Allow-Origin`.
fn trusted_origin(headers: &[(String, String)]) -> Option<String> {
    let origin = http::header(headers, "origin")?.trim();
    let host = http::header(headers, "host")?.trim();
    if origin.is_empty() || host.is_empty() {
        return None;
    }
    let (scheme, authority) = origin.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let (origin_host, origin_port) = split_authority(authority);
    if !matches!(origin_port, None | Some("80") | Some("443")) {
        return None;
    }
    let (bound_host, _) = split_authority(host);
    if origin_host.is_empty() || !origin_host.eq_ignore_ascii_case(bound_host) {
        return None;
    }
    Some(origin.to_string())
}

/// Split `[::1]:8787`, `192.168.5.1:8787` or `router.lan` into host and port.
fn split_authority(authority: &str) -> (&str, Option<&str>) {
    if let Some(rest) = authority.strip_prefix('[') {
        if let Some((host, tail)) = rest.split_once(']') {
            let port = tail.strip_prefix(':').filter(|port| !port.is_empty());
            return (host, port);
        }
    }
    match authority.rsplit_once(':') {
        Some((host, port))
            if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            (host, Some(port))
        }
        _ => (authority, None),
    }
}

fn cors_headers(origin: &str) -> Vec<(String, String)> {
    vec![
        ("Vary".into(), "Origin".into()),
        ("Access-Control-Allow-Origin".into(), origin.to_string()),
        ("Access-Control-Allow-Credentials".into(), "true".into()),
        (
            "Access-Control-Allow-Methods".into(),
            "OPTIONS, POST".into(),
        ),
        ("Access-Control-Allow-Headers".into(), "X-Vendor-OK".into()),
        ("Access-Control-Max-Age".into(), "600".into()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::auth::lan::{AuthKind, COOKIE_SESSION};

    fn lan(require_auth: bool) -> Arc<Mutex<Lan>> {
        let bind = "127.0.0.1:8787".parse().expect("bind");
        let hash = crate::platform::auth::hash_password("secret").expect("hash");
        Arc::new(Mutex::new(Lan::new(bind, bind, require_auth, Some(hash))))
    }

    fn request(method: &str, headers: &[(String, String)]) -> HttpRequest {
        HttpRequest {
            method: method.to_string(),
            path: "/vendor-login".to_string(),
            version: "HTTP/1.1".to_string(),
            headers: headers.to_vec(),
            body: Vec::new(),
        }
    }

    /// The default vendor request headers, with `headers` overriding by name
    /// (so a test can replace Origin or X-Vendor-OK rather than duplicate them).
    fn vendor(headers: &[(&str, &str)]) -> Vec<(String, String)> {
        let defaults = [
            ("Host", "192.168.5.1:8787"),
            ("Origin", "http://192.168.5.1"),
            ("X-Vendor-OK", "1"),
        ];
        let mut all: Vec<(String, String)> = defaults
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        for (key, value) in headers {
            match all
                .iter_mut()
                .find(|(existing, _)| existing.eq_ignore_ascii_case(key))
            {
                Some(pair) => pair.1 = value.to_string(),
                None => all.push((key.to_string(), value.to_string())),
            }
        }
        all
    }

    fn text(response: &[u8]) -> String {
        String::from_utf8_lossy(response).to_string()
    }

    fn cookie_value(response: &[u8], name: &str) -> Option<String> {
        text(response).lines().find_map(|line| {
            line.strip_prefix(&format!("Set-Cookie: {name}="))
                .map(|rest| rest.split(';').next().unwrap_or("").to_string())
        })
    }

    #[test]
    fn only_the_device_web_root_origin_is_trusted() {
        let headers = |origin: &str, host: &str| {
            vec![
                ("Origin".to_string(), origin.to_string()),
                ("Host".to_string(), host.to_string()),
            ]
        };
        for (origin, host) in [
            ("http://192.168.5.1", "192.168.5.1:8787"),
            ("http://192.168.5.1:80", "192.168.5.1:8787"),
            ("https://router.lan", "router.lan:8787"),
            ("http://[fd00::1]", "[fd00::1]:8787"),
        ] {
            assert_eq!(
                trusted_origin(&headers(origin, host)),
                Some(origin.to_string()),
                "{origin} for {host}"
            );
        }
        for (origin, host) in [
            ("http://192.168.5.1:8080", "192.168.5.1:8787"),
            ("http://192.168.5.1:8443", "192.168.5.1:8787"),
            ("http://192.168.5.2", "192.168.5.1:8787"),
            ("https://192.168.5.1", "192.168.5.1:"),
            ("file://192.168.5.1", "192.168.5.1:8787"),
            ("", "192.168.5.1:8787"),
        ] {
            assert_eq!(trusted_origin(&headers(origin, host)), None, "{origin}");
        }
    }

    #[test]
    fn a_vendor_request_mints_a_session_that_admission_accepts() {
        let lan = lan(true);
        let response = respond(&request("POST", &vendor(&[])), &lan);
        let head = text(&response);
        assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
        assert!(head.contains("Access-Control-Allow-Origin: http://192.168.5.1"));
        assert!(head.contains("Access-Control-Allow-Credentials: true"));
        let session = cookie_value(&response, COOKIE_SESSION).expect("session cookie");
        assert!(!session.is_empty());
        let mut state = lan.lock().expect("lan");
        assert_eq!(
            state.authenticate(&[("cookie".to_string(), format!("{COOKIE_SESSION}={session}"))]),
            Ok(AuthKind::Cookie)
        );
    }

    #[test]
    fn a_foreign_origin_cannot_mint_a_session() {
        let lan = lan(true);
        for origin in [
            "http://evil.example",
            "http://192.168.5.1:8080",
            "http://192.168.5.1:9090",
        ] {
            let response = respond(&request("POST", &vendor(&[("Origin", origin)])), &lan);
            let head = text(&response);
            assert!(head.starts_with("HTTP/1.1 403"), "{origin}: {}", head);
            assert!(!head.contains("Set-Cookie"), "{origin}: {}", head);
        }
    }

    #[test]
    fn the_assertion_header_is_required() {
        let lan = lan(true);
        let response = respond(&request("POST", &vendor(&[("X-Vendor-OK", "0")])), &lan);
        let head = text(&response);
        assert!(head.starts_with("HTTP/1.1 403"), "{}", head);
        assert!(!head.contains("Set-Cookie"), "{}", head);
    }

    #[test]
    fn the_preflight_answers_with_credentials_and_the_assertion_header() {
        let response = respond(&request("OPTIONS", &vendor(&[])), &lan(true));
        let head = text(&response);
        assert!(head.starts_with("HTTP/1.1 204"), "{}", head);
        assert!(head.contains("Access-Control-Allow-Origin: http://192.168.5.1"));
        assert!(head.contains("Access-Control-Allow-Credentials: true"));
        assert!(head.contains("Access-Control-Allow-Headers: X-Vendor-OK"));
        assert!(head.contains("Access-Control-Allow-Methods: OPTIONS, POST"));
    }

    #[test]
    fn other_methods_are_rejected() {
        let response = respond(&request("GET", &vendor(&[])), &lan(true));
        assert!(text(&response).starts_with("HTTP/1.1 405"));
    }
}
