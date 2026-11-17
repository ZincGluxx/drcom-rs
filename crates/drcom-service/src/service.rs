//! The frontend-agnostic application state machine.
//!
//! This is the old Slint glue with every window, tray, event-loop and clipboard
//! concern removed. The backend owns the state, the behaviour and the strings;
//! a frontend renders [`crate::Snapshot`] and sends [`crate::Command`] values
//! back. Long operations (the authentication session, connectivity probes, the
//! elevated `netsh` call, the startup page probe) run on their own threads and
//! report progress by publishing snapshots.
//!
//! Locking rules, because they are easy to get wrong:
//!
//! * [`Inner::state`] is the single source of truth. Every method locks it only
//!   for the few assignments it needs and never calls a sink while holding it —
//!   a sink may call [`Service::snapshot`], which locks the same mutex.
//! * [`Inner::sinks`] is a separate mutex; sink callbacks always run after both
//!   locks have been released, on the thread that caused the change.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use drcom_core::adapter;
use drcom_core::config::{self, ClientConfig};
use drcom_core::connectivity::{self, Family};
use drcom_core::event_log;
use drcom_core::page_status::{self, CampusPage};
use drcom_core::preferences::{self, Preferences};
use drcom_core::session::{LoginSession, SessionConfig, SessionError, SessionEvent, SessionPhase};
use drcom_core::static_ipv4;
use drcom_core::transport::{DEFAULT_LOCAL_PORT, DEFAULT_SERVER_PORT};

use crate::api::{
    API_VERSION, AdapterView, Command, CommandOutcome, ConnectionPhase, ConnectionView,
    FailureView, FormSeed, NetworkEditorView, Notification, ProbeKind, ProbeView, ServiceOptions,
    SettingsView, Snapshot,
};
use crate::text::{
    self, ReconnectBackoff, adapter_view, describe_failure, failure_stage_and_code,
    format_duration, host_name, initial_failure, initial_network_editor, initial_probe,
    page_probe_status, retry_attempt_admitted, retryable_error, saved_password_for_form,
    unavailable_adapter_view, validate_preferences,
};

/// How long the window stays visible after a successful login before it tucks
/// itself into the tray. The C# client waits the same three seconds, so the user
/// sees the success state instead of watching the window vanish on the same frame
/// the reply arrived.
const HIDE_AFTER_LOGIN: Duration = Duration::from_secs(3);

/// A session that stayed up this long counts as working, so the following
/// automatic reconnect starts from the first, short delay again.
const STABLE_SESSION: Duration = Duration::from_secs(60);

/// Granularity of the wait between reconnect attempts; also bounds how long a
/// stop request takes to be honoured.
const SLEEP_STEP: Duration = Duration::from_millis(100);

/// The backend's mutable state. Mirrors exactly what the old `Shared` struct in
/// the GUI glue held, minus the window/tray handoff fields (which are now
/// notifications) and plus the views the window used to own as Slint properties.
struct Shared {
    stop: Option<Arc<AtomicBool>>,
    reconnect_allowed: Arc<AtomicBool>,
    connected_since: Option<Instant>,
    pending_auto: Option<(Preferences, Instant)>,
    page_probe_pending: bool,
    /// When the window should tuck itself away after a successful login. The C#
    /// client waits three seconds so the user actually sees the success state
    /// before the window disappears; hiding on the same frame as the reply reads
    /// as a crash.
    hide_at: Option<Instant>,
    /// Mirror of the "关闭到托盘" option for the session thread, which cannot read
    /// a checkbox.
    minimize_to_tray: bool,
    quitting: bool,
    /// Set once [`Notification::ExitReady`] has been sent, so the session and the
    /// network-editor threads cannot emit it twice.
    exit_ready_sent: bool,
    edit_adapter: Option<adapter::AdapterInfo>,
    config_status: Option<String>,
    autostart: Option<bool>,
    remembered: Option<Preferences>,
    connection_state: String,
    status_detail: String,
    online_duration: String,
    campus_page_status: String,
    tooltip: String,
    failure: FailureView,
    adapter_view: AdapterView,
    probe: ProbeView,
    editor: NetworkEditorView,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            stop: None,
            reconnect_allowed: Arc::new(AtomicBool::new(false)),
            connected_since: None,
            pending_auto: None,
            page_probe_pending: false,
            hide_at: None,
            // Same default as the C# client's `MinimizeToTray`.
            minimize_to_tray: true,
            quitting: false,
            exit_ready_sent: false,
            edit_adapter: None,
            config_status: None,
            autostart: None,
            remembered: None,
            connection_state: "未连接".to_string(),
            status_detail: String::new(),
            online_duration: "--".to_string(),
            campus_page_status: String::new(),
            tooltip: String::new(),
            failure: initial_failure(),
            adapter_view: unavailable_adapter_view(),
            probe: initial_probe(),
            editor: initial_network_editor(),
        }
    }
}

/// What the one-shot startup page probe found out about the program's own state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PageProbeOutcome {
    app_session_active: bool,
    canceled_auto_login: bool,
}

fn reconcile_page_probe_state(shared: &mut Shared, page: CampusPage) -> PageProbeOutcome {
    shared.page_probe_pending = false;
    let app_session_active = shared.stop.is_some() || shared.connected_since.is_some();
    let canceled_auto_login = !app_session_active
        && page == CampusPage::CurrentSession
        && shared.pending_auto.take().is_some();
    PageProbeOutcome {
        app_session_active,
        canceled_auto_login,
    }
}

/// Callback invoked with every notification. There may be more than one (for
/// example a daemon broadcasting to several IPC clients).
pub type Sink = Arc<dyn Fn(Notification) + Send + Sync>;

