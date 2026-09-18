//! Dr.COM campus authentication, ported from the original Windows client.
//!
//! The wire format is reconstructed from two independent sources that are kept
//! separate on purpose:
//!
//! * `protocol.rs` — raw fragments recovered from the unpacked original
//!   `DrAuthSvr.dll`, including the checksum and the offline packet.
//! * `reference_login.rs` — the profile the campus deployment actually accepts,
//!   checked byte-for-byte against `reference/reference_vectors.txt`.
//!
//! `session.rs` wires them to the socket and owns the cross-stage state.
//!
//! `event_log.rs` mirrors the same state into a text file under
//! `%LOCALAPPDATA%`, because the campus network is both the only environment
//! where this program must work and the one where it cannot be inspected.

pub mod adapter;
pub mod auth;
pub mod config;
pub mod event_log;
pub mod keepalive;
pub mod login_response;
mod md5;
pub mod preferences;
pub mod protocol;
pub mod reference_login;
pub mod retry;
pub mod route;
pub mod session;
pub mod transport;

#[cfg(test)]
pub(crate) mod reference_vectors;
