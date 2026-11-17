//! Single-instance guard, ported from the C# client's `Program.cs`.
//!
//! The C# build takes a named mutex at startup and a second copy hands its
//! request over through a named pipe before exiting. The Rust build had no such
//! check, so every double-click added another process — and every process adds
//! its own notification-area icon: the Windows tray backend keys an icon by
//! `(HWND, uID)` and Slint uses a fixed `uID` in each process, so the shell has
//! no reason to merge them.
//!
//! Windows keeps the port's original design: a named event instead of a pipe,
//! which needs no I/O thread — the first instance parks a worker on
//! `WaitForSingleObject` and the duplicate only has to call `SetEvent`.
//!
//! Linux/macOS adapt the Linux branch's per-user Unix socket activation. A
//! lifetime flock serializes primary ownership before binding/cleaning a crash
//! leftover socket; a duplicate only sends one activation byte. Private runtime
//! directories, O_NOFOLLOW locks and bounded reads prevent cross-user spoofing
//! and shutdown hangs.

#[cfg(windows)]
use std::ffi::c_void;
#[cfg(windows)]
use std::ptr;
#[cfg(not(windows))]
use std::{
    io::{Read, Write},
    os::unix::{
        fs::MetadataExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// Name shared by every process of this binary. Prefixed `Local\` on Windows so
/// two logged-in users each get their own instance, and distinct from the C#
/// client's name so both builds can be installed side by side. On Unix the
/// runtime directory is already per-user and the name is sanitised into the
/// socket file name (`[A-Za-z0-9_-]` only).
pub const APP_NAME: &str = "Local\\DrComCampus-Rust-SingleInstance";

#[cfg(windows)]
const ERROR_ALREADY_EXISTS: u32 = 183;
#[cfg(windows)]
const INFINITE: u32 = 0xFFFF_FFFF;

#[cfg(windows)]
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

/// Optional footprint-test namespace, retained from the Windows frontend.
pub fn app_name() -> String {
    match std::env::var("DRCOM_INSTANCE_SUFFIX") {
        Ok(suffix) if !suffix.is_empty() => format!("{APP_NAME}-{suffix}"),
        _ => APP_NAME.to_string(),
    }
}

/// Outcome of [`claim`].
pub enum Launch {
    /// This process owns the session and should carry on starting up.
    Primary(PrimaryInstance),
    /// Another copy is already running. It has been asked to surface its
    /// window, so this process only has to exit.
    Duplicate,
    /// Unix guard setup failed. Refuse to start a competing authentication session.
    #[cfg(unix)]
    Failed(std::io::Error),
}

/// Claims the session. Call this before any window or tray icon exists — a
/// duplicate must never reach the point where it registers its own icon.
#[cfg(windows)]
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

/// Claims the session through a Unix domain socket. See the module docs.
#[cfg(not(windows))]
pub fn claim(name: &str) -> Launch {
    let socket = match socket_path(name) {
        Ok(path) => path,
        Err(error) => return Launch::Failed(error),
    };
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    // Keep this inode forever. Unlinking a held lock would allow the next
    // process to lock a new inode while the old primary still runs.
    let lock = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(socket.with_extension("lock"))
    {
        Ok(lock) => lock,
        Err(error) => return Launch::Failed(error),
    };
    let status = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if status != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::WouldBlock {
            return Launch::Failed(error);
        }
        // The winning process may still be creating its listener. Wait for the
        // activation endpoint, never unlink it and never become an unguarded copy.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(mut stream) = UnixStream::connect(&socket) {
                let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
                if stream.write_all(&[1]).is_ok() {
                    return Launch::Duplicate;
                }
            }
            if std::time::Instant::now() >= deadline {
                return Launch::Failed(std::io::Error::other(
                    "已有实例占用锁，但无法唤醒；请确认它已退出后重试",
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    // Only the lifetime-lock owner can remove a crash leftover socket. This
    // prevents concurrent restarts from unlinking each other's live endpoint.
    if socket.exists() {
        if !is_our_socket(&socket) {
            return Launch::Failed(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "单实例路径不是当前用户的 socket",
            ));
        }
        if let Err(error) = std::fs::remove_file(&socket) {
            return Launch::Failed(error);
        }
    }
    match UnixListener::bind(&socket) {
        Ok(listener) => Launch::Primary(PrimaryInstance::guarded(listener, socket, lock)),
        Err(error) => Launch::Failed(error),
    }
}

/// The per-user runtime directory, when the session provides one.
#[cfg(not(windows))]
fn runtime_directory() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|directory| directory.is_dir())
}

/// Reduces `name` to the characters allowed in a socket file name. The Windows
/// prefix `Local\` therefore becomes `Local-` instead of introducing a
/// directory separator.
#[cfg(not(windows))]
fn sanitized(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .collect()
}

/// libc works on both Linux and macOS (which has no `/proc/self`).
#[cfg(not(windows))]
fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(not(windows))]
fn socket_path(name: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    // A private, non-symlink directory prevents another local user from
    // spoofing activation requests or pre-creating a stale socket.
    let base = runtime_directory().unwrap_or_else(|| PathBuf::from("/tmp"));
    let directory = base.join(format!("drcom-{}", effective_uid()));
    match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = std::fs::symlink_metadata(&directory)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != effective_uid()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "单实例目录必须由当前用户独占（0700）",
        ));
    }
    // Compact and stable across processes; Darwin's sockaddr_un is only 104
    // bytes. FNV-1a is just a name hash, not a security primitive.
    let hash = sanitized(name)
        .bytes()
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
    let path = directory.join(format!("{hash:016x}.sock"));
    if path.as_os_str().as_encoded_bytes().len() >= 104 {
        return Err(std::io::Error::other("单实例 socket 路径过长"));
    }
    Ok(path)
}

