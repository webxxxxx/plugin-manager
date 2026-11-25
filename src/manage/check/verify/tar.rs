//! ustar entry validation: archive order, declared-file sizes, hashes, modes
//! and the payload rules that need the extracted bytes.

use super::*;

pub(super) fn verify_bytes_mode(
    bytes: &[u8],
    mode: SignatureMode,
    explicit: Option<&VerifyingKey>,
    trust_dir: &Path,
) -> Result<(Package, SignatureInfo), ProblemError> {
    let mut tar = Tar::new(bytes);

    let manifest_header = tar
        .next()?
        .ok_or_else(|| Problem::Invalid.error("manifest.json is missing"))?;
    if manifest_header.path != "manifest.json" {
        return Err(Problem::Invalid.error("manifest.json must be the first entry"));
    }
    let manifest_bytes = manifest_header
        .data
        .ok_or_else(|| Problem::Invalid.error("manifest.json must be a regular file"))?
        .to_vec();

    let mut signature_bytes = Vec::new();
    let mut signature = SignatureInfo {
        state: "missing",
        key_id: None,
        trusted: false,
    };
    let mut first_file = None;
    match tar.next()? {
        Some(header) if header.path == "signature.json" => {
            let data = header
                .data
                .ok_or_else(|| Problem::Invalid.error("signature.json must be a regular file"))?;
            signature = check_signature(&manifest_bytes, data, mode, explicit, trust_dir)?;
            signature_bytes = data.to_vec();
        }
        Some(header) if mode == SignatureMode::Required => {
            return Err(Problem::Invalid.error(unsigned_package_message(Some(&header.path))));
        }
        // Insecure installs and inspection accept a package whose second entry
        // is already a payload file; it stays in the archive and is validated
        // like any other `files[]` entry below.
        Some(header) => first_file = Some(header),
        None if mode == SignatureMode::Required => {
            return Err(Problem::Invalid.error(unsigned_package_message(None)));
        }
        None => {}
    }

    let manifest = parse_manifest(&manifest_bytes)?;
    validate_manifest(&manifest)?;
    // Inspection deliberately skips the host-platform gate: a package built
    // for another target is still a well-formed package, and the caller gets
    // `host.matchesHost` as a fact.
    if mode != SignatureMode::Inspect {
        check_platform(&manifest)?;
    }
    let declared = validate_declared_files(&manifest)?;

    let mut files = Vec::new();
    let mut seen = BTreeMap::new();
    let mut headers = Vec::new();
    if let Some(header) = first_file {
        headers.push(header);
    }
    for header in tar {
        headers.push(header?);
    }
    for header in headers {
        if header.kind == b'5' {
            continue;
        }
        let entry = declared.get(&header.path).ok_or_else(|| {
            Problem::Invalid.error(format!("file is not declared in files[]: {}", header.path))
        })?;
        if seen.insert(header.path.clone(), ()).is_some() {
            return Err(Problem::Invalid.error(format!("duplicate tar entry: {}", header.path)));
        }
        let data = header.data.ok_or_else(|| {
            Problem::Invalid.error(format!("{} must be a regular file", header.path))
        })?;
        if data.len() as u64 != entry.size {
            return Err(Problem::Invalid.error(format!(
                "size mismatch for {}: manifest {}, archive {}",
                entry.path,
                entry.size,
                data.len()
            )));
        }
        if hex_sha256(data) != entry.sha256 {
            return Err(Problem::Invalid.error(format!("sha256 mismatch for {}", entry.path)));
        }
        if header.mode & 0o777 != entry.mode & 0o777 {
            return Err(Problem::Invalid.error(format!("mode mismatch for {}", entry.path)));
        }
        files.push(FileData {
            path: header.path,
            data: data.to_vec(),
            mode: header.mode & 0o777,
        });
    }

    if seen.len() != declared.len() {
        let missing: Vec<&str> = declared
            .keys()
            .filter(|path| !seen.contains_key(*path))
            .map(String::as_str)
            .collect();
        return Err(Problem::Invalid.error(format!("missing files: {}", missing.join(", "))));
    }
    if files.is_empty() {
        return Err(Problem::Invalid.error("package has no payload files"));
    }
    #[cfg(unix)]
    check_hook_shebangs(&manifest, &files)?;
    let borrowed: Vec<(&str, &[u8])> = files
        .iter()
        .map(|file| (file.path.as_str(), file.data.as_slice()))
        .collect();
    validate_script_payload(&manifest, &borrowed)?;
    check_icon_magic(&manifest, &borrowed)?;

    Ok((
        Package {
            manifest,
            manifest_bytes,
            signature_bytes,
            files,
        },
        signature,
    ))
}

