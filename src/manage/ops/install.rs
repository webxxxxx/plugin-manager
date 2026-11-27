//! 安装：把校验通过的包写进版本目录、流式 staging 与读取。
#![cfg_attr(not(feature = "http"), allow(dead_code))]
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::kernel::failure::{Problem, ProblemError};
#[cfg(unix)]
use crate::manage::check::verify::check_hook_shebangs;
use crate::manage::check::verify::{
    check_platform, check_signature, parse_manifest, unsigned_package_message,
    validate_declared_files, validate_manifest, validate_script_payload, SignatureMode,
};
use crate::manage::model::hex::hex_bytes;
use crate::manage::model::manifest::{FileData, FileEntry, Manifest, Package};
use crate::manage::model::ustar::{StreamEntry, TarFeed, TarSink};

impl Package {
    pub fn write_to(&self, root: &Path) -> Result<(), ProblemError> {
        fs::create_dir_all(root).map_err(|error| {
            Problem::Internal.error(format!("create {}: {}", root.display(), error))
        })?;
        write_one(&root.join("manifest.json"), &self.manifest_bytes, 0o644)?;
        // An insecure install may have arrived without a signature; keep the
        // version directory honest instead of writing an empty signature file.
        if !self.signature_bytes.is_empty() {
            write_one(&root.join("signature.json"), &self.signature_bytes, 0o644)?;
        }
        for file in &self.files {
            let path = root.join(&file.path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    Problem::Internal.error(format!("create {}: {}", parent.display(), error))
                })?;
            }
            write_one(&path, &file.data, file.mode)?;
        }
        Ok(())
    }

    /// Total bytes this package writes to disk: declared files plus the two
    /// package metadata files.
    pub fn unpacked_size(&self) -> u64 {
        let files: u64 = self.files.iter().map(|file| file.data.len() as u64).sum();
        files
            .saturating_add(self.manifest_bytes.len() as u64)
            .saturating_add(self.signature_bytes.len() as u64)
    }

    /// The manifest's `config` template, if declared.
    pub fn config_template(&self) -> Option<&FileData> {
        let path = self.manifest.config.as_deref().unwrap_or("config/config");
        self.files.iter().find(|file| file.path == path)
    }
}

/// A package received as a stream and extracted into a staging directory.
///
/// The tar archive itself is never stored: only the final files are written.
/// `signature_verified` is false for the insecure fallback.
pub struct StagedPackage {
    pub manifest: Manifest,
    pub signature_verified: bool,
    pub root: std::path::PathBuf,
}

/// A package installer driven by body chunks instead of a blocking reader.
///
/// It runs the whole validation sequence in one place: manifest first, the
/// signature before any payload byte, every entry checked against `files[]`,
/// then the staged payload. A caller feeds whatever bytes arrived, so no
/// temporary archive and no per-upload thread exist.
pub struct StagingInstaller {
    tar: TarFeed,
    sink: SpkSink,
    finished: bool,
}

impl StagingInstaller {
    /// `paths` decides where the tree goes: staging is created inside the root
    /// the package will be renamed into, and that root is only known once the
    /// manifest has been parsed (ADR-0030). `requested_root` is the operator's
    /// explicit choice (`--root` / `?root=`), which must be one of
    /// `plugins_dir`.
    pub fn new(
        paths: crate::platform::paths::Paths,
        allow_insecure: bool,
        max_bytes: u64,
        requested_root: Option<PathBuf>,
    ) -> StagingInstaller {
        StagingInstaller {
            tar: TarFeed::new(max_bytes),
            sink: SpkSink::new(paths, allow_insecure, requested_root),
            finished: false,
        }
    }

    /// Feed the next body chunk. The bytes may end anywhere: inside a tar
    /// header, an entry body or its padding.
    pub fn feed(&mut self, data: &[u8]) -> Result<(), ProblemError> {
        self.tar.feed(data, &mut self.sink)
    }

    /// The body ended: require the end-of-archive marker, check every declared
    /// file landed, and validate the staged payload. Called once.
    pub fn finish(&mut self) -> Result<StagedPackage, ProblemError> {
        if self.finished {
            return Err(Problem::Internal.error("package already finished"));
        }
        self.tar.finish()?;
        let staged = self.sink.finish()?;
        self.finished = true;
        Ok(staged)
    }
}

