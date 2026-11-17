//! The frontend contract: commands in, snapshots and notifications out.
//!
//! Everything a frontend needs to render and drive the client lives here, and
//! nothing in this module is specific to Slint, a window, a tray icon or a
//! desktop. A frontend is therefore always the same two pieces of glue:
//!
//! 1. call [`crate::Service::snapshot`] / [`crate::Service::form_seed`] and render
//!    the fields;
//! 2. map user input to [`Command`] values and pass them to
//!    [`crate::Service::execute`], then re-render from the [`Notification::State`]
//!    pushes.
//!
//! The same types cross the JSON-lines IPC boundary used by non-Rust frontends
//! (`drcom-ipc` / `drcom-daemon`), so every field here is `serde`-serialisable and
//! documented with its display meaning. The wire schema is versioned through
//! [`API_VERSION`]; additive fields are fine, a breaking change bumps the
//! version.
//!
//! ## What is deliberately *not* here
//!
//! * **The password is never part of a [`Snapshot`].** Snapshots are what IPC
//!   clients can read, and credentials must not leak to anything that can open a
//!   socket. In-process frontends seed the login form from
//!   [`crate::Service::form_seed`]; other frontends supply credentials when they
//!   send [`Command::Login`].
//! * Window visibility, tray icon lifetime, clipboard and file dialogs are
//!   frontend concerns. The backend only reports *what should be shown* (see
//!   [`Notification::HideToTray`]) and frontends decide how to show it.

use serde::{Deserialize, Serialize};

use drcom_core::config::ClientConfig;

/// Version of the frontend contract. Bumped on any breaking change to the
/// snapshots, commands or notifications serialised over IPC.
pub const API_VERSION: u32 = 1;

/// Where the connection currently is, in terms a frontend can switch on.
///
/// [`ConnectionView::state_text`] carries the same information as display text;
/// this enum exists so a new frontend can localise without pattern-matching on
/// Chinese strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionPhase {
    /// No session, and nothing is being attempted.
    Disconnected,
    /// A login attempt is in progress (challenge / authenticate).
    Connecting,
    /// Authentication succeeded and the keep-alive loop is running.
    Connected,
    /// A retryable failure occurred and an automatic retry is scheduled.
    Reconnecting,
    /// A stop was requested; the logout packet is on its way.
    Disconnecting,
    /// Auto-login is waiting for a usable campus adapter.
    WaitingForNetwork,
}

/// Connection state as a frontend displays it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionView {
    /// Machine-readable phase, for frontends that localise or style states.
    pub phase: ConnectionPhase,
    /// Display text of the state — `未连接` / `连接中` / `已连接` / `重连中` /
    /// `断开中` / `等待网络`.
    pub state_text: String,
    /// One-line explanation shown under the state.
    pub detail: String,
    /// True only while the UDP session is authenticated and beating.
    pub online: bool,
    /// True when a session exists *or* is being established (the UI uses this to
    /// disable the connection form and enable "下线").
    pub session_active: bool,
    /// Whole seconds since the session came online, while it is online.
    pub online_seconds: Option<u64>,
    /// Human-readable duration (`0 秒` / `3 分 12 秒` / `1 时 02 分`), refreshed on
    /// every backend tick so frontends do not have to format it themselves.
    pub online_duration: String,
}

/// The selected physical adapter and its addressing information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterView {
    /// False when no eligible physical adapter was found.
    pub available: bool,
    /// Friendly adapter name, or `未检测到物理网卡`.
    pub name: String,
    pub ipv4: String,
    /// Uppercase colon-separated MAC (`AA:BB:CC:DD:EE:FF`), or `--`.
    pub mac: String,
    /// Comma-separated gateway list, or `--`.
    pub gateway: String,
    /// Comma-separated DNS list, or `--`.
    pub dns: String,
    /// Summary of routable IPv6 addresses (link-local addresses are hidden).
    pub ipv6: String,
}

