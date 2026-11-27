//! Sessions, login throttling and the credential checks behind the S9
//! [`super::admission::Admission`] seam.
//!
//! `bind` chooses the listener and also decides whether authentication is
//! required: loopback is open, a non-loopback bind requires `auth_passwd`.
//! Sessions are random values held in memory. Plugin UI tokens are short-lived,
//! bound to one plugin id, and travel in the plugin URL path.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64;
use base64::Engine;

use crate::server::http::model::header;

const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const PLUGIN_TOKEN_TTL: Duration = Duration::from_secs(60 * 60);
const MAX_PLUGIN_TOKENS: usize = 256;
const FAILURE_DELAY: Duration = Duration::from_secs(1);
const MAX_FAILURES: u32 = 10;
const LOCK_TIME: Duration = Duration::from_secs(5 * 60);

pub const COOKIE_SESSION: &str = "svcd_session";
pub const COOKIE_API: &str = "svcd_api";

#[derive(Debug, Clone)]
pub struct LanStatus {
    pub bind: SocketAddr,
    pub requested_bind: SocketAddr,
    pub require_auth: bool,
    pub password_set: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    None,
    Bearer,
    Cookie,
}

struct PluginToken {
    plugin: String,
    expires: Instant,
}

pub struct Lan {
    pub bind: SocketAddr,
    pub requested_bind: SocketAddr,
    pub require_auth: bool,
    password_hash: Option<String>,
    sessions: HashMap<String, Instant>,
    plugin_tokens: HashMap<String, PluginToken>,
    failures: u32,
    locked_until: Option<Instant>,
}

impl Lan {
    pub fn new(
        bind: SocketAddr,
        requested_bind: SocketAddr,
        require_auth: bool,
        password_hash: Option<String>,
    ) -> Self {
        Lan {
            bind,
            requested_bind,
            require_auth,
            password_hash,
            sessions: HashMap::new(),
            plugin_tokens: HashMap::new(),
            failures: 0,
            locked_until: None,
        }
    }

    pub fn status(&self) -> LanStatus {
        LanStatus {
            bind: self.bind,
            requested_bind: self.requested_bind,
            require_auth: self.require_auth,
            password_set: self.password_hash.is_some(),
        }
    }

    pub fn set_password(&mut self, hash: String) {
        self.password_hash = Some(hash);
        self.sessions.clear();
    }

    pub fn password_hash(&self) -> Option<&str> {
        self.password_hash.as_deref()
    }

    /// `Ok(session)` on success; sleeps 1s on failure and locks after 10.
    pub fn login(&mut self, password: &str) -> Result<String, String> {
        if let Some(until) = self.locked_until {
            if Instant::now() < until {
                return Err("too many failed logins; try again later".into());
            }
        }
        let Some(hash) = self.password_hash.as_deref() else {
            return Err("no password configured; run svc lan passwd".into());
        };
        if crate::platform::auth::verify_password(password, hash) {
            self.failures = 0;
            self.locked_until = None;
            let session = new_token();
            self.sessions
                .insert(session.clone(), Instant::now() + SESSION_TTL);
            return Ok(session);
        }
        std::thread::sleep(FAILURE_DELAY);
        self.failures = self.failures.saturating_add(1);
        if self.failures >= MAX_FAILURES {
            self.failures = 0;
            self.locked_until = Some(Instant::now() + LOCK_TIME);
            Err("too many failed logins; locked for 5 minutes".into())
        } else {
            Err("invalid password".into())
        }
    }

    /// Mint a session without a password check. Only the vendor-backend
    /// handoff (`/vendor-login`) uses it: the caller has already established
    /// that the browser's own router session is logged in, and this session is
    /// shaped exactly like a password login.
    pub fn issue_session(&mut self) -> String {
        let session = new_token();
        self.sessions
            .insert(session.clone(), Instant::now() + SESSION_TTL);
        session
    }