impl Drop for StagingInstaller {
    fn drop(&mut self) {
        // A rejected, disconnected or half-fed package must not leave a
        // staging directory behind. A finished install owns its directory and
        // the caller removes it if the daemon did not commit it.
        if !self.finished && !self.sink.staging.as_os_str().is_empty() {
            let _ = fs::remove_dir_all(&self.sink.staging);
        }
    }
}

struct SpkSink {
    /// The resolved paths: the roots it may stage into, plus the location
    /// index that says where an installed plugin already lives.
    paths: crate::platform::paths::Paths,
    /// The operator's explicit root, if the request named one.
    requested_root: Option<PathBuf>,
    /// Chosen at the first payload entry, when the manifest (and so the id and
    /// the package size) is known; empty before that.
    staging: PathBuf,
    allow_insecure: bool,
    /// Where signature verification looks up `<keyId>.pub`.
    trust_dir: PathBuf,
    manifest: Option<Manifest>,
    manifest_bytes: Vec<u8>,
    signature_bytes: Vec<u8>,
    signature_verified: bool,
    signature_checked: bool,
    payload_started: bool,
    declared: BTreeMap<String, FileEntry>,
    seen: BTreeSet<String>,
    staging_ready: bool,
    current: Option<Current>,
}

enum Current {
    Manifest(Vec<u8>),
    Signature(Vec<u8>),
    Payload {
        file: File,
        hasher: Sha256,
        declared: FileEntry,
        path: PathBuf,
        written: u64,
    },
    Directory,
}

impl SpkSink {
    fn new(
        paths: crate::platform::paths::Paths,
        allow_insecure: bool,
        requested_root: Option<PathBuf>,
    ) -> SpkSink {
        let trust_dir = paths.trust_dir.clone();
        SpkSink {
            paths,
            requested_root,
            staging: PathBuf::new(),
            allow_insecure,
            trust_dir,
            manifest: None,
            manifest_bytes: Vec::new(),
            signature_bytes: Vec::new(),
            signature_verified: false,
            signature_checked: false,
            payload_started: false,
            declared: BTreeMap::new(),
            seen: BTreeSet::new(),
            staging_ready: false,
            current: None,
        }
    }

    /// The root this package lands on. An installed plugin keeps its recorded
    /// root; a new one goes to the smallest volume that fits; an explicit root
    /// wins over the size rule but must be listed in `plugins_dir`.
    ///
    /// The question asked here is "does the **new tree** fit": the old copy is
    /// still on the volume until the commit, but its bytes are already outside
    /// the volume's free count, so adding them again would ask for two copies
    /// and refuse an upgrade that fits (#328: 12.7 MB new + 12 MB installed
    /// against 17.2 MB free).
    fn chosen_root(&self) -> Result<PathBuf, ProblemError> {
        let manifest = self.manifest.as_ref().ok_or_else(|| {
            Problem::Invalid.error("manifest.json must be the first entry of a package")
        })?;
        let fresh = staged_size(
            manifest,
            self.manifest_bytes.len(),
            self.signature_bytes.len(),
        );
        crate::manage::ops::install_plugin::install_root(
            &self.paths,
            manifest.id(),
            self.requested_root.as_deref(),
            fresh,
        )
    }

    fn prepare_staging(&mut self) -> Result<(), ProblemError> {
        if self.staging_ready {
            return Ok(());
        }
        let root = self.chosen_root()?;
        self.staging = staging_dir(&root);
        if self.staging.exists() {
            fs::remove_dir_all(&self.staging).map_err(|error| {
                Problem::Internal.error(format!("remove {}: {}", self.staging.display(), error))
            })?;
        }
        fs::create_dir_all(&self.staging).map_err(|error| {
            Problem::Internal.error(format!("create {}: {}", self.staging.display(), error))
        })?;
        write_one(
            &self.staging.join("manifest.json"),
            &self.manifest_bytes,
            0o644,
        )?;
        if !self.signature_bytes.is_empty() {
            write_one(
                &self.staging.join("signature.json"),
                &self.signature_bytes,
                0o644,
            )?;
        }
        self.staging_ready = true;
        Ok(())
    }

