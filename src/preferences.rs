//! Per-user preferences protected by Windows DPAPI. No plaintext password file.
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Preferences {
    pub username: String,
    pub password: String,
    pub auto_login: bool,
    pub auto_reconnect: bool,
    /// Mirrors the C# client's `MinimizeToTray`, which defaults to `true` and
    /// governs both the close button and the hide-after-login behaviour: with it
    /// on, closing hides the window and a successful login tucks the client away
    /// after a few seconds; with it off, closing the window really exits and the
    /// window stays put after logging in.
    pub minimize_to_tray: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            username: String::new(),
            password: String::new(),
            auto_login: false,
            auto_reconnect: true,
            minimize_to_tray: true,
        }
    }
}

/// `%LOCALAPPDATA%\DrComCampusRust` — the per-user directory this client owns.
///
/// Shared with the event log so that the credentials file and the log sit in one
/// place, which is what makes "hand me that folder" a usable support request.
pub fn data_directory() -> io::Result<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(|base| PathBuf::from(base).join("DrComCampusRust"))
        .ok_or_else(|| io::Error::other("无法定位用户配置目录"))
}

pub fn path() -> io::Result<PathBuf> {
    Ok(data_directory()?.join("preferences.dat"))
}

pub fn load(path: &Path) -> io::Result<Preferences> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&crypt(&bytes, false)?).map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Preferences::default()),
        Err(error) => Err(error),
    }
}

pub fn save(path: &Path, prefs: &Preferences) -> io::Result<()> {
    use std::io::Write;
    let bytes = crypt(&serde_json::to_vec(prefs).map_err(io::Error::other)?, true)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        replace(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(windows)]
fn crypt(bytes: &[u8], protect: bool) -> io::Result<Vec<u8>> {
    use windows_sys::Win32::{Foundation::LocalFree, Security::Cryptography::*};
    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(bytes.len()).map_err(io::Error::other)?,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: input remains alive for the synchronous call; DPAPI owns output.
    let success = unsafe {
        if protect {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful DPAPI call returns cbData readable bytes and LocalFree storage.
    let result =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(output.pbData as *mut _);
    }
    Ok(result)
}

#[cfg(windows)]
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn crypt(_: &[u8], _: bool) -> io::Result<Vec<u8>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "凭据保护需要 Windows",
    ))
}
#[cfg(not(windows))]
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

#[cfg(windows)]
pub mod autostart {
    use std::io;
    use windows_sys::Win32::System::Registry::*;
    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }
    fn key() -> Vec<u16> {
        wide(r"Software\Microsoft\Windows\CurrentVersion\Run")
    }
    fn name() -> Vec<u16> {
        wide("DrComCampusRust")
    }
    fn command() -> io::Result<String> {
        Ok(format!("\"{}\"", std::env::current_exe()?.display()))
    }
    pub fn enabled() -> io::Result<bool> {
        let mut buffer = vec![0u16; 32768];
        let mut size = (buffer.len() * 2) as u32;
        let code = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key().as_ptr(),
                name().as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if code == 2 {
            return Ok(false);
        }
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        let end = buffer.iter().position(|v| *v == 0).unwrap_or(buffer.len());
        Ok(String::from_utf16_lossy(&buffer[..end]).eq_ignore_ascii_case(&command()?))
    }
    pub fn set(enabled: bool) -> io::Result<()> {
        let code = if enabled {
            let value = wide(&command()?);
            unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    key().as_ptr(),
                    name().as_ptr(),
                    REG_SZ,
                    value.as_ptr().cast(),
                    (value.len() * 2) as u32,
                )
            }
        } else {
            unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key().as_ptr(), name().as_ptr()) }
        };
        if code == 0 || (!enabled && code == 2) {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(code as i32))
        }
    }
}

#[cfg(not(windows))]
pub mod autostart {
    use std::io;
    pub fn enabled() -> io::Result<bool> {
        Ok(false)
    }
    pub fn set(_: bool) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "开机启动需要 Windows",
        ))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn protected_roundtrip_and_tampering() {
        let secret = b"test-only password; unicode account";
        let encrypted = crypt(secret, true).unwrap();
        assert!(!encrypted.windows(secret.len()).any(|part| part == secret));
        assert_eq!(crypt(&encrypted, false).unwrap(), secret);
        assert!(crypt(b"not a DPAPI blob", false).is_err());
    }
    #[test]
    fn save_replaces_existing_preferences_without_plaintext() {
        let path =
            std::env::temp_dir().join(format!("drcom-prefs-test-{}.dat", std::process::id()));
        let prefs = Preferences {
            username: "test-user".into(),
            password: "test-secret".into(),
            auto_login: true,
            auto_reconnect: false,
            minimize_to_tray: false,
        };
        save(&path, &prefs).unwrap();
        assert!(load(&path).unwrap() == prefs);
        save(&path, &Preferences::default()).unwrap();
        assert!(load(&path).unwrap() == Preferences::default());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_file_written_before_the_tray_option_existed_still_loads() {
        // `#[serde(default)]` is what makes an upgrade safe: the field was added
        // after users already had encrypted files on disk, and dropping their
        // saved credentials over a new boolean would be unforgivable.
        assert!(Preferences::default().minimize_to_tray);
        let older = serde_json::json!({
            "username": "someone",
            "password": "secret",
            "auto_login": true,
            "auto_reconnect": false,
        });
        let decoded: Preferences = serde_json::from_value(older).unwrap();
        assert_eq!(decoded.username, "someone");
        assert!(decoded.auto_login);
        assert!(decoded.minimize_to_tray, "the new option must default on");
    }
}