/// The failure card kept for the current process lifetime.
///
/// Values survive closing the window to the tray; they are shown until the next
/// attempt overwrites them. The account and password are never recorded here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureView {
    /// Reason, with actionable guidance appended when one is known.
    pub message: String,
    /// Local time of the failure (`YYYY-MM-DD HH:MM:SS`).
    pub time: String,
    /// Stage that failed, e.g. `认证`, `网络通信`.
    pub stage: String,
    /// Stable code, e.g. `DRCOM-21`, `NETWORK-TimedOut`.
    pub code: String,
    /// How long the failed attempt lasted.
    pub duration: String,
    /// What the client will do next (`不会自动重试` / `将在 N 秒后重试…`).
    pub retry: String,
}

/// Connectivity check state (the "检测网络" button).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeView {
    /// True while a check is running; frontends should disable the button.
    pub busy: bool,
    /// Campus Ping result (`未检测` before the first check).
    pub campus: String,
    /// Public IPv4 HTTPS result.
    pub ipv4: String,
    /// Public IPv6 HTTPS result.
    pub ipv6: String,
}

/// State of the static-IPv4 editor overlay.
///
/// The editor is a backend-driven dialog: opening it fills the form, and
/// `visible` tells the frontend to show the overlay. Closing it sends
/// [`Command::CloseNetworkEditor`] so the backend snapshot stays authoritative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkEditorView {
    pub visible: bool,
    pub adapter_name: String,
    pub ip: String,
    pub mask: String,
    pub gateway: String,
    pub dns: String,
    /// Instruction or validation/error result shown inside the editor.
    pub message: String,
    /// True while the elevated `netsh` command runs.
    pub busy: bool,
}

/// Saved account/startup settings, as the frontend should display them.
///
/// These are the *persisted* values. In-progress text typed into a login form is
/// the frontend's own state; it is only sent back with [`Command::SaveSettings`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsView {
    /// Remembered account name, empty when nothing was saved.
    pub username: String,
    pub auto_login: bool,
    pub auto_reconnect: bool,
    /// "关闭到托盘" option, also used by the automatic hide after a login.
    pub minimize_to_tray: bool,
    /// Current platform's login-autostart state when it could be read.
    /// The legacy field name is retained for IPC v1 compatibility; Unix uses
    /// LaunchAgent/XDG autostart, not the Windows registry.
    pub start_with_windows: Option<bool>,
    /// Save/load feedback (`已记住你的账号`, `保存失败…`), `None` when there is
    /// nothing to say.
    pub config_status: Option<String>,
}

/// A complete description of what a frontend should show right now.
///
/// Snapshots are cheap to clone and self-contained: applying one repaints the
/// whole UI, so a frontend never needs to replay a diff or remember state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub api_version: u32,
    /// `CARGO_PKG_VERSION` of the backend build, for diagnostics and support.
    pub app_version: String,
    pub connection: ConnectionView,
    pub settings: SettingsView,
    pub adapter: AdapterView,
    pub failure: FailureView,
    pub probe: ProbeView,
    pub network_editor: NetworkEditorView,
    /// Read-only Dr.COM web portal state (`此程序 UDP 保活已建立`, `网页状态未知…`).
    pub campus_page: String,
    /// Short status shown in a tray tooltip or status bar (`已连接，正在保持在线`).
    pub tooltip: String,
}

/// Initial login-form values, for in-process frontends only.
///
/// This is the one place a saved password is handed to a frontend: it seeds the
/// form so a user can log in without retyping it. It is never serialised over
/// IPC and never logged.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FormSeed {
    pub username: String,
    /// `Some(password)` when the form should be prefilled; `Some("")` when the
    /// user previously cleared the saved password; `None` when nothing was
    /// remembered.
    pub password: Option<String>,
    pub auto_login: bool,
    pub auto_reconnect: bool,
    pub minimize_to_tray: bool,
    pub start_with_windows: Option<bool>,
    pub config_status: Option<String>,
}