struct Inner {
    options: ServiceOptions,
    /// Settings read from `drcom.ini` at startup. Kept immutable; preferences are
    /// what saving changes.
    settings: ClientConfig,
    state: Mutex<Shared>,
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
        let (settings, startup_complaint) = match &options.settings {
            Some(settings) => (settings.clone(), None),
            None => match text::load_settings() {
                Ok(settings) => (settings, None),
                Err(message) => (ClientConfig::default(), Some(message)),
            },
        };
        let service = Self {
            inner: Arc::new(Inner {
                options,
                settings,
                state: Mutex::new(Shared::default()),
                sinks: Mutex::new(Vec::new()),
            }),
        };
        service.initialize(startup_complaint);
        service
    }

    /// Registers a callback that receives every subsequent notification. The
    /// callback runs on the thread that caused the change and must not block.
    ///
    /// The new sink first receives the current state, so a frontend that
    /// subscribes late is never left without a snapshot.
    pub fn subscribe(&self, sink: Sink) {
        sink(Notification::State {
            snapshot: self.snapshot(),
        });
        self.sinks().push(sink);
    }

    /// The current complete view for the frontend.
    pub fn snapshot(&self) -> Snapshot {
        build_snapshot(&self.state(), &self.inner.settings)
    }

    /// Initial login-form values for in-process frontends (includes the saved
    /// password, so it is not part of [`Snapshot`] and never crosses IPC).
    ///
    /// Merged exactly like the old `seed_window`: the `drcom.ini` account and
    /// password seed the form, then the encrypted preferences file overrides what
    /// the user remembered there.
    pub fn form_seed(&self) -> FormSeed {
        let shared = self.state();
        let prefs = shared.remembered.clone().unwrap_or_default();
        let mut seed = FormSeed {
            username: if self.inner.settings.account.is_empty() {
                String::new()
            } else {
                self.inner.settings.account.clone()
            },
            password: (!self.inner.settings.account.is_empty())
                .then(|| self.inner.settings.password.clone()),
            auto_login: prefs.auto_login,
            auto_reconnect: prefs.auto_reconnect,
            minimize_to_tray: shared.minimize_to_tray,
            start_with_windows: shared.autostart,
            config_status: shared.config_status.clone(),
        };
        if !prefs.username.is_empty() {
            seed.username = prefs.username.clone();
        }
        if let Some(password) = saved_password_for_form(&prefs) {
            seed.password = Some(password);
        }
        seed
    }

    /// Executes one command. Never blocks longer than an adapter enumeration or a
    /// file write; long work is moved to background threads and reported through
    /// notifications.
    pub fn execute(&self, command: Command) -> CommandOutcome {
        match command {
            Command::Login {
                username,
                password,
                auto_reconnect,
            } => {
                self.login(username, password, auto_reconnect);
                CommandOutcome::Snapshot(self.snapshot())
            }
            Command::Logout => {
                self.logout();
                CommandOutcome::Snapshot(self.snapshot())
            }
            Command::RefreshAdapter => {
                self.refresh_adapter(None);
                self.publish();
                CommandOutcome::Snapshot(self.snapshot())
            }
            Command::Probe { kind } => {
                self.probe(kind);
                CommandOutcome::Snapshot(self.snapshot())
            }
            Command::OpenNetworkEditor => {
                self.open_network_editor();
                CommandOutcome::Snapshot(self.snapshot())
            }
            Command::CloseNetworkEditor => {
                self.with_state(|shared| shared.editor.visible = false);
                self.publish();
                CommandOutcome::Snapshot(self.snapshot())
            }
            Command::ApplyNetwork {
                ip,
                mask,
                gateway,
                dns,
            } => {
                self.apply_network(&ip, &mask, &gateway, &dns);
                CommandOutcome::Snapshot(self.snapshot())
            }
            Command::SaveSettings {
                username,
                password,
                auto_login,
                auto_reconnect,
                autostart,
                minimize_to_tray,
            } => {
                self.save_settings(
                    username,
                    password,
                    auto_login,
                    auto_reconnect,
                    autostart,
                    minimize_to_tray,
                );
                CommandOutcome::Snapshot(self.snapshot())
            }
            Command::ClearSavedPassword {
                username,
                auto_reconnect,
                minimize_to_tray,
            } => {
                self.clear_saved_password(username, auto_reconnect, minimize_to_tray);
                CommandOutcome::Snapshot(self.snapshot())
            }
            Command::SetMinimizeToTray { enabled } => {
                self.set_minimize_to_tray(enabled);
                CommandOutcome::Snapshot(self.snapshot())
            }
            Command::DiagnosticsText => CommandOutcome::Text(self.diagnostics_report()),
            Command::Shutdown => {
                self.shutdown();
                CommandOutcome::Snapshot(self.snapshot())
            }
        }
    }

    /// Periodic housekeeping: online duration, auto-hide-after-login, pending
    /// auto-login deadlines. Frontends call this about once per second.
    pub fn tick(&self) {
        let mut hide_note = None;
        let mut auto_start = None;
        let mut timeout = false;
        let mut publish = false;

        {
            let mut shared = self.state();
            if shared.quitting {
                return;
            }
            // The hide-after-login delay runs off this one-second timer rather than
            // a second timer of its own.
            if let Some(due) = shared.hide_at
                && Instant::now() >= due
            {
                shared.hide_at = None;
                hide_note = Some("登录成功，已在托盘运行".to_string());
                shared.tooltip = "登录成功，已在托盘运行".to_string();
                publish = true;
            }
            if let Some(start) = shared.connected_since {
                // The old window read `connection.duration` every tick; the backend
                // formats it so every frontend shows the same text.
                shared.online_duration = format_duration(start.elapsed());
                publish = true;
            }
            if hide_note.is_none()
                && let Some((prefs, deadline)) = shared.pending_auto.clone()
            {
                if shared.page_probe_pending {
                    // Wait for the one-shot page probe: it may reveal an external
                    // session that this program must not take over.
                } else if Instant::now() > deadline {
                    shared.pending_auto = None;
                    timeout = true;
                    publish = true;
                } else if text::select_route_adapter(&self.inner.settings).is_some() {
                    shared.pending_auto = None;
                    auto_start = Some(prefs);
                } else if shared.connection_state != "等待网络" {
                    shared.connection_state = "等待网络".to_string();
                    shared.status_detail = "自动登录正在等待可用网卡…".to_string();
                    publish = true;
                }
            }
        }

        if let Some(note) = hide_note {
            if publish {
                // Keep the final duration in the snapshot that announces the hide.
                self.publish();
            }
            self.emit(Notification::HideToTray { note });
            return;
        }
        if timeout {
            self.remember_failure_text("自动登录等待可用网络超时");
            self.set_connection("未连接", "自动登录等待网卡超时，请连接网络后手动连接");
            self.publish();
            return;
        }
        if let Some(prefs) = auto_start {
            self.login(prefs.username, prefs.password, prefs.auto_reconnect);
            return;
        }
        if publish {
            self.publish();
        }
    }
}

impl Service {
    /// Reads the configuration, preferences and autostart state, applies the
    /// startup complaint, initialises the adapter panel and starts the one-shot
    /// page probe. Everything here survives for the process lifetime, so it is
    /// the backend's job instead of a window's.
    fn initialize(&self, complaint: Option<String>) {
        event_log::record(&format!(
            "启动 DrCom Rust {}，配置文件：{}",
            env!("CARGO_PKG_VERSION"),
            match config::discover() {
                Some(path) => path.display().to_string(),
                None => "无（使用内置认证服务器）".to_string(),
            }
        ));

        match load_startup_preferences() {
            Ok(prefs) => {
                self.with_state(|shared| {
                    shared.minimize_to_tray = prefs.minimize_to_tray;
                    shared.config_status = if prefs.username.is_empty() {
                        None
                    } else {
                        Some("已记住你的账号".to_string())
                    };
                    let auto_ready = !prefs.username.is_empty() && !prefs.password.is_empty();
                    if complaint.is_none()
                        && self.inner.options.auto_login
                        && prefs.auto_login
                        && auto_ready
                    {
                        shared.pending_auto =
                            Some((prefs.clone(), Instant::now() + Duration::from_secs(90)));
                    }
                    shared.remembered = Some(prefs);
                });
            }
            Err(error) => {
                let non_sensitive = preferences::path()
                    .and_then(|path| preferences::load_non_sensitive(&path))
                    .ok();
                self.with_state(|shared| {
                    // A locked/missing keyring must not erase UI preferences or
                    // trigger auto-login with an empty/stale password.
                    if let Some(prefs) = non_sensitive {
                        shared.minimize_to_tray = prefs.minimize_to_tray;
                        shared.remembered = Some(prefs);
                    }
                    shared.config_status = Some(format!(
                        "凭据未读取：{error}；可手动登录，解锁密钥环后重新保存"
                    ));
                });
            }
        }
        match preferences::autostart::enabled() {
            Ok(enabled) => self.with_state(|shared| shared.autostart = Some(enabled)),
            Err(_) => self.with_state(|shared| {
                shared.config_status = Some("无法读取开机启动状态".to_string());
                shared.autostart = None;
            }),
        }

        // The old window replayed this on every open (seed_window ->
        // refresh_adapter(None)); a headless backend has to put it in the first
        // snapshot instead.
        self.refresh_adapter(None);

        if let Some(complaint) = complaint {
            self.remember_failure_text(&complaint);
            self.set_connection("未连接", &complaint);
        }

        if self.inner.options.startup_probe {
            self.start_page_probe();
        }
        self.publish();
    }

    /// One read-only portal probe per launch, with or without a frontend: the
    /// answer is kept in state and replayed to whichever frontend attaches.
    fn start_page_probe(&self) {
        self.with_state(|shared| {
            shared.page_probe_pending = true;
            shared.campus_page_status = "正在只读查询认证服务器首页…".to_string();
        });
        self.publish();
        let server = self.inner.settings.servers_or_env().first().copied();
        let backend = self.clone();
        thread::spawn(move || {
            let result = server.map(page_status::probe);
            let (page, failed) = match result {
                Some(Ok(page)) => (page, false),
                Some(Err(_)) | None => (CampusPage::Unknown, true),
            };
            backend.complete_page_probe(page, failed);
        });
    }

    fn complete_page_probe(&self, page: CampusPage, failed: bool) {
        let outcome = self.with_state(|shared| reconcile_page_probe_state(shared, page));
        self.set_campus_page_status(page_probe_status(page, failed, outcome.app_session_active));
        if outcome.canceled_auto_login {
            event_log::record("启动网页状态显示已有认证；跳过本次自动登录，未接管外部会话");
            self.set_status("网页显示已有认证；本程序未接管外部会话，也未启动 UDP 保活");
        }
        self.publish();
    }

    /// Re-enumerates adapters and updates the panel plus the status line the old
    /// window showed under it.
    fn refresh_adapter(&self, note: Option<&str>) {
        let selected = text::select_route_adapter(&self.inner.settings);
        let note = note.map(str::to_string);
        self.with_state(|shared| match selected {
            Some(found) => {
                shared.adapter_view = adapter_view(&found);
                shared.status_detail =
                    note.unwrap_or_else(|| "网络已就绪，填写账号密码后即可连接".to_string());
            }
            None => {
                shared.adapter_view = unavailable_adapter_view();
                shared.status_detail = note.unwrap_or_else(|| {
                    "没有找到可用的网络连接，请插好网线或连接校园 Wi-Fi 后刷新".to_string()
                });
            }
        });
    }

