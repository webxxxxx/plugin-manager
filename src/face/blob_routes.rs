//! Session metadata endpoints for blob and file uploads.
//!
//! The streaming upload and download paths moved to the S4 face modules:
//! `face::blob_upload` stages request bodies through the blocking pool, and
//! `face::blob` serves downloads. This module only projects the ordinary
//! buffered JSON endpoints: create, status, complete and abort.

use serde_json::{json, Value};

use crate::blob;
use crate::http;
use crate::paths::Paths;

use super::support;


pub fn create_upload(paths: &Paths, kind: blob::UploadKind) -> Vec<u8> {
    match blob::create_upload(paths, kind) {
        Ok(id) => {
            let prefix = match kind {
                blob::UploadKind::Blob => "blobs",
                blob::UploadKind::File => "file",
            };
            let body = serde_json::to_vec(&json!({
                "uploadId": id,
                "partsUrl": format!("/api/v1/{}/uploads/{}/parts", prefix, id),
            }))
            .unwrap_or_default();
            http::response_with_headers(
                200,
                "application/json",
                &[(
                    "Location".to_string(),
                    format!("/api/v1/{}/uploads/{}", prefix, id),
                )],
                &body,
            )
        }
        Err(error) => support::storage_response(&error),
    }
}

pub fn upload_status(paths: &Paths, kind: blob::UploadKind, id: &str) -> Vec<u8> {
    if !blob::valid_upload_id(id) {
        return http::json_error(400, "bad_request", "invalid upload id");
    }
    match blob::upload_parts(paths, id, kind) {
        Ok(parts) => {
            let total: u64 = parts.iter().map(|part| part.size).sum();
            let parts: Vec<Value> = parts
                .iter()
                .map(|part| {
                    json!({
                        "partNumber": part.number,
                        "size": part.size,
                        "etag": part.etag,
                    })
                })
                .collect();
            let body = serde_json::to_vec(&json!({
                "uploadId": id,
                "parts": parts,
                "totalSize": total,
            }))
            .unwrap_or_default();
            http::response(200, "application/json", &body)
        }
        Err(error) => support::storage_response(&error),
    }
}

pub fn file_upload_complete(paths: &Paths, id: &str, body: &[u8]) -> Vec<u8> {
    if !blob::valid_upload_id(id) {
        return http::json_error(400, "bad_request", "invalid upload id");
    }
    if let Some(response) = kind_rejection(paths, id, blob::UploadKind::File) {
        return response;
    }
    let mut requested = None;
    let mut name = None;
    let mut mime_type = None;
    if !body.is_empty() {
        let value: Value = match serde_json::from_slice(body) {
            Ok(value) => value,
            Err(_) => return http::json_error(400, "bad_request", "body must be JSON"),
        };
        if let Some(parts) = value.get("parts") {
            let Some(parts) = parts.as_array() else {
                return http::json_error(400, "bad_request", "parts must be an array");
            };
            let mut list = Vec::with_capacity(parts.len());
            for part in parts {
                let Some(number) = part.get("partNumber").and_then(Value::as_u64) else {
                    return http::json_error(400, "bad_request", "partNumber is required");
                };
                if number == 0 || number > blob::MAX_PART_NUMBER as u64 {
                    return http::json_error(
                        400,
                        "bad_request",
                        "partNumber must be between 1 and 10000",
                    );
                }
                let etag = part.get("etag").and_then(Value::as_str).map(str::to_string);
                list.push((number as u32, etag));
            }
            requested = Some(list);
        }
        name = value
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        mime_type = value
            .get("mimeType")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if name
            .as_ref()
            .map(|value| value.len() > 255)
            .unwrap_or(false)
            || mime_type
                .as_ref()
                .map(|value| value.len() > 255)
                .unwrap_or(false)
        {
            return http::json_error(400, "bad_request", "name or mimeType is too long");
        }
        let header_safe = |value: &str| !value.chars().any(char::is_control);
        if name
            .as_deref()
            .map(|value| !header_safe(value))
            .unwrap_or(false)
            || mime_type
                .as_deref()
                .map(|value| !header_safe(value))
                .unwrap_or(false)
        {
            return http::json_error(
                400,
                "bad_request",
                "name or mimeType contains control characters",
            );
        }
    }
    match blob::complete_file(paths, id, requested, name, mime_type) {
        Ok(meta) => {
            let body = serde_json::to_vec(&json!({
                "id": meta.id,
                "size": meta.size,
                "name": meta.name,
                "mimeType": meta.mime_type,
            }))
            .unwrap_or_default();
            let headers = vec![
                ("ETag".to_string(), format!("\"{}\"", meta.id)),
                ("Location".to_string(), format!("/api/v1/file/{}", meta.id)),
            ];
            http::response_with_headers(200, "application/json", &headers, &body)
        }
        Err(error) => support::storage_response(&error),
    }
}

pub fn abort_upload(paths: &Paths, kind: blob::UploadKind, id: &str) -> Vec<u8> {
    if !blob::valid_upload_id(id) {
        return http::json_error(400, "bad_request", "invalid upload id");
    }
    match blob::abort_upload(paths, id, kind) {
        Ok(()) => http::response(
            200,
            "application/json",
            &serde_json::to_vec(&json!({"ok": true})).unwrap_or_default(),
        ),
        Err(error) => support::storage_response(&error),
    }
}

fn kind_rejection(paths: &Paths, id: &str, kind: blob::UploadKind) -> Option<Vec<u8>> {
    match blob::require_upload_kind(paths, id, kind) {
        Ok(()) => None,
        Err(error) => Some(support::storage_response(&error)),
    }
}

pub fn complete_upload(paths: &Paths, id: &str, body: &[u8]) -> Vec<u8> {
    if !blob::valid_upload_id(id) {
        return http::json_error(400, "bad_request", "invalid upload id");
    }
    if let Some(response) = kind_rejection(paths, id, blob::UploadKind::Blob) {
        return response;
    }
    let requested = if body.is_empty() {
        None
    } else {
        let value: Value = match serde_json::from_slice(body) {
            Ok(value) => value,
            Err(_) => return http::json_error(400, "bad_request", "body must be JSON"),
        };
        let Some(parts) = value.get("parts").and_then(Value::as_array) else {
            return http::json_error(400, "bad_request", "parts is required");
        };
        let mut requested = Vec::with_capacity(parts.len());
        for part in parts {
            let Some(number) = part.get("partNumber").and_then(Value::as_u64) else {
                return http::json_error(400, "bad_request", "partNumber is required");
            };
            if number == 0 || number > blob::MAX_PART_NUMBER as u64 {
                return http::json_error(
                    400,
                    "bad_request",
                    "partNumber must be between 1 and 10000",
                );
            }
            let etag = part.get("etag").and_then(Value::as_str).map(str::to_string);
            requested.push((number as u32, etag));
        }
        Some(requested)
    };
    match blob::complete_upload(paths, id, requested) {
        Ok(meta) => {
            let body =
                serde_json::to_vec(&json!({"id": meta.id, "size": meta.size})).unwrap_or_default();
            let headers = vec![
                ("ETag".to_string(), format!("\"{}\"", meta.id)),
                ("Location".to_string(), format!("/api/v1/blobs/{}", meta.id)),
            ];
            http::response_with_headers(200, "application/json", &headers, &body)
        }
        Err(error) => support::storage_response(&error),
    }
}
