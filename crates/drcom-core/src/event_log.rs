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

#[cfg(unix)]
fn local_time() -> LocalTime {
    let mut seconds: libc::time_t = 0;
    let mut value: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both calls write only to correctly sized local storage;
    // localtime_r is the thread-safe API and uses the OS timezone database.
    unsafe { libc::time(&mut seconds) };
    if unsafe { libc::localtime_r(&seconds, &mut value) }.is_null() {
        // UTC is preferable to a fabricated 1970 date if local TZ lookup fails.
        unsafe { libc::gmtime_r(&seconds, &mut value) };
    }
    LocalTime {
        year: (value.tm_year + 1900).clamp(0, u16::MAX as i32) as u16,
        month: (value.tm_mon + 1) as u16,
        day: value.tm_mday as u16,
        hour: value.tm_hour as u16,
        minute: value.tm_min as u16,
        second: value.tm_sec as u16,
    }
}

#[cfg(not(any(windows, unix)))]
compile_error!("event log local time requires Windows or Unix");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_have_real_calendar_values_and_safe_filename_shape() {
        let stamp = timestamp();
        assert_eq!(stamp.len(), 19);
        assert!(!stamp.starts_with("1970-"));
        let compact = compact_timestamp();
        assert_eq!(compact.len(), 15);
        assert!(
            compact
                .bytes()
                .all(|value| value.is_ascii_digit() || value == b'-')
        );
    }

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
}