    fn start_payload(&mut self, entry: &StreamEntry) -> Result<(), ProblemError> {
        self.payload_started = true;
        if entry.kind == b'5' {
            self.current = Some(Current::Directory);
            return Ok(());
        }
        let declared = self
            .declared
            .get(&entry.path)
            .ok_or_else(|| {
                Problem::Invalid.error(format!("file is not declared in files[]: {}", entry.path))
            })?
            .clone();
        if !self.seen.insert(entry.path.clone()) {
            return Err(Problem::Invalid.error(format!("duplicate tar entry: {}", entry.path)));
        }
        if entry.size != declared.size {
            return Err(Problem::Invalid.error(format!(
                "size mismatch for {}: manifest {}, archive {}",
                entry.path, declared.size, entry.size
            )));
        }
        if entry.mode & 0o777 != declared.mode & 0o777 {
            return Err(Problem::Invalid.error(format!("mode mismatch for {}", entry.path)));
        }
        self.prepare_staging()?;
        let path = self.staging.join(&entry.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                Problem::Internal.error(format!("create {}: {}", parent.display(), error))
            })?;
        }
        let file = File::create(&path).map_err(|error| {
            Problem::Internal.error(format!("create {}: {}", path.display(), error))
        })?;
        self.current = Some(Current::Payload {
            file,
            hasher: Sha256::new(),
            declared,
            path,
            written: 0,
        });
        Ok(())
    }

    fn finish(&mut self) -> Result<StagedPackage, ProblemError> {
        let manifest = self
            .manifest
            .clone()
            .ok_or_else(|| Problem::Invalid.error("manifest.json is missing"))?;
        if !self.signature_checked && !self.allow_insecure {
            return Err(Problem::Invalid.error(unsigned_package_message(None)));
        }
        self.prepare_staging()?;
        if self.seen.len() != self.declared.len() {
            let missing: Vec<&str> = self
                .declared
                .keys()
                .filter(|path| !self.seen.contains(*path))
                .map(String::as_str)
                .collect();
            return Err(Problem::Invalid.error(format!("missing files: {}", missing.join(", "))));
        }
        if self.seen.is_empty() {
            return Err(Problem::Invalid.error("package has no payload files"));
        }
        validate_staged_payload(&manifest, &self.staging)?;
        Ok(StagedPackage {
            manifest,
            signature_verified: self.signature_verified,
            root: self.staging.clone(),
        })
    }
}

impl TarSink for SpkSink {
    fn entry_start(&mut self, entry: &StreamEntry) -> Result<(), ProblemError> {
        if self.manifest.is_none() {
            if entry.path != "manifest.json" {
                return Err(Problem::Invalid.error("manifest.json must be the first entry"));
            }
            if entry.size > 1024 * 1024 {
                return Err(Problem::TooLarge.error("manifest.json is too large"));
            }
            self.current = Some(Current::Manifest(Vec::with_capacity(entry.size as usize)));
            return Ok(());
        }
        if !self.signature_checked && !self.payload_started {
            if entry.path == "signature.json" {
                if entry.size > 64 * 1024 {
                    return Err(Problem::TooLarge.error("signature.json is too large"));
                }
                self.current = Some(Current::Signature(Vec::with_capacity(entry.size as usize)));
                return Ok(());
            }
            if !self.allow_insecure {
                return Err(Problem::Invalid.error(unsigned_package_message(Some(&entry.path))));
            }
            // The operator opted into unsigned packages: the first payload is
            // the second entry, exactly like the blocking install path.
            self.signature_checked = true;
            return self.start_payload(entry);
        }
        self.start_payload(entry)
    }

    fn entry_data(&mut self, data: &[u8]) -> Result<(), ProblemError> {
        match self.current.as_mut() {
            Some(Current::Manifest(buffer)) | Some(Current::Signature(buffer)) => {
                buffer.extend_from_slice(data);
                Ok(())
            }
            Some(Current::Payload {
                file,
                hasher,
                path,
                written,
                ..
            }) => {
                file.write_all(data).map_err(|error| {
                    Problem::Internal.error(format!("write {}: {}", path.display(), error))
                })?;
                hasher.update(data);
                *written = written.saturating_add(data.len() as u64);
                Ok(())
            }
            Some(Current::Directory) => Ok(()),
            None => Err(Problem::Invalid.error("tar entry data without a header")),
        }
    }

