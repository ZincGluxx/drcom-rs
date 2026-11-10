//! Pure helpers shared by the service state machine.
//!
//! Everything in this module is a plain function or a small policy struct: it
//! formats display text, classifies session failures, formats the failure card
//! and builds adapter views. Nothing here touches shared state, spawns work or
//! holds a lock, so the state machine in [`crate::service`] stays about
//! transitions and these rules can be unit-tested on their own.

use std::net::IpAddr;
use std::time::Duration;

use drcom_core::adapter::{self, AdapterInfo};
use drcom_core::config::ClientConfig;
use drcom_core::page_status::CampusPage;
use drcom_core::preferences::Preferences;
use drcom_core::session::{SessionConfig, SessionError};

use crate::api::{AdapterView, FailureView, NetworkEditorView, ProbeView};

/// Delay between automatic reconnect attempts.
///
/// The reference client re-authenticates on a fixed schedule, which for a link
/// that is down for an afternoon means a login frame every few seconds for hours
/// — pointless, and hard on an authentication server that is shared by a whole
/// building. Doubling up to a ceiling keeps the first recovery quick without
/// letting a long outage turn into a flood.
pub(crate) struct ReconnectBackoff {
    next: Duration,
}

impl ReconnectBackoff {
    const FIRST: Duration = Duration::from_secs(5);
    const CEILING: Duration = Duration::from_secs(120);

    pub(crate) fn new() -> Self {
        Self { next: Self::FIRST }
    }

    /// Back to the first step, after a session that actually held.
    pub(crate) fn reset(&mut self) {
        self.next = Self::FIRST;
    }

    /// The delay to use now, and the longer one after it.
    pub(crate) fn next_delay(&mut self) -> Duration {
        let current = self.next;
        self.next = (current * 2).min(Self::CEILING);
        current
    }
}

/// Why the failed attempt that deserves a retry had no retry.
pub(crate) fn retry_attempt_admitted(
    first_attempt: bool,
    reconnect_allowed: bool,
    stopped: bool,
) -> bool {
    !stopped && (first_attempt || reconnect_allowed)
}

/// The failure card before anything failed. Mirrors the defaults the old window
/// shipped in its `.slint` markup.
pub(crate) fn initial_failure() -> FailureView {
    FailureView {
        message: "本次运行尚未发生连接问题".to_string(),
        time: "--".to_string(),
        stage: "--".to_string(),
        code: "--".to_string(),
        duration: "--".to_string(),
        retry: "--".to_string(),
    }
}

/// The connectivity panel before the first check.
pub(crate) fn initial_probe() -> ProbeView {
    ProbeView {
        busy: false,
        campus: "未检测".to_string(),
        ipv4: "未检测".to_string(),
        ipv6: "未检测".to_string(),
    }
}

/// The static-IPv4 editor, closed.
pub(crate) fn initial_network_editor() -> NetworkEditorView {
    NetworkEditorView {
        visible: false,
        adapter_name: String::new(),
        ip: String::new(),
        mask: String::new(),
        gateway: String::new(),
        dns: String::new(),
        message: String::new(),
        busy: false,
    }
}

/// Display text for the one-shot read-only campus page probe.
///
/// `app_session_active` is what distinguishes "the page says someone is online,
/// which may be us" from "the page says someone is online and it is not us" —
/// the latter is the case that must not be silently taken over.
pub(crate) fn page_probe_status(
    page: CampusPage,
    failed: bool,
    app_session_active: bool,
) -> &'static str {
    if failed || page == CampusPage::Unknown {
        "网页状态未知；此程序连接状态未更改"
    } else if app_session_active {
        match page {
            CampusPage::CurrentSession | CampusPage::LoginSucceeded => {
                "此程序会话由左上角连接状态和 UDP 保活管理"
            }
            CampusPage::LoginForm => "网页返回登录表单；此程序 UDP 保活状态见左上角",
            CampusPage::LoginRejected => "网页返回认证失败页；不代表 UDP 会话状态",
            CampusPage::Unknown => "网页状态未知；此程序连接状态未更改",
        }
    } else {
        match page {
            CampusPage::LoginForm => "网页返回登录表单；此程序尚未建立 UDP 保活",
            CampusPage::CurrentSession => "网页显示已有认证（外部会话）；此程序未建立 UDP 保活",
            CampusPage::LoginRejected => "网页返回认证失败页；不代表 UDP 会话状态",
            CampusPage::LoginSucceeded => "网页返回登录成功页；无法确认当前在线",
            CampusPage::Unknown => "网页状态未知；此程序连接状态未更改",
        }
    }
}

