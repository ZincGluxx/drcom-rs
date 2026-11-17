// DrCom 校园网助手（Rust 版） / drcom-core
// Copyright (C) 2026 ZincGlux
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// 认证协议实现参考并复刻自上游开源项目 drcoms/jlu-drcom-client：
//   https://github.com/drcoms/jlu-drcom-client   (AGPL-3.0)
// 详见仓库根目录 THIRD-PARTY-NOTICES.md。

//! Dr.COM campus authentication engine — the headless backend core.
//!
//! This crate is deliberately frontend-free: it links no UI toolkit and has no
//! notion of a window, tray icon, clipboard or event loop. Everything here can
//! run in a service, a CLI, a test harness or another application's process.
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
//! The crates above this one build on it:
//!
//! * `drcom-service` — frontend-agnostic application state machine (commands,
//!   snapshots, notifications) that any frontend can drive.
//! * `drcom-ipc` + `drcom-daemon` — JSON-lines protocol and a headless host so
//!   that non-Rust applications can use the same backend.
//! * `drcom-gui` / `drcom-cli` — the shipped Slint frontend and the CLI frontend.
//!
//! `event_log.rs` keeps a bounded, in-memory summary for the explicit
//! copy/export actions; it never writes a log file, because the diagnostic text
//! the user chooses to share is the only place this program is meant to leave
//! evidence behind.

pub mod adapter;
pub mod auth;
pub mod config;
/// Credential-free connectivity probes (Ping / HTTPS) used by the UI and the
/// diagnostics report. No clipboard or desktop integration lives here.
pub mod connectivity;
pub mod event_log;
pub mod keepalive;
pub mod login_response;
mod md5;
pub mod page_status;
pub mod preferences;
pub mod protocol;
pub mod reference_login;
pub mod retry;
pub mod route;
pub mod session;
/// Validated static IPv4 edits (netsh), executed only after a frontend asks.
pub mod static_ipv4;
pub mod system;
pub mod transport;

#[cfg(test)]
pub(crate) mod reference_vectors;
