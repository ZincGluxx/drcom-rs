//! End-to-end Dr.COM session: challenge, login, keep-alive and logout.
//!
//! The packet builders live in `protocol.rs` (fragments recovered from the
//! unpacked original `DrAuthSvr.dll`) and `reference_login.rs` (the selected
//! third-party compatibility profile). This module wires them to the socket and owns
//! the state that has to survive between stages:
//!
//! * the login retry counter, which the original keeps across the challenge and
//!   the login send (`0x1003d410`), so a challenge timeout must not refill it;
//! * the challenge value, reused by the first digest of every primary keep-alive;
//! * the 16-byte session cookie from the login response, sent verbatim in every
//!   primary keep-alive and folded into the keep-alive tail;
//! * the authentication server that actually answered the challenge, which is
//!   the only source accepted afterwards.
//!
//! Every wait is bounded by an explicit timeout and every retry budget is
//! finite, so a silent server can never turn into an infinite send loop.

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::auth::{self, ChallengeConnectError, ChallengeExchangeError, ChallengeReceipt};
use crate::keepalive::{
    self, KeepAliveChecksum, KeepAlivePacketType, KeepAliveResponse, classify_response,
    is_message_packet, negotiated_version, update_tail,
};
use crate::login_response::{self, LoginFailure, LoginResponse};
use crate::protocol::{self, LogoutPacketError};
use crate::reference_login::{self, BuildError, ReferenceLoginConfig};
use crate::retry::{LoginRetry, LoginTimeout, RetryConfigError};
use crate::route::{self, InstalledRoute, RouteError, RouteMode};
use crate::transport::UdpTransport;

/// The reference client always sends `0x02` as the first challenge sequence and
/// `0x09` as the client option byte.
const FIRST_CHALLENGE_SEQUENCE: u8 = 0x02;
const CHALLENGE_CLIENT_OPTION: u8 = 0x09;
/// How long the client waits after a login timeout before re-challenging.
const RETRY_DELAY: Duration = Duration::from_millis(3000);
/// How long the client waits before resynchronising after a keep-alive gap.
const RESYNC_DELAY: Duration = Duration::from_millis(1000);
/// Granularity of the interruptible sleeps; also bounds stop latency.
const SLEEP_STEP: Duration = Duration::from_millis(50);
/// The 40-byte keep-alive counter value that triggers the extra packet.
///
/// `DrcomTask.alive` sends the extra form whenever its counter is a multiple of
/// 21. An extra round consumes three counts (extra, type 1, type 3) while an
/// ordinary round consumes two, so that works out to one extra packet every
/// tenth primary keep-alive — which is what `jlu-drcom-protocol.md` documents
/// and what a first reading of the code ("sent once when the session opens")
/// gets wrong.
const EXTRA_EVERY: u8 = 21;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPhase {
    Challenge,
    Authenticating,
    Online,
    KeepAlive,
    LoggedOut,
}

/// Progress reporting for a UI or CLI. Notes never contain the password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    Phase(SessionPhase),
    Note(String),
    KeepAliveCycle { sequence: u8, tail: [u8; 4] },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    Configuration(RetryConfigError),
    Transport(io::ErrorKind),
    Challenge(&'static str),
    Credentials(BuildError),
    Rejected(LoginFailure),
    NoResponse(&'static str),
    Stopped,
    LogoutPacket(LogoutPacketError),
    Route(RouteError),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(error) => write!(f, "认证服务器列表无效：{error:?}"),
            Self::Transport(io::ErrorKind::AddrInUse) => write!(
                f,
                "本地端口被占用，请关闭其它 Dr.COM 客户端，或用 --local-port 0 让系统分配端口"
            ),
            Self::Transport(kind) => write!(f, "套接字错误：{kind:?}"),
            Self::Challenge(reason) => write!(f, "挑战失败：{reason}"),
            Self::Credentials(error) => write!(f, "账号或密码长度不合法：{error:?}"),
            Self::Rejected(failure) => match failure.message_text() {
                // The server usually explains itself; that text is far more
                // useful than the numeric code it arrives with.
                Some(text) => write!(f, "{text}（错误码 {}）", failure.code),
                None => write!(f, "认证被服务器拒绝，错误码 {}", failure.code),
            },
            Self::NoResponse(stage) => write!(f, "认证服务器未响应：{stage}"),
            Self::Stopped => write!(f, "会话已停止"),
            Self::LogoutPacket(error) => write!(f, "下线报文无法构造：{error:?}"),
            Self::Route(error) => match error {
                RouteError::NoInterface => write!(f, "找不到通往认证服务器的接口"),
                RouteError::NoGateway => write!(f, "选中接口没有默认网关，无法添加认证服务器路由"),
                RouteError::AccessDenied => {
                    write!(f, "添加认证服务器路由被拒绝，请以管理员身份运行")
                }
                RouteError::Refused(code) => write!(f, "添加认证服务器路由失败，错误码 {code}"),
                RouteError::Unsupported => write!(f, "当前平台不支持认证服务器路由管理"),
            },
        }
    }
}

impl std::error::Error for SessionError {}

impl SessionError {
    /// One actionable sentence to sit next to the failure, for the cases where
    /// the message alone does not tell the user what to do about it.
    ///
    /// Only situations with a known remedy get a hint. Everything else returns
    /// `None` rather than inventing advice — a confident wrong suggestion sends
    /// someone to the network centre for nothing.
    pub fn guidance(&self) -> Option<&'static str> {
        match self {
            Self::NoResponse(_) => Some("请确认网线已插好或已连上校园 Wi-Fi，然后重试"),
            // Codes 1, 7 and 11 are the ones that come back carrying the
            // registered IP and MAC, which is what a mismatch looks like.
            Self::Rejected(failure) if matches!(failure.code, 1 | 7 | 11) => {
                Some("该账号可能已在其它设备上在线，或本机 IP/MAC 与登记不符")
            }
            Self::Transport(kind) => match kind {
                io::ErrorKind::AddrInUse => Some("本地端口 61440 同一时刻只能被一个客户端占用"),
                io::ErrorKind::NetworkDown
                | io::ErrorKind::NetworkUnreachable
                | io::ErrorKind::AddrNotAvailable => Some("网卡地址可能已变化，请点“刷新”后重试"),
                io::ErrorKind::PermissionDenied => Some("请以管理员身份运行"),
                _ => None,
            },
            Self::Route(RouteError::AccessDenied) => Some("添加认证服务器路由需要管理员权限"),
            Self::Challenge(_) => {
                Some("该地址可能不是本部署的认证服务器，请检查配置里的 auth_server")
            }
            Self::Configuration(_) | Self::Credentials(_) => {
                Some("请检查 drcom.ini 与界面上的账号密码")
            }
            Self::LogoutPacket(_) => {
                Some("下线报文构造失败，服务端会话可能仍保留，通常会在超时后自行释放")
            }
            // The server's own message is the guidance for a plain rejection, and
            // any other route failure has to be read from the error text itself.
            Self::Rejected(_) | Self::Route(_) | Self::Stopped => None,
        }
    }
}

