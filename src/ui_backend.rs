//! Glue between the Slint window and the authentication session.
//!
//! The window runs on Slint's event loop thread, while a session blocks on
//! socket timeouts, so the session lives on its own thread and every property
//! update is marshalled back with `invoke_from_event_loop`. Shared state is
//! limited to the stop flag and the connected-since timestamp, which is what
//! "断开" and the online-duration label need.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use slint::ComponentHandle;

use drcom_campus::adapter;
use drcom_campus::config::ClientConfig;
use drcom_campus::event_log;
use drcom_campus::preferences::{self, Preferences};
use drcom_campus::session::{
    LoginSession, SessionConfig, SessionError, SessionEvent, SessionPhase,
};

use crate::{AppTray, AppWindow};

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

/// Delay between automatic reconnect attempts.
///
/// The reference client re-authenticates on a fixed schedule, which for a link
/// that is down for an afternoon means a login frame every few seconds for hours
/// — pointless, and hard on an authentication server that is shared by a whole
/// building. Doubling up to a ceiling keeps the first recovery quick without
/// letting a long outage turn into a flood.
struct ReconnectBackoff {
    next: Duration,
}

impl ReconnectBackoff {
    const FIRST: Duration = Duration::from_secs(5);
    const CEILING: Duration = Duration::from_secs(120);

    fn new() -> Self {
        Self { next: Self::FIRST }
    }

    /// Back to the first step, after a session that actually held.
    fn reset(&mut self) {
        self.next = Self::FIRST;
    }

    /// The delay to use now, and the longer one after it.
    fn next_delay(&mut self) -> Duration {
        let current = self.next;
        self.next = (current * 2).min(Self::CEILING);
        current
    }
}

struct Shared {
    stop: Option<Arc<AtomicBool>>,
    connected_since: Option<Instant>,
    pending_auto: Option<(Preferences, Instant)>,
    /// When the window should tuck itself away after a successful login. The C#
    /// client waits three seconds so the user actually sees the success state
    /// before the window disappears; hiding on the same frame as the reply reads
    /// as a crash.
    hide_at: Option<Instant>,
    /// Mirror of the "关闭到托盘" option for the session thread, which cannot read
    /// the window.
    minimize_to_tray: bool,
    quitting: bool,
    probing: bool,
    edit_adapter: Option<adapter::AdapterInfo>,
    network_busy: bool,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            stop: None,
            connected_since: None,
            pending_auto: None,
            hide_at: None,
            // Same default as the C# client's `MinimizeToTray`.
            minimize_to_tray: true,
            quitting: false,
            probing: false,
            edit_adapter: None,
            network_busy: false,
        }
    }
}

pub struct Backend {
    window: slint::Weak<AppWindow>,
    /// The tray icon is a separate Slint component, so the only way to keep its
    /// tooltip in step with the session is to hold a handle to it here.
    tray: slint::Weak<AppTray>,
    shared: Arc<Mutex<Shared>>,
    /// Settings read from `drcom.ini` at startup. The prototype had no place to
    /// keep them, which is why the server list had to come from an environment
    /// variable that a GUI launch never has.
    settings: Arc<ClientConfig>,
}

