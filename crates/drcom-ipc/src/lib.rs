//! JSON-lines protocol for driving `drcom-service` from another process or
//! language.
//!
//! TODO(ipc-daemon task): request/response envelopes, notification forwarding,
//! line framing, a blocking client and a server loop live here.

pub const PROTOCOL_NAME: &str = "drcom-ipc";
