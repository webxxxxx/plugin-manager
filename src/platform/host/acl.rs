//! Explicit, protected DACLs for the control endpoints and the directories the
//! daemon executes from.
//!
//! The control channel's authorization *is* the endpoint's DACL: whoever can
//! open the pipe may speak that tier. Inherited ACEs from `C:\ProgramData` would
//! hand `BUILTIN\Users` read access, so every object here gets a DACL built from
//! scratch and marked protected — SYSTEM and Administrators always, plus the
//! `svcd` group on the read endpoint.
//!
//! Well-known principals are looked up by name. The current user is *not*: its
//! name can resolve to a different SID than the process token (domain vs local
//! account), so `private` takes the SID straight out of the token.

use std::ffi::{c_void, OsStr};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W, GRANT_ACCESS, SE_FILE_OBJECT,
    TRUSTEE_IS_NAME, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    GetLengthSid, GetTokenInformation, InitializeSecurityDescriptor, SetSecurityDescriptorControl,
    SetSecurityDescriptorDacl, TokenUser, ACL, DACL_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
    SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// The local group that shares the read endpoint.
pub(crate) const GROUP: &str = "svcd";

/// `SECURITY_DESCRIPTOR_REVISION` (the only revision Windows has ever had).
const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
/// `ERROR_NONE_MAPPED`: the trustee name does not resolve on this host.
const ERROR_NONE_MAPPED: i32 = 1332;
/// `FILE_ALL_ACCESS`.
const FULL: u32 = 0x001F_01FF;
/// `FILE_GENERIC_READ`.
const READ: u32 = 0x0012_0089;
/// `FILE_GENERIC_READ | FILE_GENERIC_WRITE`.
const READ_WRITE: u32 = 0x0012_019F;
/// `OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE`: the directory and everything
/// created below it.
const INHERIT_ALL: u32 = 0x3;

