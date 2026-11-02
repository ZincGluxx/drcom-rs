//! Bounded, in-memory diagnostic summaries for explicit copy/export.
//! No automatic file writes. The GUI omits protocol notes and heartbeat spam.
//! Callers must never include credentials or session tokens in summaries.

use std::{
    collections::VecDeque,
    sync::{Mutex, OnceLock},
};

const RECENT_LIMIT: usize = 80;
static RECENT: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();

/// Retains a short summary in memory until exit. Never writes to disk.
pub fn record(message: &str) {
    if let Ok(mut recent) = RECENT.get_or_init(|| Mutex::new(VecDeque::new())).lock() {
        if recent.len() == RECENT_LIMIT {
            recent.pop_front();
        }
        recent.push_back(format!("[{}] {}", timestamp(), single_line(message)));
    }
}

/// Recent summaries from this process only; old log files are never read.
pub fn tail(lines: usize) -> String {
    let Ok(recent) = RECENT.get_or_init(|| Mutex::new(VecDeque::new())).lock() else {
        return String::new();
    };
    recent
        .iter()
        .skip(recent.len().saturating_sub(lines))
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
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
    // chrono resolves the platform's local time zone, so Linux reports the same
    // wall-clock time the user sees; the Windows branch keeps GetLocalTime.
    use chrono::{Datelike, Timelike};
    let now = chrono::Local::now();
    LocalTime {
        year: u16::try_from(now.year()).unwrap_or(0),
        month: now.month() as u16,
        day: now.day() as u16,
        hour: now.hour() as u16,
        minute: now.minute() as u16,
        second: now.second() as u16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_are_bounded_and_current_process_only() {
        assert_eq!(tail(5), "");
        for index in 0..100 {
            record(&format!("检测结果 {index}\n下一行"));
        }
        let text = tail(100);
        assert_eq!(text.lines().count(), RECENT_LIMIT);
        assert!(!text.contains("] 检测结果 19 "));
        assert!(text.contains("] 检测结果 99 下一行"));
        assert_eq!(tail(1).lines().count(), 1);
        assert_eq!(tail(0), "");
    }

    #[cfg(not(windows))]
    #[test]
    fn timestamps_come_from_the_local_clock_not_the_epoch() {
        let stamp = timestamp();
        assert_eq!(
            stamp.len(),
            19,
            "the format is YYYY-MM-DD HH:MM:SS: {stamp}"
        );
        assert!(
            !stamp.starts_with("1970"),
            "the Unix clock stub must not come back: {stamp}"
        );
        let compact = compact_timestamp();
        assert_eq!(
            compact.len(),
            15,
            "the format is YYYYMMDD-HHMMSS: {compact}"
        );
        assert!(
            compact
                .chars()
                .all(|character| character.is_ascii_digit() || character == '-'),
            "the compact stamp must contain no separators but its dash: {compact}"
        );
    }
}
