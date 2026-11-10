//! Transport-agnostic server side of the protocol.
//!
//! [`serve_connection`] works over any [`BufRead`] + [`Write`] pair, so the same
//! loop drives stdio, a TCP socket or an in-memory `Cursor`/`Vec` in tests. The
//! application is provided through the small [`Backend`] trait, which
//! [`drcom_service::Service`] implements directly.

use std::io::{self, BufRead, Write};

use drcom_service::{API_VERSION, Command, CommandOutcome, Notification, Service, Snapshot};

use crate::protocol::{
    ReadLine, Ready, Request, RequestBody, Response, ServerInfo, encode_json_line, parse_request,
    read_line_limited, write_json_line,
};

/// The application side of an IPC server.
///
/// `drcom_service::Service` implements this; the daemon wraps it to add
/// notification fan-out and graceful shutdown. Any host that can produce a
/// [`Snapshot`] and execute a [`Command`] can serve the protocol.
pub trait Backend: Send + Sync {
    /// Protocol and build information reported by the `ready` banner and `ping`.
    fn info(&self) -> ServerInfo;
    /// Current complete view.
    fn snapshot(&self) -> Snapshot;
    /// Executes one command, exactly like a native frontend would.
    fn execute(&self, command: Command) -> CommandOutcome;

    /// Maps one request to one response. The default implementation is
    /// [`dispatch`]; a host that needs to observe shutdown can override it (and
    /// [`Backend::after_stop`]) without reimplementing the protocol mapping.
    fn handle(&self, request: Request) -> Handled {
        dispatch(self, request)
    }

    /// Called once, on the serving thread, after a response with
    /// [`Handled::stop`] has been written and flushed.
    fn after_stop(&self) {}
}

impl Backend for Service {
    fn info(&self) -> ServerInfo {
        ServerInfo::new(API_VERSION, Service::snapshot(self).app_version)
    }

    fn snapshot(&self) -> Snapshot {
        Service::snapshot(self)
    }

    fn execute(&self, command: Command) -> CommandOutcome {
        Service::execute(self, command)
    }
}

/// The result of applying one request: what to reply, and whether to stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handled {
    /// The response to write back.
    pub response: Response,
    /// When `true`, the loop calls [`Backend::after_stop`] and returns
    /// [`ConnectionEnd::Stopped`] after writing the response.
    pub stop: bool,
}

impl Handled {
    /// A normal response; the connection stays open.
    pub fn reply(response: Response) -> Self {
        Self {
            response,
            stop: false,
        }
    }

    /// A response that ends the serving loop (`shutdown`).
    pub fn stopping(response: Response) -> Self {
        Self {
            response,
            stop: true,
        }
    }
}

/// Why the request loop returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionEnd {
    /// The peer closed its input (EOF). No response is outstanding.
    Closed,
    /// A `shutdown` request was answered; [`Backend::after_stop`] has run.
    Stopped,
}

/// Applies the standard request mapping for a backend.
///
/// This is the default [`Backend::handle`] and is public so a host that overrides
/// `handle` only for the shutdown case can delegate the rest.
pub fn dispatch<B: Backend + ?Sized>(backend: &B, request: Request) -> Handled {
    match request.body {
        RequestBody::Ping => Handled::reply(Response::info(request.id, backend.info())),
        RequestBody::Snapshot => Handled::reply(Response::snapshot(request.id, backend.snapshot())),
        RequestBody::Shutdown => {
            let outcome = backend.execute(Command::Shutdown);
            Handled::stopping(response_for(request.id, outcome))
        }
        RequestBody::Command { command } => {
            let stop = matches!(command, Command::Shutdown);
            let outcome = backend.execute(command);
            Handled {
                response: response_for(request.id, outcome),
                stop,
            }
        }
    }
}

fn response_for(id: u64, outcome: CommandOutcome) -> Response {
    match outcome {
        CommandOutcome::Snapshot(snapshot) => Response::snapshot(id, snapshot),
        CommandOutcome::Text(text) => Response::text(id, text),
    }
}

/// Writes the connection banner: the `ready` object and the first `state`
/// notification, in one atomic write so no concurrent notification can slip
/// between them.
pub fn handshake<W: Write, B: Backend + ?Sized>(writer: &mut W, backend: &B) -> io::Result<()> {
    let ready = Ready::from_info(&backend.info());
    let state = Notification::State {
        snapshot: backend.snapshot(),
    };
    let mut buffer = encode_json_line(&ready).map_err(serde_to_io)?;
    buffer.push_str(&encode_json_line(&state).map_err(serde_to_io)?);
    writer.write_all(buffer.as_bytes())?;
    writer.flush()
}

fn serde_to_io(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

/// Writes the banner and then serves requests until EOF or a `shutdown` request.
pub fn serve_connection<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    backend: &dyn Backend,
) -> io::Result<ConnectionEnd> {
    handshake(writer, backend)?;
    serve_requests(reader, writer, backend)
}

/// Serves request lines after the banner has been sent.
///
/// Kept separate from [`serve_connection`] for transports that need to start
/// something (for example a per-client notification writer) between the banner
/// and the first request.
pub fn serve_requests<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    backend: &dyn Backend,
) -> io::Result<ConnectionEnd> {
    loop {
        match read_line_limited(reader)? {
            ReadLine::Eof => return Ok(ConnectionEnd::Closed),
            ReadLine::Rejected { recovered, error } => {
                let message = format!("invalid request line: {error}");
                write_json_line(writer, &Response::error(0, message))?;
                if !recovered {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, error));
                }
            }
            ReadLine::Line(line) => {
                // A UTF-8 BOM at the start of a Windows client's first line is
                // tolerated; everything else must be a JSON object.
                let line = line.strip_prefix('\u{feff}').unwrap_or(&line);
                if line.trim().is_empty() {
                    continue;
                }
                let request = match parse_request(line) {
                    Ok(request) => request,
                    Err(error) => {
                        write_json_line(writer, &Response::error(error.id, error.message))?;
                        continue;
                    }
                };
                let handled = backend.handle(request);
                write_json_line(writer, &handled.response)?;
                if handled.stop {
                    backend.after_stop();
                    return Ok(ConnectionEnd::Stopped);
                }
            }
        }
    }
}