impl Backend {
    /// Wires the window callbacks, fills the adapter panel, and seeds the login
    /// form from the configuration file.
    pub fn attach(window: &AppWindow, tray: &AppTray) -> Self {
        let (settings, complaint) = match load_settings() {
            Ok(settings) => (settings, None),
            Err(message) => (ClientConfig::default(), Some(message)),
        };
        if !settings.account.is_empty() {
            window.set_username(settings.account.clone().into());
            window.set_password(settings.password.clone().into());
        }
        let backend = Self {
            window: window.as_weak(),
            tray: tray.as_weak(),
            shared: Arc::new(Mutex::new(Shared::default())),
            settings: Arc::new(settings),
        };
        event_log::record(&format!(
            "启动 DrCom Rust {}，配置文件：{}",
            env!("CARGO_PKG_VERSION"),
            match drcom_campus::config::discover() {
                Some(path) => path.display().to_string(),
                None => "无（使用内置认证服务器）".to_string(),
            }
        ));
        match preferences::path().and_then(|path| preferences::load(&path)) {
            Ok(prefs) => {
                if !prefs.username.is_empty() {
                    window.set_config_status("已加载加密设置".into());
                    window.set_username(prefs.username.clone().into());
                    window.set_password(prefs.password.clone().into());
                }
                window.set_auto_login(prefs.auto_login);
                window.set_auto_reconnect(prefs.auto_reconnect);
                window.set_minimize_to_tray(prefs.minimize_to_tray);
                backend.shared.lock().unwrap().minimize_to_tray = prefs.minimize_to_tray;
                if complaint.is_none()
                    && prefs.auto_login
                    && !prefs.username.is_empty()
                    && !prefs.password.is_empty()
                {
                    backend.shared.lock().unwrap().pending_auto =
                        Some((prefs, Instant::now() + Duration::from_secs(90)));
                }
            }
            Err(_) => window.set_config_status("读取失败，请重新保存账号".into()),
        }
        match preferences::autostart::enabled() {
            Ok(enabled) => window.set_start_with_windows(enabled),
            Err(_) => window.set_config_status("无法读取开机启动状态".into()),
        }
        backend.refresh_adapter(None);
        if let Some(complaint) = complaint {
            backend.set_status(&complaint);
        }

        let connect = backend.clone_handle();
        window.on_connect_requested(move |username, password, auto_reconnect| {
            connect.start(username.to_string(), password.to_string(), auto_reconnect);
        });

        let disconnect = backend.clone_handle();
        window.on_disconnect_requested(move || disconnect.stop());

        let refresh = backend.clone_handle();
        window.on_refresh_requested(move || refresh.refresh_adapter(None));
        let probe = backend.clone_handle();
        window.on_probe_requested(move |kind| probe.probe(kind));
        let network = backend.clone_handle();
        window.on_network_settings_requested(move || network.open_network_editor());
        let apply = backend.clone_handle();
        window.on_apply_network_requested(move |ip, mask, gateway, dns| {
            apply.apply_network(&ip, &mask, &gateway, &dns)
        });
        let system = backend.clone_handle();
        window.on_system_network_requested(move || {
            if let Err(error) = crate::diagnostics::open_network_settings() {
                system.set_status(&format!("无法打开网络设置：{error}"));
            }
        });
        let copy = backend.clone_handle();
        window.on_copy_requested(move || {
            if let Some(window) = copy.window.upgrade() {
                match crate::diagnostics::copy(&diagnostics_report(&window)) {
                    Ok(()) => copy.set_status("诊断已复制（不包含账号密码）"),
                    Err(error) => copy.set_status(&format!("复制失败：{error}")),
                }
            }
        });

        let save = backend.clone_handle();
        window.on_save_requested(
            move |username, password, auto_login, auto_reconnect, autostart, minimize_to_tray| {
                let prefs = Preferences {
                    username: username.trim().to_string(),
                    password: password.to_string(),
                    auto_login,
                    auto_reconnect,
                    minimize_to_tray,
                };
                let result = (|| -> Result<(), String> {
                    validate_preferences(&prefs, save.settings.servers_or_env())?;
                    let path = preferences::path().map_err(|e| e.to_string())?;
                    preferences::save(&path, &prefs).map_err(|e| format!("保存失败：{e}"))?;
                    preferences::autostart::set(autostart)
                        .map_err(|e| format!("账号已保存，但开机启动设置失败：{e}"))?;
                    Ok(())
                })();
                // The close button reads this option live, so it takes effect
                // immediately rather than at the next launch like auto-login.
                if let Ok(mut shared) = save.shared.lock() {
                    shared.minimize_to_tray = minimize_to_tray;
                    if !minimize_to_tray {
                        shared.hide_at = None;
                    }
                }
                if let Some(window) = save.window.upgrade() {
                    window.set_config_status(
                        if result.is_ok() {
                            "已加密保存"
                        } else {
                            "保存未完成"
                        }
                        .into(),
                    );
                    if let Ok(enabled) = preferences::autostart::enabled() {
                        window.set_start_with_windows(enabled);
                    }
                }
                save.set_status(&match result {
                    Ok(()) => "设置已保存（自动登录在下次启动时生效）".into(),
                    Err(e) => e,
                });
            },
        );

        let export = backend.clone_handle();
        window.on_export_requested(move || export.export_diagnostics());
        backend
    }

    pub fn tick(&self) {
        let mut shared = self.shared.lock().unwrap();
        if shared.quitting {
            return;
        }
        if let Some(start) = shared.connected_since
            && let Some(window) = self.window.upgrade()
            && window.window().is_visible()
        {
            window.set_duration(format_duration(start.elapsed()).into());
        }
        // The hide-after-login delay runs off this one-second timer rather than a
        // second timer of its own.
        if let Some(due) = shared.hide_at
            && Instant::now() >= due
        {
            shared.hide_at = None;
            drop(shared);
            self.hide_to_tray("登录成功，已在托盘运行");
            return;
        }
        if let Some((prefs, deadline)) = shared.pending_auto.clone() {
            if Instant::now() > deadline {
                shared.pending_auto = None;
                drop(shared);
                self.set_connection("未连接", "自动登录等待网卡超时，请连接网络后手动连接");
            } else if adapter::select_for(self.settings.servers_or_env().first().copied()).is_some()
            {
                shared.pending_auto = None;
                drop(shared);
                self.start(prefs.username, prefs.password, prefs.auto_reconnect);
            } else {
                drop(shared);
                self.set_connection("等待网络", "自动登录正在等待可用网卡…");
            }
        }
    }