    /// Starts a session. Every early return leaves a status or failure the
    /// frontend can render; a successful call returns after the reconnect-loop
    /// thread has been spawned.
    fn login(&self, username: String, password: String, auto_reconnect: bool) {
        let can_start = self.with_state(|shared| {
            if shared.stop.is_some() || shared.quitting {
                false
            } else {
                shared.pending_auto = None;
                true
            }
        });
        if !can_start {
            self.set_status("连接已在进行中");
            self.publish();
            return;
        }
        if username.trim().is_empty() || password.is_empty() {
            self.set_status("请先填写账号和密码");
            self.publish();
            return;
        }
        let servers = self.inner.settings.servers_or_env();
        if servers.is_empty() {
            // `servers_or_env` falls back to the built-in campus address, so
            // this branch only fires if that constant is ever emptied.
            self.remember_failure_text("没有可用的认证服务器地址");
            self.set_connection("未连接", "没有可用的认证服务器地址");
            self.publish();
            return;
        }
        // Resolve the servers first so adapter choice can follow the route the
        // OS would actually use to reach them.
        let Some(found) = text::select_session_adapter(&self.inner.settings) else {
            self.remember_failure_text("没有找到可用的校园网连接");
            self.set_connection(
                "未连接",
                "没有找到可用的校园网连接，请插好网线或连接校园 Wi-Fi，再点击刷新",
            );
            self.publish();
            return;
        };

        let mut session_config = SessionConfig::new(
            servers,
            username.trim().as_bytes().to_vec(),
            password.into_bytes(),
        );
        session_config.local_ipv4 = found.ipv4;
        session_config.mac = found.mac;
        session_config.auto_adapter = self.inner.settings.local_ip.is_none();
        session_config.interface_index = Some(found.interface_index);
        session_config.adapter_mac_override = self.inner.settings.mac;
        session_config.hostname = host_name().into_bytes();
        text::apply_settings(&mut session_config, &self.inner.settings);
        session_config.allow_loopback_server = false;
        // The account never reaches the log; the interface and server are what
        // make a stale-log entry useful.
        event_log::record(&format!(
            "开始认证：网卡 {}（{}，{}），服务器 {}",
            found.friendly_name,
            found.description,
            found.ipv4,
            session_config
                .servers
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));

        let stop = Arc::new(AtomicBool::new(false));
        let reconnect_allowed = Arc::new(AtomicBool::new(auto_reconnect));
        self.with_state(|shared| {
            shared.adapter_view = adapter_view(&found);
            shared.stop = Some(Arc::clone(&stop));
            shared.reconnect_allowed = Arc::clone(&reconnect_allowed);
            shared.connected_since = None;
            shared.pending_auto = None;
        });
        self.set_connection("连接中", "正在联系校园网服务器…");
        self.publish();

        let backend = self.clone();
        thread::spawn(move || {
            let mut backoff = ReconnectBackoff::new();
            let mut first_attempt = true;
            let mut retry_result: Option<(Result<(), SessionError>, Duration)> = None;
            loop {
                // Admission and password clearing share this mutex, so clearing
                // credentials after the backoff wait is observed before another
                // session can be constructed with the captured old password.
                let admitted = backend.with_state(|shared| {
                    retry_attempt_admitted(
                        first_attempt,
                        shared.reconnect_allowed.load(Ordering::Relaxed),
                        stop.load(Ordering::Relaxed),
                    )
                });
                if !admitted {
                    let (result, elapsed) = retry_result
                        .take()
                        .unwrap_or((Err(SessionError::Stopped), Duration::ZERO));
                    let result = if stop.load(Ordering::Relaxed) {
                        Err(SessionError::Stopped)
                    } else {
                        result
                    };
                    backend.finish(result, elapsed);
                    break;
                }
                first_attempt = false;

                let started = Instant::now();
                let result = match LoginSession::new(session_config.clone()) {
                    Ok(mut session) => {
                        session.use_external_stop(Arc::clone(&stop));
                        session.run(&mut |event| backend.on_event(event))
                    }
                    Err(error) => Err(error),
                };
                let elapsed = started.elapsed();
                let retry = auto_reconnect
                    && reconnect_allowed.load(Ordering::Relaxed)
                    && !stop.load(Ordering::Relaxed)
                    && result.as_ref().is_err_and(retryable_error);
                if !retry {
                    backend.finish(result, elapsed);
                    break;
                }
                // A session that stayed up long enough to count as working starts
                // the next wait over. Without this rule, a link that flaps every
                // few seconds would keep the delay at its first step forever.
                if started.elapsed() >= STABLE_SESSION {
                    backoff.reset();
                }
                let delay = backoff.next_delay();
                let seconds = delay.as_secs();
                let reason = match &result {
                    Ok(()) => "会话结束".to_string(),
                    Err(error) => describe_failure(error),
                };
                event_log::record(&format!("连接中断：{reason}；{seconds} 秒后重试"));
                if let Err(error) = &result {
                    backend.remember_session_failure(
                        error,
                        elapsed,
                        &format!("将在 {seconds} 秒后重试；本次会复用当前会话凭据"),
                    );
                }
                backend.set_connection("重连中", &format!("{reason}（{seconds} 秒后自动重试）"));
                backend.set_tooltip(&format!("{reason}，{seconds} 秒后重试"));
                backend.with_state(|shared| {
                    shared.connected_since = None;
                    shared.hide_at = None;
                });
                backend.set_online_duration("--");
                backend.publish();
                if !Self::sleep_interruptibly(delay, &stop, &reconnect_allowed) {
                    if stop.load(Ordering::Relaxed) {
                        backend.finish(Err(SessionError::Stopped), elapsed);
                    } else {
                        backend.finish(result, elapsed);
                    }
                    break;
                }
                // Automatic sessions discover again inside the session, using
                // the last successfully authenticated GUID as first candidate.
                // A manually configured address remains fixed.
                if let Some(ip) = backend.inner.settings.local_ip {
                    session_config.interface_index = adapter::list()
                        .into_iter()
                        .find(|found| found.ipv4 == ip)
                        .map(|found| found.interface_index);
                }
                retry_result = Some((result, elapsed));
            }
        });
    }

    fn on_event(&self, event: SessionEvent) {
        match event {
            SessionEvent::AdapterSelected(found) => {
                self.with_state(|shared| shared.adapter_view = adapter_view(&found));
                self.publish();
            }
            SessionEvent::Phase(phase) => match phase {
                SessionPhase::Challenge => {
                    self.set_connection("连接中", "正在联系校园网服务器…");
                    self.publish();
                }
                SessionPhase::Authenticating => {
                    self.set_connection("连接中", "正在检查账号和密码…");
                    self.publish();
                }
                SessionPhase::Online => {
                    let minimize = self.with_state(|shared| {
                        let now = Instant::now();
                        shared.connected_since = Some(now);
                        let minimize = shared.minimize_to_tray;
                        shared.hide_at = minimize.then(|| now + HIDE_AFTER_LOGIN);
                        minimize
                    });
                    event_log::record("登录成功");
                    self.set_campus_page_status("此程序 UDP 保活已建立");
                    self.set_connection("已连接", "登录成功，可以使用校园网了");
                    self.set_online_duration("0 秒");
                    // The window is about to disappear, so the status line has to
                    // carry the news as well.
                    self.set_tooltip(if minimize {
                        "已连接，稍后最小化到托盘"
                    } else {
                        "已连接，正在保持在线"
                    });
                    self.publish();
                }
                SessionPhase::KeepAlive => {
                    self.set_connection("已连接", "连接正常，正在保持在线");
                    self.publish();
                }
                SessionPhase::LoggedOut => {
                    event_log::record("已发送下线报文");
                    self.set_campus_page_status("此程序 UDP 保活已停止");
                    self.set_connection("未连接", "已下线");
                    self.publish();
                }
            },
            // Protocol notes include salts, session tokens and sequence counters.
            // Keep them out of the everyday UI and shareable diagnostics.
            SessionEvent::Note(_) => {}
            SessionEvent::KeepAliveCycle { .. } => {
                self.set_status("连接正常，正在保持在线");
                let duration =
                    self.with_state(|shared| shared.connected_since.map(|start| start.elapsed()));
                if let Some(duration) = duration {
                    self.set_online_duration(&format_duration(duration));
                }
                self.publish();
            }
        }
    }