/// The install-time half of the "executable hook" rule: the Unix exec bit comes
/// from the manifest, the shebang has to be read from the extracted file.
#[cfg(unix)]
pub(crate) fn check_hook_shebangs(
    manifest: &Manifest,
    files: &[FileData],
) -> Result<(), ProblemError> {
    for (name, path) in [
        (
            "postInstall",
            manifest
                .hooks
                .as_ref()
                .and_then(|hooks| hooks.post_install.as_deref()),
        ),
        (
            "preRemove",
            manifest
                .hooks
                .as_ref()
                .and_then(|hooks| hooks.pre_remove.as_deref()),
        ),
    ] {
        let Some(path) = path else { continue };
        let Some(file) = files.iter().find(|file| file.path == path) else {
            continue;
        };
        if !file.data.starts_with(b"#!") {
            return Err(
                Problem::Invalid.error(format!("{} must start with a shebang: {}", name, path))
            );
        }
        if file.data.contains(&b'\r') {
            return Err(Problem::Invalid.error(format!(
                "{} must use LF line endings (the CRLF shebang fails to spawn): {}",
                name, path
            )));
        }
    }
    Ok(())
}

/// True when the payload bytes are a native executable (ELF, Mach-O or PE).
/// Anything else is treated as a script and needs a declared interpreter.
pub fn is_native_binary(bytes: &[u8]) -> bool {
    const MAGICS: [&[u8]; 10] = [
        b"\x7fELF",
        b"\xfe\xed\xfa\xce",
        b"\xfe\xed\xfa\xcf",
        b"\xce\xfa\xed\xfe",
        b"\xcf\xfa\xed\xfe",
        b"\xca\xfe\xba\xbe",
        b"\xbe\xba\xfe\xca",
        b"\xca\xfe\xba\xbf",
        b"\xbf\xba\xfe\xca",
        b"MZ",
    ];
    MAGICS.iter().any(|magic| bytes.starts_with(magic))
}

/// A declared interpreter needs a script to run: a native-only payload is
/// spawned directly, so the declaration would be silently ignored. The reverse
/// is not an error — a shebang script or a `.cmd` payload starts without an
/// interpreter block (ADR-0002, revised when the schema tiers collapsed).
pub fn validate_script_payload(
    manifest: &Manifest,
    files: &[(&str, &[u8])],
) -> Result<(), ProblemError> {
    if manifest.interpreter.is_none() {
        return Ok(());
    }
    let mut executables = vec![manifest.exec.path.as_str()];
    if let Some(lifecycle) = &manifest.lifecycle {
        for hook in [&lifecycle.start, &lifecycle.stop, &lifecycle.status]
            .into_iter()
            .flatten()
        {
            executables.push(hook.path.as_str());
        }
    }
    let mut scripts = Vec::new();
    for path in executables {
        let Some((_, data)) = files.iter().find(|(candidate, _)| *candidate == path) else {
            // A missing payload file is reported by the declaration checks.
            continue;
        };
        if !is_native_binary(data) {
            scripts.push(path.to_string());
        }
    }
    if scripts.is_empty() {
        return Err(
            Problem::Invalid.error("interpreter is declared but the payload has no script to run")
        );
    }
    Ok(())
}

/// The icon formats a package may ship, checked by magic bytes: the daemon has
/// no image library and no reason to decode a bitmap, only to refuse a file
/// whose bytes are not what the manifest calls them.
fn icon_magic(format: &str) -> Option<[u8; 4]> {
    match format {
        "png" => Some([0x89, b'P', b'N', b'G']),
        "ico" => Some([0x00, 0x00, 0x01, 0x00]),
        "icns" => Some(*b"icns"),
        _ => None,
    }
}

/// `icons` paths are checked against `files[]` by the manifest rules; the magic
/// bytes can only be checked where the payload exists.
pub fn check_icon_magic(manifest: &Manifest, files: &[(&str, &[u8])]) -> Result<(), ProblemError> {
    let Some(icons) = &manifest.icons else {
        return Ok(());
    };
    for (format, path) in icons.entries() {
        let Some((_, data)) = files.iter().find(|(candidate, _)| *candidate == path) else {
            // A missing payload file is reported by the declaration checks.
            continue;
        };
        let magic = icon_magic(format).expect("icons.entries only yields png/ico/icns");
        if !data.starts_with(&magic) {
            return Err(Problem::Invalid.error(format!(
                "icons.{} is not a {} file: magic bytes mismatch in {}",
                format, format, path
            )));
        }
    }
    Ok(())
}

/// The one `files[]` rule: reserved names, duplicate paths, setuid/setgid and
/// the file-count ceiling. Every install path calls this after
/// [`validate_manifest`], so `load_staged` cannot drift from `verify`.
pub(crate) fn validate_declared_files(
    manifest: &Manifest,
) -> Result<BTreeMap<String, FileEntry>, ProblemError> {
    let mut declared = BTreeMap::new();
    for entry in &manifest.files {
        check_path(&entry.path)?;
        if entry.path == "manifest.json" || entry.path == "signature.json" {
            return Err(Problem::Invalid.error("manifest.json/signature.json cannot be in files[]"));
        }
        if declared.insert(entry.path.clone(), entry.clone()).is_some() {
            return Err(Problem::Invalid.error(format!("duplicate files[] entry: {}", entry.path)));
        }
        if entry.mode & 0o6000 != 0 {
            return Err(
                Problem::Invalid.error(format!("setuid/setgid is not allowed: {}", entry.path))
            );
        }
        if declared.len() > MAX_FILES {
            return Err(Problem::TooLarge.error("too many files"));
        }
    }
    Ok(declared)
}