/// The failure text, plus a sentence saying what to do when one is known.
///
/// `SessionError` already renders Chinese, but "认证服务器未响应：keepalive" leaves the
/// user with nothing to try. The guidance supplies the "and now what".
pub(crate) fn describe_failure(error: &SessionError) -> String {
    let summary = match error {
        SessionError::Configuration(_) => "认证配置无效".to_string(),
        SessionError::Challenge(_) => "无法开始登录".to_string(),
        SessionError::Rejected(failure) => format!("服务器拒绝认证，错误码 {}", failure.code),
        SessionError::LogoutPacket(_) => "下线请求无法构造".to_string(),
        SessionError::Route(_) => "无法为认证服务器设置网络路由".to_string(),
        // These variants contain only a fixed stage name or an ErrorKind.
        SessionError::Transport(_) | SessionError::NoResponse(_) | SessionError::Credentials(_) => {
            error.to_string()
        }
        SessionError::Stopped => "会话已停止".to_string(),
    };
    match error.guidance() {
        Some(hint) => format!("{summary}｜{hint}"),
        None => summary,
    }
}

pub(crate) fn failure_stage_and_code(error: &SessionError) -> (String, String) {
    match error {
        SessionError::Configuration(_) => ("配置".into(), "CONFIGURATION".into()),
        SessionError::Transport(kind) => ("网络通信".into(), format!("NETWORK-{kind:?}")),
        SessionError::Challenge(_) => ("挑战".into(), "CHALLENGE".into()),
        SessionError::Credentials(_) => ("账号格式".into(), "CREDENTIALS".into()),
        SessionError::Rejected(failure) => ("认证".into(), format!("DRCOM-{}", failure.code)),
        SessionError::NoResponse(stage) => ((*stage).into(), format!("NO-RESPONSE-{stage}")),
        SessionError::Stopped => ("停止".into(), "STOPPED".into()),
        SessionError::LogoutPacket(_) => ("下线".into(), "LOGOUT-PACKET".into()),
        SessionError::Route(_) => ("网络路由".into(), "ROUTE".into()),
    }
}