    fn probe(&self, kind: i32) {
        {
            let mut shared = self.shared.lock().unwrap();
            if shared.probing || shared.quitting {
                return;
            }
            shared.probing = true;
        }
        if let Some(window) = self.window.upgrade() {
            window.set_probe_busy(true);
        }
        let backend = self.clone_handle();
        let server = self.settings.servers_or_env()[0];
        thread::spawn(move || {
            let campus = (kind == 0 || kind == 2).then(|| crate::diagnostics::campus_probe(server));
            let ipv4 = (kind == 1 || kind == 2)
                .then(|| crate::diagnostics::internet_probe(crate::diagnostics::Family::V4));
            let ipv6 = (kind == 2)
                .then(|| crate::diagnostics::internet_probe(crate::diagnostics::Family::V6));
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
            backend.shared.lock().unwrap().probing = false;
            let window = backend.window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = window.upgrade() {
                    if let Some(result) = campus {
                        window.set_campus_result(result.into());
                    }
                    if let Some(result) = ipv4 {
                        window.set_ipv4_result(result.into());
                    }
                    if let Some(result) = ipv6 {
                        window.set_ipv6_result(result.into());
                    }
                    window.set_probe_busy(false);
                }
            });
        });
    }

    pub fn quit(&self) {
        let mut shared = self.shared.lock().unwrap();
        shared.quitting = true;
        shared.pending_auto = None;
        shared.hide_at = None;
        event_log::record("请求退出");
        if shared.network_busy {
            drop(shared);
            self.set_status("等待网络设置操作完成后退出…");
            return;
        }
        if let Some(stop) = &shared.stop {
            stop.store(true, Ordering::Relaxed);
            drop(shared);
            self.set_connection("断开中", "正在下线，完成后退出…");
        } else {
            let _ = slint::quit_event_loop();
        }
    }

    fn open_network_editor(&self) {
        let mut shared = self.shared.lock().unwrap();
        if shared.stop.is_some() || shared.quitting || shared.network_busy {
            return;
        }
        let Some(found) = adapter::select_for(self.settings.servers_or_env().first().copied())
        else {
            drop(shared);
            self.set_status("未找到可配置的物理网卡");
            return;
        };
        shared.pending_auto = None;
        if let Some(window) = self.window.upgrade() {
            window.set_edit_adapter_name(found.friendly_name.clone().into());
            window.set_edit_ip(found.ipv4.to_string().into());
            window.set_edit_mask(
                found
                    .details
                    .subnet_mask
                    .map(|ip| ip.to_string())
                    .unwrap_or_default()
                    .into(),
            );
            window.set_edit_gateway(
                found
                    .details
                    .gateways
                    .iter()
                    .find(|ip| ip.is_ipv4())
                    .map(ToString::to_string)
                    .unwrap_or_default()
                    .into(),
            );
            window.set_edit_dns(
                found
                    .details
                    .dns
                    .iter()
                    .find(|ip| ip.is_ipv4())
                    .map(ToString::to_string)
                    .unwrap_or_default()
                    .into(),
            );
            window
                .set_network_message("应用时会请求管理员权限，并可能短暂中断此网卡的连接。".into());
            window.set_network_editor_visible(true);
        }
        shared.edit_adapter = Some(found);
    }

    fn apply_network(&self, ip: &str, mask: &str, gateway: &str, dns: &str) {
        let config = match crate::static_ipv4::Configuration::parse(ip, mask, gateway, dns) {
            Ok(config) => config,
            Err(error) => {
                if let Some(window) = self.window.upgrade() {
                    window.set_network_message(error.into());
                }
                return;
            }
        };
        let mut shared = self.shared.lock().unwrap();
        if shared.network_busy || shared.stop.is_some() || shared.quitting {
            return;
        }
        let Some(found) = shared.edit_adapter.clone() else {
            return;
        };
        // Confirm that the selected physical adapter still exists before elevation.
        if !adapter::list().iter().any(|item| {
            item.interface_index == found.interface_index
                && item.mac == found.mac
                && item.friendly_name == found.friendly_name
        }) {
            if let Some(window) = self.window.upgrade() {
                window.set_network_message("网卡已变化，请返回后重新打开网络设置".into());
            }
            return;
        }
        shared.network_busy = true;
        drop(shared);
        if let Some(window) = self.window.upgrade() {
            window.set_network_busy(true);
            window.set_network_message("等待管理员授权与应用结果…".into());
        }
        let backend = self.clone_handle();
        thread::spawn(move || {
            let result = config.apply(&found.friendly_name);
            let quitting = {
                let mut shared = backend.shared.lock().unwrap();
                shared.network_busy = false;
                shared.quitting
            };
            backend.refresh_adapter(None);
            let window = backend.window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = window.upgrade() {
                    window.set_network_busy(false);
                    window.set_network_message(match result {
                        Ok(()) => "IPv4 设置已应用，可返回主界面连接".into(),
                        Err(error) => error.into(),
                    });
                }
                if quitting {
                    let _ = slint::quit_event_loop();
                }
            });
        });
    }

    fn clone_handle(&self) -> Self {
        Self {
            window: self.window.clone(),
            tray: self.tray.clone(),
            shared: Arc::clone(&self.shared),
            settings: Arc::clone(&self.settings),
        }
    }

    fn set_status(&self, text: &str) {
        let window = self.window.clone();
        let text = text.to_string();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = window.upgrade() {
                window.set_status_detail(text.into());
            }
        });
    }

    fn set_connection(&self, state: &str, detail: &str) {
        let window = self.window.clone();
        let state = state.to_string();
        let detail = detail.to_string();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = window.upgrade() {
                window.set_online(state == "已连接");
                window.set_session_active(state != "未连接");
                window.set_connection_state(state.into());
                window.set_status_detail(detail.into());
            }
        });
    }

    /// Sends the window to the tray and says so in the tooltip, which is the only
    /// part of the program still visible once the window is hidden.
    fn hide_to_tray(&self, note: &str) {
        self.set_tooltip_text(note);
        let window = self.window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = window.upgrade() {
                let _ = window.hide();
            }
        });
    }

    /// Handles the window's close button. Returns `true` when the window should
    /// merely be hidden, and `false` when the caller should start shutting down.
    ///
    /// This mirrors the C# client's `OnClosing`: with 关闭到托盘 on, closing hides
    /// the window; with it off, closing really exits. The option is read live from
    /// the checkbox rather than from the saved preferences, so it applies to the
    /// running session without a restart.
    pub fn close_to_tray(&self) -> bool {
        let minimize = self
            .window
            .upgrade()
            .map(|window| window.get_minimize_to_tray())
            .unwrap_or(true);
        if let Ok(mut shared) = self.shared.lock() {
            shared.minimize_to_tray = minimize;
            shared.hide_at = None;
        }
        if minimize {
            event_log::record("关闭窗口：按设置最小化到托盘");
            self.hide_to_tray("已最小化到托盘");
        } else {
            event_log::record("关闭窗口：按设置直接退出");
        }
        minimize
    }

    /// Updates the tray hover text, which stays readable while the window is hidden.
    fn set_tooltip_text(&self, note: &str) {
        // Shell tooltips clip a long string without any ellipsis, so the useful
        // part has to be the front of it.
        let note: String = note.chars().take(48).collect();
        let tray = self.tray.clone();
        let text = format!("DrCom 校园网助手 - {note}");
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(tray) = tray.upgrade() {
                tray.set_tray_tooltip(text.into());
            }
        });
    }

    /// Sleeps in short steps, so a stop request is honoured within [`SLEEP_STEP`]
    /// rather than after the whole delay. Returns `false` once stopping has been
    /// requested.
    fn sleep_interruptibly(delay: Duration, stop: &AtomicBool) -> bool {
        let mut remaining = delay;
        while !remaining.is_zero() {
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            let step = remaining.min(SLEEP_STEP);
            thread::sleep(step);
            remaining -= step;
        }
        !stop.load(Ordering::Relaxed)
    }

    /// Writes the diagnostics text and the tail of the event log to one file, then
    /// reveals it in Explorer. Copying to the clipboard covers pasting into a chat;
    /// a file is what gets attached to a report.
    fn export_diagnostics(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        match crate::diagnostics::export(&diagnostics_report(&window)) {
            Ok(path) => {
                event_log::record(&format!("导出诊断文件：{}", path.display()));
                self.set_status(&format!("诊断已导出：{}", path.display()));
            }
            Err(error) => self.set_status(&format!("导出诊断失败：{error}")),
        }
    }

    fn refresh_adapter(&self, note: Option<&str>) {
        let selected = adapter::select_for(self.settings.servers_or_env().first().copied());
        let window = self.window.clone();
        let note = note.map(str::to_string);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(window) = window.upgrade() else {
                return;
            };
            match selected {
                Some(found) => {
                    show_network_details(&window, &found);
                    window.set_adapter_name(found.friendly_name.clone().into());
                    window.set_ipv4_address(found.ipv4.to_string().into());
                    window.set_mac_address(format_mac(found.mac).into());
                    window.set_status_detail(
                        note.unwrap_or_else(|| format!("网卡 {} 已就绪", found.description))
                            .into(),
                    );
                }
                None => {
                    window.set_adapter_name("未检测到物理网卡".into());
                    window.set_ipv4_address("--".into());
                    window.set_mac_address("--".into());
                    window.set_gateway("--".into());
                    window.set_dns("--".into());
                    window.set_ipv6("--".into());
                    window.set_status_detail(
                        note.unwrap_or_else(|| {
                            "未检测到可用的物理有线或无线网卡，已排除虚拟网卡".to_string()
                        })
                        .into(),
                    );
                }
            }
        });
    }

    fn stop(&self) {
        if let Ok(mut shared) = self.shared.lock()
            && shared.pending_auto.take().is_some()
        {
            drop(shared);
            self.set_connection("未连接", "已取消自动登录");
            return;
        }
        let stop = self
            .shared
            .lock()
            .ok()
            .and_then(|shared| shared.stop.clone());
        match stop {
            Some(stop) => {
                stop.store(true, Ordering::Relaxed);
                self.set_connection("断开中", "正在下线…");
            }
            None => self.set_status("当前没有进行中的连接"),
        }
    }

    fn show_session_adapter(&self, found: &adapter::AdapterInfo) {
        let window = self.window.clone();
        let found = found.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = window.upgrade() {
                show_network_details(&window, &found);
                window.set_adapter_name(found.friendly_name.into());
                window.set_ipv4_address(found.ipv4.to_string().into());
                window.set_mac_address(format_mac(found.mac).into());
            }
        });
    }

    fn start(&self, username: String, password: String, auto_reconnect: bool) {
        if self
            .shared
            .lock()
            .map(|shared| shared.stop.is_some() || shared.quitting)
            .unwrap_or(true)
        {
            self.set_status("连接已在进行中");
            return;
        }
        if username.trim().is_empty() || password.is_empty() {
            self.set_status("请先填写账号和密码");
            return;
        }
        let servers = self.settings.servers_or_env();
        if servers.is_empty() {
            // `servers_or_env` falls back to the built-in campus address, so
            // this branch only fires if that constant is ever emptied.
            self.set_connection("未连接", "没有可用的认证服务器地址");
            return;
        }
        // Resolve the servers first so adapter choice can follow the route the
        // OS would actually use to reach them.
        let selected = match self.settings.local_ip {
            Some(ip) => adapter::list().into_iter().find(|item| item.ipv4 == ip),
            None => adapter::select_for(servers.first().copied()),
        };
        let Some(found) = selected else {
            self.set_connection("未连接", "未检测到可用的物理网卡，已在设置中排除虚拟网卡");
            return;
        };

        let mut config = SessionConfig::new(
            servers,
            username.trim().as_bytes().to_vec(),
            password.into_bytes(),
        );
        config.local_ipv4 = found.ipv4;
        config.mac = found.mac;
        config.hostname = host_name().into_bytes();
        apply_settings(&mut config, &self.settings);
        config.allow_loopback_server = false;
        self.show_session_adapter(&found);
        // The account never reaches the log; the interface and server are what
        // make a stale-log entry useful.
        event_log::record(&format!(
            "开始认证：网卡 {}（{}，{}），服务器 {}",
            found.friendly_name,
            found.description,
            found.ipv4,
            config
                .servers
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));

        let stop = Arc::new(AtomicBool::new(false));
        if let Ok(mut shared) = self.shared.lock() {
            shared.stop = Some(Arc::clone(&stop));
            shared.connected_since = None;
            shared.pending_auto = None;
        }
        self.set_connection("连接中", "正在请求挑战…");

        let backend = self.clone_handle();
        thread::spawn(move || {
            let mut backoff = ReconnectBackoff::new();
            loop {
                let started = Instant::now();
                let result = match LoginSession::new(config.clone()) {
                    Ok(mut session) => {
                        session.use_external_stop(Arc::clone(&stop));
                        session.run(&mut |event| backend.on_event(event))
                    }
                    Err(error) => Err(error),
                };
                let retry = auto_reconnect
                    && !stop.load(Ordering::Relaxed)
                    && result.as_ref().is_err_and(retryable_error);
                if !retry {
                    backend.finish(result);
                    break;
                }
                // A session that stayed up long enough to count as working starts
                // the next wait over. Without this rule a link that flaps every
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
                backend.set_connection("重连中", &format!("连接中断，{seconds} 秒后重新认证…"));
                backend.set_tooltip_text(&format!("{reason}，{seconds} 秒后重试"));
                if let Ok(mut shared) = backend.shared.lock() {
                    shared.connected_since = None;
                    shared.hide_at = None;
                }
                backend.set_online_duration("--");
                if !Backend::sleep_interruptibly(delay, &stop) {
                    backend.finish(Err(drcom_campus::session::SessionError::Stopped));
                    break;
                }
                // Re-read the interface after a link change instead of binding
                // every retry to an address that may no longer exist.
                if let Some(found) = adapter::select_for(config.servers.first().copied()) {
                    config.local_ipv4 = backend.settings.local_ip.unwrap_or(found.ipv4);
                    config.mac = backend.settings.mac.unwrap_or(found.mac);
                    backend.show_session_adapter(&found);
                }
            }
        });
    }

    fn on_event(&self, event: SessionEvent) {
        match event {
            SessionEvent::Phase(phase) => match phase {
                SessionPhase::Challenge => self.set_connection("连接中", "正在请求挑战…"),
                SessionPhase::Authenticating => self.set_connection("连接中", "正在认证…"),
                SessionPhase::Online => {
                    let minimize = if let Ok(mut shared) = self.shared.lock() {
                        let now = Instant::now();
                        shared.connected_since = Some(now);
                        let minimize = shared.minimize_to_tray;
                        shared.hide_at = minimize.then(|| now + HIDE_AFTER_LOGIN);
                        minimize
                    } else {
                        false
                    };
                    event_log::record("认证成功，进入保活");
                    self.set_connection("已连接", "认证成功，正在保活");
                    self.set_online_duration("0 秒");
                    // The window is about to disappear, so the tray has to carry
                    // the news as well.
                    self.set_tooltip_text(if minimize {
                        "已连接，稍后最小化到托盘"
                    } else {
                        "已连接，保活中"
                    });
                }
                SessionPhase::KeepAlive => self.set_connection("已连接", "保活中"),
                SessionPhase::LoggedOut => {
                    event_log::record("已发送下线报文");
                    self.set_connection("未连接", "已下线");
                }
            },
            SessionEvent::Note(note) => {
                event_log::record(&note);
                self.set_status(&note);
            }
            SessionEvent::KeepAliveCycle { sequence, .. } => {
                // One line per cycle: at the reference 20-second interval this is
                // a few kilobytes an hour, and it is the only record that shows a
                // session quietly resynchronising instead of dropping.
                event_log::record(&format!("保活正常，序号 {sequence}"));
                self.set_status(&format!("保活正常，序号 {sequence}"));
                let duration = self
                    .shared
                    .lock()
                    .ok()
                    .and_then(|shared| shared.connected_since.map(|start| start.elapsed()));
                if let Some(duration) = duration {
                    self.set_online_duration(&format_duration(duration));
                }
            }
        }
    }

    fn set_online_duration(&self, text: &str) {
        let window = self.window.clone();
        let text = text.to_string();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = window.upgrade() {
                window.set_duration(text.into());
            }
        });
    }

    fn finish(&self, result: Result<(), drcom_campus::session::SessionError>) {
        let mut quitting = false;
        if let Ok(mut shared) = self.shared.lock() {
            shared.stop = None;
            shared.connected_since = None;
            shared.hide_at = None;
            quitting = shared.quitting;
        }
        self.set_online_duration("--");
        let detail = match result {
            Ok(()) | Err(SessionError::Stopped) => "已下线".to_string(),
            Err(error) => describe_failure(&error),
        };
        event_log::record(&format!("会话结束：{detail}"));
        // By now the window is usually in the tray, where the status line is
        // invisible. Without this the user's only clue is a green dot turning grey.
        self.set_tooltip_text(&detail);
        self.set_connection("未连接", &detail);
        if quitting {
            let _ = slint::quit_event_loop();
        }
    }
}

