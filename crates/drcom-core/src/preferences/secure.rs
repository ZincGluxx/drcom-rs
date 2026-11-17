//! Unix persistence: only settings/credential-presence metadata go to disk.
//! Tests inject a store directly; they never install a global mock keyring.
use super::{Preferences, atomic_write, paths};
use serde::{Deserialize, Serialize};
use std::{io, path::Path};

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct DiskPreferences {
    format_version: u32,
    username: String,
    password_cleared: bool,
    auto_login: bool,
    auto_reconnect: bool,
    minimize_to_tray: bool,
    credential_saved: bool,
}

impl Default for DiskPreferences {
    fn default() -> Self {
        Self::from_preferences(&Preferences::default())
    }
}

impl DiskPreferences {
    fn from_preferences(prefs: &Preferences) -> Self {
        Self {
            format_version: 1,
            username: prefs.username.clone(),
            password_cleared: prefs.password_cleared,
            auto_login: prefs.auto_login,
            auto_reconnect: prefs.auto_reconnect,
            minimize_to_tray: prefs.minimize_to_tray,
            credential_saved: !prefs.password.is_empty() && !prefs.password_cleared,
        }
    }
    fn into_preferences(self) -> Preferences {
        Preferences {
            username: self.username,
            password: String::new(),
            password_cleared: self.password_cleared,
            auto_login: self.auto_login,
            auto_reconnect: self.auto_reconnect,
            minimize_to_tray: self.minimize_to_tray,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Credential {
    // Bind the secret to its account, so a settings-file write failure cannot
    // silently pair a new account's password with the previously saved username.
    username: String,
    password: String,
}

trait Store {
    fn get(&self) -> io::Result<Option<String>>;
    fn set(&self, secret: &str) -> io::Result<()>;
    fn delete(&self) -> io::Result<()>;
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn read_disk(path: &Path) -> io::Result<Option<(DiskPreferences, bool)>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| invalid("偏好文件无效；未尝试明文凭据回退"))?;
    let legacy_password = value
        .get("password")
        .is_some_and(|value| value.as_str().is_none_or(|password| !password.is_empty()));
    let disk: DiskPreferences =
        serde_json::from_value(value).map_err(|_| invalid("偏好文件格式无效"))?;
    if disk.format_version != 1 {
        return Err(invalid("不支持此版本的偏好文件"));
    }
    Ok(Some((disk, legacy_password)))
}

fn read_with_legacy(path: &Path) -> io::Result<Option<(DiskPreferences, bool)>> {
    if let Some(disk) = read_disk(path)? {
        return Ok(Some(disk));
    }
    if let Some(legacy) = paths::legacy_preferences_path(path) {
        return read_disk(&legacy);
    }
    Ok(None)
}

pub(super) fn load_non_sensitive(path: &Path) -> io::Result<Preferences> {
    let mut prefs = read_with_legacy(path)?
        .map(|(disk, _)| disk.into_preferences())
        .unwrap_or_default();
    prefs.auto_login = false;
    Ok(prefs)
}

fn load_with(
    path: &Path,
    store: impl FnOnce() -> io::Result<Box<dyn Store>>,
) -> io::Result<Preferences> {
    let Some((disk, legacy_password)) = read_with_legacy(path)? else {
        return Ok(Preferences::default());
    };
    if legacy_password {
        return Err(invalid(
            "检测到旧版明文密码文件；不会自动使用或导入密码，请重新输入并安全保存（旧文件需由你移除）",
        ));
    }
    let credential_saved = disk.credential_saved;
    let mut prefs = disk.into_preferences();
    if credential_saved && !prefs.password_cleared {
        let encoded = store()?
            .get()?
            .ok_or_else(|| invalid("系统密钥环中的已保存密码不存在；请重新输入并安全保存"))?;
        let credential: Credential =
            serde_json::from_str(&encoded).map_err(|_| invalid("系统密钥环中的凭据格式无效"))?;
        if credential.username != prefs.username || credential.password.is_empty() {
            return Err(invalid("系统密钥环凭据与已保存账号不匹配；请重新安全保存"));
        }
        prefs.password = credential.password;
    }
    Ok(prefs)
}

fn save_with(path: &Path, prefs: &Preferences, store: &dyn Store) -> io::Result<()> {
    if prefs.password_cleared && !prefs.password.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "清除密码状态不能同时包含密码",
        ));
    }
    let disk = DiskPreferences::from_preferences(prefs);
    let bytes = serde_json::to_vec(&disk).map_err(io::Error::other)?;
    if disk.credential_saved {
        let secret = serde_json::to_string(&Credential {
            username: prefs.username.clone(),
            password: prefs.password.clone(),
        })
        .map_err(io::Error::other)?;
        store.set(&secret)?;
    } else {
        // Missing is success; unavailable, locked or denied is NOT. Do this
        // before the file write: never claim to have erased an inaccessible secret.
        store.delete()?;
    }
    atomic_write(path, &bytes)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn native_store(path: &Path) -> io::Result<Box<dyn Store>> {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    // Different explicit preference files must never overwrite each other's
    // credentials. The identifier is not a password and is never a process arg.
    let user = format!(
        "preferences:{}",
        URL_SAFE_NO_PAD.encode(absolute.as_os_str().as_encoded_bytes())
    );
    // Construct the concrete builder, not Entry::new's default: keyring's
    // no-feature default is an in-memory mock that would falsely report success.
    #[cfg(target_os = "macos")]
    let builder = keyring::macos::default_credential_builder();
    #[cfg(target_os = "linux")]
    let builder = keyring::secret_service::default_credential_builder();
    let credential = builder
        .build(None, "DrComCampusRust", &user)
        .map_err(keyring_error)?;
    Ok(Box::new(NativeStore(keyring::Entry::new_with_credential(
        credential,
    ))))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
struct NativeStore(keyring::Entry);

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn keyring_error(error: keyring::Error) -> io::Error {
    // Do not format arbitrary backend errors: BadEncoding and Ambiguous may
    // contain credential bytes/objects. Give actionable, non-secret diagnostics.
    let kind = if matches!(error, keyring::Error::NoStorageAccess(_)) {
        io::ErrorKind::PermissionDenied
    } else {
        io::ErrorKind::Other
    };
    #[cfg(target_os = "macos")]
    let message = "无法访问 macOS Keychain；请解锁登录钥匙串并允许访问，密码未回退到明文存储";
    #[cfg(target_os = "linux")]
    let message = "无法访问 Secret Service 密钥环；请在桌面会话中启用并解锁 GNOME Keyring/KWallet（需要会话 D-Bus），密码未回退到明文存储";
    io::Error::new(kind, message)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Store for NativeStore {
    fn get(&self) -> io::Result<Option<String>> {
        match self.0.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(keyring_error(error)),
        }
    }
    fn set(&self, secret: &str) -> io::Result<()> {
        self.0.set_password(secret).map_err(keyring_error)
    }
    fn delete(&self) -> io::Result<()> {
        match self.0.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(keyring_error(error)),
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) fn load(path: &Path) -> io::Result<Preferences> {
    let _guard = LOCK
        .lock()
        .map_err(|_| io::Error::other("凭据存储锁不可用"))?;
    load_with(path, || native_store(path))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) fn save(path: &Path, prefs: &Preferences) -> io::Result<()> {
    let _guard = LOCK
        .lock()
        .map_err(|_| io::Error::other("凭据存储锁不可用"))?;
    save_with(path, prefs, native_store(path)?.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::RefCell,
        path::PathBuf,
        rc::Rc,
        sync::atomic::{AtomicU64, Ordering},
    };
    #[derive(Clone, Default)]
    struct FakeStore(Rc<RefCell<Option<String>>>, bool);
    impl Store for FakeStore {
        fn get(&self) -> io::Result<Option<String>> {
            if self.1 {
                Err(io::Error::other("locked"))
            } else {
                Ok(self.0.borrow().clone())
            }
        }
        fn set(&self, secret: &str) -> io::Result<()> {
            if self.1 {
                return Err(io::Error::other("locked"));
            }
            *self.0.borrow_mut() = Some(secret.into());
            Ok(())
        }
        fn delete(&self) -> io::Result<()> {
            if self.1 {
                return Err(io::Error::other("locked"));
            }
            *self.0.borrow_mut() = None;
            Ok(())
        }
    }
    fn temporary() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir()
            .join(format!(
                "drcom-secure-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ))
            .join("preferences.dat")
    }
    #[test]
    fn roundtrip_separates_password_and_preserves_defaults() {
        let path = temporary();
        let prefs = Preferences {
            username: "student".into(),
            password: "secret-not-on-disk".into(),
            auto_login: true,
            ..Preferences::default()
        };
        let store = FakeStore::default();
        save_with(&path, &prefs, &store).unwrap();
        let json = std::fs::read_to_string(&path).unwrap();
        assert!(!json.contains("secret-not-on-disk"));
        assert!(
            serde_json::from_str::<serde_json::Value>(&json)
                .unwrap()
                .get("password")
                .is_none()
        );
        assert!(load_with(&path, || Ok(Box::new(store.clone()))).unwrap() == prefs);
        let public = load_non_sensitive(&path).unwrap();
        assert_eq!(public.username, "student");
        assert!(public.password.is_empty());
        assert!(!public.auto_login);
        let cleared = Preferences {
            password_cleared: true,
            ..Preferences::default()
        };
        save_with(&path, &cleared, &store).unwrap();
        assert!(store.get().unwrap().is_none());
        assert!(
            load_with(&path, || panic!("cleared files do not read keyring")).unwrap() == cleared
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
    #[test]
    fn unavailable_keyring_never_writes_or_claims_success() {
        let path = temporary();
        let prefs = Preferences {
            password: "secret".into(),
            ..Preferences::default()
        };
        let locked = FakeStore(Rc::default(), true);
        assert!(save_with(&path, &prefs, &locked).is_err());
        assert!(!path.exists());
        assert!(save_with(&path, &Preferences::default(), &locked).is_err());
        assert!(!path.exists());
        assert!(
            load_with(&path, || panic!("missing file must not access keyring")).unwrap()
                == Preferences::default()
        );
    }
    #[test]
    fn missing_or_mismatched_secret_is_an_error_not_empty_success() {
        let path = temporary();
        let prefs = Preferences {
            username: "u".into(),
            password: "p".into(),
            ..Preferences::default()
        };
        let store = FakeStore::default();
        save_with(&path, &prefs, &store).unwrap();
        store.delete().unwrap();
        assert!(load_with(&path, || Ok(Box::new(store.clone()))).is_err());
        store
            .set(r#"{"username":"different","password":"p"}"#)
            .unwrap();
        assert!(load_with(&path, || Ok(Box::new(store.clone()))).is_err());
        assert!(load_with(&path, || Err(io::Error::other("locked"))).is_err());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
    #[test]
    fn conflicting_clear_state_and_corrupt_files_fail_closed() {
        let path = temporary();
        let store = FakeStore::default();
        let prefs = Preferences {
            password: "must-not-persist".into(),
            password_cleared: true,
            ..Preferences::default()
        };
        assert!(save_with(&path, &prefs, &store).is_err());
        assert!(!path.exists());
        assert!(store.get().unwrap().is_none());
        for bytes in [
            b"not-json".as_slice(),
            br#"{"format_version":99}"#,
            br#"{"username":12}"#,
        ] {
            atomic_write(&path, bytes).unwrap();
            assert!(load_with(&path, || panic!("bad disk data must not access keyring")).is_err());
        }
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn unix_file_and_new_directory_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = temporary();
        save_with(&path, &Preferences::default(), &FakeStore::default()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
    #[test]
    fn legacy_plaintext_is_not_automatically_used_or_migrated() {
        let path = temporary();
        atomic_write(
            &path,
            br#"{"username":"old","password":"old-secret","auto_login":true}"#,
        )
        .unwrap();
        assert!(load_with(&path, || panic!("do not import plaintext")).is_err());
        let prefs = load_non_sensitive(&path).unwrap();
        assert_eq!(prefs.username, "old");
        assert!(prefs.password.is_empty());
        assert!(prefs.minimize_to_tray);
        assert!(!prefs.auto_login);
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("old-secret"),
            "recovery must not modify user files"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
