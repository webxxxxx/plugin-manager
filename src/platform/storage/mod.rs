//! Content-addressed blob store for payloads that do not belong in a frame.
//!
//! A tool argument may carry `{"$blob":"sha256:<hex>"}`. The daemon resolves
//! that to a path inside this store before forwarding it to a plugin, so a
//! multi-megabyte file never has to fit in a JSON-RPC frame. Blobs can also be
//! uploaded and downloaded through the management API.

mod gc;
mod meta;
#[cfg(feature = "http")]
mod part;
mod path;
#[cfg(feature = "http")]
mod upload;

pub(crate) use gc::{cleanup, release, release_id};
#[cfg(feature = "http")]
pub(crate) use gc::{forget_blob, forget_file};
#[cfg(any(feature = "http", test))]
pub(crate) use meta::Meta;
pub(crate) use meta::{materialize_result, resolve_args};
#[cfg(feature = "http")]
pub(crate) use part::{upload_parts, PartInfo, PartWriter};
#[cfg(any(feature = "http", test))]
pub(crate) use path::touch;
#[cfg(feature = "http")]
pub(crate) use upload::{
    abort_upload, complete_file, complete_upload, create_upload, file_manifest,
    require_upload_kind, Upload, UploadKind,
};

pub(crate) use path::incoming_dir;
#[cfg(any(feature = "http", test))]
pub(crate) use path::locate;
#[cfg(feature = "http")]
pub(crate) use path::valid_upload_id;
#[cfg(test)]
pub(crate) use path::{file_dir, hex, path, put, read, upload_dir};
#[cfg(test)]
pub(crate) use upload::read_upload_kind;

/// Why a storage operation failed (#151).
///
/// Storage has its own taxonomy: an upload session is not a management object,
/// and this module sits below the kernel (`platform/storage` in the target
/// layout), so it must not depend on `Problem`. The face maps each of these
/// onto the single management table (`Invalid`/`Conflict`/`TooLarge`/
/// `Internal`) plus 404 for `NotFound`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The upload session or stored object the request named is not there.
    NotFound,
    /// The request contradicts the session's commit mode.
    Conflict,
    /// The request or the archive is malformed.
    Invalid,
    /// The body or the stored object exceeds a hard limit.
    TooLarge,
    /// The store could not do what was asked.
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub kind: Kind,
    pub detail: String,
}

impl Error {
    pub fn new(kind: Kind, detail: impl Into<String>) -> Error {
        Error {
            kind,
            detail: detail.into(),
        }
    }

    pub fn invalid(detail: impl Into<String>) -> Error {
        Error::new(Kind::Invalid, detail)
    }

    pub fn not_found(detail: impl Into<String>) -> Error {
        Error::new(Kind::NotFound, detail)
    }

    pub fn conflict(detail: impl Into<String>) -> Error {
        Error::new(Kind::Conflict, detail)
    }

    pub fn too_large(detail: impl Into<String>) -> Error {
        Error::new(Kind::TooLarge, detail)
    }