/// Whether `path` is a socket owned by this user, i.e. a stale guard this
/// process may safely replace. Anything else (a directory, a regular file, a
/// socket belonging to another account) is left alone.
#[cfg(not(windows))]
fn is_our_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_socket() && metadata.uid() == effective_uid())
        .unwrap_or(false)
}

/// Held for the lifetime of the process that won the race (Windows named
/// mutex + event).
#[cfg(windows)]
pub struct PrimaryInstance {
    mutex: *mut c_void,
    event: *mut c_void,
}

#[cfg(windows)]
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

#[cfg(windows)]
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

/// Held for the lifetime of the process that won the race (Unix listener +
/// socket file).
#[cfg(not(windows))]
pub struct PrimaryInstance {
    /// Main lifetime lock; socket only transports activation requests.
    _lock: std::fs::File,
    /// The listening socket until `listen` moves it to the accept thread.
    /// `None` when the guard could not be installed at all.
    listener: Mutex<Option<UnixListener>>,
    /// The socket file this instance created; removed on drop.
    socket: Option<PathBuf>,
    stop: Arc<AtomicBool>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

#[cfg(not(windows))]
impl PrimaryInstance {
    fn guarded(listener: UnixListener, socket: PathBuf, lock: std::fs::File) -> Self {
        Self {
            _lock: lock,
            listener: Mutex::new(Some(listener)),
            socket: Some(socket),
            stop: Arc::new(AtomicBool::new(false)),
            worker: Mutex::new(None),
        }
    }

