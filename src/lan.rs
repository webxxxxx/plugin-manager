//! LAN state, admin token, sessions and login throttling.
//!
//! The token is generated with OS randomness and is equivalent to root. The
//! default state is loopback-only and LAN disabled.

use std::collections::HashMap;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64;
use base64::Engine;
use serde::{Deserialize, Serialize};

const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const FAILURE_DELAY: Duration = Duration::from_secs(1);
const MAX_FAILURES: u32 = 10;
const LOCK_TIME: Duration = Duration::from_secs(5 * 60);

pub const COOKIE_SESSION: &str = "svcd_session";
pub const COOKIE_API: &str = "svcd_api";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LanFile {
    enabled: bool,
    bind: String,
}

#[derive(Debug, Clone)]
pub struct LanStatus {
    pub enabled: bool,
    pub bind: IpAddr,
    pub token_set: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    None,
    Bearer,
    Cookie,
}

pub struct Lan {
    pub enabled: bool,
    pub bind: IpAddr,
    token: Option<String>,
    sessions: HashMap<String, Instant>,
    failures: u32,
    locked_until: Option<Instant>,
}

impl Lan {
    pub fn load(config: &Path) -> Self {
        let file = fs::read(config.join("lan.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<LanFile>(&bytes).ok());
        let enabled = file.as_ref().map(|file| file.enabled).unwrap_or(false);
        let bind = file
            .as_ref()
            .and_then(|file| file.bind.parse::<IpAddr>().ok())
            .unwrap_or_else(|| "127.0.0.1".parse().expect("loopback"));
        let mut token = fs::read_to_string(config.join("lan-token"))
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        if enabled && token.is_none() {
            token = Some(new_token());
            if let Some(token) = &token {
                let _ = write_private(&token_path(config), token.as_bytes());
            }
        }
        Lan {
            enabled,
            bind,
            token,
            sessions: HashMap::new(),
            failures: 0,
            locked_until: None,
        }
    }

    pub fn status(&self) -> LanStatus {
        LanStatus {
            enabled: self.enabled,
            bind: self.bind,
            token_set: self.token.is_some(),
        }
    }

    /// Enable LAN. Returns a token only when one was just generated.
    pub fn enable(&mut self, bind: IpAddr) -> Option<String> {
        let generated = if self.token.is_none() {
            Some(new_token())
        } else {
            None
        };
        if let Some(token) = &generated {
            self.token = Some(token.clone());
        }
        self.enabled = true;
        self.bind = bind;
        generated
    }

    pub fn disable(&mut self) {
        self.enabled = false;
        self.bind = "127.0.0.1".parse().expect("loopback");
    }

    pub fn rotate(&mut self) -> String {
        let token = new_token();
        self.token = Some(token.clone());
        self.sessions.clear();
        token
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    pub fn save(&self, config: &Path) -> Result<(), String> {
        let file = LanFile {
            enabled: self.enabled,
            bind: self.bind.to_string(),
        };
        let json = serde_json::to_vec(&file).map_err(|error| error.to_string())?;
        write_private(&config.join("lan.json"), &json)?;
        if let Some(token) = &self.token {
            write_private(&token_path(config), token.as_bytes())?;
        }
        Ok(())
    }

    /// `Ok(session)` on success; sleeps 1s on failure and locks after 10.
    pub fn login(&mut self, token: &str) -> Result<String, String> {
        if let Some(until) = self.locked_until {
            if Instant::now() < until {
                return Err("too many failed logins; try again later".into());
            }
        }
        if self.token.as_deref() == Some(token) {
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
            Err("invalid token".into())
        }
    }

    pub fn authenticate(&mut self, headers: &[(String, String)]) -> Result<AuthKind, ()> {
        if !self.enabled {
            return Ok(AuthKind::None);
        }
        if let Some(bearer) = header(headers, "authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim)
        {
            if self.token.as_deref() == Some(bearer) {
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
}

fn token_path(config: &Path) -> PathBuf {
    config.join("lan-token")
}

fn write_private(path: &Path, data: &[u8]) -> Result<(), String> {
    fs::write(path, data).map_err(|error| format!("write {}: {}", path.display(), error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("chmod {}: {}", path.display(), error))?;
    }
    Ok(())
}

fn new_token() -> String {
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

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
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
                "{}={}; Path=/plugin-ui/; HttpOnly; SameSite=Strict; Max-Age=86400",
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
                "{}=; Path=/plugin-ui/; HttpOnly; SameSite=Strict; Max-Age=0",
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
    if crate::install::is_privileged() {
        Ok(())
    } else {
        Err("root / Administrator is required for LAN changes".into())
    }
}
