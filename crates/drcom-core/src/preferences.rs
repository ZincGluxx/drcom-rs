//! Per-user preferences. Windows retains its DPAPI blob; macOS/Linux keep only
//! non-sensitive settings on disk and passwords in Keychain/Secret Service.
//! There is deliberately no plaintext or in-memory-keyring fallback.
mod paths;
#[cfg(any(target_os = "macos", target_os = "linux", test))]
mod secure;
#[cfg(any(target_os = "macos", target_os = "linux", test))]
mod unix_autostart;
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
    /// Prevents a password in `drcom.ini` from being silently copied back into
    /// the form after the user explicitly cleared this application's password.
    #[serde(default)]
    pub password_cleared: bool,
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
            password_cleared: false,
            auto_login: false,
            auto_reconnect: true,
            minimize_to_tray: true,
        }
    }
}

/// User-owned settings/log directory: `%LOCALAPPDATA%\\DrComCampusRust`,
/// `~/Library/Application Support/DrComCampusRust`, or
/// `$XDG_CONFIG_HOME/DrComCampusRust` (default `~/.config/DrComCampusRust`).
/// Environment paths must be absolute; no file is created by this lookup.
pub fn data_directory() -> io::Result<PathBuf> {
    paths::data_directory()
}

pub(crate) fn user_config_directories() -> Vec<PathBuf> {
    paths::config_directories()
}

pub fn path() -> io::Result<PathBuf> {
    Ok(data_directory()?.join("preferences.dat"))
}

#[cfg(windows)]
pub fn load(path: &Path) -> io::Result<Preferences> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&crypt(&bytes, false)?).map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Preferences::default()),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
pub fn save(path: &Path, prefs: &Preferences) -> io::Result<()> {
    let bytes = crypt(&serde_json::to_vec(prefs).map_err(io::Error::other)?, true)?;
    atomic_write(path, &bytes)
}

/// Explicit recovery API: read settings without accessing a failed/locked Unix
/// keyring. The password is empty and auto-login is disabled. Callers must still
/// display the original `load` error; this must not masquerade as a full load.
pub fn load_non_sensitive(path: &Path) -> io::Result<Preferences> {
    #[cfg(windows)]
    let mut prefs = load(path)?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let mut prefs = secure::load_non_sensitive(path)?;
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    let mut prefs = Preferences::default();
    prefs.password.clear();
    prefs.auto_login = false;
    Ok(prefs)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn load(path: &Path) -> io::Result<Preferences> {
    secure::load(path)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn save(path: &Path, prefs: &Preferences) -> io::Result<()> {
    secure::save(path, prefs)
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub fn load(_: &Path) -> io::Result<Preferences> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "当前平台不支持安全凭据存储",
    ))
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub fn save(_: &Path, _: &Preferences) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "当前平台不支持安全凭据存储",
    ))
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        // Do not chmod arbitrary caller-owned parents (e.g. ~/.config or /tmp).
        // Newly created directories are private on Unix, like origin/linux.
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(parent)?;
    }
    let temporary = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        file.write_all(bytes)?;
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
pub(crate) fn replace(from: &Path, to: &Path) -> io::Result<()> {
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
pub(crate) fn replace(from: &Path, to: &Path) -> io::Result<()> {
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
    /// The autostart entry starts in the notification area: signing in at boot
    /// should not throw a window in the user's face, and a window is the most
    /// expensive thing this process can hold.
    fn command() -> io::Result<String> {
        Ok(format!("\"{}\" --tray", std::env::current_exe()?.display()))
    }
    /// Entries written by older versions have no --tray flag, and they still count
    /// as "autostart is on" — otherwise the checkbox would silently flip off for
    /// everyone who had it enabled before the flag existed.
    fn legacy_command() -> io::Result<String> {
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
        let stored = String::from_utf16_lossy(&buffer[..end]);
        Ok(stored.eq_ignore_ascii_case(&command()?)
            || stored.eq_ignore_ascii_case(&legacy_command()?))
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

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod autostart {
    pub use super::unix_autostart::{enabled, set};
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub mod autostart {
    use std::io;
    pub fn enabled() -> io::Result<bool> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "当前平台不支持开机启动",
        ))
    }
    pub fn set(_: bool) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "当前平台不支持开机启动",
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
            password_cleared: false,
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
        assert!(!decoded.password_cleared);
    }
}
