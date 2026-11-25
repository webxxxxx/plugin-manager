//! The `Path` value of a Windows environment key, and the string editing both
//! install modes need.
//!
//! `user_path.rs` (user mode) and `system_path.rs` (machine-wide) differ only in
//! which key they open: the parsing, the "already there" comparison and the
//! `WM_SETTINGCHANGE` broadcast are one implementation.

// Only the Windows PATH editors call these three; `cfg(test)` keeps them
// compiled for the tests, which are platform-neutral. Without the gate the bare
// Linux bin target reports dead_code for every one of them (#318).
#[cfg(any(windows, test))]
/// The one line-editing rule for a `PATH`-style value.
///
/// Windows compares path entries case-insensitively and a trailing separator
/// does not make a different directory, so `C:\svcd` and `c:\svcd\` are the same
/// entry. The comparison lives here once instead of in each caller.
pub(super) fn same_entry(left: &str, right: &str) -> bool {
    let normalize = |text: &str| {
        text.trim()
            .trim_end_matches(['\\', '/'])
            .to_ascii_lowercase()
    };
    normalize(left) == normalize(right)
}

/// Append `dir` to a `;`-separated value. The bool is false when it was already
/// there, so a reinstall does not grow the value.
#[cfg(any(windows, test))]
pub(super) fn add_entry(existing: &str, dir: &str) -> (String, bool) {
    let entries: Vec<&str> = existing
        .split(';')
        .filter(|entry| !entry.trim().is_empty())
        .collect();
    if entries.iter().any(|entry| same_entry(entry, dir)) {
        return (existing.to_string(), false);
    }
    let mut updated: Vec<&str> = entries;
    if updated.is_empty() {
        return (dir.to_string(), true);
    }
    updated.push(dir);
    (updated.join(";"), true)
}

/// Drop every entry that names `dir`. The bool is false when nothing matched.
#[cfg(any(windows, test))]
pub(super) fn remove_entry(existing: &str, dir: &str) -> (String, bool) {
    let entries: Vec<&str> = existing.split(';').collect();
    let kept: Vec<&str> = entries
        .iter()
        .copied()
        .filter(|entry| entry.trim().is_empty() || !same_entry(entry, dir))
        .collect();
    if kept.len() == entries.len() {
        return (existing.to_string(), false);
    }
    let joined = kept.join(";");
    (joined.trim_matches(';').to_string(), true)
}

#[cfg(windows)]
pub(super) fn broadcast_change() {
    use std::os::windows::ffi::OsStrExt;

    // SAFETY: The broadcast carries a pointer to a NUL-terminated wide string
    // that outlives the call; the timeout keeps a hung window from blocking the
    // installer.
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
        };
        let parameter: Vec<u16> = std::ffi::OsStr::new("Environment")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            parameter.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            1000,
            std::ptr::null_mut(),
        );
    }
}

#[cfg(windows)]
#[derive(Clone, Copy)]
pub(super) enum Hive {
    CurrentUser,
    LocalMachine,
}

