//! The service implementation.
//!
//! TODO(backend-service task): this file starts as a compiling placeholder so
//! frontends and the IPC layer can be built against the frozen [`crate::api`]
//! contract in parallel. It must be replaced with the real state machine ported
//! from the former `drcom-gui/src/ui_backend.rs` (Slint glue removed):
//!
//! * session start/stop, reconnect backoff and admission checks;
//! * pending auto-login + one-shot portal probe and their interaction;
//! * adapter selection/refresh, session adapter display;
//! * failure card and diagnostics text;
//! * preferences save/clear, autostart, static-IPv4 editor operations;
//! * snapshot publication after every change and on [`Service::tick`].
//!
//! Keep the public surface below exactly as-is: frontends are being written
//! against it.

use std::sync::{Arc, Mutex};

use drcom_core::config::ClientConfig;

use crate::api::{
    API_VERSION, AdapterView, Command, CommandOutcome, ConnectionPhase, ConnectionView, FailureView,
    FormSeed, NetworkEditorView, Notification, ProbeView, ServiceOptions, SettingsView, Snapshot,
};

/// Callback invoked with every notification. There may be more than one (for
/// example a daemon broadcasting to several IPC clients).
pub type Sink = Arc<dyn Fn(Notification) + Send + Sync>;

struct Inner {
    options: ServiceOptions,
    settings: ClientConfig,
    sinks: Mutex<Vec<Sink>>,
}

/// Handle to the application backend.
///
/// Cloning is cheap and shares the same state; every method is safe to call from
/// any thread. The frontend owns the tick: call [`Service::tick`] about once per
/// second.
#[derive(Clone)]
pub struct Service {
    inner: Arc<Inner>,
}

impl Service {
    /// Builds the backend. `options` selects whether startup side effects
    /// (portal probe, saved auto-login) may run; production frontends use
    /// [`ServiceOptions::default`].
    pub fn new(options: ServiceOptions) -> Self {
        let settings = match &options.settings {
            Some(settings) => settings.clone(),
            None => load_settings(),
        };
        Self {
            inner: Arc::new(Inner {
                options,
                settings,
                sinks: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Registers a callback that receives every subsequent notification. The
    /// callback runs on the thread that caused the change and must not block.
    pub fn subscribe(&self, sink: Sink) {
        self.inner.sinks.lock().unwrap().push(sink);
    }

    /// The current complete view for the frontend.
    pub fn snapshot(&self) -> Snapshot {
        let settings = &self.inner.settings;
        Snapshot {
            api_version: API_VERSION,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            connection: ConnectionView {
                phase: ConnectionPhase::Disconnected,
                state_text: "未连接".into(),
                detail: String::new(),
                online: false,
                session_active: false,
                online_seconds: None,
                online_duration: "--".into(),
            },
            settings: SettingsView {
                username: settings.account.clone(),
                auto_login: false,
                auto_reconnect: true,
                minimize_to_tray: true,
                start_with_windows: None,
                config_status: None,
            },
            adapter: AdapterView {
                available: false,
                name: "未检测到物理网卡".into(),
                ipv4: "--".into(),
                mac: "--".into(),
                gateway: "--".into(),
                dns: "--".into(),
                ipv6: "--".into(),
            },
            failure: FailureView {
                message: "本次运行尚未发生连接问题".into(),
                time: "--".into(),
                stage: "--".into(),
                code: "--".into(),
                duration: "--".into(),
                retry: "--".into(),
            },
            probe: ProbeView {
                busy: false,
                campus: "未检测".into(),
                ipv4: "未检测".into(),
                ipv6: "未检测".into(),
            },
            network_editor: NetworkEditorView {
                visible: false,
                adapter_name: String::new(),
                ip: String::new(),
                mask: String::new(),
                gateway: String::new(),
                dns: String::new(),
                message: String::new(),
                busy: false,
            },
            campus_page: String::new(),
            tooltip: String::new(),
        }
    }

    /// Initial login-form values for in-process frontends (includes the saved
    /// password, so it is not part of [`Snapshot`] and never crosses IPC).
    pub fn form_seed(&self) -> FormSeed {
        let settings = &self.inner.settings;
        FormSeed {
            username: settings.account.clone(),
            password: (!settings.account.is_empty()).then(|| settings.password.clone()),
            ..FormSeed::default()
        }
    }

    /// Executes one command. Never blocks longer than an adapter enumeration or a
    /// file write; long work is moved to background threads and reported through
    /// notifications.
    pub fn execute(&self, command: Command) -> CommandOutcome {
        match command {
            Command::DiagnosticsText => CommandOutcome::Text(String::new()),
            _ => CommandOutcome::Snapshot(self.snapshot()),
        }
    }

    /// Periodic housekeeping: online duration, auto-hide-after-login, pending
    /// auto-login deadlines. Frontends call this about once per second.
    pub fn tick(&self) {}
}

fn load_settings() -> ClientConfig {
    match drcom_core::config::discover() {
        Some(path) => drcom_core::config::load(&path).unwrap_or_default(),
        None => ClientConfig::default(),
    }
}