    pub fn internal(detail: impl Into<String>) -> Error {
        Error::new(Kind::Internal, detail)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

pub const MAX_BLOB_BYTES: u64 = 512 * 1024 * 1024;

/// A file bundle is made of parts; the existing blob cap is the per-part cap.
#[cfg(any(feature = "http", test))]
pub const MAX_FILE_PART_BYTES: u64 = MAX_BLOB_BYTES;

/// Safety net for one logical file; the benchmark ISO is 1.59 GiB.
#[cfg(feature = "http")]
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024 * 1024;

#[cfg(feature = "http")]
pub const MAX_PART_NUMBER: u32 = 10_000;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    use serde_json::json;
    use sha2::{Digest, Sha256};

    use crate::platform::paths::Paths;

    /// Each test derives its own tree from a temporary root, so no process
    /// environment is read and two tests cannot race.
    fn test_paths() -> Paths {
        let paths = crate::platform::test_support::paths("blob-store");
        std::fs::create_dir_all(&paths.temp_dir).expect("blob root");
        paths
    }

    #[test]
    fn blobs_are_content_addressed_and_readable() {
        let paths = test_paths();
        let first = put(&paths, b"hello").expect("put");
        let second = put(&paths, b"hello").expect("put");
        assert_eq!(first, second);
        assert_eq!(read(&paths, &first.id).expect("read"), b"hello");
        assert!(path(&paths, &first.id).expect("path").is_file());
    }

    #[test]
    fn a_storage_failure_names_its_kind() {
        let paths = test_paths();
        let missing = "0".repeat(32);
        let error = read_upload_kind(&paths, &missing).expect_err("no session");
        assert_eq!(error.kind, Kind::NotFound);

        let error = upload_dir(&paths, "nope").expect_err("bad id");
        assert_eq!(error.kind, Kind::Invalid);
    }

    #[test]
    fn invalid_ids_are_rejected() {
        let paths = test_paths();
        for id in ["", "sha256:", "sha256:xyz", "../x", "sha256:../x"] {
            assert!(read(&paths, id).is_err(), "{id}");
        }
    }

    #[test]
    fn a_plugin_blob_result_becomes_a_resource_link() {
        let paths = test_paths();
        let incoming = incoming_dir(&paths);
        std::fs::create_dir_all(&incoming).expect("incoming");
        let source = incoming.join("report.txt");
        std::fs::write(&source, b"result body").expect("result");
        let mut result = json!({
            "$blobPath": source.to_string_lossy(),
            "name": "report.txt",
            "mimeType": "text/plain"
        });
        materialize_result(&paths, &mut result).expect("materialize");
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][1]["type"], "resource_link");
        assert!(result["structuredContent"]["$blob"]
            .as_str()
            .expect("blob id")
            .starts_with("sha256:"));
        assert!(!source.exists());
        let id = result["structuredContent"]["$blob"]
            .as_str()
            .expect("blob id");
        assert_eq!(read(&paths, id).expect("read"), b"result body");
    }

    #[test]
    fn cleanup_removes_old_files_and_keeps_fresh_ones() {
        let paths = test_paths();
        let incoming = incoming_dir(&paths);
        std::fs::create_dir_all(&incoming).expect("incoming");
        let old = incoming.join("old.bin");
        let fresh = incoming.join("fresh.bin");
        std::fs::write(&old, b"old").expect("old");
        std::fs::write(&fresh, b"fresh").expect("fresh");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&old)
            .expect("open old");
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(SystemTime::now() - Duration::from_secs(3 * 60 * 60)),
        )
        .expect("old mtime");
        let removed = cleanup(&paths, Duration::from_secs(2 * 60 * 60)).expect("cleanup");
        assert!(removed >= 1, "removed {removed}");
        assert!(!old.exists());
        assert!(fresh.exists());
    }

    #[test]
    fn blob_refs_are_resolved_to_paths_in_arguments() {
        let paths = test_paths();
        let meta = put(&paths, b"file body").expect("put");
        let mut args = json!({"input": {"$blob": meta.id}, "other": 1});
        resolve_args(&paths, &mut args).expect("resolve");
        let input = &args["input"];
        assert_eq!(input["$blob"], meta.id);
        assert_eq!(input["size"], 9);
        assert!(input["path"].as_str().expect("path").contains("temp"));
    }

    #[cfg(feature = "http")]
    #[test]
    fn part_writer_refuses_a_cross_kind_session() {
        let paths = test_paths();
        let upload = create_upload(&paths, UploadKind::File).expect("file upload");
        let error = match PartWriter::begin(&paths, &upload, 1, UploadKind::Blob) {
            Ok(_) => panic!("blob part accepted a file session"),
            Err(error) => error,
        };
        assert!(error.detail.contains("kind mismatch"), "{error}");
        abort_upload(&paths, &upload, UploadKind::File).expect("abort file session");
    }

    #[cfg(feature = "http")]
    #[test]
    fn cleanup_removes_a_stale_file_bundle_as_one_unit() {
        let paths = test_paths();
        let upload = create_upload(&paths, UploadKind::File).expect("file upload");
        let mut writer =
            PartWriter::begin(&paths, &upload, 1, UploadKind::File).expect("part writer");
        writer.write(b"hello").expect("part write");
        writer.finish().expect("part finish");
        let meta = complete_file(&paths, &upload, None, None, None).expect("complete file");
        let directory = file_dir(&paths, &meta.id).expect("file dir");
        let manifest = directory.join("manifest.json");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&manifest)
            .expect("open manifest");
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(SystemTime::now() - Duration::from_secs(3 * 60 * 60)),
        )
        .expect("old manifest mtime");
        let removed = cleanup(&paths, Duration::from_secs(2 * 60 * 60)).expect("cleanup");
        assert!(removed >= 1);
        assert!(!directory.exists());
    }

    /// #302: a consumer that only holds the id releases the entry without
    /// re-hashing it, and both shapes (a blob file, a bundle directory that
    /// shares the digest) go away.
    #[cfg(feature = "http")]
    #[test]
    fn release_by_id_removes_a_consumed_entry() {
        let paths = test_paths();
        let meta = put(&paths, b"one-shot").expect("put");
        let blob = locate(&paths, &meta.id).expect("blob path");
        assert!(blob.is_file());
        assert_eq!(release_id(&paths, &meta.id), 1);
        assert!(!blob.exists());
        assert_eq!(
            release_id(&paths, &meta.id),
            0,
            "a second release is a no-op"
        );

        let upload = create_upload(&paths, UploadKind::File).expect("file upload");
        let mut writer =
            PartWriter::begin(&paths, &upload, 1, UploadKind::File).expect("part writer");
        writer.write(b"hello").expect("part write");
        writer.finish().expect("part finish");
        let bundle = complete_file(&paths, &upload, None, None, None).expect("complete file");
        let directory = file_dir(&paths, &bundle.id).expect("file dir");
        assert!(directory.is_dir());
        assert_eq!(release_id(&paths, &bundle.id), 1);
        assert!(!directory.exists());
    }

    #[cfg(feature = "http")]
    #[test]
    fn file_bundles_resolve_to_a_manifest_and_size() {
        let paths = test_paths();
        let upload = create_upload(&paths, UploadKind::File).expect("file upload");
        for (number, bytes) in [(1u32, b"hello ".as_slice()), (2u32, b"world!")] {
            let mut writer =
                PartWriter::begin(&paths, &upload, number, UploadKind::File).expect("part writer");
            writer.write(bytes).expect("part write");
            writer.finish().expect("part finish");
        }
        let meta = complete_file(
            &paths,
            &upload,
            None,
            Some("hello.txt".to_string()),
            Some("text/plain".to_string()),
        )
        .expect("complete file");
        assert_eq!(meta.size, 12);
        assert_eq!(
            meta.id,
            format!("sha256:{}", hex(&Sha256::digest(b"hello world!")))
        );

        let mut args = json!({"input": {"$file": meta.id}});
        resolve_args(&paths, &mut args).expect("resolve file");
        let input = &args["input"];
        assert_eq!(input["size"], 12);
        assert_eq!(input["name"], "hello.txt");
        assert_eq!(input["mimeType"], "text/plain");
        assert!(input["manifest"]
            .as_str()
            .expect("manifest")
            .ends_with("manifest.json"));
        assert!(file_dir(&paths, &meta.id).expect("file dir").is_dir());
    }
}
