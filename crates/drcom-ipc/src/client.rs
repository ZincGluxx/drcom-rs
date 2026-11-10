//! Blocking client for hosts written in Rust.
//!
//! The client owns a reader thread, so a notification pushed by the service can
//! never be mistaken for a request response and a missing response times out
//! instead of hanging the caller. It can either spawn the daemon as a child
//! process ([`Client::spawn_stdio`]) or connect to an already-running one over
//! TCP ([`Client::connect_tcp`]).

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Child, Command as ProcessCommand, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use drcom_service::{Command, Notification};

use crate::protocol::{
    Ready, Request, RequestBody, Response, ServerInfo, ServerMessage, encode_json_line,
};

/// How long the client waits for the `ready` banner after connecting/spawning.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one request may take before [`Client`] reports a timeout. Adapter
/// enumeration and diagnostics can be slow on Windows, so this is generous.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Errors a [`Client`] can report.
#[derive(Debug)]
pub enum ClientError {
    /// Transport (pipe/socket) failure.
    Io(io::Error),
    /// The server sent something that is not part of the protocol, or the
    /// connection ended in the middle of a request.
    Protocol(String),
    /// The peer did not answer within the documented timeout.
    Timeout(String),
    /// The connection closed before a response arrived.
    Closed,
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Protocol(message) => write!(formatter, "protocol error: {message}"),
            Self::Timeout(message) => write!(formatter, "timeout: {message}"),
            Self::Closed => write!(formatter, "connection closed by the daemon"),
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ClientError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// A blocking JSON-lines client.
pub struct Client {
    child: Option<Child>,
    writer: Box<dyn Write + Send>,
    lines: Receiver<io::Result<String>>,
    _reader: Option<JoinHandle<()>>,
    ready: Ready,
    queued: VecDeque<Notification>,
    next_id: u64,
}

impl Client {
    /// Spawns a daemon process and speaks the protocol over its stdin/stdout.
    ///
    /// The caller configures the executable/arguments; the client forces
    /// `stdin`/`stdout` to pipes and leaves stderr inherited so daemon logs stay
    /// visible. The child is killed on [`Drop`] unless it already exited.
    ///
    /// ```no_run
    /// use std::process::Command;
    /// use drcom_ipc::Client;
    ///
    /// let mut command = Command::new("drcom-daemon");
    /// command.args(["--stdio", "--no-startup-probe", "--no-auto-login"]);
    /// let mut client = Client::spawn_stdio(command)?;
    /// let response = client.snapshot()?;
    /// # Ok::<(), drcom_ipc::ClientError>(())
    /// ```
    pub fn spawn_stdio(mut command: ProcessCommand) -> Result<Self, ClientError> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ClientError::Protocol("child stdin was not piped".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ClientError::Protocol("child stdout was not piped".to_owned()))?;
        let (lines, reader) = spawn_reader(BufReader::new(stdout));
        Self::finish_handshake(Some(child), Box::new(stdin), lines, Some(reader))
    }

    /// Connects to a daemon started with `--listen`.
    pub fn connect_tcp(address: impl ToSocketAddrs) -> Result<Self, ClientError> {
        let stream = TcpStream::connect(address)?;
        stream.set_nodelay(true).ok();
        let reader_stream = stream.try_clone()?;
        let (lines, reader) = spawn_reader(BufReader::new(reader_stream));
        Self::finish_handshake(None, Box::new(stream), lines, Some(reader))
    }

    fn finish_handshake(
        mut child: Option<Child>,
        writer: Box<dyn Write + Send>,
        lines: Receiver<io::Result<String>>,
        reader: Option<JoinHandle<()>>,
    ) -> Result<Self, ClientError> {
        match wait_for_ready(&lines) {
            Ok((ready, queued)) => Ok(Self {
                child,
                writer,
                lines,
                _reader: reader,
                ready,
                queued,
                next_id: 0,
            }),
            Err(error) => {
                if let Some(child) = child.as_mut() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                Err(error)
            }
        }
    }

    /// The banner received when the connection was established.
    pub fn ready(&self) -> &Ready {
        &self.ready
    }

    /// Protocol/build information from the banner.
    pub fn server_info(&self) -> ServerInfo {
        self.ready.info()
    }

    /// Allocates the next request id (starting at 1).
    pub fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Sends one request body with a fresh id and waits for the matching response.
    pub fn send(&mut self, body: RequestBody) -> Result<Response, ClientError> {
        let id = self.next_id();
        self.send_with_id(id, body)
    }