pub(crate) fn retryable_error(error: &SessionError) -> bool {
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

pub(crate) fn format_mac(mac: u64) -> String {
    mac.to_be_bytes()[2..]
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Comma-separated address list for the adapter panel, `--` when empty.
pub(crate) fn adapter_addresses(values: &[IpAddr]) -> String {
    if values.is_empty() {
        "--".into()
    } else {
        values
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
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
pub(crate) fn ipv6_display(values: &[IpAddr]) -> String {
    let routable: Vec<String> = values
        .iter()
        .filter(|value| !adapter::is_link_local(value))
        .map(ToString::to_string)
        .collect();
    match routable.len() {
        0 if values.is_empty() => "--".into(),
        0 => "仅链路本地地址（无法访问 IPv6 互联网）".into(),
        1 => routable[0].clone(),
        count => format!("{} 等 {count} 个", routable[0]),
    }
}

/// The adapter panel for a selected physical adapter.
pub(crate) fn adapter_view(found: &AdapterInfo) -> AdapterView {
    AdapterView {
        available: true,
        name: found.friendly_name.clone(),
        ipv4: found.ipv4.to_string(),
        mac: format_mac(found.mac),
        gateway: adapter_addresses(&found.details.gateways),
        dns: adapter_addresses(&found.details.dns),
        ipv6: ipv6_display(&found.details.ipv6),
    }
}

/// The adapter panel when no eligible physical adapter was found.
pub(crate) fn unavailable_adapter_view() -> AdapterView {
    AdapterView {
        available: false,
        name: "未检测到物理网卡".to_string(),
        ipv4: "--".to_string(),
        mac: "--".to_string(),
        gateway: "--".to_string(),
        dns: "--".to_string(),
        ipv6: "--".to_string(),
    }
}

pub(crate) fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds} 秒")
    } else if seconds < 3600 {
        format!("{} 分 {:02} 秒", seconds / 60, seconds % 60)
    } else {
        format!("{} 时 {:02} 分", seconds / 3600, (seconds % 3600) / 60)
    }
}

pub(crate) fn apply_settings(config: &mut SessionConfig, settings: &ClientConfig) {
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

pub(crate) fn validate_preferences(
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

/// The password the login form should show for a remembered preference file.
pub(crate) fn saved_password_for_form(prefs: &Preferences) -> Option<String> {
    if prefs.password_cleared {
        Some(String::new())
    } else if !prefs.username.is_empty() {
        Some(prefs.password.clone())
    } else {
        None
    }
}

pub(crate) fn cleared_password_preferences(
    username: String,
    auto_reconnect: bool,
    minimize_to_tray: bool,
) -> Preferences {
    Preferences {
        username,
        password: String::new(),
        password_cleared: true,
        auto_login: false,
        auto_reconnect,
        minimize_to_tray,
    }
}

pub(crate) fn host_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "DRCOM-PC".to_string())
}

/// Reads the settings file, reporting a broken one instead of silently falling
/// back to defaults.
pub(crate) fn load_settings() -> Result<ClientConfig, String> {
    match drcom_core::config::discover() {
        Some(path) => drcom_core::config::load(&path)
            .map_err(|error| format!("配置文件 {} 未生效：{error}", path.display())),
        None => Ok(ClientConfig::default()),
    }
}

/// The adapter that should carry a session: a manually configured `local_ip`
/// remains authoritative, otherwise the OS route to the first server decides.
pub(crate) fn select_session_adapter(settings: &ClientConfig) -> Option<AdapterInfo> {
    match settings.local_ip {
        Some(ip) => adapter::list().into_iter().find(|item| item.ipv4 == ip),
        None => adapter::select_for(settings.servers_or_env().first().copied()),
    }
}

/// The adapter the adapter panel and the network editor show. They always follow
/// the route to the first authentication server, like the old window did.
pub(crate) fn select_route_adapter(settings: &ClientConfig) -> Option<AdapterInfo> {
    adapter::select_for(settings.servers_or_env().first().copied())
}

#[cfg(test)]
mod tests {
    use super::*;
    use drcom_core::login_response::LoginFailure;
    use drcom_core::preferences::Preferences;

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
    fn reconnect_admission_rechecks_clear_between_backoff_and_attempt() {
        assert!(retry_attempt_admitted(true, false, false));
        assert!(retry_attempt_admitted(false, true, false));
        // The reconnect delay completed, then password clearing disabled retries
        // before the loop could construct the next LoginSession.
        assert!(!retry_attempt_admitted(false, false, false));
        assert!(!retry_attempt_admitted(false, true, true));
        assert!(!retry_attempt_admitted(true, true, true));
    }

    #[test]
    fn clearing_password_disables_saved_login_and_overrides_ini_backfill() {
        let prefs = cleared_password_preferences("test-user".into(), true, false);
        assert_eq!(prefs.username, "test-user");
        assert!(prefs.password.is_empty());
        assert!(!prefs.auto_login);
        assert!(prefs.password_cleared);
        assert!(prefs.auto_reconnect);
        assert!(!prefs.minimize_to_tray);
        assert_eq!(saved_password_for_form(&prefs).as_deref(), Some(""));

        // A legacy preference file without a username leaves the drcom.ini
        // password alone; the persisted clear marker overrides it explicitly.
        assert_eq!(saved_password_for_form(&Preferences::default()), None);
    }

    #[test]
    fn exported_failure_summary_does_not_include_server_supplied_text() {
        let error = SessionError::Rejected(LoginFailure {
            code: 21,
            ip: None,
            mac: None,
            message: Some(b"secret-packet-marker".to_vec()),
        });
        let summary = describe_failure(&error);
        assert!(summary.contains("错误码 21"));
        assert!(!summary.contains("secret-packet-marker"));
        assert_eq!(
            failure_stage_and_code(&error),
            ("认证".into(), "DRCOM-21".into())
        );
    }

    #[test]
    fn session_applies_connection_configuration() {
        let mut session = SessionConfig::new(vec![], vec![], vec![]);
        // Source port allocation must never change the destination port.
        assert_eq!(session.server_port, 61440);
        assert_eq!(session.local_port, 0);
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
        let denied = describe_failure(&SessionError::Transport(
            std::io::ErrorKind::PermissionDenied,
        ));
        assert!(denied.contains("系统保留"));
        assert!(denied.contains("local_port"));
        assert!(!denied.contains("管理员"));
        let text = describe_failure(&SessionError::NoResponse("keepalive"));
        assert!(
            text.contains("连接可能已中断"),
            "the reason must survive: {text}"
        );
        assert!(
            text.contains("网线"),
            "an unanswered server should suggest checking the cable: {text}"
        );
        // A bare server code still needs a useful next step.
        let rejected = describe_failure(&SessionError::Rejected(LoginFailure {
            code: 2,
            ip: None,
            mac: None,
            message: None,
        }));
        assert!(rejected.contains("账号密码"), "got {rejected}");
    }

    #[test]
    fn ipv6_display_hides_link_local_addresses_but_says_they_are_all_there_is() {
        use std::net::Ipv6Addr;

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

    #[test]
    fn formatting_helpers_match_the_old_window() {
        assert_eq!(format_mac(0x1122_3344_5566), "11:22:33:44:55:66");
        assert_eq!(format_duration(Duration::from_secs(0)), "0 秒");
        assert_eq!(format_duration(Duration::from_secs(59)), "59 秒");
        assert_eq!(format_duration(Duration::from_secs(60)), "1 分 00 秒");
        assert_eq!(format_duration(Duration::from_secs(192)), "3 分 12 秒");
        assert_eq!(format_duration(Duration::from_secs(3720)), "1 时 02 分");
        assert_eq!(adapter_addresses(&[]), "--");
        assert_eq!(
            adapter_addresses(&["10.0.0.1".parse().unwrap(), "10.0.0.2".parse().unwrap()]),
            "10.0.0.1, 10.0.0.2"
        );
    }

    #[test]
    fn an_unavailable_adapter_never_claims_to_be_available() {
        let view = unavailable_adapter_view();
        assert!(!view.available);
        assert_eq!(view.name, "未检测到物理网卡");
        assert_eq!(view.ipv4, "--");
        assert_eq!(view.mac, "--");
        assert_eq!(view.gateway, "--");
        assert_eq!(view.dns, "--");
        assert_eq!(view.ipv6, "--");
    }

    #[test]
    fn page_probe_texts_depend_on_whether_our_session_exists() {
        assert_eq!(
            page_probe_status(CampusPage::CurrentSession, false, false),
            "网页显示已有认证（外部会话）；此程序未建立 UDP 保活"
        );
        assert_eq!(
            page_probe_status(CampusPage::CurrentSession, false, true),
            "此程序会话由左上角连接状态和 UDP 保活管理"
        );
        assert_eq!(
            page_probe_status(CampusPage::Unknown, false, false),
            "网页状态未知；此程序连接状态未更改"
        );
        assert_eq!(
            page_probe_status(CampusPage::LoginForm, true, true),
            "网页状态未知；此程序连接状态未更改"
        );
    }
}
