//! Glue between the Slint window and the application service.
//!
//! Everything that decides *what* the client is doing lives in `drcom-service`.
//! This module only mirrors its snapshots into the window and turns window
//! callbacks into commands; no session, preference or diagnostics logic is
//! duplicated here.
//!
//! Notifications arrive on the thread that caused the change, so they are
//! marshalled into Slint's event loop before touching a component. The window is
//! built on demand and released when it goes to the tray (see `main.rs`), which is
//! why the service — not the component — is the source of truth a rebuilt window
//! replays.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use drcom_core::event_log;
use drcom_service::{
    Command, CommandOutcome, FormSeed, Notification, ProbeKind, Service, Snapshot,
};
use slint::ComponentHandle;

use crate::{AppTray, AppWindow, platform};

/// Shell tooltips clip a long string without any ellipsis, so the useful part has
/// to be the front of the note.
const TOOLTIP_NOTE_CHARS: usize = 48;

pub struct Backend {
    /// The frontend's whole application backend. Cheap to clone, safe to call
    /// from any thread, and the only thing that knows the session state.
    service: Service,
    /// The window is created on demand and dropped again when the user closes it
    /// to the tray, so anything that outlives a single window — service
    /// notifications, the tray, the release timer — looks the window up here
    /// instead of capturing one. Slint keeps the render target for as long as the
    /// component exists, and that target is the largest single block this process
    /// commits.
    ui: Arc<Mutex<slint::Weak<AppWindow>>>,
    /// The tray icon is a separate Slint component, so the only way to keep its
    /// tooltip in step with the backend is to hold a handle to it here.
    tray: slint::Weak<AppTray>,
    /// Set when the backend asked for the window to go to the tray. The event
    /// loop owns the component and therefore the release itself; a notification
    /// from a session thread can only record that the release is due.
    release_requested: Arc<AtomicBool>,
}

impl Clone for Backend {
    fn clone(&self) -> Self {
        Self {
            service: self.service.clone(),
            ui: Arc::clone(&self.ui),
            tray: self.tray.clone(),
            release_requested: Arc::clone(&self.release_requested),
        }
    }
}

impl Backend {
    /// Wires the service into the tray and the (yet to be built) window.
    pub fn new(tray: Option<&AppTray>, service: Service) -> Self {
        let backend = Self {
            service,
            ui: Arc::new(Mutex::new(slint::Weak::default())),
            tray: tray.map(|tray| tray.as_weak()).unwrap_or_default(),
            release_requested: Arc::new(AtomicBool::new(false)),
        };
        backend.subscribe();
        backend
    }

    /// Every service notification is marshalled onto the Slint thread; the sink
    /// itself never touches a component.
    fn subscribe(&self) {
        let ui = Arc::clone(&self.ui);
        let tray = self.tray.clone();
        let release_requested = Arc::clone(&self.release_requested);
        let sink = Arc::new(move |notification: Notification| {
            let ui = Arc::clone(&ui);
            let tray = tray.clone();
            let release_requested = Arc::clone(&release_requested);
            let _ = slint::invoke_from_event_loop(move || match notification {
                Notification::State { snapshot } => {
                    if let Some(window) = upgrade(&ui) {
                        apply_snapshot(&window, &snapshot);
                    }
                    // The tray is the only visible part of the program while the
                    // window is in the tray, so the status line has to follow the
                    // service there too — including when no window exists at all.
                    if !snapshot.tooltip.is_empty() {
                        set_tray_tooltip(&tray, &snapshot.tooltip);
                    }
                }
                Notification::HideToTray { note } => {
                    request_hide_to_tray(&ui, &tray, &release_requested, &note);
                }
                Notification::ExitReady => {
                    let _ = slint::quit_event_loop();
                }
            });
        });
        self.service.subscribe(sink);
    }

    /// Publishes a freshly built window, replays the state it is supposed to
    /// show, and wires its callbacks. Called on every open, not just the first
    /// one: each open builds a new component, so the callbacks the previous one
    /// carried went away with it.
    pub fn bind(&self, window: &AppWindow) {
        self.publish(window);
        window.set_ui_font_family(platform::font_family().into());
        window.set_tray_available(self.tray.upgrade().is_some());

        // The form seed is the one source of a saved password, and it is applied
        // only here. State pushes must not overwrite what the user is typing.
        seed_form(window, &self.service.form_seed());
        apply_snapshot(window, &self.service.snapshot());
        self.attach_callbacks(window);
    }

