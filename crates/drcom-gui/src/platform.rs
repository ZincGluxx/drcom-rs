//! Desktop integration, kept out of the frontend-independent backend.

use std::io;
use std::path::Path;
use std::process::Command;

/// Copy diagnostic text without putting it on the process command line.
pub fn copy(text: &str) -> io::Result<()> {
    #[cfg(windows)]
    {
        clipboard_win::set_clipboard_string(text).map_err(io::Error::other)
    }
    #[cfg(target_os = "macos")]
    {
        pipe_text(Command::new("/usr/bin/pbcopy"), text)
    }
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            let mut command = Command::new("wl-copy");
            command.args(["--type", "text/plain;charset=utf-8"]);
            if pipe_text(command, text).is_ok() {
                return Ok(());
            }
        }
        let mut command = Command::new("xclip");
        command.args(["-selection", "clipboard"]);
        if pipe_text(command, text).is_ok() {
            return Ok(());
        }
        let mut command = Command::new("xsel");
        command.args(["--clipboard", "--input"]);
        pipe_text(command, text).map_err(|_| io::Error::other(
            "无法访问剪贴板：Wayland 请安装 wl-clipboard，X11 请安装 xclip 或 xsel；也可导出诊断文件",
        ))
    }
}

#[cfg(unix)]
fn pipe_text(mut command: Command, text: &str) -> io::Result<()> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let result = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("无法打开剪贴板输入"))?
        .write_all(text.as_bytes());
    let status = child.wait()?;
    result?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("剪贴板工具执行失败"))
    }
}

pub fn export(report: &str) -> io::Result<std::path::PathBuf> {
    let directory = drcom_core::preferences::data_directory()?;
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!(
        "drcom-诊断-{}.txt",
        drcom_core::event_log::compact_timestamp()
    ));
    let mut bytes = Vec::with_capacity(report.len() + 3);
    bytes.extend_from_slice(&[0xef, 0xbb, 0xbf]);
    bytes.extend_from_slice(report.as_bytes());
    std::fs::write(&path, bytes)?;
    let _ = reveal(&directory);
    Ok(path)
}

pub fn reveal(directory: &Path) -> io::Result<()> {
    #[cfg(windows)]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new("explorer.exe");
        command.creation_flags(0x08000000);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = Command::new("/usr/bin/open");
    #[cfg(target_os = "linux")]
    let mut command = Command::new("xdg-open");
    command.arg(directory).spawn()?;
    Ok(())
}

pub fn open_network_settings() -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        Command::new("control.exe")
            .arg("ncpa.cpl")
            .creation_flags(0x08000000)
            .spawn()?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("/usr/bin/open")
            .arg("x-apple.systempreferences:com.apple.Network-Settings.extension")
            .spawn()?;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        for (program, arguments) in [
            ("nm-connection-editor", &[][..]),
            ("gnome-control-center", &["network"][..]),
            ("systemsettings", &["kcm_networkmanagement"][..]),
        ] {
            if Command::new(program).args(arguments).spawn().is_ok() {
                return Ok(());
            }
        }
        Err(io::Error::other(
            "请打开桌面的网络设置，或安装 NetworkManager 的 nm-connection-editor",
        ))
    }
}

pub fn font_family() -> &'static str {
    if cfg!(target_os = "windows") {
        "Microsoft YaHei UI"
    } else if cfg!(target_os = "macos") {
        "PingFang SC"
    } else {
        "Noto Sans CJK SC"
    }
}

/// Reused from the Linux branch. No watcher (e.g. stock GNOME) must not mean
/// an inaccessible hidden window. The caller minimizes to the dock instead.
pub fn tray_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        if let Ok(output) = Command::new("busctl")
            .args([
                "--user",
                "--timeout=2",
                "status",
                "org.kde.StatusNotifierWatcher",
            ])
            .output()
        {
            return output.status.success();
        }
        if let Ok(output) = Command::new("gdbus")
            .args([
                "call",
                "--session",
                "--dest",
                "org.freedesktop.DBus",
                "--object-path",
                "/org/freedesktop/DBus",
                "--method",
                "org.freedesktop.DBus.NameHasOwner",
                "org.kde.StatusNotifierWatcher",
            ])
            .output()
        {
            return output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("true");
        }
    }
    true
}

/// Best-effort Linux X11 theme hint from the existing Linux branch. On the
/// other platforms Slint follows native ThemeChanged notifications directly.
pub fn scheme_hint() -> Option<i32> {
    #[cfg(target_os = "linux")]
    {
        fn output(program: &str, args: &[&str]) -> Option<String> {
            let output = Command::new(program).args(args).output().ok()?;
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
        }
        if let Some(value) = output(
            "gsettings",
            &["get", "org.gnome.desktop.interface", "color-scheme"],
        ) {
            if value.contains("prefer-dark") {
                return Some(2);
            }
            if value.contains("prefer-light") {
                return Some(1);
            }
            if let Some(theme) = output(
                "gsettings",
                &["get", "org.gnome.desktop.interface", "gtk-theme"],
            ) {
                return Some(if theme.to_ascii_lowercase().contains("dark") {
                    2
                } else {
                    1
                });
            }
        }
        for tool in ["kreadconfig6", "kreadconfig5"] {
            if let Some(value) = output(
                tool,
                &[
                    "--file",
                    "kdeglobals",
                    "--group",
                    "General",
                    "--key",
                    "ColorScheme",
                ],
            ) && !value.trim().is_empty()
            {
                return Some(if value.to_ascii_lowercase().contains("dark") {
                    2
                } else {
                    1
                });
            }
        }
    }
    None
}