/// The frontend's only way to change backend state.
///
/// Commands are fire-and-forget: the result is the next snapshot, including any
/// validation feedback in `status`/`network_editor.message`. Only
/// [`Command::DiagnosticsText`] returns data directly (through
/// [`CommandOutcome::Text`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    /// Start authentication with the given credentials.
    Login {
        username: String,
        password: String,
        auto_reconnect: bool,
    },
    /// Stop the running session (or cancel a pending automatic login) and log
    /// out.
    Logout,
    /// Re-enumerate physical adapters and refresh [`AdapterView`].
    RefreshAdapter,
    /// Run connectivity checks. `kind` decides which families run.
    Probe { kind: ProbeKind },
    /// Fill [`NetworkEditorView`] from the selected adapter and open the editor.
    OpenNetworkEditor,
    /// Hide the static-IP editor without applying anything.
    CloseNetworkEditor,
    /// Apply static IPv4 settings through the elevated path.
    ApplyNetwork {
        ip: String,
        mask: String,
        gateway: String,
        dns: String,
    },
    /// Persist account/startup preferences and the platform login-autostart entry.
    SaveSettings {
        username: String,
        password: String,
        auto_login: bool,
        auto_reconnect: bool,
        autostart: bool,
        minimize_to_tray: bool,
    },
    /// Clear the remembered password, keep the account, and disable automatic
    /// reconnects that would reuse the old credentials.
    ClearSavedPassword {
        username: String,
        auto_reconnect: bool,
        minimize_to_tray: bool,
    },
    /// Apply the (unsaved) "关闭到托盘" checkbox immediately.
    SetMinimizeToTray { enabled: bool },
    /// Build the diagnostics report (adapters + recent log + failure card) and
    /// return it as text. The frontend decides where to put it.
    DiagnosticsText,
    /// Log out if needed and shut the backend down. The frontend receives
    /// [`Notification::ExitReady`] when it may terminate.
    Shutdown,
}

/// Which connectivity checks a [`Command::Probe`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeKind {
    /// Campus Ping only.
    Campus,
    /// Public IPv4 + IPv6 HTTPS only.
    Internet,
    /// Both.
    All,
}

impl ProbeKind {
    pub fn includes_campus(self) -> bool {
        matches!(self, Self::Campus | Self::All)
    }

    pub fn includes_internet(self) -> bool {
        matches!(self, Self::Internet | Self::All)
    }
}

/// What the backend wants a frontend to do next, pushed synchronously on the
/// thread that caused the change (frontends marshal it to their own UI thread).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Notification {
    /// A new snapshot is available. Sent after every state-changing command and
    /// on every backend tick while a session is online.
    State { snapshot: Snapshot },
    /// The window should be sent to the tray (after a successful login, or when
    /// the user asked to close to the tray). `note` is the tooltip text.
    ///
    /// The frontend owns window lifetime: it hides and releases the window if it
    /// keeps one. The backend does not touch windows.
    HideToTray { note: String },
    /// The backend finished shutting down; the frontend may terminate its event
    /// loop or exit its process.
    ExitReady,
}

/// Result of executing a [`Command`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    /// The command completed; this snapshot reflects the new state. Frontends may
    /// render it directly (identical to the pushed [`Notification::State`]).
    Snapshot(Snapshot),
    /// The command produced text instead of a state change.
    Text(String),
}

/// Options for constructing the backend.
#[derive(Debug, Clone)]
pub struct ServiceOptions {
    /// Query the Dr.COM web portal once at startup (read-only). Disabled by
    /// tests and by `drcom-daemon --no-startup-probe`.
    pub startup_probe: bool,
    /// Honour a saved account with `auto_login`. Disabled by tests and by
    /// `drcom-daemon --no-auto-login` so a diagnostic run never claims the UDP
    /// session on the real campus network.
    pub auto_login: bool,
    /// Override the `drcom.ini` that would otherwise be discovered. `None` loads
    /// the normal search path.
    pub settings: Option<ClientConfig>,
}

impl Default for ServiceOptions {
    fn default() -> Self {
        Self {
            startup_probe: true,
            auto_login: true,
            settings: None,
        }
    }
}

impl ServiceOptions {
    /// Side-effect-free options for unit tests and tooling: no portal probe, no
    /// automatic login, no dependency on a `drcom.ini` next to the test binary.
    pub fn for_test() -> Self {
        Self {
            startup_probe: false,
            auto_login: false,
            settings: Some(ClientConfig::default()),
        }
    }
}
