//! User-login autostart. Linux adapts origin/linux's XDG entry, with complete
//! desktop-entry escaping; macOS uses LaunchAgent ProgramArguments (no shell).
#[cfg(any(target_os = "macos", target_os = "linux"))]
use super::{atomic_write, paths};
use std::path::PathBuf;
use std::{io, path::Path};
#[cfg(any(target_os = "linux", test))]
const DESKTOP_NAME: &str = "drcom-campus.desktop";
#[cfg(any(target_os = "macos", test))]
const AGENT_NAME: &str = "io.github.drcom-campus-rust.plist";

#[cfg(any(target_os = "macos", test))]
fn agent_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents").join(AGENT_NAME)
}

#[cfg(any(target_os = "linux", test))]
fn desktop_path(base: &Path) -> PathBuf {
    base.join("autostart").join(DESKTOP_NAME)
}

fn executable_text(executable: &Path) -> io::Result<&str> {
    let text = executable.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "开机启动程序路径必须是有效 UTF-8",
        )
    })?;
    if text.is_empty() || text.chars().any(char::is_control) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "开机启动程序路径不能包含控制字符",
        ));
    }
    Ok(text)
}

/// An Exec argument has two escape layers: quoting first, then general desktop
/// string escapes. '%' must be doubled, or e.g. a directory '%f' is a field code.
#[cfg(any(target_os = "linux", test))]
fn quote_exec(executable: &Path) -> io::Result<String> {
    let mut argument = String::from("\"");
    for character in executable_text(executable)?.chars() {
        match character {
            '%' => argument.push_str("%%"),
            '\\' | '"' | '$' | '`' => {
                argument.push('\\');
                argument.push(character);
            }
            other => argument.push(other),
        }
    }
    argument.push('"');
    Ok(argument.replace('\\', "\\\\"))
}

#[cfg(any(target_os = "linux", test))]
fn desktop_entry(executable: &Path) -> io::Result<String> {
    Ok(format!(
        "[Desktop Entry]\nType=Application\nName=DrCom 校园网助手\nExec={} --tray\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
        quote_exec(executable)?
    ))
}

#[cfg(any(target_os = "macos", test))]
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(any(target_os = "macos", test))]
fn launch_agent(executable: &Path) -> io::Result<String> {
    let executable = xml_escape(executable_text(executable)?);
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>io.github.drcom-campus-rust</string>\n  <key>ProgramArguments</key>\n  <array><string>{executable}</string><string>--tray</string></array>\n  <key>RunAtLoad</key><true/>\n</dict>\n</plist>\n"
    ))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn entry_path() -> io::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    return Ok(agent_path(&paths::home_directory()?));
    #[cfg(target_os = "linux")]
    return Ok(desktop_path(&paths::xdg_config_base(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )?));
}

#[cfg(any(target_os = "linux", test))]
fn desktop_enabled(text: &str, executable: &Path) -> io::Result<bool> {
    let mut in_entry = false;
    let mut program = None;
    let mut application = false;
    let mut hidden = false;
    let mut disabled = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "Exec" => program = Some(value.trim()),
                "Type" => application = value.trim() == "Application",
                "Hidden" => hidden = value.trim().eq_ignore_ascii_case("true"),
                "X-GNOME-Autostart-enabled" => {
                    disabled = value.trim().eq_ignore_ascii_case("false")
                }
                _ => {}
            }
        }
    }
    let quoted = quote_exec(executable)?;
    // Keep origin/linux's --silent entries and pre-tray entries recognizable.
    Ok(application
        && !hidden
        && !disabled
        && program.is_some_and(|value| {
            value == quoted
                || value == format!("{quoted} --tray")
                || value == format!("{quoted} --silent")
        }))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn enabled() -> io::Result<bool> {
    let text = match std::fs::read_to_string(entry_path()?) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let executable = std::env::current_exe()?;
    #[cfg(target_os = "macos")]
    return Ok(text == launch_agent(&executable)?);
    #[cfg(target_os = "linux")]
    return desktop_enabled(&text, &executable);
}