    /// Sends one request with an explicit id and waits for the matching response.
    ///
    /// Only one request may be outstanding at a time; notifications received
    /// while waiting are queued and returned by [`Client::next_notification`].
    pub fn send_with_id(&mut self, id: u64, body: RequestBody) -> Result<Response, ClientError> {
        let line = encode_json_line(&Request { id, body })
            .map_err(|error| ClientError::Protocol(error.to_string()))?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()?;

        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ClientError::Timeout(format!(
                    "no response to request {id} within {REQUEST_TIMEOUT:?}"
                )));
            }
            match self.lines.recv_timeout(remaining) {
                Ok(Ok(line)) => {
                    if let Ok(response) = serde_json::from_str::<Response>(&line) {
                        if response.id != id {
                            return Err(ClientError::Protocol(format!(
                                "response id {} does not match request {id}",
                                response.id
                            )));
                        }
                        return Ok(response);
                    }
                    match serde_json::from_str::<ServerMessage>(&line) {
                        Ok(ServerMessage::Notification(notification)) => {
                            self.queued.push_back(notification);
                        }
                        Ok(ServerMessage::Ready(_)) => {
                            // Duplicate banner; the first one is authoritative.
                        }
                        Err(error) => {
                            return Err(ClientError::Protocol(format!(
                                "cannot parse server line: {error}: {line}"
                            )));
                        }
                    }
                }
                Ok(Err(error)) => return Err(ClientError::Io(error)),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(ClientError::Timeout(format!(
                        "no response to request {id} within {REQUEST_TIMEOUT:?}"
                    )));
                }
                Err(RecvTimeoutError::Disconnected) => return Err(ClientError::Closed),
            }
        }
    }

    /// Health check (`ping`).
    pub fn ping(&mut self) -> Result<Response, ClientError> {
        self.send(RequestBody::Ping)
    }

    /// Reads the current snapshot without changing anything.
    pub fn snapshot(&mut self) -> Result<Response, ClientError> {
        self.send(RequestBody::Snapshot)
    }

    /// Forwards a service command.
    pub fn command(&mut self, command: Command) -> Result<Response, ClientError> {
        self.send(RequestBody::Command { command })
    }

    /// Asks the daemon to shut down gracefully.
    pub fn shutdown(&mut self) -> Result<Response, ClientError> {
        self.send(RequestBody::Shutdown)
    }

    /// Returns the next notification, waiting at most `timeout`.
    ///
    /// `Ok(None)` means the timeout elapsed. Notifications that arrived while a
    /// request was in flight are returned first.
    pub fn next_notification(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<Notification>, ClientError> {
        if let Some(notification) = self.queued.pop_front() {
            return Ok(Some(notification));
        }
        match self.lines.recv_timeout(timeout) {
            Ok(Ok(line)) => {
                if let Ok(ServerMessage::Notification(notification)) =
                    serde_json::from_str::<ServerMessage>(&line)
                {
                    return Ok(Some(notification));
                }
                if let Ok(response) = serde_json::from_str::<Response>(&line) {
                    return Err(ClientError::Protocol(format!(
                        "unexpected response with id {} while waiting for a notification",
                        response.id
                    )));
                }
                Err(ClientError::Protocol(format!(
                    "cannot parse server line: {line}"
                )))
            }
            Ok(Err(error)) => Err(ClientError::Io(error)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(ClientError::Closed),
        }
    }

    /// Drains notifications that were buffered while requests were in flight.
    pub fn take_queued_notifications(&mut self) -> Vec<Notification> {
        self.queued.drain(..).collect()
    }

    /// Waits up to `timeout` for a spawned daemon to exit.
    ///
    /// Returns `Ok(None)` when no child exists (TCP client) or the timeout
    /// elapsed, and `Ok(Some(status))` once the process exited.
    pub fn wait_for_exit(&mut self, timeout: Duration) -> Result<Option<ExitStatus>, ClientError> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_reader<R: BufRead + Send + 'static>(
    mut reader: R,
) -> (Receiver<io::Result<String>>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    while line.ends_with('\n') || line.ends_with('\r') {
                        line.pop();
                    }
                    if line.is_empty() {
                        continue;
                    }
                    if sender.send(Ok(line)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                    break;
                }
            }
        }
    });
    (receiver, handle)
}

fn wait_for_ready(
    lines: &Receiver<io::Result<String>>,
) -> Result<(Ready, VecDeque<Notification>), ClientError> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let mut queued = VecDeque::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ClientError::Timeout(
                "no ready banner from the daemon".to_owned(),
            ));
        }
        match lines.recv_timeout(remaining) {
            Ok(Ok(line)) => match serde_json::from_str::<ServerMessage>(&line) {
                Ok(ServerMessage::Ready(ready)) => return Ok((ready, queued)),
                Ok(ServerMessage::Notification(notification)) => queued.push_back(notification),
                Err(error) => {
                    return Err(ClientError::Protocol(format!(
                        "cannot parse ready banner: {error}: {line}"
                    )));
                }
            },
            Ok(Err(error)) => return Err(ClientError::Io(error)),
            Err(RecvTimeoutError::Timeout) => {
                return Err(ClientError::Timeout(
                    "no ready banner from the daemon".to_owned(),
                ));
            }
            Err(RecvTimeoutError::Disconnected) => return Err(ClientError::Closed),
        }
    }
}