    fn entry_end(&mut self) -> Result<(), ProblemError> {
        let Some(current) = self.current.take() else {
            return Err(Problem::Invalid.error("tar entry ended without a header"));
        };
        match current {
            Current::Manifest(bytes) => {
                let manifest = parse_manifest(&bytes)?;
                validate_manifest(&manifest)?;
                check_platform(&manifest)?;
                self.declared = validate_declared_files(&manifest)?;
                self.manifest_bytes = bytes;
                self.manifest = Some(manifest);
                Ok(())
            }
            Current::Signature(bytes) => {
                let verified = if self.allow_insecure {
                    check_signature(
                        &self.manifest_bytes,
                        &bytes,
                        SignatureMode::Required,
                        None,
                        &self.trust_dir,
                    )
                    .is_ok()
                } else {
                    check_signature(
                        &self.manifest_bytes,
                        &bytes,
                        SignatureMode::Required,
                        None,
                        &self.trust_dir,
                    )?;
                    true
                };
                self.signature_verified = verified;
                self.signature_bytes = bytes;
                self.signature_checked = true;
                Ok(())
            }
            Current::Payload {
                file,
                hasher,
                declared,
                path,
                written,
            } => {
                if written != declared.size {
                    return Err(
                        Problem::Invalid.error(format!("size mismatch for {}", declared.path))
                    );
                }
                let digest = hex_bytes(&hasher.finalize());
                if digest != declared.sha256 {
                    return Err(
                        Problem::Invalid.error(format!("sha256 mismatch for {}", declared.path))
                    );
                }
                file.sync_all().map_err(|error| {
                    Problem::Internal.error(format!("sync {}: {}", path.display(), error))
                })?;
                set_mode(&path, declared.mode)?;
                Ok(())
            }
            Current::Directory => Ok(()),
        }
    }
}

/// Validate the executable/interpreter and hook constraints from the files
/// that already landed in the staging directory. Only the small script/hook
/// files are read; binaries are classified from their first bytes.
pub fn validate_staged_payload(manifest: &Manifest, root: &Path) -> Result<(), ProblemError> {
    let mut executables = vec![manifest.exec.path.as_str()];
    if let Some(lifecycle) = &manifest.lifecycle {
        for hook in [
            &lifecycle.start,
            &lifecycle.started,
            &lifecycle.stop,
            &lifecycle.status,
        ]
        .into_iter()
        .flatten()
        {
            executables.push(hook.path.as_str());
        }
    }
    let mut files: Vec<(&str, Vec<u8>)> = Vec::new();
    for path in executables {
        let data = fs::read(root.join(path))
            .map_err(|error| Problem::Internal.error(format!("read {}: {}", path, error)))?;
        files.push((path, data.into_iter().take(8).collect()));
    }
    let borrowed: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(path, data)| (*path, data.as_slice()))
        .collect();
    validate_script_payload(manifest, &borrowed)?;
    #[cfg(unix)]
    {
        let mut hooks = Vec::new();
        for path in [
            manifest
                .hooks
                .as_ref()
                .and_then(|hooks| hooks.post_install.as_deref()),
            manifest
                .hooks
                .as_ref()
                .and_then(|hooks| hooks.pre_remove.as_deref()),
        ]
        .into_iter()
        .flatten()
        {
            let data = fs::read(root.join(path)).map_err(|error| {
                Problem::Internal.error(format!("read hook {}: {}", path, error))
            })?;
            hooks.push(FileData {
                path: path.to_string(),
                data,
                mode: 0o755,
            });
        }
        check_hook_shebangs(manifest, &hooks)?;
    }
    Ok(())
}

/// Re-open a staging directory produced by [`StagingInstaller`] without
/// reading the payload again. Used by the daemon's `plugin.install_staged` RPC, which
/// trusts the HTTP layer to have streamed and hashed the files.
pub fn load_staged(
    root: &Path,
    allow_insecure: bool,
    trust_dir: &Path,
) -> Result<StagedPackage, ProblemError> {
    let manifest_path = root.join("manifest.json");
    let manifest_bytes = fs::read(&manifest_path).map_err(|error| {
        Problem::Internal.error(format!("read {}: {}", manifest_path.display(), error))
    })?;
    let manifest = parse_manifest(&manifest_bytes)?;
    validate_manifest(&manifest)?;
    check_platform(&manifest)?;
    let _declared = validate_declared_files(&manifest)?;

    let signature_path = root.join("signature.json");
    let signature_bytes = match fs::read(&signature_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(Problem::Internal.error(format!(
                "read {}: {}",
                signature_path.display(),
                error
            )))
        }
    };
    let signature_verified = if signature_bytes.is_empty() {
        if !allow_insecure {
            return Err(Problem::Invalid.error(unsigned_package_message(None)));
        }
        false
    } else if allow_insecure {
        check_signature(
            &manifest_bytes,
            &signature_bytes,
            SignatureMode::Required,
            None,
            trust_dir,
        )
        .is_ok()
    } else {
        check_signature(
            &manifest_bytes,
            &signature_bytes,
            SignatureMode::Required,
            None,
            trust_dir,
        )?;
        true
    };
    validate_staged_payload(&manifest, root)?;
    Ok(StagedPackage {
        manifest,
        signature_verified,
        root: root.to_path_buf(),
    })
}

