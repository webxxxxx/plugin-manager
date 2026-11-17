//! Streaming HTTP upload for plugin packages.
//!
//! `POST /api/v1/plugins` used to buffer the body, write a temporary `.spk`,
//! and hand its path to the daemon. This module parses the tar as it arrives and
//! extracts the payload into a staging directory under `plugins_dir`; the
//! daemon then commits that directory with a rename.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::http;
use crate::lan::{AuthKind, Lan};
use crate::limits::Limits;
use crate::package;
use crate::paths;
use crate::settings;

pub fn route(head: &http::Head) -> bool {
    if head.method != "POST" {
        return false;
    }
    let path = head
        .path
        .split_once('?')
        .map(|(path, _)| path)
        .unwrap_or(head.path.as_str());
    path == "/api/v1/plugins"
}

pub fn serve(
    stream: &mut TcpStream,
    head: &http::Head,
    initial: &[u8],
    lan: &Arc<Mutex<Lan>>,
    config: &Path,
    limits: &Limits,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(limits.transfer_timeout));
    let _ = stream.set_write_timeout(Some(limits.transfer_timeout));

    let auth = match lan.lock() {
        Ok(mut state) => match state.authenticate(&head.headers) {
            Ok(auth) => auth,
            Err(()) => {
                let _ = write_error(stream, 401, "unauthorized", "authentication required");
                return;
            }
        },
        Err(_) => {
            let _ = write_error(stream, 500, "internal", "LAN state unavailable");
            return;
        }
    };
    if auth == AuthKind::Cookie && !http::csrf_ok(&head.headers) {
        let _ = write_error(
            stream,
            403,
            "forbidden",
            "cookie writes require Origin and X-Svcd-Csrf",
        );
        return;
    }
    let content_type = http::header(&head.headers, "content-type").unwrap_or("");
    let query = head
        .path
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or("");
    let strategy = query_value(query, "strategy").unwrap_or_else(|| "replace".into());
    let enable = query_bool(query, "enable").unwrap_or(true);
    let allow_hooks = query_bool(query, "allow_hooks").unwrap_or(false);
    let framing = match http::framing(&head.headers) {
        Ok(framing) => framing,
        Err(message) => {
            let _ = write_error(stream, 400, "bad_request", &message);
            return;
        }
    };
    if !content_type.starts_with("application/octet-stream") {
        drain_body(stream, initial, framing, limits.max_package_bytes);
        let _ = write_error(
            stream,
            400,
            "bad_request",
            "Content-Type must be application/octet-stream",
        );
        return;
    }
    if strategy != "keep-old" && strategy != "replace" {
        drain_body(stream, initial, framing, limits.max_package_bytes);
        let _ = write_error(
            stream,
            400,
            "bad_request",
            "strategy must be keep-old or replace",
        );
        return;
    }
    match framing {
        http::Framing::None | http::Framing::Length(0) => {
            let _ = write_error(stream, 400, "bad_request", "empty package body");
            return;
        }
        http::Framing::Length(length) if length > limits.max_package_bytes => {
            let _ = write_error(stream, 413, "payload_too_large", "package is too large");
            return;
        }
        _ => {}
    }
    if http::header(&head.headers, "expect")
        .map(|value| value.eq_ignore_ascii_case("100-continue"))
        .unwrap_or(false)
        && head.version.eq_ignore_ascii_case("HTTP/1.1")
    {
        let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }

    let plugins = match paths::plugins_dir().canonicalize() {
        Ok(path) => path,
        Err(_) => paths::plugins_dir(),
    };
    let _ = std::fs::create_dir_all(&plugins);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let staging = plugins.join(format!(".staging-{}-{}", std::process::id(), nanos));
    let _staging_guard = StagingGuard(staging.clone());

    let body = BodyReader::new(stream, initial, framing, limits.max_package_bytes);
    let staged = match package::install_stream(
        body,
        &staging,
        settings::allow_insecure_install(),
        limits.max_package_bytes,
    ) {
        Ok(staged) => staged,
        Err(message) => {
            let _ = std::fs::remove_dir_all(&staging);
            let status = if message.contains("too large") {
                413
            } else if message.contains("signature")
                || message.contains("platform")
                || message.contains("path")
                || message.contains("unsafe")
                || message.contains("setuid")
                || message.contains("checksum")
                || message.contains("declared")
                || message.contains("duplicate")
                || message.contains("missing")
                || message.contains("invalid")
            {
                400
            } else {
                500
            };
            let _ = write_error(stream, status, code_for(status), &message);
            return;
        }
    };
    let id = staged.manifest.name.clone();
    let version = staged.manifest.version.clone();
    let response = match crate::api::rpc(
        config,
        "plugin.install_staged",
        json!({
            "path": staging.to_string_lossy(),
            "id": id,
            "version": version,
            "strategy": strategy,
            "allowHooks": allow_hooks,
        }),
    ) {
        Ok(status) => {
            if !enable {
                let _ = crate::api::rpc(config, "svc.disable", json!({ "id": id }));
            }
            let summary = status.get("summary").cloned().unwrap_or(Value::Null);
            let body = serde_json::to_vec(&json!({
                "id": summary.get("id").cloned().unwrap_or(Value::Null),
                "version": summary.get("version").cloned().unwrap_or(Value::Null),
                "state": summary.get("state").cloned().unwrap_or(Value::Null),
            }))
            .unwrap_or_default();
            http::response(200, "application/json", &body)
        }
        Err(response) => {
            let _ = std::fs::remove_dir_all(&staging);
            response
        }
    };
    let _ = stream.write_all(&response);
    let _ = stream.flush();
}

