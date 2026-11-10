//! Wire envelopes and line framing.
//!
//! Every frame is exactly one compact UTF-8 JSON object followed by `\n`. Nothing
//! else is allowed on the stream: no length prefixes, no multi-line objects, no
//! logging. A receiver caps a line at [`MAX_LINE_BYTES`] bytes so a broken or
//! malicious peer cannot make it allocate without bound.

use std::fmt;
use std::io::{self, BufRead, Read, Write};

use drcom_service::{Command, Notification, Snapshot};
use serde::{Deserialize, Serialize};

/// Name of the protocol, carried in the `ready` banner.
pub const PROTOCOL_NAME: &str = "drcom-ipc";

/// Version of the envelope protocol (this crate's message shapes). Bumped on a
/// breaking change to the framing or the `Request`/`Response`/`Ready` shapes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum size of one JSON object, in bytes, excluding the trailing newline.
///
/// Lines larger than this are rejected without buffering them; the reader drains
/// to the next newline so the connection can usually continue.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// How much of an oversized line is drained before the connection is considered
/// unrecoverable.
const MAX_DRAIN_BYTES: usize = 16 * 1024 * 1024;

/// Size of the discarded chunks used while draining an oversized line.
const DRAIN_CHUNK_BYTES: usize = 64 * 1024;

/// Marker for the mandatory `"event":"ready"` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadyMarker {
    /// The only value: `"ready"`.
    Ready,
}

/// Build/runtime information shared by the `ready` banner and the `ping` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    /// Always [`PROTOCOL_NAME`] for this version.
    pub protocol: String,
    /// Envelope protocol version ([`PROTOCOL_VERSION`]).
    pub protocol_version: u32,
    /// Frozen frontend API version ([`drcom_service::API_VERSION`]).
    pub api_version: u32,
    /// `CARGO_PKG_VERSION` of the binary that serves the connection.
    pub app_version: String,
}

impl ServerInfo {
    /// Builds the advertised information for this protocol version.
    pub fn new(api_version: u32, app_version: impl Into<String>) -> Self {
        Self {
            protocol: PROTOCOL_NAME.to_owned(),
            protocol_version: PROTOCOL_VERSION,
            api_version,
            app_version: app_version.into(),
        }
    }
}

/// The first object sent on every connection.
///
/// Serialises as:
/// `{"event":"ready","protocol":"drcom-ipc","protocol_version":1,"api_version":1,"app_version":"1.2.1"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ready {
    /// Always `"ready"`.
    pub event: ReadyMarker,
    /// Protocol name ([`PROTOCOL_NAME`]).
    pub protocol: String,
    /// Envelope protocol version ([`PROTOCOL_VERSION`]).
    pub protocol_version: u32,
    /// Frozen frontend API version ([`drcom_service::API_VERSION`]).
    pub api_version: u32,
    /// Version of the daemon/backend binary.
    pub app_version: String,
}

impl Ready {
    /// Builds the banner from a [`ServerInfo`].
    pub fn from_info(info: &ServerInfo) -> Self {
        Self {
            event: ReadyMarker::Ready,
            protocol: info.protocol.clone(),
            protocol_version: info.protocol_version,
            api_version: info.api_version,
            app_version: info.app_version.clone(),
        }
    }

    /// Extracts the advertised information from the banner.
    pub fn info(&self) -> ServerInfo {
        ServerInfo {
            protocol: self.protocol.clone(),
            protocol_version: self.protocol_version,
            api_version: self.api_version,
            app_version: self.app_version.clone(),
        }
    }
}

/// One request from a client.
///
/// Serialises with `id` first, then the flattened [`RequestBody`]:
/// `{"id":1,"type":"snapshot"}` or
/// `{"id":2,"type":"command","command":{"command":"refresh_adapter"}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Client-chosen request id, echoed in the matching [`Response`]. May be `0`;
    /// ids only need to be unique among the client's outstanding requests.
    pub id: u64,
    /// What to do.
    #[serde(flatten)]
    pub body: RequestBody,
}