    /// Tears a session down: clears the stop handle and success timers, records
    /// the failure card (unless the reason was the user's own stop) and hands the
    /// frontend the final "未连接" state. If a shutdown is waiting for the
    /// session, this is also where the exit becomes possible.
    fn finish(&self, result: Result<(), SessionError>, elapsed: Duration) {
        self.with_state(|shared| {
            shared.stop = None;
            shared.connected_since = None;
            shared.hide_at = None;
        });
        self.set_online_duration("--");
        let detail = match result {
            Ok(()) | Err(SessionError::Stopped) => "已下线".to_string(),
            Err(error) => {
                let detail = describe_failure(&error);
                self.remember_session_failure(&error, elapsed, "不会自动重试");
                if let SessionError::Transport(kind) = &error {
                    event_log::record(&format!("网络错误类别：{kind:?}"));
                }
                detail
            }
        };
        event_log::record(&format!("会话结束：{detail}"));
        // By now the window is usually in the tray, where the status line is
        // invisible. Without this the user's only clue is a green dot turning grey.
        self.set_tooltip(&detail);
        self.set_connection("未连接", &detail);
        self.publish();
        self.maybe_emit_exit_ready();
    }

    fn logout(&self) {
        let had_pending = self.with_state(|shared| shared.pending_auto.take().is_some());
        if had_pending {
            self.set_connection("未连接", "已取消自动登录");
            self.publish();
            return;
        }
        let stop = self.with_state(|shared| {
            shared.reconnect_allowed.store(false, Ordering::Relaxed);
            shared.stop.clone()
        });
        match stop {
            Some(stop) => {
                stop.store(true, Ordering::Relaxed);
                self.set_connection("断开中", "正在下线…");
                self.publish();
            }
            None => {
                self.set_status("当前没有进行中的连接");
                self.publish();
            }
        }
    }

    fn probe(&self, kind: ProbeKind) {
        let started = self.with_state(|shared| {
            if shared.probe.busy || shared.quitting {
                false
            } else {
                shared.probe.busy = true;
                true
            }
        });
        if !started {
            return;
        }
        self.publish();
        let selected = text::select_session_adapter(&self.inner.settings);
        let backend = self.clone();
        thread::spawn(move || {
            let include_campus = kind.includes_campus();
            let include_internet = kind.includes_internet();
            let (campus, ipv4, ipv6) = thread::scope(|scope| {
                let campus = scope.spawn(|| include_campus.then(connectivity::campus_probe));
                let ipv4 = scope.spawn(|| {
                    include_internet
                        .then(|| connectivity::internet_probe(Family::V4, selected.as_ref()))
                });
                let ipv6 = scope.spawn(|| {
                    include_internet
                        .then(|| connectivity::internet_probe(Family::V6, selected.as_ref()))
                });
                (
                    campus.join().unwrap(),
                    ipv4.join().unwrap(),
                    ipv6.join().unwrap(),
                )
            });
            // Recorded so a user reporting "the check says IPv6 is down" leaves
            // behind what it actually saw; otherwise the only evidence is the
            // screenshot they did not take.
            for (label, result) in [
                ("校内测试", campus.as_deref()),
                ("公网测试 IPv4", ipv4.as_deref()),
                ("公网测试 IPv6", ipv6.as_deref()),
            ] {
                if let Some(text) = result {
                    event_log::record(&format!("{label}：{text}"));
                }
            }
            backend.with_state(|shared| {
                shared.probe.busy = false;
                if let Some(result) = campus {
                    shared.probe.campus = result;
                }
                // The window showed only the verdict before the "｜" separator;
                // the report uses the same truncated string, so the full sentence
                // stays in the event log instead of in the snapshot.
                if let Some(result) = ipv4 {
                    shared.probe.ipv4 = result.split('｜').next().unwrap_or(&result).to_string();
                }
                if let Some(result) = ipv6 {
                    shared.probe.ipv6 = result.split('｜').next().unwrap_or(&result).to_string();
                }
            });
            backend.publish();
        });
    }

    fn open_network_editor(&self) {
        if let Some(reason) = static_ipv4::unsupported_reason() {
            self.set_status(reason);
            self.publish();
            return;
        }
        let blocked = self
            .with_state(|shared| shared.stop.is_some() || shared.quitting || shared.editor.busy);
        if blocked {
            return;
        }
        let Some(found) = text::select_route_adapter(&self.inner.settings) else {
            self.set_status("未找到可配置的物理网卡");
            self.publish();
            return;
        };
        let mask = found
            .details
            .subnet_mask
            .map(|ip| ip.to_string())
            .unwrap_or_default();
        let gateway = found
            .details
            .gateways
            .iter()
            .find(|ip| ip.is_ipv4())
            .map(ToString::to_string)
            .unwrap_or_default();
        let dns = found
            .details
            .dns
            .iter()
            .find(|ip| ip.is_ipv4())
            .map(ToString::to_string)
            .unwrap_or_default();
        let editor = NetworkEditorView {
            visible: true,
            adapter_name: found.friendly_name.clone(),
            ip: found.ipv4.to_string(),
            mask,
            gateway,
            dns,
            message: "应用时会请求管理员权限，并可能短暂中断此网卡的连接。".to_string(),
            busy: false,
        };
        self.with_state(|shared| {
            shared.pending_auto = None;
            shared.edit_adapter = Some(found);
            shared.editor = editor;
        });
        self.publish();
    }

    fn apply_network(&self, ip: &str, mask: &str, gateway: &str, dns: &str) {
        let config = match static_ipv4::Configuration::parse(ip, mask, gateway, dns) {
            Ok(config) => config,
            Err(error) => {
                self.with_state(|shared| shared.editor.message = error);
                self.publish();
                return;
            }
        };
        let (blocked, edit_adapter) = self.with_state(|shared| {
            (
                shared.editor.busy || shared.stop.is_some() || shared.quitting,
                shared.edit_adapter.clone(),
            )
        });
        if blocked {
            return;
        }
        let Some(found) = edit_adapter else {
            return;
        };
        // Confirm that the selected physical adapter still exists before elevation.
        if !adapter::list().iter().any(|item| {
            item.interface_index == found.interface_index
                && item.mac == found.mac
                && item.friendly_name == found.friendly_name
        }) {
            self.with_state(|shared| {
                shared.editor.message = "网卡已变化，请返回后重新打开网络设置".to_string();
            });
            self.publish();
            return;
        }
        self.with_state(|shared| {
            shared.editor.busy = true;
            shared.editor.message = "等待管理员授权与应用结果…".to_string();
        });
        self.publish();
        let backend = self.clone();
        thread::spawn(move || {
            let result = config.apply(&found.friendly_name);
            // Keep the busy flag raised until every visible field is final, so a
            // shutdown that races this thread can only observe "still applying".
            backend.refresh_adapter(None);
            backend.with_state(|shared| {
                shared.editor.busy = false;
                shared.editor.message = match result {
                    Ok(()) => "IPv4 设置已应用，可返回主界面连接".to_string(),
                    Err(error) => error,
                };
            });
            backend.publish();
            backend.maybe_emit_exit_ready();
        });
    }

    fn save_settings(
        &self,
        username: String,
        password: String,
        auto_login: bool,
        auto_reconnect: bool,
        autostart: bool,
        minimize_to_tray: bool,
    ) {
        let password_cleared = password.is_empty();
        let prefs = Preferences {
            username: username.trim().to_string(),
            password,
            password_cleared,
            auto_login,
            auto_reconnect,
            minimize_to_tray,
        };
        let mut saved = false;
        let result = (|| -> Result<(), String> {
            validate_preferences(&prefs, self.inner.settings.servers_or_env())?;
            let path = preferences::path().map_err(|e| e.to_string())?;
            preferences::save(&path, &prefs).map_err(|e| format!("保存失败：{e}"))?;
            saved = true;
            preferences::autostart::set(autostart)
                .map_err(|e| format!("账号已保存，但开机启动设置失败：{e}"))?;
            Ok(())
        })();
        let autostart_now = preferences::autostart::enabled().ok();
        let succeeded = result.is_ok();
        let config_status = if succeeded {
            "账号已安全保存"
        } else {
            "保存未完成"
        }
        .to_string();
        let status = match &result {
            Ok(()) => "设置已保存（自动登录在下次启动时生效）".to_string(),
            Err(error) => error.clone(),
        };
        self.with_state(|shared| {
            // The close button reads this option live, so it takes effect
            // immediately rather than at the next launch like auto-login.
            shared.minimize_to_tray = minimize_to_tray;
            if !minimize_to_tray {
                shared.hide_at = None;
            }
            shared.config_status = Some(config_status);
            if let Some(enabled) = autostart_now {
                shared.autostart = Some(enabled);
            }
            if saved {
                // Mirrors what is now on disk, so the next snapshot does not keep
                // showing the previously saved account.
                shared.remembered = Some(prefs);
            }
            shared.status_detail = status;
        });
        self.publish();
    }