/// The text that both 复制诊断 and 导出诊断 produce.
///
/// The account and password are deliberately absent: this is the string a user
/// pastes into a public issue or hands to a stranger at the network centre. The
/// tail of the event log is included because "它连不上" is not a report.
fn diagnostics_report(window: &AppWindow) -> String {
    let recent = event_log::tail(40);
    let recent = if recent.is_empty() {
        "（本次运行还没有日志）".to_string()
    } else {
        recent
    };
    format!(
        "DrCom Rust {}\n\
         导出时间：{}\n\
         状态：{}\n\
         网卡：{}\n\
         IPv4：{}\n\
         MAC：{}\n\
         网关：{}\n\
         DNS：{}\n\
         IPv6：{}\n\
         在线时长：{}\n\
         校内：{}\n\
         公网 IPv4：{}\n\
         公网 IPv6：{}\n\
         关闭窗口最小化到托盘：{}\n\
         探测使用系统路由，不等同于认证状态。\n\
         \n\
         认证服务器：{}\n\
         最近日志：\n{}",
        env!("CARGO_PKG_VERSION"),
        event_log::timestamp(),
        window.get_connection_state(),
        window.get_adapter_name(),
        window.get_ipv4_address(),
        window.get_mac_address(),
        window.get_gateway(),
        window.get_dns(),
        window.get_ipv6(),
        window.get_duration(),
        window.get_campus_result(),
        window.get_ipv4_result(),
        window.get_ipv6_result(),
        if window.get_minimize_to_tray() {
            "是"
        } else {
            "否"
        },
        drcom_campus::config::discover()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "无配置文件（使用内置认证服务器）".to_string()),
        recent
    )
}