impl From<RetryConfigError> for SessionError {
    fn from(value: RetryConfigError) -> Self {
        Self::Configuration(value)
    }
}

impl From<BuildError> for SessionError {
    fn from(value: BuildError) -> Self {
        Self::Credentials(value)
    }
}

impl From<io::Error> for SessionError {
    fn from(value: io::Error) -> Self {
        Self::Transport(value.kind())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfig {
    /// Authentication servers in priority order.
    pub servers: Vec<Ipv4Addr>,
    pub server_port: u16,
    /// IPv4 address of the adapter that carries the authentication traffic. It
    /// is what the login frame and the type 3 keep-alive report, and it is the
    /// address the socket binds to unless `bind_ipv4` overrides it.
    pub local_ipv4: Ipv4Addr,
    /// Socket bind address, when it has to differ from the reported adapter
    /// address. `None` binds to `local_ipv4`, which is what keeps authentication
    /// traffic on the real adapter; only tests point this elsewhere.
    pub bind_ipv4: Option<Ipv4Addr>,
    pub local_port: u16,
    pub username: Vec<u8>,
    pub password: Vec<u8>,
    pub hostname: Vec<u8>,
    pub mac: u64,
    pub primary_dns: [u8; 4],
    pub dhcp_server: [u8; 4],
    pub auth_version: [u8; 2],
    /// Fallback pair for offsets 6..8 of the 40-byte keep-alive. Both reference
    /// implementations prefer the value the server returns at offsets 28..30 of
    /// the `0xff` reply, because the Python client's own comment records that
    /// the constant it shipped with differed from the captured traffic. This is
    /// used only when the reply is too short to carry one.
    pub keepalive_version: [u8; 2],
    /// Whether the `data3` form folds a checksum into offsets 24..28.
    /// [`KeepAliveChecksum::Zero`] is what the Python client sends and what the
    /// deployment accepts; folding is the Java/Android behaviour.
    pub keepalive_checksum: KeepAliveChecksum,
    pub control_status: u8,
    pub adapter_num: u8,
    pub ip_dog: u8,
    pub request_timeout: Duration,
    pub keepalive_interval: Duration,
    /// Send attempts before a keep-alive gap is reported as a failure.
    pub keepalive_retries: u8,
    /// Only tests set this; a campus deployment lists routable servers.
    pub allow_loopback_server: bool,
    /// Whether the session installs the `/32` route the original keeps for the
    /// authentication server. [`RouteMode::Off`] by default: the route changes
    /// the machine's routing table and needs an elevated process.
    pub auth_route: RouteMode,
}

impl SessionConfig {
    pub fn new(servers: Vec<Ipv4Addr>, username: Vec<u8>, password: Vec<u8>) -> Self {
        Self {
            servers,
            server_port: crate::transport::DEFAULT_LOCAL_PORT,
            local_ipv4: Ipv4Addr::UNSPECIFIED,
            bind_ipv4: None,
            local_port: crate::transport::DEFAULT_LOCAL_PORT,
            username,
            password,
            hostname: Vec::new(),
            // The default MAC used when the adapter cannot be queried.
            mac: 0x8888_8888_8888,
            primary_dns: [10, 10, 10, 10],
            dhcp_server: [0, 0, 0, 0],
            auth_version: [0x68, 0x00],
            keepalive_version: [0xdc, 0x02],
            keepalive_checksum: KeepAliveChecksum::Zero,
            control_status: 0x20,
            adapter_num: 0x03,
            ip_dog: 0x01,
            request_timeout: Duration::from_millis(3000),
            keepalive_interval: Duration::from_millis(20_000),
            keepalive_retries: 3,
            allow_loopback_server: false,
            auth_route: RouteMode::Off,
        }
    }
}

/// A live authentication session. The socket is opened lazily on the first
/// send so constructing a session never touches the network.
pub struct LoginSession {
    config: SessionConfig,
    retry: LoginRetry,
    transport: Option<UdpTransport>,
    /// Sequence used for the *next* challenge; `run` starts one below
    /// [`FIRST_CHALLENGE_SEQUENCE`] because the helper increments before send.
    sequence: u8,
    salt: [u8; 4],
    cookie: [u8; 16],
    /// Version pair the 40-byte keep-alive currently advertises. Starts at the
    /// configured fallback and is replaced as soon as the server returns one.
    keepalive_version: [u8; 2],
    server: Option<SocketAddrV4>,
    stop: Arc<AtomicBool>,
    connected_since: Option<Instant>,
}

impl LoginSession {
    pub fn new(config: SessionConfig) -> Result<Self, SessionError> {
        let retry = if config.allow_loopback_server {
            LoginRetry::new_accepting_loopback(config.servers.clone())?
        } else {
            LoginRetry::new(config.servers.clone())?
        };
        let keepalive_version = config.keepalive_version;
        Ok(Self {
            config,
            retry,
            transport: None,
            sequence: FIRST_CHALLENGE_SEQUENCE.wrapping_sub(1),
            salt: [0; 4],
            cookie: [0; 16],
            keepalive_version,
            server: None,
            stop: Arc::new(AtomicBool::new(false)),
            connected_since: None,
        })
    }

    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// Shared stop flag. Setting it makes the session leave its current wait
    /// within [`SLEEP_STEP`] and stop sending.
    pub fn stop_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    /// Lets a caller (CLI, tray, window) end the session from another thread.
    /// The flag is shared rather than replaced, so a caller that has already
    /// raised it stops the session immediately.
    pub fn use_external_stop(&mut self, handle: Arc<AtomicBool>) {
        self.stop = handle;
    }

    pub fn session_cookie(&self) -> [u8; 16] {
        self.cookie
    }

    pub fn connected_for(&self) -> Option<Duration> {
        self.connected_since.map(|start| start.elapsed())
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn open(&mut self) -> Result<(), SessionError> {
        if self.transport.is_none() {
            let bind_ip = self.config.bind_ipv4.unwrap_or(self.config.local_ipv4);
            let transport = UdpTransport::bind(bind_ip, self.config.local_port)?;
            transport.set_read_timeout(Some(self.config.request_timeout))?;
            self.transport = Some(transport);
        }
        Ok(())
    }

    fn transport(&self) -> Result<&UdpTransport, SessionError> {
        self.transport
            .as_ref()
            .ok_or(SessionError::Transport(io::ErrorKind::NotConnected))
    }

    fn active_server(&self) -> Result<SocketAddrV4, SessionError> {
        self.server
            .ok_or(SessionError::Challenge("尚未选定认证服务器"))
    }

    /// Waits for one datagram from the authentication server that answered the
    /// challenge; anything else is discarded, which is what stops a stray
    /// broadcast from being treated as a valid reply.
    fn receive_from_server(&self, stage: &'static str) -> Result<Vec<u8>, SessionError> {
        let expected = self.active_server()?;
        let transport = self.transport()?;
        let deadline = Instant::now() + self.config.request_timeout;
        let mut buffer = [0u8; 2048];
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|value| !value.is_zero())
                .ok_or(SessionError::NoResponse(stage))?;
            transport.set_read_timeout(Some(remaining))?;
            match transport.receive(&mut buffer) {
                Ok((length, source)) => {
                    if source == SocketAddr::V4(expected) {
                        return Ok(buffer[..length].to_vec());
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(SessionError::NoResponse(stage));
                }
                // A latched ICMP error from an earlier datagram says nothing
                // about this reply, so keep waiting until the deadline.
                Err(error) if crate::transport::is_transient_icmp(&error) => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn discard_pending_datagrams(&self) {
        let Ok(transport) = self.transport() else {
            return;
        };
        if transport.set_read_timeout(Some(SLEEP_STEP)).is_err() {
            return;
        }
        let mut buffer = [0u8; 2048];
        while let Ok((length, source)) = transport.receive(&mut buffer) {
            if self
                .active_server()
                .is_ok_and(|expected| source == SocketAddr::V4(expected))
                && length == 0
            {
                break;
            }
        }
    }

    /// Interruptible sleep. Returns `false` once the session has been stopped.
    fn sleep(&self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            if self.stopped() {
                return false;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            thread::sleep(remaining.min(SLEEP_STEP));
        }
        !self.stopped()
    }

    fn reference_config(&self) -> ReferenceLoginConfig {
        ReferenceLoginConfig {
            host_ipv4: self.config.local_ipv4.octets(),
            mac: self.config.mac,
            hostname: self.config.hostname.clone(),
            primary_dns: self.config.primary_dns,
            dhcp_server: self.config.dhcp_server,
            auth_version: self.config.auth_version,
            control_status: self.config.control_status,
            adapter_num: self.config.adapter_num,
            ip_dog: self.config.ip_dog,
        }
    }

    /// Requests one accepted challenge, reusing the shared login retry counter
    /// so the challenge and the login send cannot each get their own budget.
    fn request_challenge(
        &mut self,
        events: &mut dyn FnMut(SessionEvent),
    ) -> Result<ChallengeReceipt, SessionError> {
        self.open()?;
        let server_port = self.config.server_port;
        let timeout = self.config.request_timeout;
        // Borrow the socket field directly so the mutable borrows of `retry` and
        // `sequence` below stay disjoint from it.
        let transport = self
            .transport
            .as_ref()
            .ok_or(SessionError::Transport(io::ErrorKind::NotConnected))?;
        let challenge = auth::connect_challenge_with(
            transport,
            &mut self.retry,
            server_port,
            &mut self.sequence,
            CHALLENGE_CLIENT_OPTION,
            timeout,
            || reference_login::challenge_nonce(unix_seconds()),
        )
        .map_err(|error| match error {
            ChallengeConnectError::AllServersTimedOut => SessionError::NoResponse("挑战"),
            ChallengeConnectError::Exchange(ChallengeExchangeError::TimedOut) => {
                SessionError::NoResponse("挑战")
            }
            ChallengeConnectError::Exchange(ChallengeExchangeError::Rejected) => {
                SessionError::Challenge("服务器拒绝了挑战请求")
            }
            ChallengeConnectError::Exchange(ChallengeExchangeError::Io(error)) => {
                SessionError::Transport(error.kind())
            }
        })?;
        // Only the server that answered is accepted from now on.
        self.server = match challenge.source {
            SocketAddr::V4(address) => Some(address),
            SocketAddr::V6(_) => None,
        };
        self.salt = challenge.challenge;
        events(SessionEvent::Note(format!(
            "挑战已接受：salt={} 序号={} 来自 {}",
            hex(&challenge.challenge),
            self.sequence,
            challenge.source
        )));
        Ok(challenge)
    }

    /// Full login: challenge, login frame, response handling and retries.
    pub fn login(&mut self, events: &mut dyn FnMut(SessionEvent)) -> Result<(), SessionError> {
        let reference = self.reference_config();
        loop {
            if self.stopped() {
                return Err(SessionError::Stopped);
            }
            events(SessionEvent::Phase(SessionPhase::Challenge));
            self.request_challenge(events)?;
            let server = self.active_server()?;

            events(SessionEvent::Phase(SessionPhase::Authenticating));
            let frame = reference_login::build_login_packet(
                &self.salt,
                &self.config.username,
                &self.config.password,
                &reference,
            )?;
            events(SessionEvent::Note(format!(
                "登录报文 {} 字节，发往 {}",
                frame.len(),
                server
            )));
            self.transport()?.send(server, &frame)?;

            let datagram = match self.receive_from_server("登录") {
                Ok(datagram) => datagram,
                Err(SessionError::NoResponse(stage)) => {
                    events(SessionEvent::Note(format!("{stage} 响应超时")));
                    self.retry_or_fail(events, "登录")?;
                    continue;
                }
                Err(error) => return Err(error),
            };

            match login_response::parse(&datagram) {
                LoginResponse::Success(success) => {
                    self.cookie = success.session_cookie;
                    self.connected_since = Some(Instant::now());
                    events(SessionEvent::Note(format!(
                        "认证成功，会话尾码 {}",
                        hex(&success.session_cookie)
                    )));
                    events(SessionEvent::Phase(SessionPhase::Online));
                    return Ok(());
                }
                LoginResponse::Failure(failure) => {
                    // A server-side rejection (wrong password, account in use,
                    // address mismatch) is not fixed by sending the same frame
                    // again, so the loop stops instead of hammering the server.
                    events(SessionEvent::Note(format!(
                        "认证被拒绝，错误码 {}",
                        failure.code
                    )));
                    return Err(SessionError::Rejected(failure));
                }
                LoginResponse::Other(kind) => {
                    events(SessionEvent::Note(format!(
                        "未识别的响应类型 0x{kind:02x}，重新认证"
                    )));
                }
                LoginResponse::TooShort => {
                    events(SessionEvent::Note("响应过短，重新认证".to_string()));
                }
            }
            self.retry_or_fail(events, "登录")?;
        }
    }

    /// Consumes one send from the shared login budget.
    fn retry_or_fail(
        &mut self,
        events: &mut dyn FnMut(SessionEvent),
        stage: &'static str,
    ) -> Result<(), SessionError> {
        match self.retry.on_login_timeout() {
            LoginTimeout::AllServersExhausted => Err(SessionError::NoResponse(stage)),
            LoginTimeout::RetrySameServer(server) => {
                events(SessionEvent::Note(format!("重试{stage}，仍发往 {server}")));
                if !self.sleep(RETRY_DELAY) {
                    return Err(SessionError::Stopped);
                }
                Ok(())
            }
            LoginTimeout::SwitchServer(server) => {
                events(SessionEvent::Note(format!(
                    "{stage}改用认证服务器 {server}"
                )));
                if !self.sleep(RETRY_DELAY) {
                    return Err(SessionError::Stopped);
                }
                Ok(())
            }
        }
    }

    /// Sends the primary keep-alive (`0xff`) and waits for any `0x07` reply.
    fn send_primary_keepalive(
        &mut self,
        events: &mut dyn FnMut(SessionEvent),
    ) -> Result<(), SessionError> {
        let server = self.active_server()?;
        let digest = reference_login::first_digest(&self.salt, &self.config.password);
        let packet = keepalive::primary_keepalive(digest, self.cookie, unix_seconds());
        for _ in 0..self.config.keepalive_retries {
            self.transport()?.send(server, &packet)?;
            match self.receive_from_server("主保活") {
                Ok(datagram) if datagram.first() == Some(&0x07) => {
                    self.adopt_keepalive_version(&datagram, events);
                    return Ok(());
                }
                Ok(_) => continue,
                Err(SessionError::NoResponse(_)) => continue,
                Err(error) => return Err(error),
            }
        }
        events(SessionEvent::Note("主保活连续无响应".to_string()));
        Err(SessionError::NoResponse("主保活"))
    }

    /// Takes the keep-alive version pair from a server reply that carries one.
    ///
    /// The Python client hard-codes this pair, and its own source notes that the
    /// value it captured from the wire differed, so the reply is the better
    /// source. The Java rewrite and the Android port both read it here.
    fn adopt_keepalive_version(&mut self, reply: &[u8], events: &mut dyn FnMut(SessionEvent)) {
        let Some(version) = negotiated_version(reply) else {
            return;
        };
        if version == self.keepalive_version {
            return;
        }
        events(SessionEvent::Note(format!(
            "保活版本 {} 改为 {}（取自服务器响应）",
            hex(&self.keepalive_version),
            hex(&version)
        )));
        self.keepalive_version = version;
    }

    /// Sends one type 1 or type 3 keep-alive and returns the accepted reply.
    ///
    /// A `07 .. 10` reply is the server pushing a message, not an
    /// acknowledgement of the keep-alive. Both reference implementations treat
    /// it as a prompt to resend the same form with the next counter, so that is
    /// what happens here, bounded by the retry budget. The counter advances only
    /// inside this call; the caller's sequence stays one short, which both
    /// reference clients also allow because they match on `07 00 28 00`.
    fn send_keepalive_packet(
        &mut self,
        events: &mut dyn FnMut(SessionEvent),
        mut sequence: u8,
        tail: [u8; 4],
        packet_type: KeepAlivePacketType,
        mut first: bool,
        handshake: bool,
    ) -> Result<Vec<u8>, SessionError> {
        let server = self.active_server()?;
        let mut last = Err(SessionError::NoResponse(keepalive_stage(packet_type)));
        for _ in 0..self.config.keepalive_retries {
            let packet = keepalive::build_packet_negotiated(
                sequence,
                tail,
                packet_type,
                first,
                keepalive::REFERENCE_NONCE,
                self.keepalive_version,
                self.config.local_ipv4.octets(),
                self.config.keepalive_checksum,
            );
            self.transport()?.send(server, &packet)?;
            match self.receive_from_server(keepalive_stage(packet_type)) {
                Ok(datagram) => {
                    if is_message_packet(&datagram) {
                        events(SessionEvent::Note(format!(
                            "服务器推送 {} 字节消息，保活序号推进到 {}",
                            datagram.len(),
                            sequence.wrapping_add(1)
                        )));
                        sequence = sequence.wrapping_add(1);
                        // The Python client resends with the negotiated version
                        // rather than the `0f 27` extra marker, so the extra form
                        // only ever opens a keep-alive run. That is why the first
                        // send here is `first=true` while every resend is not.
                        first = false;
                        last = Ok(datagram);
                        continue;
                    }
                    if classify_response(&datagram, sequence, handshake)
                        != KeepAliveResponse::Invalid
                    {
                        self.adopt_keepalive_version(&datagram, events);
                        return Ok(datagram);
                    }
                    last = Ok(datagram);
                }
                Err(error) => last = Err(error),
            }
        }
        last
    }

    /// One keep-alive session: primary packet, the type 1 / type 3 pair, the
    /// extra packet on every tenth round, then the 20-second cycle. Returns
    /// `Ok(())` once the session is stopped and `NoResponse` when the server
    /// goes quiet.
    ///
    /// The counter is the one `DrcomTask.alive` keeps: every 40-byte packet
    /// takes one, the extra form included, and the extra form goes out whenever
    /// that counter has reached a multiple of [`EXTRA_EVERY`]. The tail handed
    /// to the pair is the one the previous pair's replies carried, which is what
    /// both reference clients do — the login cookie is only ever used by the
    /// 42-byte primary packet.
    fn keepalive_session(
        &mut self,
        events: &mut dyn FnMut(SessionEvent),
    ) -> Result<(), SessionError> {
        let mut counter = 0u8;
        let mut cycle_tail = [0u8; 4];

        loop {
            self.send_primary_keepalive(events)?;

            if counter.is_multiple_of(EXTRA_EVERY) {
                // Neither reference client reads the extra reply's tail, so the
                // answer is deliberately dropped here too.
                self.send_keepalive_packet(
                    events,
                    counter,
                    cycle_tail,
                    KeepAlivePacketType::Type1,
                    true,
                    true,
                )?;
                counter = counter.wrapping_add(1);
            }

            let response = self.send_keepalive_packet(
                events,
                counter,
                cycle_tail,
                KeepAlivePacketType::Type1,
                false,
                false,
            )?;
            counter = counter.wrapping_add(1);
            update_tail(&mut cycle_tail, &response);

            let response = self.send_keepalive_packet(
                events,
                counter,
                cycle_tail,
                KeepAlivePacketType::Type3,
                false,
                false,
            )?;
            counter = counter.wrapping_add(1);
            update_tail(&mut cycle_tail, &response);

            events(SessionEvent::KeepAliveCycle {
                sequence: counter,
                tail: cycle_tail,
            });

            if !self.sleep(self.config.keepalive_interval) {
                return Ok(());
            }
        }
    }

    /// Runs keep-alive until the stop flag is set. A silent server restarts the
    /// session from the primary packet a bounded number of times; the original
    /// never gives up, but an unbounded loop would make a failure invisible.
    pub fn run_keepalive(
        &mut self,
        events: &mut dyn FnMut(SessionEvent),
    ) -> Result<(), SessionError> {
        events(SessionEvent::Phase(SessionPhase::KeepAlive));
        let mut restarts = 0u8;
        loop {
            if self.stopped() {
                return Ok(());
            }
            match self.keepalive_session(events) {
                Ok(()) => return Ok(()),
                Err(SessionError::NoResponse(stage)) => {
                    restarts = restarts.saturating_add(1);
                    if restarts >= self.config.keepalive_retries {
                        return Err(SessionError::NoResponse(stage));
                    }
                    events(SessionEvent::Note(format!(
                        "{stage}失败，重新进入主保活 ({restarts}/{})",
                        self.config.keepalive_retries
                    )));
                    self.discard_pending_datagrams();
                    if !self.sleep(RESYNC_DELAY) {
                        return Ok(());
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Proactive logout in the order the original uses: a client ping, a short
    /// pause, a fresh challenge from the online server, then the 80-byte frame.
    /// The logout response type is not decoded yet, so the reply is only
    /// reported and the frame is never resent on failure.
    pub fn logout(&mut self, events: &mut dyn FnMut(SessionEvent)) -> Result<(), SessionError> {
        let server = self.active_server()?;
        let digest = reference_login::first_digest(&self.salt, &self.config.password);
        let ping = keepalive::primary_keepalive(digest, self.cookie, unix_seconds());
        self.sequence = self.sequence.wrapping_add(1);
        let sequence = self.sequence;
        let receipt = auth::exchange_logout_challenge(
            self.transport()?,
            server,
            &ping,
            sequence,
            reference_login::challenge_nonce(unix_seconds()),
            CHALLENGE_CLIENT_OPTION,
            self.config.request_timeout,
        )
        .map_err(|error| match error {
            ChallengeExchangeError::TimedOut => SessionError::NoResponse("下线挑战"),
            ChallengeExchangeError::Rejected => SessionError::Challenge("服务器拒绝了下线挑战请求"),
            ChallengeExchangeError::Io(error) => SessionError::Transport(error.kind()),
        })?;

        let mac: [u8; 6] = self.config.mac.to_be_bytes()[2..8]
            .try_into()
            .expect("MAC is six bytes");
        let frame = protocol::logout_packet(
            receipt.challenge,
            &self.config.username,
            &self.config.password,
            self.config.control_status,
            sequence,
            mac,
            self.cookie,
        )
        .map_err(SessionError::LogoutPacket)?;
        self.transport()?.send(server, &frame)?;
        match self.receive_from_server("下线") {
            Ok(datagram) => events(SessionEvent::Note(format!(
                "下线响应 0x{:02x}，{} 字节",
                datagram.first().copied().unwrap_or_default(),
                datagram.len()
            ))),
            Err(SessionError::NoResponse(_)) => {
                events(SessionEvent::Note(
                    "下线响应超时，连接已交由服务器回收".to_string(),
                ));
            }
            Err(error) => return Err(error),
        }
        events(SessionEvent::Phase(SessionPhase::LoggedOut));
        Ok(())
    }

    /// Installs the `/32` route the original keeps for its authentication
    /// server. Off by default; a dry run reports the plan without changing the
    /// table. When the caller asked for real management and no listed server
    /// ended up routable, the session fails before authenticating rather than
    /// waiting out a timeout against an address it cannot reach.
    fn install_auth_route(
        &self,
        events: &mut dyn FnMut(SessionEvent),
    ) -> Result<Vec<InstalledRoute>, SessionError> {
        if !self.config.auth_route.is_enabled() {
            return Ok(Vec::new());
        }
        let applying = self.config.auth_route.applies();
        let mut notes: Vec<String> = Vec::new();
        let outcome = route::install_for(
            &self.config.servers,
            self.config.auth_route,
            |plan, action, result| {
                let action = match action {
                    route::RouteAction::Add => "添加",
                    route::RouteAction::Delete => "删除",
                };
                notes.push(match result {
                    Ok(()) if applying => format!(
                        "{action}认证服务器路由 {} 经由 {} IF {}",
                        plan.server, plan.gateway, plan.interface_index
                    ),
                    Ok(()) => format!(
                        "认证服务器路由（试运行，未修改路由表）{} 经由 {} IF {}",
                        plan.server, plan.gateway, plan.interface_index
                    ),
                    Err(error) => format!("{action}认证服务器路由 {} 失败：{error:?}", plan.server),
                });
            },
        );
        for note in notes {
            events(SessionEvent::Note(note));
        }
        if applying
            && !self.config.servers.is_empty()
            && !outcome.is_satisfied(self.config.servers.len())
        {
            let error = outcome
                .failed
                .first()
                .map(|(_, error)| *error)
                .unwrap_or(RouteError::Refused(0));
            return Err(SessionError::Route(error));
        }
        Ok(outcome.installed)
    }

    /// Removes exactly the routes this session installed, so a route owned by
    /// another Dr.COM component is never torn down.
    fn restore_auth_route(
        &self,
        events: &mut dyn FnMut(SessionEvent),
        installed: Vec<InstalledRoute>,
    ) {
        for route in installed {
            match route.remove() {
                Ok(()) => events(SessionEvent::Note(format!(
                    "已移除认证服务器路由 {}",
                    route.plan.server
                ))),
                Err(error) => events(SessionEvent::Note(format!(
                    "移除认证服务器路由 {} 失败：{error:?}",
                    route.plan.server
                ))),
            }
        }
    }

    /// Login, keep-alive until stopped, then a best-effort logout.
    ///
    /// The authentication-server route is installed before login and removed
    /// after logout, which is the order the original uses; routing the teardown
    /// through a single exit keeps the table as it was found even when login or
    /// keep-alive fails.
    pub fn run(&mut self, events: &mut dyn FnMut(SessionEvent)) -> Result<(), SessionError> {
        let installed = match self.install_auth_route(events) {
            Ok(installed) => installed,
            Err(error) => {
                events(SessionEvent::Note(error.to_string()));
                return Err(error);
            }
        };
        let outcome = self.run_online(events);
        self.restore_auth_route(events, installed);
        outcome
    }

    fn run_online(&mut self, events: &mut dyn FnMut(SessionEvent)) -> Result<(), SessionError> {
        self.login(events)?;
        match self.run_keepalive(events) {
            Ok(()) | Err(SessionError::Stopped) => {}
            Err(error) => return Err(error),
        }
        match self.logout(events) {
            Ok(()) => Ok(()),
            Err(SessionError::Stopped) => Ok(()),
            Err(error) => {
                events(SessionEvent::Note(format!("下线失败：{error}")));
                Ok(())
            }
        }
    }
}

fn keepalive_stage(packet_type: KeepAlivePacketType) -> &'static str {
    match packet_type {
        KeepAlivePacketType::Type1 => "保活 type1",
        KeepAlivePacketType::Type3 => "保活 type3",
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reference_vectors::vector;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    const SALT: [u8; 4] = [0x1a, 0x2b, 0x3c, 0x4d];
    const COOKIE: [u8; 16] = [
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e,
        0x1f,
    ];

    /// Loopback stand-in for the authentication server. `behaviour` decides how
    /// it answers so each test can drive one branch.
    enum Behaviour {
        /// Challenge, then a successful login, then keep-alive until the client
        /// stops, then serve any further datagram so a race cannot fail the test.
        Serving,
        /// Reject the login frame with error code 5.
        RejectLogin,
        /// Answer the first keep-alive with a `07 .. 10` message packet, then
        /// behave like `Serving`.
        AnnounceMessage,
        /// Behave like `Serving`, but keep answering for eleven complete rounds
        /// so that the extra packet's ten-round period becomes visible.
        ElevenCycles,
        /// Never answer.
        Silent,
    }

    fn challenge_reply(sequence: u8, nonce: u16) -> Vec<u8> {
        let mut reply = vec![0u8; 0x2c];
        reply[0] = 0x02;
        reply[1] = sequence;
        reply[2..4].copy_from_slice(&nonce.to_le_bytes());
        reply[4..8].copy_from_slice(&SALT);
        reply[0x2a..0x2c].copy_from_slice(&0x8001u16.to_le_bytes());
        reply
    }

    /// Version pair the mock advertises at offsets 28..30. Deliberately not the
    /// `dc 02` the reference client hard-codes, so a test can prove the client
    /// prefers the server's value over its own constant.
    const NEGOTIATED: [u8; 2] = [0x34, 0x12];

    /// A keep-alive reply. `message` selects the `07 .. 10` form the server uses
    /// to push a message, which is not an acknowledgement of the keep-alive.
    fn keepalive_reply(sequence: u8, message: bool, version: [u8; 2]) -> Vec<u8> {
        let mut reply = vec![0u8; 30];
        reply[0] = 0x07;
        reply[1] = sequence;
        reply[2] = if message { 0x10 } else { 0x28 };
        reply[16..20].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        reply[28..30].copy_from_slice(&version);
        reply
    }

    fn spawn_mock(
        behaviour: Behaviour,
        stop: Arc<AtomicBool>,
        observed: mpsc::Sender<Vec<Vec<u8>>>,
    ) -> SocketAddrV4 {
        let server = UdpTransport::bind(Ipv4Addr::LOCALHOST, 0).unwrap();
        let address = match server.local_addr().unwrap() {
            SocketAddr::V4(address) => address,
            SocketAddr::V6(_) => unreachable!(),
        };
        server
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        thread::spawn(move || {
            let mut buffer = [0u8; 2048];
            let mut seen: Vec<Vec<u8>> = Vec::new();
            let mut keepalive_replies = 0usize;
            let mut type3_seen = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let Ok((length, source)) = server.receive(&mut buffer) else {
                    continue;
                };
                let datagram = buffer[..length].to_vec();
                let Some(client) = (match source {
                    SocketAddr::V4(address) => Some(address),
                    SocketAddr::V6(_) => None,
                }) else {
                    continue;
                };
                let reply: Option<Vec<u8>> = match datagram.first() {
                    Some(0x01) => match behaviour {
                        Behaviour::Silent => None,
                        _ => Some(challenge_reply(
                            datagram[1],
                            u16::from_le_bytes([datagram[2], datagram[3]]),
                        )),
                    },
                    Some(0x03) => match behaviour {
                        Behaviour::RejectLogin => {
                            let mut reply = vec![0u8; 5];
                            reply[0] = 0x05;
                            reply[4] = 5;
                            Some(reply)
                        }
                        _ => {
                            let mut reply = vec![0u8; 39];
                            reply[0] = 0x04;
                            reply[23..39].copy_from_slice(&COOKIE);
                            Some(reply)
                        }
                    },
                    Some(0xff) => match behaviour {
                        Behaviour::Silent => None,
                        _ => Some(keepalive_reply(0, false, NEGOTIATED)),
                    },
                    Some(0x07) => {
                        let packet_type = datagram[5];
                        let message = matches!(behaviour, Behaviour::AnnounceMessage)
                            && keepalive_replies == 0;
                        if packet_type == 3 {
                            // One complete cycle is enough for most tests; the
                            // extra-packet test needs ten more to see a repeat.
                            let wanted = match behaviour {
                                Behaviour::ElevenCycles => 11,
                                _ => 1,
                            };
                            type3_seen += 1;
                            if type3_seen >= wanted {
                                stop.store(true, Ordering::Relaxed);
                            }
                        }
                        keepalive_replies += 1;
                        Some(keepalive_reply(datagram[1], message, NEGOTIATED))
                    }
                    _ => None,
                };
                if let Some(reply) = reply {
                    let _ = server.send(client, &reply);
                }
                if length > 0 {
                    seen.push(datagram);
                }
                if stop.load(Ordering::Relaxed) {
                    let _ = observed.send(seen.clone());
                    // Serve shutdown traffic for a moment so the client's last
                    // packets are never mistaken for a silent server.
                    let deadline = Instant::now() + Duration::from_secs(1);
                    while Instant::now() < deadline {
                        if let Ok((length, source)) = server.receive(&mut buffer)
                            && let SocketAddr::V4(client) = source
                        {
                            if buffer[..length].first() == Some(&0x07) {
                                let _ = server
                                    .send(client, &keepalive_reply(buffer[1], false, NEGOTIATED));
                            } else {
                                let _ = server.send(client, &keepalive_reply(0, false, NEGOTIATED));
                            }
                        }
                    }
                    return;
                }
            }
            let _ = observed.send(seen);
        });
        address
    }

    fn session_config(server_port: u16) -> SessionConfig {
        let mut config = SessionConfig::new(
            vec![Ipv4Addr::LOCALHOST],
            b"testuser".to_vec(),
            b"testpass".to_vec(),
        );
        config.server_port = server_port;
        // The frame reports 10.0.0.9 so it can be compared with the reference
        // vector, while the socket stays on loopback for the mock server.
        config.local_ipv4 = Ipv4Addr::new(10, 0, 0, 9);
        config.bind_ipv4 = Some(Ipv4Addr::LOCALHOST);
        config.local_port = 0;
        config.hostname = b"TESTHOST".to_vec();
        config.mac = 0x1122_8877_6655;
        config.request_timeout = Duration::from_millis(500);
        config.keepalive_interval = Duration::from_millis(200);
        config.keepalive_retries = 2;
        config.allow_loopback_server = true;
        config
    }

    #[test]
    fn stopping_during_keepalive_receive_still_sends_logout() {
        let server = UdpTransport::bind(Ipv4Addr::LOCALHOST, 0).unwrap();
        server
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let port = server.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_server = stop.clone();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut buffer = [0u8; 2048];
            while Instant::now() < deadline {
                let Ok((length, SocketAddr::V4(client))) = server.receive(&mut buffer) else {
                    continue;
                };
                if length == 0 {
                    continue;
                }
                let reply = match buffer[0] {
                    0x01 => Some(challenge_reply(
                        buffer[1],
                        u16::from_le_bytes([buffer[2], buffer[3]]),
                    )),
                    0x03 => {
                        let mut reply = vec![0u8; 39];
                        reply[0] = 0x04;
                        reply[23..39].copy_from_slice(&COOKIE);
                        Some(reply)
                    }
                    0xff => {
                        stop_server.store(true, Ordering::Relaxed);
                        None
                    }
                    0x06 => return true,
                    _ => None,
                };
                if let Some(reply) = reply {
                    server.send(client, &reply).unwrap();
                }
            }
            false
        });
        let mut session = LoginSession::new(session_config(port)).unwrap();
        session.use_external_stop(stop);
        session.run(&mut |_| {}).unwrap();
        assert!(
            worker.join().unwrap(),
            "stopping while awaiting keepalive must send the logout frame"
        );
    }

    #[test]
    fn login_sends_the_reference_frame_and_captures_the_cookie() {
        let (sender, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let server = spawn_mock(Behaviour::Serving, Arc::clone(&stop), sender);
        let mut session = LoginSession::new(session_config(server.port())).unwrap();

        let mut notes = Vec::new();
        session
            .login(&mut |event| {
                if let SessionEvent::Note(note) = event {
                    notes.push(note);
                }
            })
            .unwrap();
        assert_eq!(session.session_cookie(), COOKIE);

        // The mock always answers with the pinned salt, so the frame the client
        // put on the wire must equal the reference implementation's bytes.
        stop.store(true, Ordering::Relaxed);
        let observed = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        let challenge = &observed[0];
        assert_eq!(challenge.len(), 20);
        assert_eq!(challenge[0], 0x01);
        assert_eq!(challenge[1], FIRST_CHALLENGE_SEQUENCE);
        assert_eq!(challenge[4], CHALLENGE_CLIENT_OPTION);
        assert_eq!(&observed[1], &vector("login.user8.pwd8"));
        assert!(notes.iter().any(|note| note.contains("认证成功")));
    }

    #[test]
    fn login_rejection_stops_without_retrying() {
        let (sender, _receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let server = spawn_mock(Behaviour::RejectLogin, Arc::clone(&stop), sender);
        let mut session = LoginSession::new(session_config(server.port())).unwrap();

        let error = session.login(&mut |_| {}).unwrap_err();
        assert_eq!(
            error,
            SessionError::Rejected(LoginFailure {
                code: 5,
                ip: None,
                mac: None,
                message: None,
            })
        );
        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn challenge_timeout_exhausts_the_shared_retry_budget() {
        let (sender, _receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let server = spawn_mock(Behaviour::Silent, Arc::clone(&stop), sender);
        let mut config = session_config(server.port());
        config.request_timeout = Duration::from_millis(50);
        config.servers = vec![Ipv4Addr::LOCALHOST];
        let mut session = LoginSession::new(config).unwrap();

        let error = session.login(&mut |_| {}).unwrap_err();
        // One server, three sends: the fourth timeout reports exhaustion.
        assert_eq!(error, SessionError::NoResponse("挑战"));
        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn keepalive_runs_one_cycle_then_stops_cleanly() {
        let (sender, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let server = spawn_mock(Behaviour::Serving, Arc::clone(&stop), sender);
        let mut session = LoginSession::new(session_config(server.port())).unwrap();
        // The mock decides when the client should stop, so it has to drive the
        // client's stop flag rather than a flag private to the test.
        session.use_external_stop(Arc::clone(&stop));

        let mut cycles = 0usize;
        session
            .run(&mut |event| {
                if matches!(event, SessionEvent::KeepAliveCycle { .. }) {
                    cycles += 1;
                }
            })
            .unwrap();
        assert!(cycles >= 1, "the session must complete a keep-alive cycle");
        assert!(stop.load(Ordering::Relaxed));

        let observed = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        let keepalive: Vec<&Vec<u8>> = observed
            .iter()
            .filter(|datagram| datagram.first() == Some(&0x07))
            .collect();
        assert_eq!(
            keepalive[0][5], 1,
            "first packet after the primary is the extra form"
        );
        assert_eq!(
            &keepalive[0][6..8],
            &[0x0f, 0x27],
            "the extra form is a handshake"
        );
        assert_eq!(keepalive[0][1], 0);
        // Every 40-byte packet takes one count, the extra form included, so the
        // extra packet and the first data1 do not share a number.
        assert_eq!(
            keepalive[1][1], 1,
            "the type 1 pair follows the extra packet"
        );
        // The mock advertises a version the client's own constant does not
        // carry, so these bytes prove the reply was adopted rather than ignored.
        assert_eq!(&keepalive[1][6..8], &NEGOTIATED);
        assert_eq!(keepalive[2][5], 3, "then the type 3 packet");
        assert_eq!(keepalive[2][1], 2);
        // The reference policy leaves the checksum slot zero.
        assert_eq!(&keepalive[2][24..28], &[0u8; 4]);

        let primary = observed
            .iter()
            .find(|datagram| datagram.first() == Some(&0xff))
            .expect("the primary keep-alive must be sent first");
        assert_eq!(primary.len(), 42);
        let digest = reference_login::first_digest(&SALT, b"testpass");
        assert_eq!(&primary[1..17], &digest);
        assert_eq!(&primary[20..36], &COOKIE);
        assert!(primary[38..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn a_message_packet_is_resent_rather_than_taken_as_an_acknowledgement() {
        let (sender, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let server = spawn_mock(Behaviour::AnnounceMessage, Arc::clone(&stop), sender);
        let mut session = LoginSession::new(session_config(server.port())).unwrap();
        session.use_external_stop(Arc::clone(&stop));

        let mut notes = Vec::new();
        session
            .run(&mut |event| {
                if let SessionEvent::Note(note) = event {
                    notes.push(note);
                }
            })
            .unwrap();

        let observed = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        let keepalive: Vec<&Vec<u8>> = observed
            .iter()
            .filter(|datagram| datagram.first() == Some(&0x07))
            .collect();
        // The extra form is sent twice: once, then again with the next counter
        // because the reply was a message packet instead of an acknowledgement.
        assert_eq!(keepalive[0][5], 1);
        assert_eq!(&keepalive[0][6..8], &[0x0f, 0x27]);
        assert_eq!(keepalive[0][1], 0);
        assert_eq!(keepalive[1][5], 1);
        // The resend drops the `0f 27` marker for the negotiated version, which
        // is exactly what the Python client's `first=False` branch produces.
        assert_eq!(&keepalive[1][6..8], &NEGOTIATED);
        assert_eq!(keepalive[1][1], 1, "the resent packet advances the counter");
        assert!(
            notes.iter().any(|note| note.contains("推送")),
            "the message must be reported, got {notes:?}"
        );
    }

    #[test]
    fn the_extra_packet_returns_every_tenth_keep_alive() {
        let (sender, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let server = spawn_mock(Behaviour::ElevenCycles, Arc::clone(&stop), sender);
        let mut session = LoginSession::new(session_config(server.port())).unwrap();
        session.use_external_stop(Arc::clone(&stop));

        session.run(&mut |_| {}).unwrap();

        let observed = receiver.recv_timeout(Duration::from_secs(15)).unwrap();
        let keepalive: Vec<&Vec<u8>> = observed
            .iter()
            .filter(|datagram| datagram.first() == Some(&0x07))
            .collect();
        let extras: Vec<usize> = keepalive
            .iter()
            .enumerate()
            .filter(|(_, packet)| packet[6] == 0x0f && packet[7] == 0x27)
            .map(|(index, _)| index)
            .collect();
        // Round one opens with the extra form and the next one lands ten primary
        // keep-alives later, which is 21 counts in: three for the extra round
        // plus nine ordinary rounds of two.
        assert_eq!(extras, vec![0, 21], "got {extras:?}");
        assert_eq!(
            keepalive[21][1], 21,
            "the counter a packet carries is its own number"
        );
    }
}