    fn publish(&self, window: &AppWindow) {
        match self.ui.lock() {
            Ok(mut slot) => *slot = window.as_weak(),
            Err(poisoned) => *poisoned.into_inner() = window.as_weak(),
        }
    }

    /// The window currently on screen, if any. Running with no window at all is a
    /// normal state here: the process lives in the tray and only builds a window
    /// when the user asks for one.
    fn ui(&self) -> Option<AppWindow> {
        upgrade(&self.ui)
    }

    /// Takes the pending "send the window to the tray" request, if any. Whoever
    /// takes it is responsible for hiding and releasing the component.
    pub fn take_release_request(&self) -> bool {
        self.release_requested.swap(false, Ordering::Relaxed)
    }

    /// Forgets a request that a rebuilt window has already satisfied.
    pub fn clear_release_request(&self) {
        self.release_requested.store(false, Ordering::Relaxed);
    }

    /// Drops the handle so a window that is going away stops receiving writes.
    pub fn unbind(&self) {
        match self.ui.lock() {
            Ok(mut slot) => *slot = slint::Weak::default(),
            Err(poisoned) => *poisoned.into_inner() = slint::Weak::default(),
        }
    }

    /// One second of housekeeping. The duration refresh and the auto-hide after a
    /// login run inside the service; the event loop only has to keep the clock,
    /// then release the window it may have been asked to hide.
    pub fn tick(&self) {
        self.service.tick();
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
            .ui()
            .map(|window| window.get_minimize_to_tray())
            .unwrap_or(true);
        // Apply the (unsaved) checkbox immediately, exactly like the old glue
        // did: the backend reports a login-hide based on this value.
        self.service
            .execute(Command::SetMinimizeToTray { enabled: minimize });
        if minimize {
            event_log::record("关闭窗口：按设置最小化到托盘");
            self.hide_to_tray("已最小化到托盘");
        } else {
            event_log::record("关闭窗口：按设置直接退出");
        }
        minimize
    }

    /// Asks the service to log out and shut down. The event loop is not quit here:
    /// the service sends `Notification::ExitReady` once the logout packet (if any)
    /// has been sent, and the sink then quits the loop.
    pub fn quit(&self) {
        self.service.execute(Command::Shutdown);
    }

    /// Sends the window to the tray and says so in the tooltip, which is the only
    /// part of the program still visible once the window is hidden.
    fn hide_to_tray(&self, note: &str) {
        request_hide_to_tray(&self.ui, &self.tray, &self.release_requested, note);
    }

    fn set_status_detail(&self, text: &str) {
        if let Some(window) = self.ui() {
            window.set_status_detail(text.into());
        }
    }

    /// Wires the window callbacks to service commands. Every callback runs on the
    /// Slint thread, which is the thread `Service::execute` is called on, as in
    /// the old in-process glue.
    fn attach_callbacks(&self, window: &AppWindow) {
        let connect = self.clone();
        window.on_connect_requested(move |username, password, auto_reconnect| {
            connect.service.execute(Command::Login {
                username: username.to_string(),
                password: password.to_string(),
                auto_reconnect,
            });
        });

        let disconnect = self.clone();
        window.on_disconnect_requested(move || {
            disconnect.service.execute(Command::Logout);
        });

        let refresh = self.clone();
        window.on_refresh_requested(move || {
            refresh.service.execute(Command::RefreshAdapter);
        });

        let probe = self.clone();
        window.on_probe_requested(move |kind| {
            probe.service.execute(Command::Probe {
                kind: probe_kind(kind),
            });
        });

        let network = self.clone();
        window.on_network_settings_requested(move || {
            if drcom_core::static_ipv4::is_supported() {
                network.service.execute(Command::OpenNetworkEditor);
            } else if let Err(error) = platform::open_network_settings() {
                network.set_status_detail(&format!("无法打开网络设置：{error}"));
            }
        });

        let close_editor = self.clone();
        window.on_close_network_editor_requested(move || {
            close_editor.service.execute(Command::CloseNetworkEditor);
        });

        let system = self.clone();
        window.on_system_network_requested(move || {
            if let Err(error) = platform::open_network_settings() {
                system.set_status_detail(&format!("无法打开网络设置：{error}"));
            }
        });

        let apply = self.clone();
        window.on_apply_network_requested(move |ip, mask, gateway, dns| {
            apply.service.execute(Command::ApplyNetwork {
                ip: ip.to_string(),
                mask: mask.to_string(),
                gateway: gateway.to_string(),
                dns: dns.to_string(),
            });
        });

        let save = self.clone();
        window.on_save_requested(
            move |username, password, auto_login, auto_reconnect, autostart, minimize_to_tray| {
                save.service.execute(Command::SaveSettings {
                    username: username.to_string(),
                    password: password.to_string(),
                    auto_login,
                    auto_reconnect,
                    autostart,
                    minimize_to_tray,
                });
            },
        );

        let clear = self.clone();
        window.on_clear_password_requested(move || {
            let Some(window) = clear.ui() else {
                return;
            };
            clear.service.execute(Command::ClearSavedPassword {
                username: window.get_username().trim().to_string(),
                auto_reconnect: window.get_auto_reconnect(),
                minimize_to_tray: window.get_minimize_to_tray(),
            });
            // The password is never part of a snapshot, so the form has to be
            // cleared here; automatic login would otherwise retry with the
            // credentials the user just asked the backend to forget.
            window.set_password("".into());
            window.set_auto_login(false);
            window.set_failure_details_expanded(false);
        });

        let copy = self.clone();
        window.on_copy_requested(move || {
            if let CommandOutcome::Text(report) = copy.service.execute(Command::DiagnosticsText) {
                match platform::copy(&report) {
                    Ok(()) => copy.set_status_detail("诊断已复制（不包含账号密码）"),
                    Err(error) => copy.set_status_detail(&format!("复制失败：{error}")),
                }
            }
        });

        let export = self.clone();
        window.on_export_requested(move || {
            if let CommandOutcome::Text(report) = export.service.execute(Command::DiagnosticsText) {
                match platform::export(&report) {
                    Ok(path) => {
                        event_log::record(&format!("导出诊断文件：{}", path.display()));
                        export.set_status_detail(&format!("诊断已导出：{}", path.display()));
                    }
                    Err(error) => export.set_status_detail(&format!("导出诊断失败：{error}")),
                }
            }
        });
    }
}