fn show_network_details(window: &AppWindow, found: &adapter::AdapterInfo) {
    fn addresses(values: &[std::net::IpAddr]) -> slint::SharedString {
        if values.is_empty() {
            "--".into()
        } else {
            values
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
                .into()
        }
    }
    window.set_gateway(addresses(&found.details.gateways));
    window.set_dns(addresses(&found.details.dns));
    window.set_ipv6(ipv6_display(&found.details.ipv6));
}

/// Summarises the adapter's IPv6 addresses down to what the user needs to judge.
///
/// Two things are deliberately not shown verbatim. A link-local address is not
/// routable, and listing `fe80::…` next to a global address makes "IPv6 is
/// running" indistinguishable from "IPv6 is running but can reach nothing" —
/// which is exactly the fault users describe as "IPv6 does not work". And an
/// adapter normally holds several global addresses at once (a stable one plus
/// RFC 4941 temporary ones), which is noise rather than information, so the
/// rest are counted.
fn ipv6_display(values: &[std::net::IpAddr]) -> slint::SharedString {
    let routable: Vec<String> = values
        .iter()
        .filter(|value| !adapter::is_link_local(value))
        .map(ToString::to_string)
        .collect();
    match routable.len() {
        0 if values.is_empty() => "--".into(),
        0 => "仅链路本地地址（无法访问 IPv6 互联网）".into(),
        1 => routable[0].clone().into(),
        count => format!("{} 等 {count} 个", routable[0]).into(),
    }
}

