//! Single-instance guard, ported from the C# client's `Program.cs`.
//!
//! The C# build takes a named mutex at startup and a second copy hands its
//! request over through a named pipe before exiting. The Rust build had no such
//! check, so every double-click added another process — and every process adds
//! its own notification-area icon: the Windows tray backend keys an icon by
//! `(HWND, uID)` and Slint uses a fixed `uID` in each process, so the shell has
//! no reason to merge them.
//!
//! The handover here uses a named event instead of a pipe, which needs no I/O
//! thread: the first instance parks a worker on `WaitForSingleObject` and the
//! duplicate only has to call `SetEvent`.

use std::ffi::c_void;
use std::ptr;

/// Name shared by every process of this binary. Prefixed `Local\` so two
/// logged-in users each get their own instance, and distinct from the C#
/// client's name so both builds can be installed side by side.
pub const APP_NAME: &str = "Local\\DrComCampus-Rust-SingleInstance";

const ERROR_ALREADY_EXISTS: u32 = 183;
const INFINITE: u32 = 0xFFFF_FFFF;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateMutexW(attributes: *const c_void, initial_owner: i32, name: *const u16)
    -> *mut c_void;
    fn CreateEventW(
        attributes: *const c_void,
        manual_reset: i32,
        initial_state: i32,
        name: *const u16,
    ) -> *mut c_void;
    fn SetEvent(handle: *mut c_void) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn SetLastError(code: u32);
    fn GetLastError() -> u32;
}

/// Outcome of [`claim`].
pub enum Launch {
    /// This process owns the session and should carry on starting up.
    Primary(PrimaryInstance),
    /// Another copy is already running. It has been asked to surface its
    /// window, so this process only has to exit.
    Duplicate,
}

/// Claims the session. Call this before any window or tray icon exists — a
/// duplicate must never reach the point where it registers its own icon.
pub fn claim(name: &str) -> Launch {
    // The event is created first so that, whenever the mutex says "someone else
    // is running", that someone has already published the handle we signal.
    let event_name = wide(&format!("{name}-Activate"));
    let event = unsafe { CreateEventW(ptr::null(), 0, 0, event_name.as_ptr()) };
    if event.is_null() {
        // Without the event the handover cannot work. Starting without a guard
        // is the old behaviour; refusing to start would be worse.
        return Launch::Primary(PrimaryInstance {
            mutex: ptr::null_mut(),
            event: ptr::null_mut(),
        });
    }

    // `CreateMutexW` reports "already existed" through the last-error slot, so
    // clear it first and read it back immediately.
    let mutex_name = wide(name);
    unsafe { SetLastError(0) };
    let mutex = unsafe { CreateMutexW(ptr::null(), 0, mutex_name.as_ptr()) };
    let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;

    if already_running {
        if !mutex.is_null() {
            unsafe { CloseHandle(mutex) };
        }
        unsafe {
            SetEvent(event);
            CloseHandle(event);
        }
        return Launch::Duplicate;
    }

    Launch::Primary(PrimaryInstance { mutex, event })
}

/// Held for the lifetime of the process that won the race.
pub struct PrimaryInstance {
    mutex: *mut c_void,
    event: *mut c_void,
}

impl PrimaryInstance {
    /// Runs `on_activate` on a worker thread whenever another process starts.
    /// The callback fires on that thread, so an event-loop hop is still needed.
    pub fn listen(&self, on_activate: impl Fn() + Send + 'static) {
        if self.event.is_null() {
            return;
        }
        // The handle stays valid for the whole process (`PrimaryInstance` lives
        // in `main` and the worker never outlives it), so passing it as an
        // integer is sound as long as the worker only ever waits on it.
        let event = self.event as usize;
        std::thread::spawn(move || {
            loop {
                unsafe { WaitForSingleObject(event as *mut c_void, INFINITE) };
                on_activate();
            }
        });
    }
}

impl Drop for PrimaryInstance {
    fn drop(&mut self) {
        unsafe {
            if !self.mutex.is_null() {
                CloseHandle(self.mutex);
            }
            if !self.event.is_null() {
                CloseHandle(self.event);
            }
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    // One name per test: the harness runs tests on parallel threads inside a
    // single process, so a shared name would make them claim each other's lock.
    const HANDSHAKE_TEST: &str = "Local\\DrComCampus-Rust-Test-Handshake";
    const DUPLICATE_TEST: &str = "Local\\DrComCampus-Rust-Test-Duplicate";

    #[test]
    fn the_second_claim_is_reported_as_a_duplicate() {
        let Launch::Primary(primary) = claim(DUPLICATE_TEST) else {
            panic!("the first claim has to win");
        };
        assert!(matches!(claim(DUPLICATE_TEST), Launch::Duplicate));
        // Dropping releases the mutex, so a later claim could win again.
        drop(primary);
    }

    #[test]
    fn a_duplicate_asks_the_running_copy_to_show_its_window() {
        let Launch::Primary(primary) = claim(HANDSHAKE_TEST) else {
            panic!("the first claim has to win");
        };
        let signalled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&signalled);
        primary.listen(move || flag.store(true, Ordering::SeqCst));

        assert!(matches!(claim(HANDSHAKE_TEST), Launch::Duplicate));

        let deadline = Instant::now() + Duration::from_secs(5);
        while !signalled.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            signalled.load(Ordering::SeqCst),
            "the duplicate never reached the running instance"
        );
    }
}
