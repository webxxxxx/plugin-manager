use super::*;

/// Debug-build seam: a user-mode install edits the profile of whoever runs the
/// tests (HKCU PATH on Windows, shell profiles on Unix). Tests that only assert
/// the installed files set this to keep the host profile untouched.
#[cfg(debug_assertions)]
pub(super) fn user_path_update_skipped() -> bool {
    std::env::var_os("SVCD_TEST_NO_USER_PATH").is_some()
}

#[cfg(not(debug_assertions))]
pub(super) fn user_path_update_skipped() -> bool {
    false
}

#[cfg(unix)]
pub(super) fn remove_user_path(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())?;
    let escaped = dir
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`");
    let block = format!(
        "# svcd user path\n\
         if [ -d \"{}\" ]; then\n\
           case \":$PATH:\" in\n\
             *\":{}\"*) ;;\n\
             *) export PATH=\"{}:$PATH\" ;;\n\
           esac\n\
         fi\n",
        escaped, escaped, escaped
    );
    let mut changed = Vec::new();
    for name in [".profile", ".bashrc", ".zshrc"] {
        let path = home.join(name);
        if !path.is_file() {
            continue;
        }
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => continue,
        };
        if !text.contains(&block) {
            continue;
        }
        let mut updated = text.replace(&format!("\n{}", block), "");
        updated = updated.replace(&block, "");
        if let Err(error) = fs::write(&path, updated) {
            return Err(format!("write {}: {}", path.display(), error));
        }
        changed.push(path);
    }
    Ok(changed)
}

#[cfg(unix)]
pub(super) fn add_user_path(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())?;
    let escaped = dir
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`");
    let block = format!(
        "# svcd user path\n\
         if [ -d \"{}\" ]; then\n\
           case \":$PATH:\" in\n\
             *\":{}\"*) ;;\n\
             *) export PATH=\"{}:$PATH\" ;;\n\
           esac\n\
         fi\n",
        escaped, escaped, escaped
    );
    let mut files = Vec::new();
    for name in [".profile", ".bashrc", ".zshrc"] {
        let path = home.join(name);
        if !path.is_file() {
            continue;
        }
        let text = fs::read_to_string(&path)
            .map_err(|error| format!("read {}: {}", path.display(), error))?;
        if text.contains(&format!("# svcd user path\n")) && text.contains(&escaped) {
            files.push(path);
            continue;
        }
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|error| format!("open {}: {}", path.display(), error))?;
        use std::io::Write;
        file.write_all(b"\n")
            .and_then(|_| file.write_all(block.as_bytes()))
            .map_err(|error| format!("write {}: {}", path.display(), error))?;
        files.push(path);
    }
    if files.is_empty() {
        let path = home.join(".profile");
        fs::write(&path, format!("\n{}", block))
            .map_err(|error| format!("write {}: {}", path.display(), error))?;
        files.push(path);
    }
    Ok(files)
}

#[cfg(windows)]
pub(super) fn remove_user_path(dir: &Path) -> Result<bool, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_READ, KEY_SET_VALUE, REG_EXPAND_SZ,
    };

    fn wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let subkey = wide("Environment");
    let value_name = wide("Path");
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: The registry/PATH handle is opened by this function and closed on every path; buffers are valid for the call.
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_READ | KEY_SET_VALUE,
            &mut key,
        )
    };
    if opened != 0 {
        return Err(format!("open HKCU\\Environment: code {}", opened));
    }
    let mut kind = 0u32;
    let mut size = 0u32;
    // SAFETY: The registry/PATH handle is opened by this function and closed on every path; buffers are valid for the call.
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
        // SAFETY: The registry/PATH handle is opened by this function and closed on every path; buffers are valid for the call.
        unsafe { RegCloseKey(key) };
        return Ok(false);
    }
    let mut current = vec![0u8; size as usize];
    let mut read_size = size;
    // SAFETY: The registry/PATH handle is opened by this function and closed on every path; buffers are valid for the call.
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
    if read != 0 {
        // SAFETY: The registry/PATH handle is opened by this function and closed on every path; buffers are valid for the call.
        unsafe { RegCloseKey(key) };
        return Err(format!("read HKCU\\Environment\\Path: code {}", read));
    }
    current.truncate(read_size as usize);
    let existing = String::from_utf16_lossy(
        &current
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .take_while(|unit| *unit != 0)
            .collect::<Vec<_>>(),
    );
    let directory = dir.display().to_string();
    let entries: Vec<&str> = existing
        .split(';')
        .filter(|entry| !entry.trim().eq_ignore_ascii_case(&directory))
        .collect();
    let updated = entries.join(";");
    if updated == existing {
        // SAFETY: The registry/PATH handle is opened by this function and closed on every path; buffers are valid for the call.
        unsafe { RegCloseKey(key) };
        return Ok(false);
    }
    let encoded: Vec<u16> = updated.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes: Vec<u8> = encoded.iter().flat_map(|unit| unit.to_le_bytes()).collect();
    // SAFETY: The registry/PATH handle is opened by this function and closed on every path; buffers are valid for the call.
    let written = unsafe {
        RegSetValueExW(
            key,
            value_name.as_ptr(),
            0,
            REG_EXPAND_SZ,
            bytes.as_ptr(),
            bytes.len() as u32,
        )
    };
    // SAFETY: The registry/PATH handle is opened by this function and closed on every path; buffers are valid for the call.
    unsafe { RegCloseKey(key) };
    if written != 0 {
        return Err(format!("write HKCU\\Environment\\Path: code {}", written));
    }
    // SAFETY: The registry/PATH handle is opened by this function and closed on every path; buffers are valid for the call.
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
        };
        let parameter = wide("Environment");
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
    Ok(true)
}

