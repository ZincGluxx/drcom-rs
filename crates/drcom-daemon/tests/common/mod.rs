//! Shared harness for the daemon integration tests.
//!
//! Every test owns a child process and all reads are bounded by a timeout, so a
//! regression can only fail the test — it can never hang the suite. [`Daemon`]
//! kills the child in `Drop`, which covers assertion failures and panics too.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Default upper bound for one protocol line / process exit in these tests.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// A spawned `drcom-daemon` with protocol lines and stderr on channels.
pub struct Daemon {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout_lines: Receiver<String>,
    stderr_lines: Receiver<String>,
    stdout_reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
    stderr_buffer: Arc<Mutex<String>>,
}

impl Daemon {
    /// Spawns `drcom-daemon` with the given arguments.
    pub fn spawn(args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_drcom-daemon"))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn drcom-daemon");
        let stdin = child.stdin.take().expect("daemon stdin was piped");
        let stdout = child.stdout.take().expect("daemon stdout was piped");
        let stderr = child.stderr.take().expect("daemon stderr was piped");

        let (stdout_sender, stdout_lines) = mpsc::channel();
        let stdout_reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if stdout_sender.send(line).is_err() {
                    break;
                }
            }
        });

        let (stderr_sender, stderr_lines) = mpsc::channel();
        let stderr_buffer = Arc::new(Mutex::new(String::new()));
        let thread_buffer = Arc::clone(&stderr_buffer);
        let stderr_reader = thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                {
                    let mut buffer = thread_buffer.lock().expect("stderr buffer");
                    buffer.push_str(&line);
                    buffer.push('\n');
                }
                if stderr_sender.send(line).is_err() {
                    break;
                }
            }
        });

        Self {
            child,
            stdin: Some(stdin),
            stdout_lines,
            stderr_lines,
            stdout_reader: Some(stdout_reader),
            stderr_reader: Some(stderr_reader),
            stderr_buffer,
        }
    }

    fn next_json_within(&mut self, timeout: Duration, context: &str) -> serde_json::Value {
        match self.stdout_lines.recv_timeout(timeout) {
            Ok(line) => serde_json::from_str(&line).unwrap_or_else(|error| {
                panic!("{context}: protocol line is not JSON: {line:?} ({error})")
            }),
            Err(error) => {
                self.kill();
                panic!("{context}: no protocol line within {timeout:?} ({error})")
            }
        }
    }

    /// Reads the next server line, asserting it is JSON.
    pub fn next_json(&mut self, context: &str) -> serde_json::Value {
        self.next_json_within(TIMEOUT, context)
    }

    /// Reads lines until one is a response with `id`; notifications are skipped.
    pub fn read_response(&mut self, id: u64) -> serde_json::Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "no response with id {id} within {TIMEOUT:?}"
            );
            let value = self.next_json_within(remaining, &format!("waiting for response {id}"));
            if value.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                return value;
            }
            assert!(
                value.get("event").is_some(),
                "unexpected line while waiting for response {id}: {value}"
            );
        }
    }

    /// Reads lines until one has `"event": <name>`; other notifications are skipped.
    pub fn read_event(&mut self, name: &str) -> serde_json::Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "no {name:?} event within {TIMEOUT:?}");
            let value = self.next_json_within(remaining, &format!("waiting for event {name}"));
            if value.get("event").and_then(serde_json::Value::as_str) == Some(name) {
                return value;
            }
            assert!(
                value.get("event").is_some(),
                "unexpected response while waiting for event {name:?}: {value}"
            );
        }
    }

    /// Writes one JSON request line.
    pub fn send(&mut self, value: &serde_json::Value) {
        let line = serde_json::to_string(value).expect("serialise request");
        self.send_raw(&line);
    }

    /// Writes one raw line (for malformed-input tests).
    pub fn send_raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("daemon stdin is open");
        stdin.write_all(line.as_bytes()).expect("write request");
        stdin.write_all(b"\n").expect("write newline");
        stdin.flush().expect("flush request");
    }

    /// Closes stdin, which the daemon treats as a shutdown request.
    pub fn close_stdin(&mut self) {
        self.stdin.take();
    }

    /// Waits for the process to exit, killing it on timeout.
    pub fn wait_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait().expect("try_wait") {
                Some(status) => return status,
                None => {
                    if Instant::now() >= deadline {
                        self.kill();
                        panic!("drcom-daemon did not exit within {timeout:?}");
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Reads one stderr line, if one arrives within `timeout`.
    pub fn next_stderr(&mut self, timeout: Duration) -> Option<String> {
        self.stderr_lines.recv_timeout(timeout).ok()
    }

    /// Waits for the `drcom-daemon listening <addr>` line and parses the address.
    pub fn listen_address(&mut self) -> SocketAddr {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "no listening banner within {TIMEOUT:?}"
            );
            let Some(line) = self.next_stderr(remaining) else {
                self.kill();
                panic!("stderr closed before the listening banner");
            };
            if let Some(address) = line.strip_prefix("drcom-daemon listening ") {
                return address
                    .trim()
                    .parse()
                    .expect("listening banner contains a socket address");
            }
        }
    }

    /// Everything stderr produced so far. Only call after the process exited.
    pub fn stderr_text(&mut self) -> String {
        self.join_readers();
        self.stderr_buffer.lock().expect("stderr buffer").clone()
    }

    fn join_readers(&mut self) {
        if let Some(handle) = self.stdout_reader.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_reader.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Never leave a daemon behind when an assertion panics.
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.join_readers();
    }
}
