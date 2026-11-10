//! JSON-lines protocol for driving [`drcom_service`] from another process or
//! language.
//!
//! The application layer (`drcom-service`) is frontend-agnostic, but it is a Rust
//! library: a Rust frontend can link it in-process (as the Slint GUI does), while
//! a Python script, a PowerShell script or another process cannot link it at all.
//! This crate defines the wire format that makes the same service usable from
//! anywhere — and from any language — without reimplementing a line of it: one
//! compact UTF-8 JSON object per `\n`-terminated line, over stdio or TCP loopback.
//!
//! ```
//! # use drcom_service::{Service, ServiceOptions};
//! // A Rust host can use the crate directly:
//! let service = Service::new(ServiceOptions::for_test());
//! let snapshot = drcom_ipc::Backend::snapshot(&service);
//! assert_eq!(snapshot.api_version, drcom_service::API_VERSION);
//! ```
//!
//! The pieces:
//!
//! * [`protocol`] — the envelope types ([`Request`], [`Response`], [`Ready`],
//!   [`ServerMessage`]) and line framing with a 1 MiB cap ([`read_line_limited`],
//!   [`write_json_line`]).
//! * [`server`] — the transport-agnostic connection loop. It only needs a
//!   [`Backend`] (anything that can snapshot and execute commands), so it is
//!   testable with in-memory `Cursor`/`Vec` pairs and reusable with any transport.
//! * [`hub`] — a broadcast hub for several TCP clients. The notification is
//!   serialised once and fanned out over bounded per-client queues; slow or dead
//!   clients are dropped instead of stalling the service.
//! * [`client`] — a blocking [`Client`] for hosts that speak the protocol from
//!   Rust (spawned daemon over stdio, or a TCP connection).
//!
//! The daemon binary (`drcom-daemon`) is the ready-made server; the wire schema is
//! documented in `docs/ipc-protocol.md` at the repository root.
//!
//! # Message flow
//!
//! ```text
//! server -> client: {"event":"ready","protocol":"drcom-ipc",...}
//! server -> client: {"event":"state","snapshot":{...}}
//! client -> server: {"id":1,"type":"snapshot"}
//! server -> client: {"id":1,"ok":true,"snapshot":{...}}
//! server -> client: {"event":"exit_ready"}          (service shutting down)
//! ```
//!
//! Unknown fields are ignored by serde, so additive protocol changes do not break
//! older clients. A breaking change bumps [`PROTOCOL_VERSION`]; the service
//! contract has its own [`drcom_service::API_VERSION`].

pub mod client;
pub mod hub;
pub mod protocol;
pub mod server;

pub use client::{Client, ClientError};
pub use hub::{CLIENT_QUEUE_LEN, ClientId, Hub};
pub use protocol::{
    MAX_LINE_BYTES, PROTOCOL_NAME, PROTOCOL_VERSION, ReadLine, Ready, ReadyMarker, Request,
    RequestBody, RequestError, Response, ServerInfo, ServerMessage, encode_json_line,
    parse_request, read_line_limited, write_encoded_line, write_json_line,
};
pub use server::{
    Backend, ConnectionEnd, Handled, dispatch, handshake, serve_connection, serve_requests,
};

#[cfg(test)]
mod tests;