/// The Slint probe callback still passes the old integer: 0 = campus only,
/// 1 = internet only, 2 = both.
fn probe_kind(kind: i32) -> ProbeKind {
    match kind {
        0 => ProbeKind::Campus,
        1 => ProbeKind::Internet,
        _ => ProbeKind::All,
    }
}

fn upgrade(ui: &Mutex<slint::Weak<AppWindow>>) -> Option<AppWindow> {
    match ui.lock() {
        Ok(slot) => slot.upgrade(),
        Err(poisoned) => poisoned.into_inner().upgrade(),
    }
}

fn seed_form(window: &AppWindow, seed: &FormSeed) {
    if !seed.username.is_empty() {
        window.set_username(seed.username.clone().into());
    }
    if let Some(password) = &seed.password {
        window.set_password(password.clone().into());
    }
    window.set_auto_login(seed.auto_login);
    window.set_auto_reconnect(seed.auto_reconnect);
    window.set_minimize_to_tray(seed.minimize_to_tray);
    if let Some(enabled) = seed.start_with_windows {
        window.set_start_with_windows(enabled);
    }
    if let Some(status) = &seed.config_status {
        window.set_config_status(status.clone().into());
    }
}

/// Mirrors a service snapshot into the window.
///
/// The login form belongs to the user: state pushes never overwrite username,
/// password, auto-login, auto-reconnect or 关闭到托盘. Those are seeded once in
/// [`Backend::bind`]. Everything else here is backend-owned display state.
fn apply_snapshot(window: &AppWindow, snapshot: &Snapshot) {
    let connection = &snapshot.connection;
    window.set_connection_state(connection.state_text.clone().into());
    window.set_status_detail(connection.detail.clone().into());
    window.set_online(connection.online);
    window.set_session_active(connection.session_active);
    window.set_duration(connection.online_duration.clone().into());

    let adapter = &snapshot.adapter;
    window.set_adapter_name(adapter.name.clone().into());
    window.set_ipv4_address(adapter.ipv4.clone().into());
    window.set_mac_address(adapter.mac.clone().into());
    window.set_gateway(adapter.gateway.clone().into());
    window.set_dns(adapter.dns.clone().into());
    window.set_ipv6(adapter.ipv6.clone().into());

    let failure = &snapshot.failure;
    window.set_last_failure(failure.message.clone().into());
    window.set_last_failure_time(failure.time.clone().into());
    window.set_last_failure_stage(failure.stage.clone().into());
    window.set_last_failure_code(failure.code.clone().into());
    window.set_last_failure_duration(failure.duration.clone().into());
    window.set_last_failure_retry(failure.retry.clone().into());

    let probe = &snapshot.probe;
    window.set_probe_busy(probe.busy);
    window.set_campus_result(probe.campus.clone().into());
    window.set_ipv4_result(probe.ipv4.clone().into());
    window.set_ipv6_result(probe.ipv6.clone().into());

    let editor = &snapshot.network_editor;
    window.set_network_editor_visible(editor.visible);
    window.set_network_busy(editor.busy);
    window.set_edit_adapter_name(editor.adapter_name.clone().into());
    window.set_edit_ip(editor.ip.clone().into());
    window.set_edit_mask(editor.mask.clone().into());
    window.set_edit_gateway(editor.gateway.clone().into());
    window.set_edit_dns(editor.dns.clone().into());
    window.set_network_message(editor.message.clone().into());

    window.set_campus_page_status(snapshot.campus_page.clone().into());

    // Persisted startup/config feedback is backend-owned, but only when the
    // backend actually has an answer.
    if let Some(status) = &snapshot.settings.config_status {
        window.set_config_status(status.clone().into());
    }
    if let Some(enabled) = snapshot.settings.start_with_windows {
        window.set_start_with_windows(enabled);
    }
}