/// The action part of a [`Request`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestBody {
    /// Forward a [`Command`] to [`drcom_ipc::Backend::execute`].
    Command {
        /// The service command, one-to-one with the Rust enum.
        command: Command,
    },
    /// Read [`drcom_ipc::Backend::snapshot`] without changing anything.
    Snapshot,
    /// Health check; answers with [`Response::info`].
    Ping,
    /// Ask the daemon to shut down. The response is written before the daemon
    /// starts closing connections.
    Shutdown,
}

/// Failure to parse one request line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestError {
    /// Request id recovered from the line, or `0` when it could not be read.
    pub id: u64,
    /// Human-readable, developer-facing reason.
    pub message: String,
}

impl fmt::Display for RequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl std::error::Error for RequestError {}

/// One response to one [`Request`].
///
/// Exactly one payload field is set:
///
/// * `snapshot` for `snapshot`, command and most `shutdown` requests;
/// * `text` for commands that return data (currently only `diagnostics_text`);
/// * `info` for `ping`;
/// * `error` when `ok` is `false`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    /// Echoes [`Request::id`]; `0` when a malformed line had no readable id.
    pub id: u64,
    /// `true` for a successful request.
    pub ok: bool,
    /// New complete view, when the request produced a state change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Snapshot>,
    /// Text result, when the request produced text instead of state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Build/protocol information, for `ping` and the ready banner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info: Option<ServerInfo>,
    /// Failure reason when [`Response::ok`] is `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    /// Successful response carrying a new snapshot.
    pub fn snapshot(id: u64, snapshot: Snapshot) -> Self {
        Self {
            id,
            ok: true,
            snapshot: Some(snapshot),
            text: None,
            info: None,
            error: None,
        }
    }

    /// Successful response carrying text (for example diagnostics).
    pub fn text(id: u64, text: impl Into<String>) -> Self {
        Self {
            id,
            ok: true,
            snapshot: None,
            text: Some(text.into()),
            info: None,
            error: None,
        }
    }

    /// Successful response carrying protocol/build information.
    pub fn info(id: u64, info: ServerInfo) -> Self {
        Self {
            id,
            ok: true,
            snapshot: None,
            text: None,
            info: Some(info),
            error: None,
        }
    }

    /// Failed response. A malformed line uses id `0`.
    pub fn error(id: u64, message: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            snapshot: None,
            text: None,
            info: None,
            error: Some(message.into()),
        }
    }
}

/// Anything a server may send that is not a [`Response`].
///
/// The service's [`Notification`] already serialises with an `"event"` tag
/// (`state`, `hide_to_tray`, `exit_ready`), so the banner and the notifications
/// share one untagged envelope type on the receiving side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
// The variants are only held for the duration of parsing one line; boxing the
// notification would add an allocation to every state message for no benefit.
#[allow(clippy::large_enum_variant)]
pub enum ServerMessage {
    /// The connection banner.
    Ready(Ready),
    /// A service notification ([`Notification`]).
    Notification(Notification),
}

/// Outcome of one [`read_line_limited`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadLine {
    /// One complete line; the trailing `\n` (and an optional `\r`) is stripped.
    Line(String),
    /// A line that cannot be used as a request. `recovered` says whether the
    /// stream is now positioned at the next line boundary; when it is not, the
    /// caller should close the connection after reporting the error.
    Rejected {
        /// Whether the rest of the bad line was successfully discarded.
        recovered: bool,
        /// Stable, developer-facing description.
        error: &'static str,
    },
    /// The peer closed its input.
    Eof,
}

