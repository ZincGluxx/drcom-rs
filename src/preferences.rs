//! Per-user preferences.
//!
//! On Windows the saved file is encrypted with DPAPI and lives in
//! `%LOCALAPPDATA%\DrComCampusRust` as an opaque blob.
//!
//! Linux has no DPAPI, and this port does not invent fake crypto: the Unix build
//! stores the same JSON as plaintext and relies on the private file-system
//! permissions of the per-user data directory instead. [`save`] creates the
//! containing directory `0700` and the file `0600`, so only the owning account
//! can read the saved password. The write is still atomic on both platforms:
//! the bytes go to a temporary file that is then renamed over the target, so a
//! crash cannot leave a half-written preferences file behind.
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

/// `%LOCALAPPDATA%\DrComCampusRust` — the per-user directory this client owns.
///
/// Shared with the event log so that the credentials file and the log sit in one
/// place, which is what makes "hand me that folder" a usable support request.
#[cfg(windows)]
pub fn data_directory() -> io::Result<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(|base| PathBuf::from(base).join("DrComCampusRust"))
        .ok_or_else(|| io::Error::other("无法定位用户配置目录"))
}

/// `$XDG_DATA_HOME/drcom-campus` (`~/.local/share/drcom-campus` when the
/// variable is unset) — the Unix equivalent of the Windows per-user directory.
#[cfg(not(windows))]
pub fn data_directory() -> io::Result<PathBuf> {
    unix_data_directory(
        std::env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
        std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
    )
}