/// Sets the tray tooltip, records the release request and hides the window.
/// Runs on the Slint thread.
fn request_hide_to_tray(
    ui: &Mutex<slint::Weak<AppWindow>>,
    tray: &slint::Weak<AppTray>,
    release_requested: &AtomicBool,
    note: &str,
) {
    // Unix keeps a Dock/taskbar entry: Wayland cannot reliably hide/re-show a
    // native window, GNOME may have no tray, and Finder reopening an existing
    // macOS app does not spawn a duplicate executable for the socket guard.
    // Keep the OS window restorable rather than touching winit's private delegate.
    if cfg!(unix) || tray.upgrade().is_none() {
        if let Some(window) = upgrade(ui) {
            window.window().set_minimized(true);
        }
        return;
    }
    set_tray_tooltip(tray, note);
    // Hiding alone leaves a window whose renderer still believes its buffer holds
    // a full frame; `main.rs` releases the component on the next timer tick (or
    // immediately on the next open) so the next window starts from a fresh render
    // target.
    release_requested.store(true, Ordering::Relaxed);
    if let Some(window) = upgrade(ui) {
        let _ = window.hide();
    }
}

fn set_tray_tooltip(tray: &slint::Weak<AppTray>, note: &str) {
    if let Some(tray) = tray.upgrade() {
        tray.set_tray_tooltip(tray_tooltip_text(note).into());
    }
}

/// The shell clips a tooltip without an ellipsis, so only the front of a long
/// note survives; keep the prefix out of the budget for the same reason the old
/// glue truncated before adding it.
fn tray_tooltip_text(note: &str) -> String {
    let note: String = note.chars().take(TOOLTIP_NOTE_CHARS).collect();
    format!("DrCom 校园网助手 - {note}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use drcom_service::ServiceOptions;

    #[test]
    fn probe_kinds_match_the_slint_callback_contract() {
        assert_eq!(probe_kind(0), ProbeKind::Campus);
        assert_eq!(probe_kind(1), ProbeKind::Internet);
        assert_eq!(probe_kind(2), ProbeKind::All);
        assert_eq!(probe_kind(99), ProbeKind::All);
    }

    #[test]
    fn tray_tooltip_keeps_the_prefix_and_only_the_front_of_a_long_note() {
        assert_eq!(tray_tooltip_text("已连接"), "DrCom 校园网助手 - 已连接");
        let long = "断".repeat(80);
        let text = tray_tooltip_text(&long);
        assert!(text.starts_with("DrCom 校园网助手 - "));
        let note = text.trim_start_matches("DrCom 校园网助手 - ");
        assert_eq!(note.chars().count(), TOOLTIP_NOTE_CHARS);
    }

    #[test]
    fn state_pushes_leave_the_login_form_alone() {
        i_slint_backend_testing::init_no_event_loop();
        let window = AppWindow::new().expect("headless Slint component");
        window.set_username("typed-user".into());
        window.set_password("typed-pass".into());
        window.set_auto_login(true);
        window.set_auto_reconnect(false);
        window.set_minimize_to_tray(false);

        // The skeleton snapshot has different defaults; they must not land on the
        // form the user is editing.
        let snapshot = Service::new(ServiceOptions::for_test()).snapshot();
        apply_snapshot(&window, &snapshot);

        assert_eq!(window.get_username().as_str(), "typed-user");
        assert_eq!(window.get_password().as_str(), "typed-pass");
        assert!(window.get_auto_login());
        assert!(!window.get_auto_reconnect());
        assert!(!window.get_minimize_to_tray());
        assert_eq!(window.get_connection_state().as_str(), "未连接");
    }
}