/// One grant in a DACL: a well-known name or a binary SID plus its access mask.
#[derive(Clone, Copy)]
enum Trustee<'a> {
    Name(&'a str),
    Sid(&'a [u8]),
}

/// One built DACL plus the descriptor that references it.
///
/// The ACL is allocated by `SetEntriesInAcl`, so it lives on the heap and the
/// descriptor only stores a pointer: moving `Acl` is sound, and the value can be
/// shared across the accept threads.
pub(crate) struct Acl {
    acl: *mut ACL,
    descriptor: SECURITY_DESCRIPTOR,
}

// SAFETY: The ACL is immutable after construction and owned by this value.
unsafe impl Send for Acl {}
// SAFETY: Shared access only reads the immutable descriptor; mutation happens before publication.
unsafe impl Sync for Acl {}

impl Acl {
    /// System mode, read endpoint: SYSTEM / Administrators full, `svcd` read and
    /// write.
    pub(crate) fn shared(inherit: bool) -> io::Result<Self> {
        Self::build(
            &[
                (Trustee::Name("SYSTEM"), FULL),
                (Trustee::Name("Administrators"), FULL),
                (Trustee::Name(GROUP), READ_WRITE),
            ],
            inherit,
        )
        .map_err(missing_group)
    }

    /// System mode, admin endpoint: SYSTEM / Administrators only.
    pub(crate) fn admin_only(inherit: bool) -> io::Result<Self> {
        Self::build(
            &[
                (Trustee::Name("SYSTEM"), FULL),
                (Trustee::Name("Administrators"), FULL),
            ],
            inherit,
        )
    }

    /// System mode, machine-wide directories: SYSTEM / Administrators full,
    /// `svcd` group read-only. The group needs read access to inspect config and
    /// logs, but must not be able to replace `services.d` scripts or plugin
    /// payloads that the daemon runs as SYSTEM.
    pub(crate) fn shared_read_only(inherit: bool) -> io::Result<Self> {
        Self::build(
            &[
                (Trustee::Name("SYSTEM"), FULL),
                (Trustee::Name("Administrators"), FULL),
                (Trustee::Name(GROUP), READ),
            ],
            inherit,
        )
        .map_err(missing_group)
    }

    /// User mode: SYSTEM / Administrators full, the process token's user full.
    pub(crate) fn private(inherit: bool) -> io::Result<Self> {
        let sid = current_user_sid()?;
        Self::build(
            &[
                (Trustee::Name("SYSTEM"), FULL),
                (Trustee::Name("Administrators"), FULL),
                (Trustee::Sid(&sid), FULL),
            ],
            inherit,
        )
    }

    fn build(grants: &[(Trustee<'_>, u32)], inherit: bool) -> io::Result<Self> {
        // Every ACE points at one of these buffers; they must outlive the
        // `SetEntriesInAclW` call below.
        let mut names: Vec<Vec<u16>> = Vec::with_capacity(grants.len());
        let entries: Vec<EXPLICIT_ACCESS_W> = grants
            .iter()
            .map(|(trustee, mask)| {
                let (form, name) = match trustee {
                    Trustee::Name(name) => {
                        names.push(wide(name));
                        let last = names.last().expect("just pushed").as_ptr() as *mut u16;
                        (TRUSTEE_IS_NAME, last)
                    }
                    Trustee::Sid(sid) => (TRUSTEE_IS_SID, sid.as_ptr() as *mut u16),
                };
                EXPLICIT_ACCESS_W {
                    grfAccessPermissions: *mask,
                    grfAccessMode: GRANT_ACCESS,
                    grfInheritance: if inherit { INHERIT_ALL } else { 0 },
                    Trustee: TRUSTEE_W {
                        pMultipleTrustee: null_mut(),
                        MultipleTrusteeOperation: 0,
                        TrusteeForm: form,
                        TrusteeType: TRUSTEE_IS_UNKNOWN,
                        ptstrName: name,
                    },
                }
            })
            .collect();

        let mut acl: *mut ACL = null_mut();
        let code =
            // SAFETY: Buffers and SIDs are owned for the duration of the ACL call and the sizes are passed alongside them.
            unsafe { SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), null(), &mut acl) };
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code as i32));
        }

        // SAFETY: Buffers and SIDs are owned for the duration of the ACL call and the sizes are passed alongside them.
        let mut descriptor: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
        // SAFETY: Buffers and SIDs are owned for the duration of the ACL call and the sizes are passed alongside them.
        let initialized = unsafe {
            InitializeSecurityDescriptor(
                &mut descriptor as *mut SECURITY_DESCRIPTOR as *mut c_void,
                SECURITY_DESCRIPTOR_REVISION,
            )
        };
        if initialized == 0 {
            let error = io::Error::last_os_error();
            // SAFETY: Buffers and SIDs are owned for the duration of the ACL call and the sizes are passed alongside them.
            unsafe { LocalFree(acl as *mut c_void) };
            return Err(error);
        }
        // SAFETY: Buffers and SIDs are owned for the duration of the ACL call and the sizes are passed alongside them.
        let dacl_set = unsafe {
            SetSecurityDescriptorDacl(
                &mut descriptor as *mut SECURITY_DESCRIPTOR as *mut c_void,
                1,
                acl,
                0,
            )
        };
        if dacl_set == 0 {
            let error = io::Error::last_os_error();
            // SAFETY: Buffers and SIDs are owned for the duration of the ACL call and the sizes are passed alongside them.
            unsafe { LocalFree(acl as *mut c_void) };
            return Err(error);
        }
        // Protected: no inherited ACE may widen this again.
        // SAFETY: Buffers and SIDs are owned for the duration of the ACL call and the sizes are passed alongside them.
        let protected = unsafe {
            SetSecurityDescriptorControl(
                &mut descriptor as *mut SECURITY_DESCRIPTOR as *mut c_void,
                SE_DACL_PROTECTED,
                SE_DACL_PROTECTED,
            )
        };
        if protected == 0 {
            let error = io::Error::last_os_error();
            // SAFETY: Buffers and SIDs are owned for the duration of the ACL call and the sizes are passed alongside them.
            unsafe { LocalFree(acl as *mut c_void) };
            return Err(error);
        }

        Ok(Acl { acl, descriptor })
    }

    /// The `SECURITY_ATTRIBUTES` a pipe instance is created with.
    pub(crate) fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: &self.descriptor as *const SECURITY_DESCRIPTOR as *mut c_void,
            bInheritHandle: 0,
        }
    }

    /// Replace the object's DACL with this one, protected from inheritance.
    pub(crate) fn apply_to_path(&self, path: &Path) -> io::Result<()> {
        let name = wide_path(path);
        // SAFETY: Buffers and SIDs are owned for the duration of the ACL call and the sizes are passed alongside them.
        let code = unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                self.acl,
                null(),
            )
        };
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        Ok(())
    }
}

impl Drop for Acl {
    fn drop(&mut self) {
        if !self.acl.is_null() {
            // SAFETY: Buffers and SIDs are owned for the duration of the ACL call and the sizes are passed alongside them.
            unsafe { LocalFree(self.acl as *mut c_void) };
        }
    }
}