/// XDG resolution, split from the environment so the fallback chain is testable.
#[cfg(not(windows))]
fn unix_data_directory(
    xdg_data_home: Option<PathBuf>,
    home: Option<PathBuf>,
) -> io::Result<PathBuf> {
    let base = xdg_data_home
        .or_else(|| home.map(|home| home.join(".local").join("share")))
        .ok_or_else(|| io::Error::other("无法定位用户配置目录（HOME 未设置）"))?;
    Ok(base.join("drcom-campus"))
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
        make_private_directory(parent)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // `mode` only applies when the file is created, which `create_new`
        // guarantees. The rename that follows keeps this inode, so the final
        // file inherits the private mode.
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        replace(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    if result.is_ok() {
        // Belt and braces: the mode is set at creation, but an exotic parent
        // directory, or a file system that ignores the create mode, must not
        // leave the credentials readable by other accounts.
        make_private_file(path)?;
    }
    result
}

/// Makes the application's own data directory private (`0700`). A no-op on
/// platforms without POSIX permissions.
#[cfg(unix)]
fn make_private_directory(directory: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn make_private_directory(_: &Path) -> io::Result<()> {
    Ok(())
}

/// Makes the preferences file owner-readable only (`0600`).
#[cfg(unix)]
fn make_private_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn make_private_file(_: &Path) -> io::Result<()> {
    Ok(())
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

/// Unix has no DPAPI, so this is an identity transform: the bytes on disk are
/// the plaintext JSON. [`save`] installs owner-only file permissions instead;
/// see the module documentation. There is deliberately no home-made cipher here,
/// because a key that ships next to the ciphertext would be security theatre.
#[cfg(not(windows))]
fn crypt(bytes: &[u8], _: bool) -> io::Result<Vec<u8>> {
    Ok(bytes.to_vec())
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

/// XDG autostart entry: `$XDG_CONFIG_HOME/autostart/drcom-campus.desktop`.
///
/// Linux desktops discover the applications to start at login in the user's
/// `autostart` directory, so enabling this writes a desktop entry there and
/// disabling removes it. Unlike the Windows `Run` registry value, the desktop
/// entry stores the command line in a file, which means the executable path has
/// to be quoted and escaped according to the desktop-entry specification.
#[cfg(not(windows))]
pub mod autostart {
    use std::{
        io,
        path::{Path, PathBuf},
    };

    const FILE_NAME: &str = "drcom-campus.desktop";

    fn desktop_directory() -> io::Result<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|value| !value.is_empty())
                    .map(|home| PathBuf::from(home).join(".config"))
            })
            .ok_or_else(|| io::Error::other("无法定位自动启动目录（HOME 未设置）"))?;
        Ok(base.join("autostart"))
    }

    fn desktop_path() -> io::Result<PathBuf> {
        Ok(desktop_directory()?.join(FILE_NAME))
    }

    /// The whole file, so that the quoting and the key set stay in one place.
    ///
    /// `--silent` keeps a login start from popping the window: it still logs in
    /// with the saved credentials and keeps the session alive in the background.
    fn entry_text(executable: &Path) -> String {
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=DrCom 校园网助手\n\
             Exec={} --silent\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n",
            quote_exec(executable)
        )
    }

    /// Encodes a path as a single quoted `Exec` argument.
    ///
    /// Two escape layers apply to a desktop-entry value: the general string
    /// escapes are decoded first (`\\` becomes one backslash), and the `Exec`
    /// quoting rules are undone afterwards while the argument is unquoted. A
    /// literal backslash therefore needs four backslashes in the file; `"`,
    /// backtick and `$` need one. Quoting every path, even a plain one, keeps a
    /// space or a shell character in the installation directory from splitting
    /// the command line.
    fn quote_exec(path: &Path) -> String {
        let mut quoted = String::from("\"");
        for character in path.to_string_lossy().chars() {
            match character {
                '\\' => quoted.push_str("\\\\\\\\"),
                '"' | '\u{60}' | '$' => {
                    quoted.push('\\');
                    quoted.push(character);
                }
                other => quoted.push(other),
            }
        }
        quoted.push('"');
        quoted
    }

    /// Decodes the general desktop-entry string escapes in a raw value.
    fn unescape_value(value: &str) -> String {
        let mut text = String::with_capacity(value.len());
        let mut characters = value.chars();
        while let Some(character) = characters.next() {
            if character != '\\' {
                text.push(character);
                continue;
            }
            match characters.next() {
                Some('s') => text.push(' '),
                Some('n') => text.push('\n'),
                Some('t') => text.push('\t'),
                Some('r') => text.push('\r'),
                Some('\\') => text.push('\\'),
                // Not a valid general escape: desktop implementations keep the
                // backslash, and the `Exec` quoting layer consumes it next.
                Some(other) => {
                    text.push('\\');
                    text.push(other);
                }
                None => text.push('\\'),
            }
        }
        text
    }

    /// The program named by an `Exec` value (its first argument, unquoted).
    fn exec_program(value: &str) -> Option<String> {
        let value = unescape_value(value);
        let value = value.trim_start();
        let mut characters = value.chars();
        if characters.next() != Some('"') {
            return value.split_whitespace().next().map(str::to_string);
        }
        let mut program = String::new();
        while let Some(character) = characters.next() {
            match character {
                '\\' => program.push(characters.next()?),
                '"' => return Some(program),
                other => program.push(other),
            }
        }
        None
    }

    fn exec_value(text: &str) -> Option<&str> {
        text.lines()
            .find_map(|line| line.trim().strip_prefix("Exec=").map(str::trim))
    }

    /// Whether the entry at `path` would launch `executable`.
    fn enabled_at(path: &Path) -> io::Result<bool> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let Some(program) = exec_value(&text).and_then(exec_program) else {
            return Ok(false);
        };
        Ok(Path::new(&program) == std::env::current_exe()?)
    }

    pub fn enabled() -> io::Result<bool> {
        enabled_at(&desktop_path()?)
    }

    pub fn set(enabled: bool) -> io::Result<()> {
        let path = desktop_path()?;
        if !enabled {
            return match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            };
        }
        if let Some(directory) = path.parent() {
            std::fs::create_dir_all(directory)?;
        }
        std::fs::write(&path, entry_text(&std::env::current_exe()?))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn temporary_root(label: &str) -> PathBuf {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            std::env::temp_dir().join(format!(
                "drcom-autostart-{}-{}-{label}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ))
        }

        #[test]
        fn exec_quoting_survives_both_escape_layers() {
            for path in [
                Path::new("/usr/bin/drcom-campus"),
                Path::new("/opt/Dr Com/校园 助手/drcom-campus"),
                Path::new("/tmp/a \"quoted\" $dollar `tick`/drcom-campus"),
                Path::new("/tmp/back\\slash/drcom-campus"),
            ] {
                let text = entry_text(path);
                let value = exec_value(&text).expect("the entry has an Exec line");
                assert_eq!(
                    exec_program(value).as_deref(),
                    Some(path.to_str().expect("the test path is UTF-8")),
                    "quoting roundtrip failed for {path:?}\n{text}"
                );
                assert!(
                    value.ends_with("--silent"),
                    "a login start must not pop the window: {text}"
                );
            }
        }

        #[test]
        fn enabled_follows_the_exec_line_of_the_entry() {
            let root = temporary_root("enabled");
            std::fs::create_dir_all(&root).expect("temporary directory");
            let path = root.join(FILE_NAME);
            assert!(
                !enabled_at(&path).expect("missing file"),
                "a missing entry is not enabled"
            );

            let executable = std::env::current_exe().expect("current executable");
            std::fs::write(&path, entry_text(&executable)).expect("write entry");
            assert!(enabled_at(&path).expect("our own entry"));

            // An entry written by an older build (no --silent) still counts as ours.
            std::fs::write(
                &path,
                format!(
                    "[Desktop Entry]\nType=Application\nExec={}\n",
                    quote_exec(&executable)
                ),
            )
            .expect("write entry");
            assert!(
                enabled_at(&path).expect("legacy entry"),
                "an entry without the silent flag must still count as enabled"
            );

            std::fs::write(&path, entry_text(Path::new("/bin/false"))).expect("write entry");
            assert!(
                !enabled_at(&path).expect("foreign entry"),
                "an entry for another program is not enabled"
            );

            std::fs::write(&path, "[Desktop Entry]\nType=Application\n").expect("write entry");
            assert!(
                !enabled_at(&path).expect("entry without Exec"),
                "an entry without an Exec line is not enabled"
            );
            std::fs::remove_dir_all(root).expect("clean up");
        }
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

#[cfg(all(test, not(windows)))]
mod unix_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temporary_root(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "drcom-prefs-{}-{}-{label}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn saved_preferences_roundtrip_and_are_owner_only() {
        let directory = temporary_root("roundtrip");
        let path = directory.join("preferences.dat");
        let prefs = Preferences {
            username: "test-user".into(),
            password: "test-secret".into(),
            password_cleared: false,
            auto_login: true,
            auto_reconnect: false,
            minimize_to_tray: false,
        };
        save(&path, &prefs).expect("save");
        assert!(load(&path).expect("load") == prefs);
        let mode = std::fs::metadata(&path)
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "the credential file must not be group/world readable"
        );
        let directory_mode = std::fs::metadata(&directory)
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            directory_mode, 0o700,
            "the data directory must stay private"
        );

        // Replacing an existing file keeps the same guarantees.
        save(&path, &Preferences::default()).expect("replace");
        assert!(load(&path).expect("load") == Preferences::default());
        assert_eq!(
            std::fs::metadata(&path)
                .expect("file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "replacement must not widen the file mode"
        );
        std::fs::remove_dir_all(&directory).expect("clean up");
    }

    #[test]
    fn unix_data_directory_prefers_xdg_then_home() {
        let home = PathBuf::from("/home/someone");
        assert_eq!(
            unix_data_directory(Some(PathBuf::from("/xdg/data")), Some(home.clone()))
                .expect("xdg wins"),
            PathBuf::from("/xdg/data/drcom-campus")
        );
        assert_eq!(
            unix_data_directory(None, Some(home)).expect("home fallback"),
            PathBuf::from("/home/someone/.local/share/drcom-campus")
        );
        assert!(
            unix_data_directory(None, None).is_err(),
            "without HOME there is nowhere to store the credentials"
        );
    }

    #[test]
    fn preferences_are_plaintext_on_disk_without_inventing_crypto() {
        // Honest behaviour check: the Unix build does not pretend to protect the
        // password with a shipped key. The file permissions are the protection,
        // so the stored bytes must still be valid JSON.
        let directory = temporary_root("plaintext");
        let path = directory.join("preferences.dat");
        let prefs = Preferences {
            username: "someone".into(),
            password: "secret".into(),
            ..Preferences::default()
        };
        save(&path, &prefs).expect("save");
        let bytes = std::fs::read(&path).expect("read");
        let decoded: Preferences = serde_json::from_slice(&bytes).expect("plaintext JSON");
        assert!(decoded == prefs);
        std::fs::remove_dir_all(&directory).expect("clean up");
    }
}