/// Read a key's `Path` value. A missing value is `None`, not an error: the
/// machine key always carries one, a fresh user profile may not.
#[cfg(windows)]
pub(super) fn read(hive: Hive, subkey: &str) -> Result<Option<String>, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, KEY_READ, KEY_SET_VALUE,
    };

    fn wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let root: HKEY = match hive {
        Hive::CurrentUser => windows_sys::Win32::System::Registry::HKEY_CURRENT_USER,
        Hive::LocalMachine => windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE,
    };
    let subkey_wide = wide(subkey);
    let value_name = wide("Path");
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: The key handle is opened here and closed on every path below;
    // every buffer passed is valid for the duration of the call.
    let opened = unsafe {
        RegOpenKeyExW(
            root,
            subkey_wide.as_ptr(),
            0,
            KEY_READ | KEY_SET_VALUE,
            &mut key,
        )
    };
    if opened == 2 {
        // ERROR_FILE_NOT_FOUND: the key has never been written. A missing value
        // is `None`, which the caller treats as an empty PATH.
        return Ok(None);
    }
    if opened != 0 {
        return Err(format!("open {}: code {}", label(hive, subkey), opened));
    }
    let mut kind = 0u32;
    let mut size = 0u32;
    // SAFETY: Same handle and buffer discipline as above; a null data pointer
    // is how the size is queried.
    let queried = unsafe {
        RegQueryValueExW(
            key,
            value_name.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if queried != 0 || size == 0 {
        // SAFETY: The handle was opened above and is closed exactly once.
        unsafe { RegCloseKey(key) };
        return Ok(None);
    }
    let mut current = vec![0u8; size as usize];
    let mut read_size = size;
    // SAFETY: `current` is `size` bytes, which is what the first query reported.
    let read = unsafe {
        RegQueryValueExW(
            key,
            value_name.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            current.as_mut_ptr(),
            &mut read_size,
        )
    };
    // SAFETY: The handle was opened above and is closed exactly once.
    unsafe { RegCloseKey(key) };
    if read != 0 {
        return Err(format!("read {}: code {}", label(hive, subkey), read));
    }
    current.truncate(read_size as usize);
    let units: Vec<u16> = current
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    Ok(Some(String::from_utf16_lossy(&units)))
}

/// Write a key's `Path` value as `REG_EXPAND_SZ`, which is the type the machine
/// key uses (`%SystemRoot%` and friends stay expandable).
#[cfg(windows)]
pub(super) fn write(hive: Hive, subkey: &str, value: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, KEY_READ, KEY_SET_VALUE, REG_EXPAND_SZ,
        REG_OPTION_NON_VOLATILE,
    };

    let root: HKEY = match hive {
        Hive::CurrentUser => windows_sys::Win32::System::Registry::HKEY_CURRENT_USER,
        Hive::LocalMachine => windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE,
    };
    let subkey_wide: Vec<u16> = std::ffi::OsStr::new(subkey)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let value_name: Vec<u16> = std::ffi::OsStr::new("Path")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut key: HKEY = std::ptr::null_mut();
    let mut disposition = 0u32;
    // Create-or-open: the machine environment key always exists, a user profile
    // may not have its `Environment` key yet, and a fresh profile must still be
    // usable.
    // SAFETY: The key handle is created-or-opened here and closed on every path
    // below; the class pointer is null and the security descriptor inherited.
    let opened = unsafe {
        RegCreateKeyExW(
            root,
            subkey_wide.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_READ | KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            &mut disposition,
        )
    };
    if opened != 0 {
        return Err(format!(
            "open {} for writing: code {}",
            label(hive, subkey),
            opened
        ));
    }
    let encoded: Vec<u8> = value
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    // SAFETY: The bytes outlive the call and the handle was opened above.
    let written = unsafe {
        RegSetValueExW(
            key,
            value_name.as_ptr(),
            0,
            REG_EXPAND_SZ,
            encoded.as_ptr(),
            encoded.len() as u32,
        )
    };
    // SAFETY: The handle was opened above and is closed exactly once.
    unsafe { RegCloseKey(key) };
    if written != 0 {
        return Err(format!("write {}: code {}", label(hive, subkey), written));
    }
    Ok(())
}

#[cfg(windows)]
fn label(hive: Hive, subkey: &str) -> String {
    let hive = match hive {
        Hive::CurrentUser => "HKCU",
        Hive::LocalMachine => "HKLM",
    };
    format!("{}\\{}\\Path", hive, subkey)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_is_added_once_and_case_does_not_make_it_new() {
        let (value, added) = add_entry(r"C:\Windows;C:\Windows\System32", r"c:\svcd\");
        assert!(added);
        assert_eq!(value, r"C:\Windows;C:\Windows\System32;c:\svcd\");

        let (again, added) = add_entry(&value, r"C:\svcd");
        assert!(!added);
        assert_eq!(again, value);
    }

    #[test]
    fn an_empty_value_becomes_the_directory() {
        assert_eq!(add_entry("", r"C:\svcd"), (r"C:\svcd".to_string(), true));
        assert_eq!(add_entry(";;", r"C:\svcd"), (r"C:\svcd".to_string(), true));
    }

    #[test]
    fn removing_drops_every_spelling_and_spares_the_neighbours() {
        let value = r"C:\svcd;C:\Windows;c:\SVCD\;";
        let (updated, changed) = remove_entry(value, r"C:\svcd");
        assert!(changed);
        assert_eq!(updated, r"C:\Windows");

        let (same, changed) = remove_entry(r"C:\Windows", r"C:\svcd");
        assert!(!changed);
        assert_eq!(same, r"C:\Windows");
    }
}