fn write_error(stream: &mut TcpStream, status: u16, code: &str, message: &str) -> io::Result<()> {
    stream.write_all(&http::json_error(status, code, message))
}

/// Consume a request body that will not be parsed, so Windows closes the
/// connection with a clean FIN instead of a RST while the client is still
/// uploading.
fn drain_body(stream: &mut TcpStream, initial: &[u8], framing: http::Framing, max: u64) {
    let mut body = BodyReader::new(stream, initial, framing, max);
    let _ = io::copy(&mut body, &mut io::sink());
}

fn code_for(status: u16) -> &'static str {
    match status {
        400 => "bad_request",
        413 => "payload_too_large",
        500 => "internal",
        _ => "error",
    }
}

fn query_value(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if http::percent_decode(name, true) == key {
            return Some(http::percent_decode(value, true));
        }
    }
    None
}

fn query_bool(query: &str, key: &str) -> Option<bool> {
    let value = query_value(query, key)?;
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

pub fn import_route(head: &http::Head) -> bool {
    if head.method != "POST" {
        return false;
    }
    let path = head
        .path
        .split_once('?')
        .map(|(path, _)| path)
        .unwrap_or(head.path.as_str());
    path == "/api/v1/import"
}

pub fn serve_import(
    stream: &mut TcpStream,
    head: &http::Head,
    initial: &[u8],
    lan: &Arc<Mutex<Lan>>,
    config: &Path,
    limits: &Limits,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(limits.transfer_timeout));
    let _ = stream.set_write_timeout(Some(limits.transfer_timeout));
    let auth = match lan.lock() {
        Ok(mut state) => match state.authenticate(&head.headers) {
            Ok(auth) => auth,
            Err(()) => {
                let _ = write_error(stream, 401, "unauthorized", "authentication required");
                return;
            }
        },
        Err(_) => {
            let _ = write_error(stream, 500, "internal", "LAN state unavailable");
            return;
        }
    };
    if auth == AuthKind::Cookie && !http::csrf_ok(&head.headers) {
        let _ = write_error(
            stream,
            403,
            "forbidden",
            "cookie writes require Origin and X-Svcd-Csrf",
        );
        return;
    }
    let query = head
        .path
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or("");
    let config_id = query_value(query, "config_id");
    let no_config = query_bool(query, "no_config").unwrap_or(false);
    let force = query_bool(query, "force").unwrap_or(false);
    let framing = match http::framing(&head.headers) {
        Ok(framing) => framing,
        Err(message) => {
            let _ = write_error(stream, 400, "bad_request", &message);
            return;
        }
    };
    match framing {
        http::Framing::None | http::Framing::Length(0) => {
            let _ = write_error(stream, 400, "bad_request", "empty import body");
            return;
        }
        http::Framing::Length(length) if length > limits.max_package_bytes => {
            let _ = write_error(stream, 413, "payload_too_large", "import is too large");
            return;
        }
        _ => {}
    }
    if http::header(&head.headers, "expect")
        .map(|value| value.eq_ignore_ascii_case("100-continue"))
        .unwrap_or(false)
        && head.version.eq_ignore_ascii_case("HTTP/1.1")
    {
        let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }

    let mut body = BodyReader::new(stream, initial, framing, limits.max_package_bytes);
    let mut prefix = Vec::new();
    let mut buffer = [0u8; 512];
    while prefix.len() < 512 {
        match body.read(&mut buffer[..512 - prefix.len()]) {
            Ok(0) => break,
            Ok(read) => prefix.extend_from_slice(&buffer[..read]),
            Err(error) => {
                let _ = write_error(stream, 400, "bad_request", &format!("read body: {}", error));
                return;
            }
        }
    }
    let first_path = match package::tar_header_path(&prefix) {
        Ok(path) => path,
        Err(_) => None,
    };

    let response = match first_path.as_deref() {
        Some("manifest.json") => {
            let staging = staging_path();
            match package::install_stream(
                PrefixReader {
                    prefix,
                    at: 0,
                    inner: body,
                },
                &staging,
                settings::allow_insecure_install(),
                limits.max_package_bytes,
            ) {
                Ok(staged) => match install_rpc(
                    config,
                    &staging,
                    &staged.manifest.name,
                    &staged.manifest.version,
                    "replace",
                ) {
                    Ok(status) => import_response(status, None, false, Vec::new()),
                    Err(response) => {
                        let _ = std::fs::remove_dir_all(&staging);
                        response
                    }
                },
                Err(message) => {
                    let _ = std::fs::remove_dir_all(&staging);
                    error_for(message)
                }
            }
        }
        Some("backup.json") => match import_backup(
            PrefixReader {
                prefix,
                at: 0,
                inner: body,
            },
            config,
            limits,
            no_config,
            force,
        ) {
            Ok(response) => response,
            Err(message) => error_for(message),
        },
        _ => {
            // Not a tar package: a raw config (or a malformed archive). Read it
            // into a small buffer and let the shared importer classify it.
            let mut data = prefix;
            let mut rest = Vec::new();
            let mut buffer = [0u8; 8192];
            loop {
                match body.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        rest.extend_from_slice(&buffer[..read]);
                        if (data.len() + rest.len()) as u64 > limits.max_body {
                            let _ = write_error(
                                stream,
                                413,
                                "payload_too_large",
                                "import is too large",
                            );
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = write_error(
                            stream,
                            400,
                            "bad_request",
                            &format!("read body: {}", error),
                        );
                        return;
                    }
                }
            }
            data.extend_from_slice(&rest);
            match crate::transfer::import(&data, config_id.as_deref(), no_config, force, false) {
                Ok(outcome) => import_outcome_json(outcome),
                Err(crate::transfer::Error::Usage(message)) => {
                    http::json_error(400, "bad_request", &message)
                }
                Err(crate::transfer::Error::Failed(message)) => crate::api::map_error(&message),
            }
        }
    };
    let _ = stream.write_all(&response);
    let _ = stream.flush();
}