    pub fn authenticate(&mut self, headers: &[(String, String)]) -> Result<AuthKind, ()> {
        if !self.require_auth {
            return Ok(AuthKind::None);
        }
        if let Some(bearer) = header(headers, "authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim)
        {
            if self
                .password_hash
                .as_deref()
                .map(|hash| crate::platform::auth::verify_password_cached(bearer, hash))
                .unwrap_or(false)
            {
                return Ok(AuthKind::Bearer);
            }
        }
        let cookies = header(headers, "cookie").unwrap_or("");
        for (name, value) in cookie_pairs(cookies) {
            if name != COOKIE_API && name != COOKIE_SESSION {
                continue;
            }
            if let Some(expiry) = self.sessions.get(value) {
                if *expiry > Instant::now() {
                    self.sessions
                        .insert(value.to_string(), Instant::now() + SESSION_TTL);
                    return Ok(AuthKind::Cookie);
                }
            }
        }
        Err(())
    }

    pub fn logout(&mut self, headers: &[(String, String)]) {
        let cookies = header(headers, "cookie").unwrap_or("");
        for (name, value) in cookie_pairs(cookies) {
            if name == COOKIE_API || name == COOKIE_SESSION {
                self.sessions.remove(value);
            }
        }
    }

    /// Mint a one-hour token for one plugin's UI. The token is part of the
    /// plugin URL; it is not a cookie, so another plugin cannot be handed it
    /// by the browser just because the request URL belongs to that plugin.
    pub fn issue_plugin_token(&mut self, plugin: &str) -> String {
        let now = Instant::now();
        self.plugin_tokens.retain(|_, token| token.expires > now);
        if self.plugin_tokens.len() >= MAX_PLUGIN_TOKENS {
            if let Some(oldest) = self
                .plugin_tokens
                .iter()
                .min_by_key(|(_, token)| token.expires)
                .map(|(key, _)| key.clone())
            {
                self.plugin_tokens.remove(&oldest);
            }
        }
        let token = new_token();
        self.plugin_tokens.insert(
            token.clone(),
            PluginToken {
                plugin: plugin.to_string(),
                expires: now + PLUGIN_TOKEN_TTL,
            },
        );
        token
    }

    /// A token is valid only for the plugin it was minted for.
    pub fn validate_plugin_token(&mut self, token: &str, plugin: &str) -> bool {
        let Some(entry) = self.plugin_tokens.get(token) else {
            return false;
        };
        if entry.plugin != plugin {
            return false;
        }
        if entry.expires <= Instant::now() {
            self.plugin_tokens.remove(token);
            return false;
        }
        true
    }
}

pub fn plugin_token_ttl_secs() -> u64 {
    PLUGIN_TOKEN_TTL.as_secs()
}

pub(crate) fn new_token() -> String {
    let mut bytes = [0u8; 32];
    if getrandom::getrandom(&mut bytes).is_err() {
        // Fall back to a time-derived token only if the OS source is unavailable.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        bytes[..16].copy_from_slice(&nanos.to_le_bytes());
    }
    BASE64.encode(bytes)
}

fn cookie_pairs(header: &str) -> impl Iterator<Item = (&str, &str)> {
    header.split(';').filter_map(|pair| {
        let (name, value) = pair.trim().split_once('=')?;
        Some((name.trim(), value.trim()))
    })
}

pub fn cookie_headers(session: &str) -> Vec<(String, String)> {
    vec![
        (
            "Set-Cookie".into(),
            format!(
                "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=86400",
                COOKIE_SESSION, session
            ),
        ),
        (
            "Set-Cookie".into(),
            format!(
                "{}={}; Path=/api/v1/; HttpOnly; SameSite=Strict; Max-Age=86400",
                COOKIE_API, session
            ),
        ),
    ]
}

pub fn clear_cookie_headers() -> Vec<(String, String)> {
    vec![
        (
            "Set-Cookie".into(),
            format!(
                "{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0",
                COOKIE_SESSION
            ),
        ),
        (
            "Set-Cookie".into(),
            format!(
                "{}=; Path=/api/v1/; HttpOnly; SameSite=Strict; Max-Age=0",
                COOKIE_API
            ),
        ),
    ]
}

pub fn require_root() -> Result<(), String> {
    #[cfg(debug_assertions)]
    if std::env::var_os("SVCD_TEST_UNPRIVILEGED").is_some() {
        return Ok(());
    }
    if crate::platform::is_privileged() || crate::platform::paths::user_mode() {
        Ok(())
    } else {
        Err("root / Administrator is required for auth changes".into())
    }
}