    /// Runs `on_activate` on a worker thread whenever another process starts.
    /// The callback fires on that thread, so an event-loop hop is still needed.
    /// Calling this more than once does nothing: there is only one socket.
    pub fn listen(&self, on_activate: impl Fn() + Send + 'static) {
        let Ok(mut slot) = self.listener.lock() else {
            return;
        };
        let Some(listener) = slot.take() else {
            return;
        };
        // A blocking `accept` would pin the worker forever and `Drop` could not
        // stop it; poll instead so shutdown is bounded by the poll interval.
        if listener.set_nonblocking(true).is_err() {
            return;
        }
        let stop = Arc::clone(&self.stop);
        let worker = std::thread::Builder::new()
            .name("drcom-single-instance".into())
            .spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _address)) => {
                            // Reading the duplicate's byte avoids resetting the
                            // connection in its face; the content means nothing.
                            let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
                            let mut byte = [0u8; 1];
                            if stream.read_exact(&mut byte).is_ok() && byte == [1] {
                                on_activate();
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
            });
        if let Ok(worker) = worker
            && let Ok(mut slot) = self.worker.lock()
        {
            *slot = Some(worker);
        }
    }
}

#[cfg(not(windows))]
impl Drop for PrimaryInstance {
    fn drop(&mut self) {
        // Stop the poller first: once it has joined, nothing is accepting on the
        // socket any more and unlinking it cannot race with a new connection.
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut worker) = self.worker.lock()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
        if let Some(socket) = &self.socket {
            let _ = std::fs::remove_file(socket);
        }
    }
}

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    // One name per test, and unique per test process: the harness runs tests on
    // parallel threads inside a single process, and a concurrent `cargo test`
    // (another checkout, a CI job) is a second process. On Unix the guard is a
    // per-user socket file keyed by this name, so including the process id keeps
    // two simultaneous runs from claiming each other's lock.
    fn test_name(label: &str) -> String {
        format!(
            "Local--DrComCampus-Rust-Test-{label}-{}",
            std::process::id()
        )
    }

    #[test]
    fn the_second_claim_is_reported_as_a_duplicate() {
        let name = test_name("duplicate");
        let Launch::Primary(primary) = claim(&name) else {
            panic!("the first claim has to win");
        };
        assert!(matches!(claim(&name), Launch::Duplicate));
        // Dropping releases the mutex, so a later claim could win again.
        drop(primary);
    }

    #[test]
    fn a_duplicate_asks_the_running_copy_to_show_its_window() {
        let name = test_name("handshake");
        let Launch::Primary(primary) = claim(&name) else {
            panic!("the first claim has to win");
        };
        let signalled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&signalled);
        primary.listen(move || flag.store(true, Ordering::SeqCst));

        assert!(matches!(claim(&name), Launch::Duplicate));

        let deadline = Instant::now() + Duration::from_secs(5);
        while !signalled.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            signalled.load(Ordering::SeqCst),
            "the duplicate never reached the running instance"
        );
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_restarts_cannot_both_replace_a_stale_socket() {
        let name = test_name("stale-race");
        let socket = socket_path(&name).unwrap();
        let stale = UnixListener::bind(&socket).unwrap();
        drop(stale);
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let (tx, rx) = std::sync::mpsc::channel();
        let mut workers = Vec::new();
        for _ in 0..2 {
            let name = name.clone();
            let barrier = Arc::clone(&barrier);
            let tx = tx.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                match claim(&name) {
                    Launch::Primary(primary) => {
                        primary.listen(|| {});
                        tx.send(true).unwrap();
                        // Keep the lifetime lock until the parent observed both outcomes.
                        barrier.wait();
                        drop(primary);
                    }
                    Launch::Duplicate => {
                        tx.send(false).unwrap();
                        barrier.wait();
                    }
                    Launch::Failed(error) => panic!("guard failed: {error}"),
                }
            }));
        }
        barrier.wait();
        let outcomes = [
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        ];
        barrier.wait();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(outcomes.into_iter().filter(|primary| *primary).count(), 1);
    }

    #[test]
    fn dropping_the_primary_releases_the_name_for_a_later_claim() {
        let name = test_name("release");
        let Launch::Primary(primary) = claim(&name) else {
            panic!("the first claim has to win");
        };
        assert!(matches!(claim(&name), Launch::Duplicate));
        drop(primary);
        let Launch::Primary(again) = claim(&name) else {
            panic!("after the primary is dropped a later claim has to win");
        };
        drop(again);
    }
}