/// Mode 1 installation: install into the current user's data directory and
/// never touch system service managers. The same `--prefix`, `--conf`,
/// `--manual`, `--no-service` and `--dry-run` flags are accepted.
#[cfg(windows)]
pub(super) fn add_user_path(dir: &Path) -> Result<bool, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_READ, KEY_SET_VALUE, REG_EXPAND_SZ,
    };

    fn wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let subkey = wide("Environment");
    let value_name = wide("Path");
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: The registry/PATH handle is opened by this function and closed on every path; buffers are valid for the call.
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_READ | KEY_SET_VALUE,
            &mut key,
        )
    };
    if opened != 0 {
        return Err(format!("open HKCU\\Environment: code {}", opened));
    }
    let mut kind = 0u32;
    let mut size = 0u32;
    // SAFETY: The registry/PATH handle is opened by this function and closed on every path; buffers are valid for the call.
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
    let mut current = Vec::new();
    if queried == 0 && size > 0 {
        current.resize(size as usize, 0u8);
        let mut read_kind = 0u32;
        let mut read_size = size;
        // SAFETY: The registry/PATH handle is opened by this function and closed on every path; buffers are valid for the call.
        let read = unsafe {
            RegQueryValueExW(
                key,
                value_name.as_ptr(),
                std::ptr::null_mut(),
                &mut read_kind,
                current.as_mut_ptr(),
                &mut read_size,
            )
        };
        if read != 0 {
            // SAFETY: The registry/PATH handle is opened by this function and closed on every path; buffers are valid for the call.
            unsafe { RegCloseKey(key) };
            return Err(format!("read HKCU\\Environment\\Path: code {}", read));
        }
        current.truncate(read_size as usize);
    }
    let existing = String::from_utf16_lossy(
        &current
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .take_while(|unit| *unit != 0)
            .collect::<Vec<_>>(),
    );
    let directory = dir.display().to_string();
    let already = existing
        .split(';')
        .any(|entry| entry.trim().eq_ignore_ascii_case(&directory));
    if already {
        // SAFETY: The registry/PATH handle is opened by this function and closed on every path; buffers are valid for the call.
        unsafe { RegCloseKey(key) };
        return Ok(false);
    }
    let updated = if existing.trim().is_empty() {
        directory
    } else {
        format!("{};{}", existing.trim_end_matches(';'), directory)
    };
    let encoded: Vec<u16> = updated.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes: Vec<u8> = encoded.iter().flat_map(|unit| unit.to_le_bytes()).collect();
    // SAFETY: The registry/PATH handle is opened by this function and closed on every path; buffers are valid for the call.
    let written = unsafe {
        RegSetValueExW(
            key,
            value_name.as_ptr(),
            0,
            REG_EXPAND_SZ,
            bytes.as_ptr(),
            bytes.len() as u32,
        )
    };
    // SAFETY: The registry/PATH handle is opened by this function and closed on every path; buffers are valid for the call.
    unsafe { RegCloseKey(key) };
    if written != 0 {
        return Err(format!("write HKCU\\Environment\\Path: code {}", written));
    }
    // SAFETY: The registry/PATH handle is opened by this function and closed on every path; buffers are valid for the call.
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
        };
        let parameter = wide("Environment");
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
    Ok(true)
}