fn apply_settings(config: &mut SessionConfig, settings: &ClientConfig) {
    if let Some(value) = settings.local_ip {
        config.local_ipv4 = value;
    }
    if let Some(value) = settings.mac {
        config.mac = value;
    }
    if let Some(value) = &settings.hostname {
        config.hostname = value.as_bytes().to_vec();
    }
    if let Some(value) = settings.dns {
        config.primary_dns = value.octets();
    }
    if let Some(value) = settings.dhcp {
        config.dhcp_server = value.octets();
    }
    if let Some(value) = settings.server_port {
        config.server_port = value;
    }
    if let Some(value) = settings.local_port {
        config.local_port = value;
    }
    if let Some(value) = settings.auth_route {
        config.auth_route = value;
    }
}

fn validate_preferences(
    prefs: &Preferences,
    servers: Vec<std::net::Ipv4Addr>,
) -> Result<(), String> {
    if !prefs.auto_login && prefs.username.is_empty() && prefs.password.is_empty() {
        return Ok(());
    }
    ClientConfig {
        account: prefs.username.clone(),
        password: prefs.password.clone(),
        servers,
        ..Default::default()
    }
    .validate()
    .map_err(|e| e.to_string())
}

/// The failure text, plus a sentence saying what to do when one is known.
///
/// `SessionError` already renders Chinese, but "认证服务器未响应：keepalive" leaves the
/// user with nothing to try. The guidance supplies the "and now what".
fn describe_failure(error: &SessionError) -> String {
    match error.guidance() {
        Some(hint) => format!("{error}｜{hint}"),
        None => error.to_string(),
    }
}