fn staging_path() -> std::path::PathBuf {
    let plugins = paths::plugins_dir()
        .canonicalize()
        .unwrap_or_else(|_| paths::plugins_dir());
    let _ = std::fs::create_dir_all(&plugins);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    plugins.join(format!(".staging-{}-{}", std::process::id(), nanos))
}

fn install_rpc(
    config: &Path,
    staging: &Path,
    id: &str,
    version: &str,
    strategy: &str,
) -> Result<Value, Vec<u8>> {
    crate::api::rpc(
        config,
        "plugin.install_staged",
        json!({
            "path": staging.to_string_lossy(),
            "id": id,
            "version": version,
            "strategy": strategy,
        }),
    )
}

fn import_response(
    status: Value,
    config_id: Option<String>,
    config_applied: bool,
    warnings: Vec<String>,
) -> Vec<u8> {
    let summary = status.get("summary").cloned().unwrap_or(Value::Null);
    let mut body = json!({
        "ok": true,
        "configApplied": config_applied,
    });
    if !summary.is_null() {
        body["id"] = summary.get("id").cloned().unwrap_or(Value::Null);
        body["version"] = summary.get("version").cloned().unwrap_or(Value::Null);
        body["state"] = summary.get("state").cloned().unwrap_or(Value::Null);
    } else if let Some(id) = config_id {
        body["id"] = json!(id);
    }
    if !warnings.is_empty() {
        body["warnings"] = json!(warnings);
    }
    http::response(
        200,
        "application/json",
        &serde_json::to_vec(&body).unwrap_or_default(),
    )
}

