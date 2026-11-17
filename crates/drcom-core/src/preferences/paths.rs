//! Environment-independent path resolution; never creates or migrates files.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
use std::path::Path;
use std::{io, path::PathBuf};

const APP_DIR: &str = "DrComCampusRust";

fn variable(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn unix_absolute(path: Option<PathBuf>) -> Option<PathBuf> {
    // A Unix path must start with '/', even when these pure helpers are tested
    // on Windows. XDG explicitly says to ignore relative environment values.
    path.filter(|path| path.as_os_str().as_encoded_bytes().starts_with(b"/"))
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn xdg_config_base(xdg: Option<PathBuf>, home: Option<PathBuf>) -> io::Result<PathBuf> {
    unix_absolute(xdg)
        .or_else(|| unix_absolute(home).map(|home| home.join(".config")))
        .ok_or_else(|| {
            io::Error::other("无法定位用户配置目录（需要绝对路径 HOME 或 XDG_CONFIG_HOME）")
        })
}

#[cfg(any(target_os = "macos", test))]
fn macos_directory(home: Option<PathBuf>) -> io::Result<PathBuf> {
    unix_absolute(home)
        .map(|home| home.join("Library/Application Support").join(APP_DIR))
        .ok_or_else(|| io::Error::other("无法定位用户配置目录（需要绝对路径 HOME）"))
}

#[cfg(target_os = "macos")]
pub(super) fn home_directory() -> io::Result<PathBuf> {
    unix_absolute(variable("HOME"))
        .ok_or_else(|| io::Error::other("无法定位用户目录（需要绝对路径 HOME）"))
}

pub(super) fn data_directory() -> io::Result<PathBuf> {
    #[cfg(windows)]
    return variable("LOCALAPPDATA")
        .filter(|path| path.is_absolute())
        .map(|base| base.join(APP_DIR))
        .ok_or_else(|| {
            io::Error::other("无法定位用户配置目录（LOCALAPPDATA 未设置或不是绝对路径）")
        });
    #[cfg(target_os = "macos")]
    return macos_directory(variable("HOME"));
    #[cfg(target_os = "linux")]
    return xdg_config_base(variable("XDG_CONFIG_HOME"), variable("HOME"))
        .map(|base| base.join(APP_DIR));
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "当前平台不支持用户配置目录",
    ))
}

pub(super) fn config_directories() -> Vec<PathBuf> {
    let mut directories = Vec::new();
    if let Ok(directory) = data_directory() {
        directories.push(directory);
    }
    #[cfg(windows)]
    if let Some(base) = variable("APPDATA").filter(|path| path.is_absolute()) {
        // Keep the old Windows roaming directory for deployed installations.
        directories.push(base.join("DrComCampus"));
    }
    #[cfg(target_os = "linux")]
    if let Ok(base) = xdg_config_base(variable("XDG_CONFIG_HOME"), variable("HOME")) {
        // origin/linux used this spelling; do not strand its drcom.ini.
        directories.push(base.join("drcom-campus"));
    }
    directories
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn legacy_linux_directory(
    xdg_data: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    unix_absolute(xdg_data)
        .or_else(|| unix_absolute(home).map(|home| home.join(".local/share")))
        .map(|base| base.join("drcom-campus"))
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(super) fn legacy_preferences_path(requested: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    if data_directory().ok()?.join("preferences.dat") == requested {
        return legacy_linux_directory(variable("XDG_DATA_HOME"), variable("HOME"))
            .map(|directory| directory.join("preferences.dat"));
    }
    let _ = requested;
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn xdg_uses_only_absolute_values_and_falls_back_to_home() {
        let home = Some(PathBuf::from("/home/student"));
        assert_eq!(
            xdg_config_base(Some("/xdg/config".into()), home.clone()).unwrap(),
            PathBuf::from("/xdg/config")
        );
        for invalid in [None, Some(PathBuf::new()), Some("relative/config".into())] {
            assert_eq!(
                xdg_config_base(invalid, home.clone()).unwrap(),
                PathBuf::from("/home/student/.config")
            );
        }
        assert!(xdg_config_base(Some("relative".into()), None).is_err());
        assert!(xdg_config_base(None, Some("relative/home".into())).is_err());
    }
    #[test]
    fn macos_uses_application_support_not_xdg() {
        assert_eq!(
            macos_directory(Some("/Users/学生".into())).unwrap(),
            PathBuf::from("/Users/学生/Library/Application Support/DrComCampusRust")
        );
        assert!(macos_directory(None).is_err());
    }
    #[test]
    fn linux_branch_legacy_data_path_is_preserved_for_explicit_recovery() {
        assert_eq!(
            legacy_linux_directory(Some("/xdg/data".into()), None),
            Some(PathBuf::from("/xdg/data/drcom-campus"))
        );
        assert_eq!(
            legacy_linux_directory(Some("relative".into()), Some("/home/u".into())),
            Some(PathBuf::from("/home/u/.local/share/drcom-campus"))
        );
    }
}
