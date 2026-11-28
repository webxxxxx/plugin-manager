//! 导出：把一个已安装版本描述成 `.spk` 的 tar 计划。
use std::fs;
use std::path::Path;

use crate::kernel::failure::{Problem, ProblemError};
use crate::manage::check::verify::parse_manifest;
use crate::manage::model::ustar::{
    tar_entry_bytes, tar_header_bytes, tar_padding_bytes, TarPlan, TAR_TRAILER,
};

/// The installed signature, or `None` when the package was installed unsigned
/// and the host opted into that with `allow_insecure_install`.
fn read_export_signature(
    version_dir: &Path,
    allow_unsigned: bool,
) -> Result<Option<Vec<u8>>, ProblemError> {
    match fs::read(version_dir.join("signature.json")) {
        Ok(signature) => Ok(Some(signature)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && allow_unsigned => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(Problem::Invalid.error(
                "installed package has no signature (installed insecurely); set allow_insecure_install = on to export it unsigned, or reinstall a signed package",
            ))
        }
        Err(error) => Err(Problem::Internal.error(format!("read signature: {}", error))),
    }
}

/// The installed .spk, described as parts. Payload bytes are not read here;
/// only the manifest, the signature and each payload's size are.
pub fn plan_installed_spk(
    version_dir: &Path,
    allow_unsigned: bool,
) -> Result<TarPlan, ProblemError> {
    let manifest_bytes = fs::read(version_dir.join("manifest.json"))
        .map_err(|error| Problem::Internal.error(format!("read manifest: {}", error)))?;
    let manifest = parse_manifest(&manifest_bytes)?;
    let signature = read_export_signature(version_dir, allow_unsigned)?;
    let mut plan = TarPlan::new();
    plan.push_bytes(tar_entry_bytes("manifest.json", &manifest_bytes, 0o644)?);
    if let Some(signature) = signature {
        plan.push_bytes(tar_entry_bytes("signature.json", &signature, 0o644)?);
    }
    let mut files = manifest.files.clone();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    for file in files {
        let path = version_dir.join(&file.path);
        // parse_manifest validated the path; the size check keeps a truncated
        // install from advertising a Content-Length it cannot send.
        let metadata = fs::metadata(&path)
            .map_err(|error| Problem::Internal.error(format!("stat {}: {}", file.path, error)))?;
        if metadata.len() != file.size {
            return Err(
                Problem::Internal.error(format!("installed file is corrupt: {}", file.path))
            );
        }
        plan.push_bytes(tar_header_bytes(&file.path, file.size, file.mode & 0o777)?);
        if file.size > 0 {
            plan.push_file(path, file.size, Some(file.sha256.clone()));
        }
        plan.push_bytes(tar_padding_bytes(file.size)?);
    }
    plan.push_bytes(vec![0u8; TAR_TRAILER]);
    Ok(plan)
}