fn import_outcome_json(outcome: crate::transfer::Outcome) -> Vec<u8> {
    let mut body = json!({
        "ok": true,
        "configApplied": outcome.config.is_some(),
    });
    if let Some(status) = outcome.status {
        let value = serde_json::to_value(status.summary).unwrap_or(Value::Null);
        body["id"] = value.get("id").cloned().unwrap_or(Value::Null);
        body["version"] = value.get("version").cloned().unwrap_or(Value::Null);
        body["state"] = value.get("state").cloned().unwrap_or(Value::Null);
    } else if let Some(id) = outcome.config {
        body["id"] = json!(id);
    }
    if !outcome.warnings.is_empty() {
        body["warnings"] = json!(outcome.warnings);
    }
    http::response(
        200,
        "application/json",
        &serde_json::to_vec(&body).unwrap_or_default(),
    )
}

fn error_for(message: String) -> Vec<u8> {
    let status = if message.contains("too large") {
        413
    } else if message.contains("signature")
        || message.contains("platform")
        || message.contains("path")
        || message.contains("unsafe")
        || message.contains("setuid")
        || message.contains("checksum")
        || message.contains("declared")
        || message.contains("duplicate")
        || message.contains("missing")
        || message.contains("invalid")
        || message.contains("backup")
    {
        400
    } else {
        500
    };
    http::json_error(status, code_for(status), &message)
}

fn read_entry_vec<R: Read>(
    tar: &mut package::StreamTar<R>,
    entry: &package::StreamEntry,
    cap: u64,
) -> Result<Vec<u8>, String> {
    if entry.size > cap {
        return Err(format!("{} is too large", entry.path));
    }
    let mut data = Vec::with_capacity(entry.size as usize);
    tar.copy_entry(entry, &mut |chunk| {
        data.extend_from_slice(chunk);
        Ok(())
    })?;
    Ok(data)
}

fn import_backup<R: Read>(
    reader: R,
    config: &Path,
    limits: &Limits,
    no_config: bool,
    force: bool,
) -> Result<Vec<u8>, String> {
    let mut outer = package::StreamTar::new(reader, limits.max_package_bytes);
    let backup_entry = outer
        .next_entry()?
        .ok_or_else(|| "backup.json is missing".to_string())?;
    if backup_entry.path != "backup.json" {
        return Err("backup.json must be the first entry".into());
    }
    let backup_bytes = read_entry_vec(&mut outer, &backup_entry, 64 * 1024)?;
    let backup: crate::plugin::Backup = serde_json::from_slice(&backup_bytes)
        .map_err(|error| format!("invalid backup.json: {}", error))?;
    if backup.schema != 1 {
        return Err(format!("unsupported backup schema {}", backup.schema));
    }
    let spk_entry = outer
        .next_entry()?
        .ok_or_else(|| "plugin.spk is missing".to_string())?;
    if spk_entry.path != "plugin.spk" {
        return Err("plugin.spk must be the second entry".into());
    }
    let staging = staging_path();
    let _staging_guard = StagingGuard(staging.clone());
    let staged = {
        let inner = outer.entry_reader(&spk_entry)?;
        package::install_stream(
            inner,
            &staging,
            settings::allow_insecure_install(),
            limits.max_package_bytes,
        )
    }?;
    if staged.manifest.name != backup.id {
        return Err(format!(
            "backup id {} does not match package id {}",
            backup.id, staged.manifest.name
        ));
    }
    if staged.manifest.version != backup.version {
        return Err(format!(
            "backup version {} does not match package version {}",
            backup.version, staged.manifest.version
        ));
    }
    let mut warnings = Vec::new();
    if let Some(current) = crate::plugin::current_version(&backup.id) {
        if current != backup.version {
            warnings.push(format!(
                "installed {} is {}, importing {}",
                backup.id, current, backup.version
            ));
        } else if !force {
            return Err(format!(
                "{} {} is already installed; use --force",
                backup.id, backup.version
            ));
        }
    }
    let config_entry = outer.next_entry()?;
    let config_data = match config_entry {
        Some(entry) if entry.path == "config" => {
            Some(read_entry_vec(&mut outer, &entry, 64 * 1024)?)
        }
        Some(entry) => return Err(format!("unexpected entry: {}", entry.path)),
        None => None,
    };
    if outer.next_entry()?.is_some() {
        return Err("unexpected entries after config".into());
    }
    outer.drain_to_end()?;
    if let Some(expected) = &backup.config_sha256 {
        let Some(config) = config_data.as_ref() else {
            return Err("backup declares a config hash but config is missing".into());
        };
        if &package::hex_sha256(config) != expected {
            return Err("backup config hash does not match".into());
        }
    }
    let applied = !no_config && config_data.is_some();
    let status = match install_rpc(
        config,
        &staging,
        &staged.manifest.name,
        &staged.manifest.version,
        "replace",
    ) {
        Ok(status) => status,
        Err(response) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Ok(response);
        }
    };
    if applied {
        crate::transfer::apply_config(&backup.id, config_data.as_ref().expect("config"), false)
            .map_err(|error| match error {
                crate::transfer::Error::Usage(message)
                | crate::transfer::Error::Failed(message) => message,
            })?;
    }
    Ok(import_response(status, Some(backup.id), applied, warnings))
}