    fn clear_saved_password(&self, username: String, auto_reconnect: bool, minimize_to_tray: bool) {
        let prefs = text::cleared_password_preferences(
            username.trim().to_string(),
            auto_reconnect,
            minimize_to_tray,
        );
        let saved = preferences::path()
            .and_then(|path| preferences::save(&path, &prefs))
            .is_ok();
        let ini_has_password = !self.inner.settings.password.is_empty();
        self.with_state(|shared| {
            shared.pending_auto = None;
            shared.reconnect_allowed.store(false, Ordering::Relaxed);
            // Reports the cleared persisted preference (auto-login off, password
            // empty) without modelling the form-clearing that used to live in the
            // window.
            shared.remembered = Some(prefs);
            shared.config_status = Some(
                if saved {
                    "已清除此程序保存的密码"
                } else {
                    "密码已从界面清除，但清除状态未能保存"
                }
                .to_string(),
            );
            if shared.connection_state == "重连中" {
                shared.failure.retry = "已取消重连；不会再使用本次会话凭据".to_string();
            }
            shared.status_detail = if ini_has_password {
                if saved {
                    "此程序保存的密码已清除；drcom.ini 中仍有你配置的密码，本程序不会自动回填或修改它"
                } else {
                    "界面密码已清空，但清除状态未保存；drcom.ini 仍由你管理，当前不会自动重连"
                }
            } else if saved {
                "已清除保存的密码；当前 UDP 保活继续运行，连接中断后不会用旧密码重连"
            } else {
                "界面密码已清空；当前 UDP 保活继续运行，连接中断后不会自动重连"
            }
            .to_string();
        });
        event_log::record(if saved {
            "用户清除已保存密码；取消待自动登录并停用后续凭据重连"
        } else {
            "用户请求清除已保存密码；保存清除状态失败，取消待自动登录并停用后续凭据重连"
        });
        self.publish();
    }

    /// Applies the (unsaved) "关闭到托盘" checkbox immediately; saving is the
    /// frontend's separate action. Clearing an auto-hide that is already due
    /// keeps the old window's promise: with the option off you never get hidden.
    fn set_minimize_to_tray(&self, enabled: bool) {
        self.with_state(|shared| {
            shared.minimize_to_tray = enabled;
            if !enabled {
                shared.hide_at = None;
            }
        });
        self.publish();
    }

    fn shutdown(&self) {
        let (network_busy, stop) = self.with_state(|shared| {
            shared.quitting = true;
            shared.pending_auto = None;
            shared.hide_at = None;
            (shared.editor.busy, shared.stop.clone())
        });
        event_log::record("请求退出");
        if network_busy {
            if let Some(stop) = &stop {
                stop.store(true, Ordering::Relaxed);
                self.with_state(|shared| {
                    shared.reconnect_allowed.store(false, Ordering::Relaxed);
                });
            }
            if stop.is_some() {
                self.set_connection("断开中", "等待网络设置操作完成后退出…");
            } else {
                self.set_status("等待网络设置操作完成后退出…");
            }
            self.publish();
            // The network thread (and the session thread, if any) call
            // `maybe_emit_exit_ready`, so the frontend only exits once both are done.
            return;
        }
        if let Some(stop) = stop {
            stop.store(true, Ordering::Relaxed);
            self.with_state(|shared| {
                shared.reconnect_allowed.store(false, Ordering::Relaxed);
            });
            self.set_connection("断开中", "正在下线，完成后退出…");
            self.publish();
        } else {
            self.publish();
            self.maybe_emit_exit_ready();
        }
    }