/// Reads one newline-terminated line without ever buffering more than
/// [`MAX_LINE_BYTES`] + 2 bytes.
///
/// A final line without a trailing newline is accepted at EOF. Oversized lines
/// are drained (up to a safety cap) so the connection can continue with the next
/// line; invalid UTF-8 is reported the same way.
pub fn read_line_limited<R: BufRead>(reader: &mut R) -> io::Result<ReadLine> {
    let mut buffer = Vec::new();
    loop {
        // Allow MAX payload bytes plus `\r\n`. When the buffer already holds
        // that much, one more byte decides whether the newline is there.
        let limit = MAX_LINE_BYTES + 2 - buffer.len();
        let read = reader
            .by_ref()
            .take(limit as u64)
            .read_until(b'\n', &mut buffer)?;
        if read == 0 {
            if buffer.is_empty() {
                return Ok(ReadLine::Eof);
            }
            return Ok(finish_line(buffer));
        }
        if buffer.last() == Some(&b'\n') {
            return Ok(finish_line(buffer));
        }
        if buffer.len() >= MAX_LINE_BYTES + 2 {
            break;
        }
    }

    match drain_line(reader)? {
        DrainOutcome::TooLong => Ok(ReadLine::Rejected {
            recovered: false,
            error: "line exceeds the 1048576-byte limit",
        }),
        DrainOutcome::Newline | DrainOutcome::Eof => Ok(ReadLine::Rejected {
            recovered: true,
            error: "line exceeds the 1048576-byte limit",
        }),
    }
}

fn finish_line(mut buffer: Vec<u8>) -> ReadLine {
    if buffer.last() == Some(&b'\n') {
        buffer.pop();
    }
    if buffer.last() == Some(&b'\r') {
        buffer.pop();
    }
    if buffer.len() > MAX_LINE_BYTES {
        return ReadLine::Rejected {
            recovered: true,
            error: "line exceeds the 1048576-byte limit",
        };
    }
    match String::from_utf8(buffer) {
        Ok(line) => ReadLine::Line(line),
        Err(_) => ReadLine::Rejected {
            recovered: true,
            error: "line is not valid UTF-8",
        },
    }
}

enum DrainOutcome {
    Newline,
    Eof,
    TooLong,
}

fn drain_line<R: BufRead>(reader: &mut R) -> io::Result<DrainOutcome> {
    let mut discarded = 0;
    let mut scratch = Vec::new();
    loop {
        scratch.clear();
        let read = reader
            .by_ref()
            .take(DRAIN_CHUNK_BYTES as u64)
            .read_until(b'\n', &mut scratch)?;
        if read == 0 {
            return Ok(DrainOutcome::Eof);
        }
        if scratch.last() == Some(&b'\n') {
            return Ok(DrainOutcome::Newline);
        }
        discarded += read;
        if discarded >= MAX_DRAIN_BYTES {
            return Ok(DrainOutcome::TooLong);
        }
    }
}

/// Serialises one value as a compact JSON line, including the trailing `\n`.
pub fn encode_json_line<T: Serialize + ?Sized>(value: &T) -> Result<String, serde_json::Error> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    Ok(line)
}

/// Writes one compact JSON line and flushes it. The whole line is written with a
/// single `write_all`, so several threads can share a writer as long as its
/// `write_all` is atomic (a `Mutex` around it is enough).
pub fn write_json_line<W: Write, T: Serialize + ?Sized>(
    writer: &mut W,
    value: &T,
) -> io::Result<()> {
    let line = encode_json_line(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    writer.write_all(line.as_bytes())?;
    writer.flush()
}

/// Writes one already-encoded line (with its trailing `\n`) and flushes it.
pub fn write_encoded_line<W: Write>(writer: &mut W, line: &str) -> io::Result<()> {
    writer.write_all(line.as_bytes())?;
    writer.flush()
}

/// Parses one request line, recovering the id even when the body is invalid.
pub fn parse_request(line: &str) -> Result<Request, RequestError> {
    let value: serde_json::Value = serde_json::from_str(line).map_err(|error| RequestError {
        id: 0,
        message: format!("invalid JSON: {error}"),
    })?;
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if !value.is_object() {
        return Err(RequestError {
            id,
            message: "request must be a JSON object".to_owned(),
        });
    }
    serde_json::from_value(value).map_err(|error| RequestError {
        id,
        message: format!("invalid request: {error}"),
    })
}
