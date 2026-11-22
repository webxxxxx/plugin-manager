//! `plugin bundle`: pack a local directory into a signed, canonical `.spk`.
//!
//! The artifact defaults to `<name>-<version>-<platform>.spk`: the whole target
//! triple, because the architecture prefix alone cannot distinguish musl from
//! gnu (or other ABIs sharing an arch). An `--out` directory keeps that name;
//! an `--out` file path is used verbatim.
//!
//! The directory is the source of truth for `files[]`: every regular file is
//! hashed, sized and mode-checked here, then the manifest is serialized with
//! sorted keys and no trailing newline before it is signed. An optional
//! `manifest.json` supplies author fields such as capabilities and arguments;
//! `files[]` is always recomputed.

use std::fs;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::Signer;

use crate::kernel::failure::{Problem, ProblemError};
use crate::manage as package;

mod manifest;
mod scan;
mod signing;

pub use manifest::Options;
use manifest::{assemble_manifest, read_skeleton};
use scan::{collect, valid_platform};
use signing::{default_key_id, read_private_key};

#[cfg(test)]
use ed25519_dalek::SigningKey;
#[cfg(all(test, unix))]
use scan::is_sparse;
#[cfg(test)]
use serde_json::Value;

pub fn bundle(options: &Options) -> Result<PathBuf, ProblemError> {
    let dir = fs::canonicalize(&options.dir).map_err(|error| {
        Problem::Invalid.error(format!("cannot read {}: {}", options.dir.display(), error))
    })?;
    if !dir.is_dir() {
        return Err(Problem::Invalid.error(format!("not a directory: {}", dir.display())));
    }
    if options.unsigned && (options.key.is_some() || options.key_id.is_some()) {
        return Err(Problem::Invalid.error("--unsigned cannot be combined with --key or --key-id"));
    }
    let key_path = match (options.key.as_deref(), options.unsigned) {
        (_, true) => None,
        (Some(key), false) => {
            let key = fs::canonicalize(key).map_err(|error| {
                Problem::Internal.error(format!("read key {}: {}", key.display(), error))
            })?;
            if key.starts_with(&dir) {
                return Err(Problem::Invalid
                    .error("the signing key must not be inside the bundle directory"));
            }
            Some(key)
        }
        (None, false) => {
            return Err(Problem::Invalid.error(
                "bundle requires --key <ed25519-private.pem> (or --unsigned for local builds)",
            ))
        }
    };
    if let Some(platform) = &options.platform {
        if !valid_platform(platform) {
            return Err(Problem::UnsupportedPlatform
                .error(format!("invalid target triple: {:?}", platform)));
        }
    }

    let mut sources = collect(&dir)?;
    if sources.is_empty() {
        return Err(Problem::Invalid.error("directory contains no files to bundle"));
    }
    if sources.len() > package::MAX_FILES {
        return Err(Problem::Invalid.error(format!(
            "too many files to bundle (limit {})",
            package::MAX_FILES
        )));
    }
    let total: u64 = sources.iter().map(|source| source.data.len() as u64).sum();
    if total > package::MAX_PACKAGE_BYTES {
        return Err(Problem::TooLarge.error(format!(
            "package exceeds {} MiB",
            package::MAX_PACKAGE_BYTES / (1024 * 1024)
        )));
    }

    let skeleton = read_skeleton(&dir)?;
    let (manifest_bytes, parsed) =
        assemble_manifest(&dir, &mut sources, skeleton.unwrap_or_default(), options)?;

    // The filename carries the version and the whole target triple: `x86_64`
    // alone is shared by the musl and gnu packages, so an arch-only name
    // cannot identify which host may install the file. A directory passed to
    // `-o` keeps that default name, so an output directory can never collect
    // unversioned artifacts that overwrite each other.
    let default_name = format!("{}-{}-{}.spk", parsed.name, parsed.version, parsed.platform);
    let path = match &options.out {
        Some(out) if out.is_dir() => out.join(&default_name),
        Some(out) => out.clone(),
        None => PathBuf::from(&default_name),
    };
    let absolute = absolute_output(&path)?;
    if absolute.starts_with(&dir) {
        return Err(Problem::Invalid.error("package output must be outside the bundle directory"));
    }

    let signature_bytes = match &key_path {
        None => None,
        Some(key_path) => {
            let key_id = match &options.key_id {
                Some(key_id) => key_id.clone(),
                None => default_key_id(key_path)?,
            };
            if !package::valid_key_id(&key_id) {
                return Err(
                    Problem::Invalid.error(format!("invalid keyId: {} (pass --key-id)", key_id))
                );
            }
            let signing_key = read_private_key(key_path)?;
            let signature = signing_key.sign(&manifest_bytes);
            let signature = serde_json::json!({
                "alg": "ed25519",
                "keyId": key_id,
                "sig": BASE64.encode(signature.to_bytes()),
            });
            Some(serde_json::to_vec(&signature).map_err(|error| {
                Problem::Invalid.error(format!("serialize signature: {}", error))
            })?)
        }
    };

    let mut entries: Vec<(&str, &[u8], u32)> = vec![("manifest.json", &manifest_bytes, 0o644)];
    if let Some(signature_bytes) = &signature_bytes {
        entries.push(("signature.json", signature_bytes.as_slice(), 0o644));
    }
    for source in &sources {
        entries.push((source.path.as_str(), source.data.as_slice(), source.mode));
    }
    let archive = package::write_tar(&entries)?;
    if archive.len() as u64 > package::MAX_PACKAGE_BYTES {
        return Err(Problem::TooLarge.error(format!(
            "package exceeds {} MiB",
            package::MAX_PACKAGE_BYTES / (1024 * 1024)
        )));
    }

    fs::write(&path, &archive)
        .map_err(|error| Problem::Internal.error(format!("write {}: {}", path.display(), error)))?;

    Ok(path)
}

fn absolute_output(path: &Path) -> Result<PathBuf, ProblemError> {
    let absolute = std::path::absolute(path).map_err(|error| {
        Problem::Invalid.error(format!("resolve {}: {}", path.display(), error))
    })?;
    let parent = match absolute.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let parent = fs::canonicalize(parent).map_err(|error| {
        Problem::Invalid.error(format!("resolve {}: {}", path.display(), error))
    })?;
    Ok(match absolute.file_name() {
        Some(name) => parent.join(name),
        None => parent,
    })
}

#[cfg(test)]
mod tests;