/// The SID of the process token's user, copied out of the token buffer.
fn current_user_sid() -> io::Result<Vec<u8>> {
    // SAFETY: Buffers and SIDs are owned for the duration of the ACL call and the sizes are passed alongside them.
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut needed = 0u32;
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut needed);
        if needed == 0 {
            let error = io::Error::last_os_error();
            CloseHandle(token);
            return Err(error);
        }
        let mut buffer = vec![0u8; needed as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr() as *mut c_void,
            needed,
            &mut needed,
        );
        CloseHandle(token);
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        let sid = user.User.Sid;
        let length = GetLengthSid(sid) as usize;
        let mut bytes = vec![0u8; length];
        std::ptr::copy_nonoverlapping(sid as *const u8, bytes.as_mut_ptr(), length);
        Ok(bytes)
    }
}

/// Whether the daemon serving `config` is the machine-wide (SCM) install.
///
/// The config location is the honest discriminator: the SCM install uses
/// `%ProgramData%\svcd`, a user-mode one lives under the user's profile and has
/// no group to share with.
pub(crate) fn is_machine_wide(config: &Path) -> bool {
    let base = std::env::var_os("ProgramData")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\ProgramData"));
    config.starts_with(base.join("svcd"))
}

fn missing_group(error: io::Error) -> io::Error {
    if error.raw_os_error() == Some(ERROR_NONE_MAPPED) {
        return io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "the `{GROUP}` local group does not exist; run `svcd install-service` to create it"
            ),
        );
    }
    error
}

fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// The daemon runs `services.d` scripts and plugin payloads as LocalSystem, so
/// those directories must not be writable by anyone but SYSTEM, Administrators
/// and the `svcd` group. Without this a local user could drop a `.cmd` into
/// `services.d` and have it executed as SYSTEM.
pub(crate) fn harden_directories(paths: &crate::platform::paths::Paths) -> std::io::Result<()> {
    for dir in [
        paths.config_dir.clone(),
        paths.services_dir.clone(),
        paths.plugins_dir.clone(),
    ] {
        // Create first: a first start has no `services.d` / `plugins` yet, and a
        // directory created later would inherit `C:\ProgramData`'s Users-write
        // ACE instead of getting the protected DACL below.
        std::fs::create_dir_all(&dir)?;
        // Group members may read the config and inspect the tree, but they must
        // not be able to replace a `services.d` script or plugin payload that
        // this daemon executes as SYSTEM.
        Acl::shared_read_only(true)?.apply_to_path(&dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = crate::platform::test_support::temp_dir().join(format!(
            "svcd-acl-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn the_admin_acl_needs_no_group_lookup() {
        let acl = Acl::admin_only(false).expect("admin acl");
        let attributes = acl.attributes();
        assert!(!attributes.lpSecurityDescriptor.is_null());
        assert_eq!(
            attributes.nLength,
            std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32
        );
    }

    /// The default system-mode config path must land in the machine-wide tier;
    /// otherwise the control pipe gets a private ACL and no admin can reach it.
    #[test]
    fn the_default_system_config_is_machine_wide() {
        assert!(is_machine_wide(
            &crate::platform::host::dirs::platform_config_dir()
        ));
    }

    /// A host without the `svcd` group must fail with the actionable message
    /// instead of a bare Win32 error.
    #[test]
    fn the_shared_acl_explains_a_missing_group() {
        match Acl::shared(false) {
            Ok(_) => {}
            Err(error) => {
                assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
                assert!(error.to_string().contains("install-service"), "{error}");
            }
        }
    }

    /// The directory tier resolves the same group; a host without it must get
    /// the same actionable message.
    #[test]
    fn the_read_only_directory_acl_explains_a_missing_group() {
        match Acl::shared_read_only(false) {
            Ok(_) => {}
            Err(error) => {
                assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
                assert!(error.to_string().contains("install-service"), "{error}");
            }
        }
    }

    /// The owner keeps full control: the token SID ACE must match the process
    /// that wrote the DACL.
    #[test]
    fn a_private_acl_keeps_the_owner_working() {
        let dir = scratch("dir");
        let acl = Acl::private(true).expect("private acl");
        acl.apply_to_path(&dir).expect("apply the protected DACL");
        // Applying again must stay idempotent (the daemon re-runs on restart).
        acl.apply_to_path(&dir).expect("apply again");

        let file = dir.join("probe");
        std::fs::write(&file, b"x").expect("the owner must still create files");
        std::fs::read(&file).expect("the owner must still read its own file");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