struct StagingGuard(std::path::PathBuf);

impl Drop for StagingGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct PrefixReader<R: Read> {
    prefix: Vec<u8>,
    at: usize,
    inner: R,
}

impl<R: Read> Read for PrefixReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.at < self.prefix.len() {
            let take = std::cmp::min(buffer.len(), self.prefix.len() - self.at);
            buffer[..take].copy_from_slice(&self.prefix[self.at..self.at + take]);
            self.at += take;
            return Ok(take);
        }
        self.inner.read(buffer)
    }
}

struct BodyReader<'a> {
    stream: &'a mut TcpStream,
    initial: Vec<u8>,
    initial_at: usize,
    framing: http::Framing,
    remaining: u64,
    decoder: Option<http::ChunkedDecoder>,
    decoded: Vec<u8>,
    decoded_at: usize,
    done: bool,
    total: u64,
    max: u64,
}

impl<'a> BodyReader<'a> {
    fn new(stream: &'a mut TcpStream, initial: &[u8], framing: http::Framing, max: u64) -> Self {
        let remaining = match framing {
            http::Framing::Length(length) => length,
            _ => 0,
        };
        let decoder = match framing {
            http::Framing::Chunked => Some(http::ChunkedDecoder::new(max)),
            _ => None,
        };
        BodyReader {
            stream,
            initial: initial.to_vec(),
            initial_at: 0,
            framing,
            remaining,
            decoder,
            decoded: Vec::new(),
            decoded_at: 0,
            done: false,
            total: 0,
            max,
        }
    }

    fn read_stream(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.stream.read(buffer) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated request body",
                    ))
                }
                Ok(read) => return Ok(read),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

impl Read for BodyReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.initial_at < self.initial.len() {
            let mut take = std::cmp::min(buffer.len(), self.initial.len() - self.initial_at);
            if let http::Framing::Length(_) = self.framing {
                take = std::cmp::min(take as u64, self.remaining) as usize;
            }
            if take == 0 {
                return Ok(0);
            }
            buffer[..take].copy_from_slice(&self.initial[self.initial_at..self.initial_at + take]);
            self.initial_at += take;
            if let http::Framing::Length(_) = self.framing {
                self.remaining -= take as u64;
            }
            self.total = self.total.saturating_add(take as u64);
            return Ok(take);
        }
        match self.framing {
            http::Framing::None => Ok(0),
            http::Framing::Length(_) => {
                if self.remaining == 0 {
                    return Ok(0);
                }
                let take = std::cmp::min(buffer.len() as u64, self.remaining) as usize;
                let read = self.read_stream(&mut buffer[..take])?;
                self.remaining -= read as u64;
                self.total = self.total.saturating_add(read as u64);
                Ok(read)
            }
            http::Framing::Chunked => {
                if self.decoded_at < self.decoded.len() {
                    let take = std::cmp::min(buffer.len(), self.decoded.len() - self.decoded_at);
                    buffer[..take]
                        .copy_from_slice(&self.decoded[self.decoded_at..self.decoded_at + take]);
                    self.decoded_at += take;
                    return Ok(take);
                }
                if self.done {
                    return Ok(0);
                }
                let mut chunk = [0u8; 8192];
                loop {
                    let read = self.read_stream(&mut chunk)?;
                    self.total = self.total.saturating_add(read as u64);
                    if self.total > self.max {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "package is too large",
                        ));
                    }
                    let decoder = self.decoder.as_mut().expect("chunked decoder");
                    self.decoded.clear();
                    self.decoded_at = 0;
                    decoder
                        .feed(&chunk[..read], &mut |bytes| {
                            self.decoded.extend_from_slice(bytes);
                            Ok(())
                        })
                        .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
                    if !self.decoded.is_empty() {
                        self.done = decoder.is_done();
                        let take = std::cmp::min(buffer.len(), self.decoded.len());
                        buffer[..take].copy_from_slice(&self.decoded[..take]);
                        self.decoded_at = take;
                        return Ok(take);
                    }
                    if decoder.is_done() {
                        self.done = true;
                        return Ok(0);
                    }
                }
            }
        }
    }
}