    /// The text that both 复制诊断 and 导出诊断 produce.
    ///
    /// The account and password are deliberately absent: this is the string a user
    /// pastes into a public issue or hands to a stranger at the network centre. The
    /// tail of the event log is included because "它连不上" is not a report.
    fn diagnostics_report(&self) -> String {
        let snapshot = self.snapshot();
        let recent = event_log::tail(40);
        let adapters = adapter::list()
            .into_iter()
            .map(|item| {
                format!(
                    "{} / {} / IPv4 {} / 网关 {} / IPv6 {}",
                    item.friendly_name,
                    item.description,
                    item.ipv4,
                    item.details
                        .gateways
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", "),
                    item.details
                        .ipv6
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let local_port = self.inner.settings.local_port.unwrap_or(DEFAULT_LOCAL_PORT);
        format!(
            "DrCom 校园网助手 {}\n检测结果生成时间：{}\n\n\
             当前状态：{}\n当前说明：{}\n网页只读状态：{}\n最近一次连接问题：{}\n\
             错误阶段：{}\n错误码：{}\n发生时间：{}\n本次会话/尝试耗时：{}\n重试安全摘要：{}\n\n\
             登录使用的网卡：{}\nIPv4：{}\nMAC：{}\n网关：{}\nDNS：{}\nIPv6：{}\n在线时长：{}\n\n\
             校内（Ping jlu.edu.cn / IPv4）：{}\n外网（HTTPS / IPv4）：{}\n外网（HTTPS / IPv6）：{}\n\
             外网通过所选校园网网卡直接访问 HTTPS 网站，分别指定 IPv4 / IPv6，不使用应用代理，验证证书与网站响应，主站失败后使用备用站点复核。校内仍使用 Ping，Ping 无回复不等于断网。\n\n\
             认证服务器：{}\n服务器端口：{}\n本地端口设置：{}\n配置文件：{}\n\n\
             可用物理网卡：\n{}\n\n本次运行的诊断摘要（仅保存在内存，退出后清空）：\n{}",
            env!("CARGO_PKG_VERSION"),
            event_log::timestamp(),
            snapshot.connection.state_text,
            snapshot.connection.detail,
            snapshot.campus_page,
            snapshot.failure.message,
            snapshot.failure.stage,
            snapshot.failure.code,
            snapshot.failure.time,
            snapshot.failure.duration,
            snapshot.failure.retry,
            snapshot.adapter.name,
            snapshot.adapter.ipv4,
            snapshot.adapter.mac,
            snapshot.adapter.gateway,
            snapshot.adapter.dns,
            snapshot.adapter.ipv6,
            snapshot.connection.online_duration,
            snapshot.probe.campus,
            snapshot.probe.ipv4,
            snapshot.probe.ipv6,
            self.inner
                .settings
                .servers_or_env()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            self.inner
                .settings
                .server_port
                .unwrap_or(DEFAULT_SERVER_PORT),
            if local_port == 0 {
                "自动选择可用端口".to_string()
            } else {
                format!("固定端口 {local_port}")
            },
            config::discover()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "使用默认设置".to_string()),
            adapters,
            recent
        )
    }

    /// Sleeps in short steps, so a stop request is honoured within [`SLEEP_STEP`]
    /// rather than after the whole delay. Returns `false` once stopping has been
    /// requested.
    fn sleep_interruptibly(
        delay: Duration,
        stop: &AtomicBool,
        reconnect_allowed: &AtomicBool,
    ) -> bool {
        let mut remaining = delay;
        while !remaining.is_zero() {
            if stop.load(Ordering::Relaxed) || !reconnect_allowed.load(Ordering::Relaxed) {
                return false;
            }
            let step = remaining.min(SLEEP_STEP);
            thread::sleep(step);
            remaining -= step;
        }
        !stop.load(Ordering::Relaxed) && reconnect_allowed.load(Ordering::Relaxed)
    }

    fn remember_failure(&self, detail: &str) {
        self.with_state(|shared| {
            shared.failure.message = detail.to_string();
            shared.failure.time = event_log::timestamp();
        });
    }

    fn remember_failure_text(&self, detail: &str) {
        // The timestamp is taken once for both assignments, like the old helper
        // did (remember_failure then a second lock for the fixed category).
        self.remember_failure(detail);
        self.with_state(|shared| {
            shared.failure.stage = "连接准备".to_string();
            shared.failure.code = "LOCAL-NETWORK".to_string();
            shared.failure.duration = "未开始".to_string();
            shared.failure.retry = "不会自动重试".to_string();
        });
    }

    fn remember_session_failure(
        &self,
        error: &SessionError,
        elapsed: Duration,
        retry_summary: &str,
    ) {
        let reason = describe_failure(error);
        let (stage, code) = failure_stage_and_code(error);
        self.with_state(|shared| {
            shared.failure.message = reason;
            shared.failure.time = event_log::timestamp();
            shared.failure.stage = stage;
            shared.failure.code = code;
            shared.failure.duration =
                format!("{}.{:03} 秒", elapsed.as_secs(), elapsed.subsec_millis());
            shared.failure.retry = retry_summary.to_string();
        });
    }

    fn set_status(&self, text: &str) {
        self.with_state(|shared| shared.status_detail = text.to_string());
    }

    fn set_connection(&self, state: &str, detail: &str) {
        self.with_state(|shared| {
            shared.connection_state = state.to_string();
            shared.status_detail = detail.to_string();
        });
    }

    fn set_online_duration(&self, text: &str) {
        self.with_state(|shared| shared.online_duration = text.to_string());
    }

    fn set_campus_page_status(&self, status: &str) {
        self.with_state(|shared| shared.campus_page_status = status.to_string());
    }

    fn set_tooltip(&self, note: &str) {
        self.with_state(|shared| shared.tooltip = note.to_string());
    }

    /// Sends `Notification::ExitReady` once every reason to stay alive is gone:
    /// the user asked to quit, no elevated network operation is running and no
    /// session is still shutting down. Called by the session thread after
    /// [`Service::finish`] and by the network-editor thread after applying.
    fn maybe_emit_exit_ready(&self) {
        let ready = self.with_state(|shared| {
            if shared.quitting
                && !shared.exit_ready_sent
                && !shared.editor.busy
                && shared.stop.is_none()
            {
                shared.exit_ready_sent = true;
                true
            } else {
                false
            }
        });
        if ready {
            self.emit(Notification::ExitReady);
        }
    }

    fn sinks(&self) -> std::sync::MutexGuard<'_, Vec<Sink>> {
        self.inner
            .sinks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn state(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn with_state<R>(&self, mutate: impl FnOnce(&mut Shared) -> R) -> R {
        let mut shared = self.state();
        mutate(&mut shared)
    }

    /// Publishes the current snapshot to every sink.
    ///
    /// The snapshot is built before the sink list is locked and the callbacks run
    /// with no lock held: a sink is allowed to call [`Service::snapshot`] or
    /// [`Service::form_seed`].
    fn publish(&self) {
        let snapshot = self.snapshot();
        self.broadcast(Notification::State { snapshot });
    }

    fn emit(&self, notification: Notification) {
        self.broadcast(notification);
    }

    fn broadcast(&self, notification: Notification) {
        let sinks = self.sinks().clone();
        for sink in sinks {
            // Deliberately no lock held: sinks marshal into their own frontend
            // and may call back into this service.
            sink(notification.clone());
        }
    }
}

/// The current snapshot, built from the shared state and the immutable settings.
fn build_snapshot(shared: &Shared, settings: &ClientConfig) -> Snapshot {
    let prefs = shared.remembered.clone().unwrap_or_default();
    let phase = match shared.connection_state.as_str() {
        "已连接" => ConnectionPhase::Connected,
        "连接中" => ConnectionPhase::Connecting,
        "重连中" => ConnectionPhase::Reconnecting,
        "断开中" => ConnectionPhase::Disconnecting,
        "等待网络" => ConnectionPhase::WaitingForNetwork,
        _ => ConnectionPhase::Disconnected,
    };
    Snapshot {
        api_version: API_VERSION,
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        connection: ConnectionView {
            phase,
            state_text: shared.connection_state.clone(),
            detail: shared.status_detail.clone(),
            online: shared.connection_state == "已连接",
            session_active: shared.connection_state != "未连接",
            online_seconds: shared
                .connected_since
                .map(|start| start.elapsed().as_secs()),
            online_duration: shared.online_duration.clone(),
        },
        settings: SettingsView {
            username: if prefs.username.is_empty() {
                settings.account.clone()
            } else {
                prefs.username.clone()
            },
            auto_login: prefs.auto_login,
            auto_reconnect: prefs.auto_reconnect,
            minimize_to_tray: shared.minimize_to_tray,
            start_with_windows: shared.autostart,
            config_status: shared.config_status.clone(),
        },
        adapter: shared.adapter_view.clone(),
        failure: shared.failure.clone(),
        probe: shared.probe.clone(),
        network_editor: shared.editor.clone(),
        campus_page: shared.campus_page_status.clone(),
        tooltip: shared.tooltip.clone(),
    }
}

// ---------------------------------------------------------------------------
// Startup preferences. Tests replace the DPAPI file read with a thread-local
// value so they never depend on (or disturb) the real user's saved credentials.
// ---------------------------------------------------------------------------

#[cfg(test)]
thread_local! {
    /// `None` means "no override"; `Some(None)` means "no saved preferences".
    static TEST_PREFERENCES: std::cell::RefCell<Option<Option<Preferences>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_test_preferences(prefs: Option<Preferences>) {
    TEST_PREFERENCES.with(|slot| *slot.borrow_mut() = Some(prefs));
}

#[cfg(test)]
fn test_preferences_override() -> Option<Option<Preferences>> {
    TEST_PREFERENCES.with(|slot| slot.borrow().clone())
}

#[cfg(not(test))]
fn test_preferences_override() -> Option<Option<Preferences>> {
    None
}

fn load_startup_preferences() -> std::io::Result<Preferences> {
    if let Some(override_prefs) = test_preferences_override() {
        // "No saved preferences" is the same state as a missing file, not an I/O
        // error, so tests keep the normal defaults.
        return Ok(override_prefs.unwrap_or_default());
    }
    preferences::path().and_then(|path| preferences::load(&path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ProbeKind;
    use crate::text::ipv6_display;
    use serde_json::json;
    use std::sync::Mutex as StdMutex;

    /// Builds a service with deterministic, side-effect-free startup state: the
    /// `for_test` options plus a thread-local override for the encrypted
    /// preferences file. Nothing here starts a session or touches the network.
    fn service_with_preferences(
        prefs: Option<Preferences>,
        settings: Option<ClientConfig>,
    ) -> Service {
        set_test_preferences(prefs);
        let mut options = ServiceOptions::for_test();
        if let Some(settings) = settings {
            options.settings = Some(settings);
        }
        Service::new(options)
    }

    fn recording_sink() -> (Sink, Arc<StdMutex<Vec<Notification>>>) {
        let received = Arc::new(StdMutex::new(Vec::new()));
        let sink: Sink = {
            let received = Arc::clone(&received);
            Arc::new(move |notification| {
                received.lock().unwrap().push(notification);
            })
        };
        (sink, received)
    }

    fn default_prefs() -> Preferences {
        Preferences::default()
    }

    #[test]
    fn external_session_probe_cancels_pending_auto_login_without_starting_our_session() {
        let mut shared = Shared {
            page_probe_pending: true,
            pending_auto: Some((
                Preferences::default(),
                Instant::now() + Duration::from_secs(90),
            )),
            ..Default::default()
        };

        let outcome = reconcile_page_probe_state(&mut shared, CampusPage::CurrentSession);

        assert!(!outcome.app_session_active);
        assert!(outcome.canceled_auto_login);
        assert!(!shared.page_probe_pending);
        assert!(shared.pending_auto.is_none());
        assert!(shared.stop.is_none());
        assert_eq!(
            page_probe_status(
                CampusPage::CurrentSession,
                false,
                outcome.app_session_active
            ),
            "网页显示已有认证（外部会话）；此程序未建立 UDP 保活"
        );
    }

    #[test]
    fn late_page_probe_preserves_an_active_manual_udp_session() {
        let stop = Arc::new(AtomicBool::new(false));
        let mut shared = Shared {
            stop: Some(Arc::clone(&stop)),
            connected_since: Some(Instant::now()),
            page_probe_pending: true,
            ..Shared::default()
        };

        let outcome = reconcile_page_probe_state(&mut shared, CampusPage::CurrentSession);

        assert!(outcome.app_session_active);
        assert!(!outcome.canceled_auto_login);
        assert!(!shared.page_probe_pending);
        assert!(Arc::ptr_eq(shared.stop.as_ref().unwrap(), &stop));
        assert!(!stop.load(Ordering::Relaxed));
        assert!(shared.connected_since.is_some());
        assert_eq!(
            page_probe_status(
                CampusPage::CurrentSession,
                false,
                outcome.app_session_active
            ),
            "此程序会话由左上角连接状态和 UDP 保活管理"
        );
    }

    #[test]
    fn login_form_probe_does_not_cancel_pending_auto_login() {
        let mut shared = Shared::default();
        let prefs = Preferences {
            auto_login: true,
            username: "test-user".into(),
            password: "test-pass".into(),
            ..Default::default()
        };
        shared.pending_auto = Some((prefs, Instant::now() + Duration::from_secs(90)));

        let outcome = reconcile_page_probe_state(&mut shared, CampusPage::LoginForm);

        assert!(!outcome.app_session_active);
        assert!(!outcome.canceled_auto_login);
        assert!(shared.pending_auto.is_some());
    }

    #[test]
    fn closing_to_tray_is_on_by_default_like_the_c_sharp_client() {
        assert!(Shared::default().minimize_to_tray);
        assert!(Preferences::default().minimize_to_tray);
    }

    #[test]
    fn initial_snapshot_has_the_old_window_defaults() {
        let service = service_with_preferences(Some(default_prefs()), None);
        let snapshot = service.snapshot();

        assert_eq!(snapshot.api_version, API_VERSION);
        assert_eq!(snapshot.app_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(snapshot.connection.phase, ConnectionPhase::Disconnected);
        assert_eq!(snapshot.connection.state_text, "未连接");
        assert!(!snapshot.connection.online);
        assert!(!snapshot.connection.session_active);
        assert_eq!(snapshot.connection.online_seconds, None);
        assert_eq!(snapshot.connection.online_duration, "--");
        // The adapter panel is initialised from the real machine, but it must
        // always have a name and never claim availability without one.
        assert!(!snapshot.adapter.name.is_empty());
        assert_eq!(snapshot.adapter.available, snapshot.adapter.ipv4 != "--");
        assert_eq!(snapshot.failure, initial_failure());
        assert_eq!(snapshot.probe, initial_probe());
        assert!(!snapshot.network_editor.visible);
        assert!(!snapshot.network_editor.busy);
        assert!(snapshot.network_editor.message.is_empty());
        assert!(!snapshot.settings.auto_login);
        assert!(snapshot.settings.auto_reconnect);
        assert!(snapshot.settings.minimize_to_tray);
        assert!(snapshot.campus_page.is_empty());
        assert!(snapshot.tooltip.is_empty());
    }

    #[test]
    fn snapshot_serialises_without_the_password() {
        let prefs = Preferences {
            username: "snapshot-account-marker".into(),
            password: "snapshot-password-marker".into(),
            auto_login: true,
            ..Default::default()
        };
        let service = service_with_preferences(Some(prefs), None);
        let encoded = serde_json::to_string(&service.snapshot()).unwrap();
        assert!(
            !encoded.contains("snapshot-password-marker"),
            "the password must never be part of a snapshot: {encoded}"
        );
        assert!(
            !encoded.contains("\"password\""),
            "snapshots have no password field: {encoded}"
        );
        // The remembered account is a display value and is expected to be there.
        assert!(encoded.contains("snapshot-account-marker"));
    }

    #[test]
    fn diagnostics_text_is_useful_and_never_leaks_credentials() {
        let prefs = Preferences {
            username: "diag-account-marker".into(),
            password: "diag-password-marker".into(),
            auto_login: true,
            ..Default::default()
        };
        let service = service_with_preferences(Some(prefs), None);
        let report = match service.execute(Command::DiagnosticsText) {
            CommandOutcome::Text(text) => text,
            CommandOutcome::Snapshot(_) => panic!("DiagnosticsText must return text"),
        };
        assert!(report.contains("DrCom 校园网助手"));
        assert!(report.contains("当前状态：未连接"));
        assert!(report.contains("本次运行的诊断摘要"));
        assert!(
            !report.contains("diag-password-marker"),
            "diagnostics must not leak the password"
        );
        assert!(
            !report.contains("diag-account-marker"),
            "diagnostics must not leak the account"
        );
    }

    #[test]
    fn subscribe_sends_the_current_snapshot_before_registering() {
        let service = service_with_preferences(Some(default_prefs()), None);
        let (sink, received) = recording_sink();
        service.subscribe(sink);
        let received = received.lock().unwrap();
        assert_eq!(received.len(), 1);
        match &received[0] {
            Notification::State { snapshot } => {
                assert_eq!(snapshot.api_version, service.snapshot().api_version);
                assert_eq!(snapshot.connection.state_text, "未连接");
            }
            other => panic!("expected an initial state notification, got {other:?}"),
        }
    }

    #[test]
    fn shutdown_when_idle_sends_exit_ready() {
        let service = service_with_preferences(Some(default_prefs()), None);
        let (sink, received) = recording_sink();
        service.subscribe(sink);
        let outcome = service.execute(Command::Shutdown);
        assert!(matches!(outcome, CommandOutcome::Snapshot(_)));
        let received = received.lock().unwrap();
        assert!(
            matches!(received.last(), Some(Notification::ExitReady)),
            "the last notification must release the frontend: {received:?}"
        );
        assert!(!service.snapshot().connection.session_active);
    }

    #[test]
    fn logout_without_a_session_reports_it_instead_of_blocking() {
        let service = service_with_preferences(Some(default_prefs()), None);
        let snapshot = match service.execute(Command::Logout) {
            CommandOutcome::Snapshot(snapshot) => snapshot,
            CommandOutcome::Text(_) => panic!("Logout must return a snapshot"),
        };
        assert_eq!(snapshot.connection.state_text, "未连接");
        assert_eq!(snapshot.connection.detail, "当前没有进行中的连接");
    }

    #[test]
    fn login_with_empty_credentials_never_starts_a_session() {
        let service = service_with_preferences(Some(default_prefs()), None);
        let snapshot = match service.execute(Command::Login {
            username: "  ".into(),
            password: "secret".into(),
            auto_reconnect: true,
        }) {
            CommandOutcome::Snapshot(snapshot) => snapshot,
            CommandOutcome::Text(_) => panic!("Login must return a snapshot"),
        };
        assert_eq!(snapshot.connection.state_text, "未连接");
        assert_eq!(snapshot.connection.detail, "请先填写账号和密码");
        assert!(!snapshot.connection.session_active);

        let snapshot = match service.execute(Command::Login {
            username: "someone".into(),
            password: String::new(),
            auto_reconnect: false,
        }) {
            CommandOutcome::Snapshot(snapshot) => snapshot,
            CommandOutcome::Text(_) => panic!("Login must return a snapshot"),
        };
        assert_eq!(snapshot.connection.detail, "请先填写账号和密码");
        assert!(!snapshot.connection.session_active);
    }

    #[test]
    fn refresh_adapter_updates_the_panel_and_old_detail_text() {
        let service = service_with_preferences(Some(default_prefs()), None);
        let snapshot = match service.execute(Command::RefreshAdapter) {
            CommandOutcome::Snapshot(snapshot) => snapshot,
            CommandOutcome::Text(_) => panic!("RefreshAdapter must return a snapshot"),
        };
        if snapshot.adapter.available {
            assert_eq!(
                snapshot.connection.detail,
                "网络已就绪，填写账号密码后即可连接"
            );
            assert_ne!(snapshot.adapter.mac, "--");
        } else {
            assert_eq!(
                snapshot.connection.detail,
                "没有找到可用的网络连接，请插好网线或连接校园 Wi-Fi 后刷新"
            );
            assert_eq!(snapshot.adapter.name, "未检测到物理网卡");
        }
    }

    #[test]
    fn applying_an_invalid_static_ipv4_reports_validation_without_elevation() {
        let service = service_with_preferences(Some(default_prefs()), None);
        let snapshot = match service.execute(Command::ApplyNetwork {
            ip: "not-an-address".into(),
            mask: "255.255.255.0".into(),
            gateway: "10.0.0.1".into(),
            dns: "1.1.1.1".into(),
        }) {
            CommandOutcome::Snapshot(snapshot) => snapshot,
            CommandOutcome::Text(_) => panic!("ApplyNetwork must return a snapshot"),
        };
        assert_eq!(snapshot.network_editor.message, "IPv4 地址 格式不正确");
        assert!(!snapshot.network_editor.busy);
    }

    #[test]
    fn set_minimize_to_tray_applies_live() {
        let service = service_with_preferences(Some(default_prefs()), None);
        let snapshot = match service.execute(Command::SetMinimizeToTray { enabled: false }) {
            CommandOutcome::Snapshot(snapshot) => snapshot,
            CommandOutcome::Text(_) => panic!("SetMinimizeToTray must return a snapshot"),
        };
        assert!(!snapshot.settings.minimize_to_tray);
    }

    #[test]
    fn form_seed_merges_ini_and_remembered_preferences_like_the_old_window() {
        // Only drcom.ini knows the account: both fields come from it.
        let ini = ClientConfig {
            account: "ini-account".into(),
            password: "ini-password".into(),
            ..Default::default()
        };
        let service = service_with_preferences(None, Some(ini.clone()));
        let seed = service.form_seed();
        assert_eq!(seed.username, "ini-account");
        assert_eq!(seed.password.as_deref(), Some("ini-password"));
        assert!(!seed.auto_login);
        assert!(seed.auto_reconnect);

        // Preferences override the account and password.
        let prefs = Preferences {
            username: "saved-account".into(),
            password: "saved-password".into(),
            auto_login: true,
            auto_reconnect: false,
            minimize_to_tray: false,
            ..Default::default()
        };
        let service = service_with_preferences(Some(prefs), Some(ini.clone()));
        let seed = service.form_seed();
        assert_eq!(seed.username, "saved-account");
        assert_eq!(seed.password.as_deref(), Some("saved-password"));
        assert!(seed.auto_login);
        assert!(!seed.auto_reconnect);
        assert!(!seed.minimize_to_tray);
        assert_eq!(seed.config_status.as_deref(), Some("已记住你的账号"));

        // A cleared password wins over the drcom.ini password.
        let cleared = Preferences {
            username: "saved-account".into(),
            password_cleared: true,
            ..Default::default()
        };
        let service = service_with_preferences(Some(cleared), Some(ini));
        assert_eq!(service.form_seed().password.as_deref(), Some(""));
    }

    #[test]
    fn command_and_notification_json_tags_are_stable_for_ipc() {
        assert_eq!(
            serde_json::to_value(Command::Login {
                username: "u".into(),
                password: "p".into(),
                auto_reconnect: true,
            })
            .unwrap(),
            json!({"command": "login", "username": "u", "password": "p", "auto_reconnect": true})
        );
        assert_eq!(
            serde_json::to_value(Command::Logout).unwrap(),
            json!({"command": "logout"})
        );
        assert_eq!(
            serde_json::to_value(Command::DiagnosticsText).unwrap(),
            json!({"command": "diagnostics_text"})
        );
        assert_eq!(
            serde_json::to_value(Command::Probe {
                kind: ProbeKind::All
            })
            .unwrap(),
            json!({"command": "probe", "kind": "all"})
        );
        assert_eq!(
            serde_json::to_value(Notification::ExitReady).unwrap(),
            json!({"event": "exit_ready"})
        );
        assert_eq!(
            serde_json::to_value(Notification::HideToTray {
                note: "已最小化到托盘".into()
            })
            .unwrap(),
            json!({"event": "hide_to_tray", "note": "已最小化到托盘"})
        );
        assert_eq!(
            serde_json::to_value(ConnectionPhase::WaitingForNetwork).unwrap(),
            json!("waiting_for_network")
        );

        let service = service_with_preferences(Some(default_prefs()), None);
        let notification = Notification::State {
            snapshot: service.snapshot(),
        };
        let encoded = serde_json::to_string(&notification).unwrap();
        assert!(encoded.contains("\"event\":\"state\""));
        let decoded: Notification = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, notification);
    }

    #[test]
    fn probe_results_are_truncated_before_the_separator_in_the_snapshot() {
        // The probe threads are not started here: this pins the documented shape
        // of the display fields (the part before "｜") without touching the network.
        let shared = Shared::default();
        let snapshot = build_snapshot(&shared, &ClientConfig::default());
        assert_eq!(snapshot.probe.ipv4, "未检测");
        assert_eq!(snapshot.probe.ipv6, "未检测");
        assert_eq!(ipv6_display(&[]), "--");
        // And the failure card never borrows state from an earlier attempt.
        assert_eq!(snapshot.failure.message, "本次运行尚未发生连接问题");
    }

    #[test]
    fn a_tick_without_a_session_does_not_publish_anything() {
        let service = service_with_preferences(Some(default_prefs()), None);
        let (sink, received) = recording_sink();
        service.subscribe(sink);
        received.lock().unwrap().clear();
        service.tick();
        assert!(received.lock().unwrap().is_empty());
    }

    #[test]
    fn hide_to_tray_is_notified_when_the_auto_hide_is_due() {
        let service = service_with_preferences(Some(default_prefs()), None);
        let (sink, received) = recording_sink();
        service.subscribe(sink);
        received.lock().unwrap().clear();
        service.with_state(|shared| {
            shared.connected_since = Some(Instant::now());
            shared.online_duration = "0 秒".to_string();
            shared.connection_state = "已连接".to_string();
            shared.hide_at = Some(Instant::now());
        });
        service.tick();
        let received = received.lock().unwrap();
        assert!(
            received.iter().any(|notification| matches!(
                notification,
                Notification::HideToTray { note } if note == "登录成功，已在托盘运行"
            )),
            "auto-hide must be a notification: {received:?}"
        );
        assert!(service.snapshot().connection.online);
        assert!(service.with_state(|shared| shared.hide_at.is_none()));
    }

    #[test]
    fn online_ticks_republish_the_duration_even_without_a_new_event() {
        let service = service_with_preferences(Some(default_prefs()), None);
        service.with_state(|shared| {
            shared.connected_since = Some(Instant::now());
            shared.connection_state = "已连接".to_string();
            shared.status_detail = "连接正常，正在保持在线".to_string();
        });
        let (sink, received) = recording_sink();
        service.subscribe(sink);
        received.lock().unwrap().clear();
        service.tick();
        let received = received.lock().unwrap();
        let last = received.last().expect("online ticks must publish");
        match last {
            Notification::State { snapshot } => {
                assert!(snapshot.connection.online);
                assert_eq!(snapshot.connection.online_duration, "0 秒");
            }
            other => panic!("expected a state notification, got {other:?}"),
        }
        assert_eq!(
            service.snapshot().connection.online_duration,
            "0 秒",
            "the snapshot and the pushed state must agree"
        );
    }

    #[test]
    fn shutdown_waits_for_the_network_editor_thread_before_exit_ready() {
        let service = service_with_preferences(Some(default_prefs()), None);
        let (sink, received) = recording_sink();
        service.subscribe(sink);
        received.lock().unwrap().clear();
        // Pretend the elevated apply is in flight: the frontend must not exit
        // before that operation has finished.
        service.with_state(|shared| shared.editor.busy = true);
        service.execute(Command::Shutdown);
        assert_eq!(
            service.snapshot().connection.detail,
            "等待网络设置操作完成后退出…"
        );
        assert!(
            !received
                .lock()
                .unwrap()
                .iter()
                .any(|notification| matches!(notification, Notification::ExitReady))
        );
        // This is what the apply thread does when it finishes.
        service.with_state(|shared| shared.editor.busy = false);
        service.maybe_emit_exit_ready();
        assert!(matches!(
            received.lock().unwrap().last(),
            Some(Notification::ExitReady)
        ));
    }

    #[test]
    fn saving_incomplete_automatic_login_preferences_is_rejected_before_writing() {
        let service = service_with_preferences(Some(default_prefs()), None);
        let snapshot = match service.execute(Command::SaveSettings {
            username: String::new(),
            password: String::new(),
            auto_login: true,
            auto_reconnect: true,
            autostart: false,
            minimize_to_tray: true,
        }) {
            CommandOutcome::Snapshot(snapshot) => snapshot,
            CommandOutcome::Text(_) => panic!("SaveSettings must return a snapshot"),
        };
        assert_eq!(
            snapshot.settings.config_status.as_deref(),
            Some("保存未完成")
        );
        assert_eq!(snapshot.connection.detail, "缺少 account（账号）");
    }
}