fn write_one(path: &Path, data: &[u8], mode: u32) -> Result<(), ProblemError> {
    let mut handle = File::create(path).map_err(|error| {
        Problem::Internal.error(format!("create {}: {}", path.display(), error))
    })?;
    handle
        .write_all(data)
        .map_err(|error| Problem::Internal.error(format!("write {}: {}", path.display(), error)))?;
    handle
        .sync_all()
        .map_err(|error| Problem::Internal.error(format!("sync {}: {}", path.display(), error)))?;
    set_mode(path, mode)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), ProblemError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))
        .map_err(|error| Problem::Internal.error(format!("chmod {}: {}", path.display(), error)))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), ProblemError> {
    Ok(())
}

/// A fresh staging directory inside `root`. The name is the marker the daemon
/// and the startup sweep look for; being inside the root is what keeps the
/// commit's `rename` on one filesystem.
fn staging_dir(root: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    root.join(format!(".staging-{}-{}", std::process::id(), nanos))
}

/// What a package will write to disk, known as soon as the manifest is read:
/// the declared payload plus the two metadata files.
fn staged_size(manifest: &Manifest, manifest_bytes: usize, signature_bytes: usize) -> u64 {
    let files: u64 = manifest.files.iter().map(|file| file.size).sum();
    files
        .saturating_add(manifest_bytes as u64)
        .saturating_add(signature_bytes as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manage::test_support::*;
    use crate::platform::test_support::TempDir;
    use std::path::PathBuf;

    /// A stager whose plugin root is `root/plugins`, so the chosen staging
    /// directory is observable and the cleanup assertion can list the root.
    fn stager(root: &Path, allow_insecure: bool, max_bytes: u64) -> StagingInstaller {
        let mut paths = crate::platform::paths::Paths::under(root);
        paths.trust_dir = trust_dir().to_path_buf();
        paths.plugin_roots = vec![root.join("plugins")];
        std::fs::create_dir_all(root.join("plugins")).expect("plugin root");
        StagingInstaller::new(paths, allow_insecure, max_bytes, None)
    }

    /// Directories a crash or a rejection would leave in the plugin root.
    fn staging_leftovers(root: &Path) -> Vec<PathBuf> {
        fs::read_dir(root.join("plugins"))
            .expect("plugin root")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().starts_with(".staging-"))
                    .unwrap_or(false)
            })
            .collect()
    }

    fn install_bytes(bytes: &[u8], root: &Path) -> Result<StagedPackage, ProblemError> {
        let mut installer = stager(root, false, 1024 * 1024);
        installer.feed(bytes)?;
        installer.finish()
    }

    /// A staged upgrade asks the volume for the new tree only (#329). The old
    /// copy is still on disk while the new one is staged, but its bytes already
    /// came out of `free`; asking for them again refused upgrades that fit
    /// (#328: 12.7 MB new + 12 MB installed against 17.2 MB free).
    #[test]
    fn a_staged_upgrade_asks_for_the_new_tree_not_its_old_copy() {
        let key = setup();
        let bytes = package(&key);
        let root = TempDir::new();
        let mut installer = stager(root.path(), false, 1024 * 1024);
        installer.feed(&bytes).expect("feed");

        // Put a copy of the plugin on the volume the installer would pick, so
        // the refusal has an `installed` number to report.
        let installed: u64 = 4096;
        let paths = installer.sink.paths.clone();
        crate::platform::locations::record(&paths, "demo", paths.default_plugin_root())
            .expect("record");
        let plugin = paths.default_plugin_root().join("demo");
        std::fs::create_dir_all(plugin.join("1.0.0")).expect("version");
        std::fs::write(plugin.join("current"), "1.0.0").expect("current");
        std::fs::write(
            plugin.join("1.0.0/payload.bin"),
            vec![0u8; installed as usize],
        )
        .expect("payload");

        // The real package fits any volume; inflate the declared size so the
        // check has to refuse, then read what it asked for. That number is the
        // new tree (payload + the two metadata files), and re-adding the
        // installed copy is the bug this pins.
        let fresh = u64::MAX / 2;
        let manifest = installer.sink.manifest.as_mut().expect("manifest");
        manifest.files.first_mut().expect("declared file").size = fresh;
        let declared: u64 = manifest.files.iter().map(|file| file.size).sum();
        let metadata = installer.sink.manifest_bytes.len() as u64
            + installer.sink.signature_bytes.len() as u64;
        let error = installer
            .sink
            .chosen_root()
            .expect_err("no volume is that big");
        let data = error.data().expect("the refusal carries numbers");
        assert_eq!(data["need"], declared + metadata);
        assert_eq!(data["installed"], installed);
        assert_ne!(data["need"], declared + metadata + installed);
    }

    #[test]
    fn streaming_install_matches_the_in_memory_path() {
        let key = setup();
        let bytes = package(&key);
        let root = TempDir::new();
        let staged = install_bytes(&bytes, root.path()).expect("stream install");
        assert_eq!(staged.manifest.id(), "demo");
        assert!(staged.signature_verified);
        assert_eq!(
            std::fs::read(staged.root.join("payload/bin/demo")).expect("read"),
            native_payload()
        );

        let truncated = &bytes[..bytes.len() / 2];
        let broken = TempDir::new();
        assert!(install_bytes(truncated, broken.path()).is_err());
        assert!(staging_leftovers(broken.path()).is_empty());
    }

    #[test]
    fn an_unsigned_stream_says_the_package_is_not_signed() {
        let (manifest, files) = manifest(crate::platform::target());
        let bytes = unsigned_package_from(&manifest, &files);
        let root = TempDir::new();

        let error = match install_bytes(&bytes, root.path()) {
            Ok(_) => panic!("an unsigned stream must be refused"),
            Err(error) => error,
        };
        let detail = error.detail();
        assert!(detail.contains("package is not signed"), "{detail}");
        assert!(detail.contains("allow_insecure_install"), "{detail}");
        assert!(!detail.contains("must be the second entry"), "{detail}");
        assert!(staging_leftovers(root.path()).is_empty());
    }

    #[test]
    fn a_streamed_install_accepts_any_chunk_boundary() {
        let key = setup();
        let bytes = package(&key);
        for chunk in [1usize, 3, 7, 511, 512, 513, 1000] {
            let root = TempDir::new();
            let mut installer = stager(root.path(), false, 1024 * 1024);
            for part in bytes.chunks(chunk) {
                installer
                    .feed(part)
                    .unwrap_or_else(|error| panic!("chunk {}: {}", chunk, error));
            }
            let staged = installer
                .finish()
                .unwrap_or_else(|error| panic!("chunk {}: {}", chunk, error));
            assert_eq!(staged.manifest.id(), "demo");
            assert!(staged.signature_verified);
            assert_eq!(
                std::fs::read(staged.root.join("payload/bin/demo")).expect("read"),
                native_payload()
            );
        }
    }

    #[test]
    fn a_truncated_streamed_install_removes_its_staging_directory() {
        let key = setup();
        let bytes = package(&key);
        let root = TempDir::new();
        let mut installer = stager(root.path(), false, 1024 * 1024);
        installer
            .feed(&bytes[..bytes.len() / 2])
            .expect("prefix feed");
        assert!(
            !staging_leftovers(root.path()).is_empty(),
            "staging started"
        );
        assert!(installer.finish().is_err());
        drop(installer);
        assert!(
            staging_leftovers(root.path()).is_empty(),
            "a rejected package must not leave a staging directory"
        );
    }

    #[test]
    fn a_streamed_payload_is_hashed_against_the_manifest() {
        let key = setup();
        let mut bytes = package(&key);
        let payload = native_payload();
        let needle = payload.as_slice();
        let position = bytes
            .windows(needle.len())
            .position(|window| window == needle)
            .expect("payload");
        bytes[position] = b'j';
        let root = TempDir::new();
        let error = match install_bytes(&bytes, root.path()) {
            Ok(_) => panic!("a tampered payload is rejected"),
            Err(error) => error,
        };
        assert!(error.detail().contains("sha256"), "{error}");
        assert!(
            staging_leftovers(root.path()).is_empty(),
            "staging survives a rejected payload"
        );
    }
}