fn retryable_error(error: &SessionError) -> bool {
    match error {
        SessionError::NoResponse(_) => true,
        SessionError::Transport(kind) => matches!(
            kind,
            std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::Interrupted
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::AddrNotAvailable
                | std::io::ErrorKind::NetworkDown
                | std::io::ErrorKind::NetworkUnreachable
                | std::io::ErrorKind::HostUnreachable
        ),
        _ => false,
    }
}

fn format_mac(mac: u64) -> String {
    mac.to_be_bytes()[2..]
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Reads the settings file, reporting a broken one instead of silently falling
/// back to defaults.
fn load_settings() -> Result<ClientConfig, String> {
    match drcom_campus::config::discover() {
        Some(path) => drcom_campus::config::load(&path)
            .map_err(|error| format!("配置文件 {} 未生效：{error}", path.display())),
        None => Ok(ClientConfig::default()),
    }
}

fn host_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "DRCOM-PC".to_string())
}

fn format_duration(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds} 秒")
    } else if seconds < 3600 {
        format!("{} 分 {:02} 秒", seconds / 60, seconds % 60)
    } else {
        format!("{} 时 {:02} 分", seconds / 3600, (seconds % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_login_requires_valid_saved_credentials() {
        let servers = vec![std::net::Ipv4Addr::new(10, 100, 61, 3)];
        assert!(validate_preferences(&Preferences::default(), servers.clone()).is_ok());
        let mut prefs = Preferences {
            auto_login: true,
            ..Default::default()
        };
        assert!(validate_preferences(&prefs, servers.clone()).is_err());
        prefs.username = "test-user".into();
        prefs.password = "test-pass".into();
        assert!(validate_preferences(&prefs, servers).is_ok());
    }

    #[test]
    fn gui_applies_connection_configuration() {
        let mut session = SessionConfig::new(vec![], vec![], vec![]);
        let settings = ClientConfig {
            server_port: Some(6000),
            local_port: Some(0),
            dns: Some(std::net::Ipv4Addr::new(1, 1, 1, 1)),
            hostname: Some("TEST".into()),
            ..Default::default()
        };
        apply_settings(&mut session, &settings);
        assert_eq!(session.server_port, 6000);
        assert_eq!(session.local_port, 0);
        assert_eq!(session.primary_dns, [1, 1, 1, 1]);
        assert_eq!(session.hostname, b"TEST");
    }

    #[test]
    fn reconnect_only_retries_recoverable_network_errors() {
        assert!(retryable_error(&SessionError::NoResponse("keepalive")));
        assert!(retryable_error(&SessionError::Transport(
            std::io::ErrorKind::NetworkDown
        )));
        assert!(!retryable_error(&SessionError::Transport(
            std::io::ErrorKind::AddrInUse
        )));
        assert!(!retryable_error(&SessionError::Transport(
            std::io::ErrorKind::PermissionDenied
        )));
        assert!(!retryable_error(&SessionError::Stopped));
        assert!(!retryable_error(&SessionError::Challenge("invalid reply")));
    }

    #[test]
    fn reconnect_delay_doubles_then_holds_at_the_ceiling() {
        let mut backoff = ReconnectBackoff::new();
        let seen: Vec<u64> = (0..8).map(|_| backoff.next_delay().as_secs()).collect();
        assert_eq!(seen[..4], [5, 10, 20, 40], "delay should double each time");
        assert_eq!(
            *seen.last().unwrap(),
            120,
            "delay should stop at the ceiling"
        );
        assert!(
            seen.windows(2).all(|pair| pair[1] >= pair[0]),
            "delay must never shrink on its own: {seen:?}"
        );
        backoff.reset();
        assert_eq!(
            backoff.next_delay().as_secs(),
            5,
            "a session that held starts the next wait over"
        );
    }

    #[test]
    fn a_failure_shows_the_advice_next_to_the_reason() {
        let text = describe_failure(&SessionError::NoResponse("keepalive"));
        assert!(
            text.contains("keepalive"),
            "the reason must survive: {text}"
        );
        assert!(
            text.contains("网线"),
            "an unanswered server should suggest checking the cable: {text}"
        );
        // A rejection's own text is the advice, so nothing is appended.
        let rejected = describe_failure(&SessionError::Rejected(
            drcom_campus::login_response::LoginFailure {
                code: 2,
                ip: None,
                mac: None,
                message: None,
            },
        ));
        assert!(!rejected.contains('｜'), "got {rejected}");
    }

    #[test]
    fn closing_to_tray_is_on_by_default_like_the_c_sharp_client() {
        assert!(Shared::default().minimize_to_tray);
        assert!(Preferences::default().minimize_to_tray);
    }

    #[test]
    fn ipv6_display_hides_link_local_addresses_but_says_they_are_all_there_is() {
        use std::net::{IpAddr, Ipv6Addr};

        let global: IpAddr = "2001:da8:b000:6703:928f:a777:f63e:d0ee".parse().unwrap();
        let temporary: IpAddr = "2001:da8:b000:6703:8493:26f:45f9:7dfd".parse().unwrap();
        let link_local: IpAddr = "fe80::60f6:408e:ea0a:8690".parse().unwrap();

        // One routable address: shown as itself.
        assert_eq!(ipv6_display(&[global, link_local]), global.to_string());
        // Several: the first identifies the prefix, the rest would be noise.
        assert_eq!(
            ipv6_display(&[global, temporary, link_local]),
            format!("{global} 等 2 个")
        );
        // Only link-local is the case that must never look like a working stack.
        let text = ipv6_display(&[link_local]);
        assert!(text.contains("仅链路本地"), "got {text}");
        assert!(
            text.contains("无法"),
            "the verdict must be explicit: {text}"
        );
        assert_eq!(ipv6_display(&[]), "--");
        // Guard the exact boundary of fe80::/10, which the bit test could off-by-one.
        assert!(adapter::is_link_local(
            &"fe80::1".parse::<Ipv6Addr>().unwrap().into()
        ));
        assert!(adapter::is_link_local(
            &"febf::1".parse::<Ipv6Addr>().unwrap().into()
        ));
        assert!(!adapter::is_link_local(
            &"fec0::1".parse::<Ipv6Addr>().unwrap().into()
        ));
        assert!(!adapter::is_link_local(
            &"2001:da8::1".parse::<Ipv6Addr>().unwrap().into()
        ));
    }
}