/// Installs/removes the current user's next-login entry only. It does not launch
/// a second process now, unload the running app, or require administrator rights.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn set(enabled: bool) -> io::Result<()> {
    let path = entry_path()?;
    if !enabled {
        return match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
    }
    #[cfg(target_os = "macos")]
    let text = launch_agent(&std::env::current_exe()?)?;
    #[cfg(target_os = "linux")]
    let text = desktop_entry(&std::env::current_exe()?)?;
    atomic_write(&path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn decode_exec(value: &str) -> String {
        // Model the two documented escape layers, independent of the encoder.
        let mut chars = value.chars();
        let mut general = String::new();
        while let Some(c) = chars.next() {
            if c == '\\' {
                assert_eq!(chars.next(), Some('\\'));
                general.push('\\');
            } else {
                general.push(c);
            }
        }
        let mut chars = general.chars();
        assert_eq!(chars.next(), Some('"'));
        let mut decoded = String::new();
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    assert!(chars.next().is_none());
                    return decoded;
                }
                '\\' => decoded.push(chars.next().unwrap()),
                '%' => {
                    assert_eq!(chars.next(), Some('%'));
                    decoded.push('%');
                }
                c => decoded.push(c),
            }
        }
        panic!("missing closing quote")
    }
    #[test]
    fn desktop_exec_survives_both_escape_layers_and_literal_field_codes() {
        for value in [
            "/usr/bin/drcom-campus",
            "/opt/Dr Com/校园/drcom-campus",
            "/tmp/a \"quote\" $dollar `tick`/drcom-campus",
            "/tmp/back\\slash/%f %u %%/drcom-campus",
        ] {
            assert_eq!(decode_exec(&quote_exec(Path::new(value)).unwrap()), value);
        }
        assert!(quote_exec(Path::new("/tmp/a\nHidden=true")).is_err());
        assert!(quote_exec(Path::new("/tmp/a\rExec=/bin/false")).is_err());
        assert!(quote_exec(Path::new("/tmp/tab\tname")).is_err());
    }
    #[test]
    fn desktop_enabled_honors_disabled_flags_and_legacy_silent() {
        let executable = Path::new("/opt/Dr Com/drcom-campus");
        let entry = desktop_entry(executable).unwrap();
        assert!(desktop_enabled(&entry, executable).unwrap());
        assert!(desktop_enabled(&entry.replace("--tray", "--silent"), executable).unwrap());
        assert!(!desktop_enabled(&format!("{entry}Hidden=true\n"), executable).unwrap());
        assert!(
            !desktop_enabled(&entry.replace("enabled=true", "enabled=false"), executable).unwrap()
        );
        assert!(!desktop_enabled(&entry, Path::new("/bin/false")).unwrap());
        assert!(!desktop_enabled("[Other]\nType=Application\nExec=/bin/a\n", executable).unwrap());
    }
    #[test]
    fn login_entries_resolve_inside_user_directories() {
        assert_eq!(
            agent_path(Path::new("/Users/student")),
            PathBuf::from("/Users/student/Library/LaunchAgents/io.github.drcom-campus-rust.plist")
        );
        assert_eq!(
            desktop_path(Path::new("/home/u/.config")),
            PathBuf::from("/home/u/.config/autostart/drcom-campus.desktop")
        );
    }
    #[test]
    fn launch_agent_escapes_xml_without_shell_interpolation() {
        let entry = launch_agent(Path::new(
            "/Applications/A & B/\"$HOME\"<'校园'>/drcom-campus",
        ))
        .unwrap();
        assert!(entry.contains("A &amp; B/&quot;$HOME&quot;&lt;&apos;校园&apos;&gt;"));
        assert!(entry.contains("<array><string>"));
        assert!(entry.contains("<string>--tray</string></array>"));
        assert!(entry.contains("<key>RunAtLoad</key><true/>"));
        assert!(!entry.contains("/bin/sh"));
        assert!(launch_agent(Path::new("/bad\0path")).is_err());
    }
}
