//! Frontend-agnostic application service for the Dr.COM client.
//!
//! `drcom-core` knows how to authenticate; this crate knows what the *application*
//! is doing — whether a session is being established, reconnecting, waiting for a
//! campus adapter, whether auto-login is pending, what the failure card says,
//! which adapter is selected, and so on. It exposes that as a pair of types:
//!
//! * [`Command`] — everything a frontend can ask the backend to do;
//! * [`Snapshot`] / [`Notification`] — everything the backend wants a frontend to
//!   render.
//!
//! There is no window, widget, event loop, tray, clipboard or file dialog in this
//! crate. A frontend in any language can therefore either link it (Rust) or talk
//! to the same service through `drcom-ipc` / `drcom-daemon` (JSON-lines).
//!
//! # Minimal frontend loop
//!
//! ```no_run
//! use std::sync::Arc;
//! use drcom_service::{Command, Notification, Service, ServiceOptions};
//!
//! let service = Service::new(ServiceOptions::default());
//! let sink = {
//!     let service = service.clone();
//!     Arc::new(move |notification: Notification| {
//!         // Marshal to the UI thread; never block here.
//!         if let Notification::State { snapshot } = notification {
//!             render(&snapshot);
//!         }
//!     })
//! };
//! service.subscribe(sink);
//! service.execute(Command::RefreshAdapter);
//! # fn render(_: &drcom_service::Snapshot) {}
//! ```
//!
//! # Threading
//!
//! [`Service`] is cheap to clone and all methods are callable from any thread.
//! Long operations (authentication, connectivity probes, `netsh`) run on their
//! own threads; subscriptions are called from whichever thread caused a change,
//! so a GUI must marshall into its event loop itself. The owning frontend calls
//! [`Service::tick`] about once per second so durations, the post-login auto-hide
//! and pending auto-login progress.

// The frozen contract passes a whole `Snapshot` by value in
// `Notification::State` and `CommandOutcome::Snapshot`: a frontend applies one
// self-contained state at a time, and boxing it would allocate on every tick and
// change the IPC schema. The size inequality between those variants and the
// small ones is therefore intentional, not an oversight.
#![allow(clippy::large_enum_variant)]

mod api;
mod service;
mod text;

pub use api::{
    API_VERSION, AdapterView, Command, CommandOutcome, ConnectionPhase, ConnectionView,
    FailureView, FormSeed, NetworkEditorView, Notification, ProbeKind, ProbeView, ServiceOptions,
    SettingsView, Snapshot,
};
pub use service::{Service, Sink};
