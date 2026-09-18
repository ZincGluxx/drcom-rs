//! A plain-text session log, for the one place this program is hardest to debug.
//!
//! A campus deployment is simultaneously the only environment where the client
//! has to work and the one with no debugger, no console and no way to reproduce
//! the failure later — the user can only describe the symptom, usually as "连不
//! 上". So every state transition, keep-alive sequence number and failure reason
//! is also appended to a rotating text file that the user can hand over.
//!
//! **Credentials never reach this file.** Nothing here inspects or filters its
//! input: callers pass short human-readable lines and are responsible for not
//! putting the account or password into them. The event sites in `ui_backend`
//! and `drcom-cli` were written with that rule in mind, and the tests below pin
//! the file format rather than trying to detect a leak after the fact.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

/// Rotate once the log passes this size. The interesting window is the last few
/// minutes before a failure, and one previous generation is enough to cover a
/// user who reproduces a problem twice.
const MAX_BYTES: u64 = 256 * 1024;

/// Suffix of the single previous generation.
const ROTATED_SUFFIX: &str = "1";

/// `%LOCALAPPDATA%\DrComCampusRust\drcom.log`, next to the saved credentials.
pub fn path() -> io::Result<PathBuf> {
    Ok(crate::preferences::data_directory()?.join("drcom.log"))
}

/// Where the single rotated generation lives.
pub fn rotated_path() -> io::Result<PathBuf> {
    let base = path()?;
    let mut name = base.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{ROTATED_SUFFIX}"));
    Ok(base.with_file_name(name))
}

/// Appends one timestamped line to the default log.
///
/// The error is swallowed on purpose: a full disk or a locked file must never be
/// the reason a login fails. [`append`] returns it, which is how the format stays
/// testable.
pub fn record(message: &str) {
    if let Ok(file) = path() {
        let _ = append(&file, message);
    }
}

/// Appends one timestamped line to `file`, rotating first if it has grown.
pub fn append(file: &Path, message: &str) -> io::Result<()> {
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)?;
    }
    rotate_if_large(file)?;
    let mut handle = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)?;
    writeln!(handle, "[{}] {}", timestamp(), single_line(message))
}

/// The last `lines` lines, oldest first, for embedding in an exported report.
///
/// Missing files are not an error: a user who never connected has no log, and
/// the diagnostics text should say so rather than fail to be produced.
pub fn tail(lines: usize) -> String {
    let Ok(file) = path() else {
        return String::new();
    };
    let Ok(text) = fs::read_to_string(&file) else {
        return String::new();
    };
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// Renames the log to its single rotated generation once it grows past
/// [`MAX_BYTES`], replacing whatever was there.
fn rotate_if_large(file: &Path) -> io::Result<()> {
    let too_large = fs::metadata(file)
        .map(|meta| meta.len() > MAX_BYTES)
        .unwrap_or(false);
    if !too_large {
        return Ok(());
    }
    let Some(parent) = file.parent() else {
        return Ok(());
    };
    let mut name = file.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{ROTATED_SUFFIX}"));
    let rotated = parent.join(name);
    // A failed rotation is not worth failing the write over: the append below
    // still succeeds and the log simply keeps growing.
    let _ = fs::remove_file(&rotated);
    let _ = fs::rename(file, &rotated);
    Ok(())
}

/// Keeps one log line on one line, so `tail` can split the file safely.
fn single_line(message: &str) -> String {
    message.replace(['\r', '\n'], " ")
}

/// Local wall-clock time, because that is what the user compares against.
pub fn timestamp() -> String {
    let now = local_time();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        now.year, now.month, now.day, now.hour, now.minute, now.second
    )
}

/// The same instant in a form safe for a file name.
pub fn compact_timestamp() -> String {
    let now = local_time();
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        now.year, now.month, now.day, now.hour, now.minute, now.second
    )
}

/// The pieces of the local clock this module needs, kept separate from the Win32
/// struct so the formatting above is plain arithmetic.
struct LocalTime {
    year: u16,
    month: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
}

#[cfg(windows)]
fn local_time() -> LocalTime {
    use windows_sys::Win32::{Foundation::SYSTEMTIME, System::SystemInformation::GetLocalTime};
    let mut now: SYSTEMTIME = unsafe { std::mem::zeroed() };
    // SAFETY: GetLocalTime only writes to the struct it is handed.
    unsafe { GetLocalTime(&mut now) };
    LocalTime {
        year: now.wYear,
        month: now.wMonth,
        day: now.wDay,
        hour: now.wHour,
        minute: now.wMinute,
        second: now.wSecond,
    }
}

#[cfg(not(windows))]
fn local_time() -> LocalTime {
    // Getting local time without a platform call needs a timezone database. This
    // client only ships for Windows, so the fallback exists only to keep the
    // module compiling for cross-checks and is deliberately not clever.
    LocalTime {
        year: 1970,
        month: 1,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("drcom-log-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("scratch directory");
        directory.join(name)
    }

    #[test]
    fn lines_carry_a_timestamp_and_survive_multiline_input() {
        let file = scratch("format.log");
        let _ = fs::remove_file(&file);
        append(&file, "认证成功\n保活中").unwrap();
        append(&file, "已下线").unwrap();
        let text = fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one call must produce exactly one line");
        for line in &lines {
            assert!(
                line.starts_with('[') && line.contains("] "),
                "missing timestamp: {line}"
            );
        }
        assert!(lines[0].ends_with("认证成功 保活中"));
        assert!(lines[1].ends_with("已下线"));
        let _ = fs::remove_file(&file);
    }

    #[test]
    fn a_full_log_rotates_to_one_previous_generation() {
        let file = scratch("rotate.log");
        let rotated = file.with_file_name(format!("rotate.log.{ROTATED_SUFFIX}"));
        let _ = fs::remove_file(&file);
        let _ = fs::remove_file(&rotated);
        // Fill past the limit in one write so the next append has to rotate.
        fs::write(&file, vec![b'x'; (MAX_BYTES + 1) as usize]).unwrap();
        append(&file, "轮转之后").unwrap();
        assert!(rotated.is_file(), "the oversized log was not rotated");
        assert_eq!(fs::metadata(&rotated).unwrap().len(), MAX_BYTES + 1);
        let current = fs::read_to_string(&file).unwrap();
        assert!(
            current.trim_end().ends_with("轮转之后"),
            "the fresh log should hold the new line, got {current:?}"
        );
        assert!(
            current.len() < 128,
            "the current log should start over, got {} bytes",
            current.len()
        );
        let _ = fs::remove_file(&file);
        let _ = fs::remove_file(&rotated);
    }

    #[test]
    fn tail_is_empty_rather_than_an_error_when_there_is_no_log() {
        // `tail` reads the real default path, which this test must not create.
        // Removing it first is safe: nothing else in the suite writes the log.
        if let Ok(file) = path() {
            let _ = fs::remove_file(&file);
        }
        assert_eq!(tail(5), "");
    }
}
