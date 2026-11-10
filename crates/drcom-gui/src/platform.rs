//! Desktop integration that belongs to the GUI frontend, not to the backend.
//!
//! Clipboard access, revealing a file in Explorer and opening the Windows
//! adapter UI are all "there is a logged-in desktop user here" operations. The
//! headless backend (`drcom-core` / `drcom-service`) deliberately knows nothing
//! about them, so other frontends (daemon, CLI, mobile shell) can reuse the
//! backend without dragging in a windowing assumption.

/// Copies the diagnostics text to the Windows clipboard.
pub fn copy(text: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        clipboard_win::set_clipboard_string(text).map_err(std::io::Error::other)
    }
    #[cfg(not(windows))]
    {
        let _ = text;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "剪贴板暂不支持当前平台",
        ))
    }
}

/// Writes the diagnostics text to a timestamped file beside the event log and
/// shows it in Explorer, so a user can attach it to a report instead of trying to
/// paste a wall of text into a chat window.
pub fn export(report: &str) -> std::io::Result<std::path::PathBuf> {
    let directory = drcom_core::preferences::data_directory()?;
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!(
        "drcom-诊断-{}.txt",
        drcom_core::event_log::compact_timestamp()
    ));
    // Prefixed so Notepad — which still guesses the code page from the bytes on
    // older builds — opens the Chinese text correctly instead of as mojibake.
    let mut bytes = Vec::with_capacity(report.len() + 3);
    bytes.extend_from_slice(&[0xef, 0xbb, 0xbf]);
    bytes.extend_from_slice(report.as_bytes());
    std::fs::write(&path, bytes)?;
    let _ = reveal(&directory);
    Ok(path)
}

/// Opens a folder in Explorer. Failure is not worth reporting: the file is
/// already written, and the path is shown in the window either way.
pub fn reveal(directory: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("explorer.exe")
            .arg(directory)
            .creation_flags(0x08000000)
            .spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = directory;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "当前平台无法打开文件夹",
        ))
    }
}

/// Opens the standard Windows adapter UI for privileged IPv4/DNS edits.
pub fn open_network_settings() -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("control.exe")
            .arg("ncpa.cpl")
            .creation_flags(0x08000000)
            .spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "请打开系统网络设置",
        ))
    }
}
