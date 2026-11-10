//! Headless backend host.
//!
//! Runs a [`drcom_service::Service`] — the same application layer the GUI
//! renders in-process — without any window, and exposes it over the
//! [`drcom_ipc`] JSON-lines protocol. Another program — in any language — can
//! therefore drive Dr.COM authentication by spawning this process (stdio) or
//! connecting to its loopback port. (`drcom-cli` deliberately links
//! `drcom-core` directly instead; it is not part of the service/IPC stack.)
//!
//! * stdout is protocol-only in stdio mode; every log/status line goes to stderr;
//! * one ticker thread calls [`drcom_service::Service::tick`] once a second;
//! * a `shutdown` request (or closing stdin in stdio mode) performs a graceful
//!   logout and exits with status 0.
//!
//! ```text
//! drcom-daemon --stdio --no-startup-probe --no-auto-login
//! drcom-daemon --listen 127.0.0.1:47800 --no-auto-login
//! ```
//!
//! The wire schema is documented in `docs/ipc-protocol.md`.

use std::io::{self, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use drcom_ipc::{
    Backend, ConnectionEnd, Handled, Hub, Request, RequestBody, ServerInfo, dispatch,
    write_json_line,
};
use drcom_service::{
    API_VERSION, Command, CommandOutcome, Notification, Service, ServiceOptions, Snapshot,
};

const USAGE: &str = "\
drcom-daemon — Dr.COM 无界面后端宿主（JSON-lines 协议）

用法：
  drcom-daemon [--stdio] [选项]
  drcom-daemon --listen <ip:port> [选项]

传输：
  --stdio                 通过标准输入/输出提供协议（默认）
  --listen <ip:port>      在回环地址上监听 TCP；端口 0 表示自动分配，
                          实际地址会打印到 stderr
  --no-startup-probe      启动时不访问 Dr.COM 网页状态（只读探测）
  --no-auto-login         忽略配置中的自动登录；诊断脚本务必加上，
                          避免自动抢占真实校园网会话
  --help, -h              显示本帮助并退出
  --version, -V           显示版本并退出

说明：
  每行一个 UTF-8 JSON 对象；连接建立后先收到 ready，随后是 state。
  只允许回环地址（127.0.0.1 / ::1 / localhost），因为登录命令会携带明文密码。
  协议文档：docs/ipc-protocol.md
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args) {
        Ok(Invocation::Help) => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok(Invocation::Version) => {
            println!(
                "drcom-daemon {} (protocol {} {}, api {})",
                env!("CARGO_PKG_VERSION"),
                drcom_ipc::PROTOCOL_NAME,
                drcom_ipc::PROTOCOL_VERSION,
                API_VERSION
            );
            ExitCode::SUCCESS
        }
        Ok(Invocation::Run(config)) => match run(config) {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("drcom-daemon: {message}");
                ExitCode::from(1)
            }
        },
        Err(message) => {
            eprintln!("drcom-daemon: {message}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Mode {
    Stdio,
    Tcp(SocketAddr),
}

#[derive(Debug, Clone, Copy)]
struct Config {
    mode: Mode,
    startup_probe: bool,
    auto_login: bool,
}

enum Invocation {
    Help,
    Version,
    Run(Config),
}

fn parse_args(args: &[String]) -> Result<Invocation, String> {
    let mut stdio = false;
    let mut listen: Option<SocketAddr> = None;
    let mut startup_probe = true;
    let mut auto_login = true;
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        match argument {
            "--help" | "-h" => return Ok(Invocation::Help),
            "--version" | "-V" => return Ok(Invocation::Version),
            "--stdio" => stdio = true,
            "--no-startup-probe" => startup_probe = false,
            "--no-auto-login" => auto_login = false,
            "--listen" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    "--listen 需要一个地址，例如 --listen 127.0.0.1:47800".to_owned()
                })?;
                if listen.is_some() {
                    return Err("--listen 只能指定一次".to_owned());
                }
                listen = Some(parse_listen(value)?);
            }
            _ if argument.starts_with("--listen=") => {
                let value = &argument["--listen=".len()..];
                if listen.is_some() {
                    return Err("--listen 只能指定一次".to_owned());
                }
                listen = Some(parse_listen(value)?);
            }
            _ => return Err(format!("未知参数“{argument}”")),
        }
        index += 1;
    }

    let mode = if stdio {
        if listen.is_some() {
            return Err("--stdio 与 --listen 不能同时使用".to_owned());
        }
        Mode::Stdio
    } else {
        match listen {
            Some(address) => Mode::Tcp(address),
            None => Mode::Stdio,
        }
    };

    Ok(Invocation::Run(Config {
        mode,
        startup_probe,
        auto_login,
    }))
}

fn parse_listen(value: &str) -> Result<SocketAddr, String> {
    let address = match value.parse::<SocketAddr>() {
        Ok(address) => address,
        Err(_) => match value.strip_prefix("localhost:") {
            Some(port) => {
                let port: u16 = port
                    .parse()
                    .map_err(|_| format!("无效端口“{value}”，应为 0~65535"))?;
                SocketAddr::from(([127, 0, 0, 1], port))
            }
            None => {
                return Err(format!(
                    "无效监听地址“{value}”，应为 <ip>:<端口>，例如 127.0.0.1:47800"
                ));
            }
        },
    };
    if !address.ip().is_loopback() {
        return Err(format!(
            "拒绝监听非回环地址 {address}：登录命令会携带明文密码，协议只允许本机连接"
        ));
    }
    Ok(address)
}

fn run(config: Config) -> Result<(), String> {
    let service = Service::new(ServiceOptions {
        startup_probe: config.startup_probe,
        auto_login: config.auto_login,
        settings: None,
    });
    let shutdown = Arc::new(ShutdownState::new());
    let info = ServerInfo::new(API_VERSION, env!("CARGO_PKG_VERSION").to_string());
    match config.mode {
        Mode::Stdio => run_stdio(service, shutdown, info),
        Mode::Tcp(address) => run_tcp(service, shutdown, info, address),
    }
}

// ---------------------------------------------------------------------------
// Shared output / notification plumbing
// ---------------------------------------------------------------------------

/// A writer that can be cloned across threads. `Write::write_all` is overridden
/// so a whole protocol line is written under one lock and cannot interleave with
/// another thread's line.
struct SharedWriter<W> {
    inner: Arc<Mutex<W>>,
}

impl<W> Clone for SharedWriter<W> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<W: Write> SharedWriter<W> {
    fn new(writer: W) -> Self {
        Self {
            inner: Arc::new(Mutex::new(writer)),
        }
    }

    fn lock(&self) -> io::Result<MutexGuard<'_, W>> {
        self.inner
            .lock()
            .map_err(|_| io::Error::other("writer lock poisoned"))
    }
}

impl<W: Write> Write for SharedWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.lock()?.write(buffer)
    }

    fn write_all(&mut self, buffer: &[u8]) -> io::Result<()> {
        self.lock()?.write_all(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.lock()?.flush()
    }
}

enum Target {
    Stdout(SharedWriter<io::Stdout>),
    Hub(Hub),
}

/// Forwards service notifications to the active transport.
///
/// A *hold* buffers notifications instead of writing them. The daemon holds the
/// stream while a `shutdown` command runs, so the request response is written
/// before `exit_ready` even when the service emits it synchronously.
struct Notifier {
    target: Target,
    held: Mutex<Option<Vec<Notification>>>,
}

impl Notifier {
    fn stdout(writer: SharedWriter<io::Stdout>) -> Self {
        Self {
            target: Target::Stdout(writer),
            held: Mutex::new(None),
        }
    }

    fn hub(hub: Hub) -> Self {
        Self {
            target: Target::Hub(hub),
            held: Mutex::new(None),
        }
    }

    fn lock_held(&self) -> MutexGuard<'_, Option<Vec<Notification>>> {
        self.held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn begin_hold(&self) {
        *self.lock_held() = Some(Vec::new());
    }

    fn end_hold(&self) {
        let held = self.lock_held().take();
        if let Some(notifications) = held {
            for notification in notifications {
                self.write(notification);
            }
        }
    }

    fn publish(&self, notification: Notification) {
        {
            let mut held = self.lock_held();
            if let Some(notifications) = held.as_mut() {
                notifications.push(notification);
                return;
            }
        }
        self.write(notification);
    }

    fn write(&self, notification: Notification) {
        let result = match &self.target {
            Target::Stdout(writer) => {
                let mut writer = writer.clone();
                write_json_line(&mut writer, &notification)
            }
            Target::Hub(hub) => hub
                .publish(&notification)
                .map_err(|error| io::Error::other(error.to_string())),
        };
        if let Err(error) = result {
            eprintln!("drcom-daemon: 通知写入失败：{error}");
        }
    }
}

// ---------------------------------------------------------------------------
// Application backend
// ---------------------------------------------------------------------------

struct ShutdownState {
    requested: AtomicBool,
    exit_ready: AtomicBool,
    expect_exit_ready: AtomicBool,
    finished: AtomicBool,
}

impl ShutdownState {
    fn new() -> Self {
        Self {
            requested: AtomicBool::new(false),
            exit_ready: AtomicBool::new(false),
            expect_exit_ready: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        }
    }

    /// Blocks the calling thread until the daemon process is about to exit.
    /// Used by TCP connection threads so their subscription outlives the final
    /// `exit_ready` broadcast.
    fn wait_finished(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !self.finished.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
    }
}

struct DaemonBackend {
    service: Service,
    notifier: Arc<Notifier>,
    shutdown: Arc<ShutdownState>,
    info: ServerInfo,
}

impl DaemonBackend {
    fn new(
        service: Service,
        notifier: Arc<Notifier>,
        shutdown: Arc<ShutdownState>,
        info: ServerInfo,
    ) -> Self {
        Self {
            service,
            notifier,
            shutdown,
            info,
        }
    }
}

impl Backend for DaemonBackend {
    fn info(&self) -> ServerInfo {
        self.info.clone()
    }

    fn snapshot(&self) -> Snapshot {
        self.service.snapshot()
    }

    fn execute(&self, command: Command) -> CommandOutcome {
        self.service.execute(command)
    }

    fn handle(&self, request: Request) -> Handled {
        let stopping = match &request.body {
            RequestBody::Shutdown => true,
            RequestBody::Command { command } => matches!(command, Command::Shutdown),
            RequestBody::Snapshot | RequestBody::Ping => false,
        };
        if stopping {
            self.notifier.begin_hold();
        }
        let mut handled = dispatch(self, request);
        if stopping {
            handled.stop = true;
        }
        handled
    }

    fn after_stop(&self) {
        // The response to the shutdown request has been written; release any
        // notifications the service emitted while the command ran.
        self.notifier.end_hold();
        let snapshot = self.service.snapshot();
        if snapshot.connection.session_active {
            self.shutdown
                .expect_exit_ready
                .store(true, Ordering::SeqCst);
        }
        self.shutdown.requested.store(true, Ordering::SeqCst);
    }
}

fn subscribe(service: &Service, notifier: &Arc<Notifier>, shutdown: &Arc<ShutdownState>) {
    let notifier = Arc::clone(notifier);
    let shutdown = Arc::clone(shutdown);
    service.subscribe(Arc::new(move |notification: Notification| {
        let exit_ready = matches!(notification, Notification::ExitReady);
        notifier.publish(notification);
        if exit_ready {
            shutdown.exit_ready.store(true, Ordering::SeqCst);
            shutdown.requested.store(true, Ordering::SeqCst);
        }
    }));
}

fn spawn_ticker(service: &Service, shutdown: &Arc<ShutdownState>) {
    let service = service.clone();
    let shutdown = Arc::clone(shutdown);
    thread::spawn(move || {
        while !shutdown.requested.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_secs(1));
            if shutdown.requested.load(Ordering::SeqCst) {
                break;
            }
            service.tick();
        }
    });
}

/// Executes the shutdown command after stdin reached EOF (there is no request to
/// answer in that case).
fn shutdown_after_eof(service: &Service, notifier: &Notifier, shutdown: &ShutdownState) {
    if shutdown.requested.load(Ordering::SeqCst) {
        return;
    }
    notifier.begin_hold();
    service.execute(Command::Shutdown);
    notifier.end_hold();
    let snapshot = service.snapshot();
    if snapshot.connection.session_active {
        shutdown.expect_exit_ready.store(true, Ordering::SeqCst);
    }
    shutdown.requested.store(true, Ordering::SeqCst);
}

fn wait_for_exit_ready(shutdown: &ShutdownState) {
    let expected = shutdown.expect_exit_ready.load(Ordering::SeqCst);
    let grace = if expected {
        Duration::from_secs(10)
    } else {
        Duration::from_millis(300)
    };
    let deadline = Instant::now() + grace;
    while !shutdown.exit_ready.load(Ordering::SeqCst) {
        if Instant::now() >= deadline {
            if expected {
                eprintln!("drcom-daemon: 后端未在 10 秒内确认下线，继续退出");
            }
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Publishes a final `exit_ready` (if the service did not), lets the writer
/// threads flush, then marks the daemon as finished.
fn finish(notifier: &Notifier, shutdown: &ShutdownState, settle: Duration) {
    if !shutdown.exit_ready.load(Ordering::SeqCst) {
        notifier.publish(Notification::ExitReady);
    }
    thread::sleep(settle);
    shutdown.finished.store(true, Ordering::SeqCst);
    thread::sleep(Duration::from_millis(100));
}

// ---------------------------------------------------------------------------
// stdio transport
// ---------------------------------------------------------------------------

fn run_stdio(
    service: Service,
    shutdown: Arc<ShutdownState>,
    info: ServerInfo,
) -> Result<(), String> {
    eprintln!(
        "drcom-daemon: stdio 模式，协议 {} v{}（api v{}）",
        drcom_ipc::PROTOCOL_NAME,
        drcom_ipc::PROTOCOL_VERSION,
        API_VERSION
    );

    let writer = SharedWriter::new(io::stdout());
    let notifier = Arc::new(Notifier::stdout(writer.clone()));
    // Hold notifications until the banner (ready + first state) is out.
    notifier.begin_hold();
    let backend = DaemonBackend::new(
        service.clone(),
        Arc::clone(&notifier),
        Arc::clone(&shutdown),
        info,
    );
    subscribe(&service, &notifier, &shutdown);

    let mut handshake_writer = writer.clone();
    if let Err(error) = drcom_ipc::handshake(&mut handshake_writer, &backend) {
        return Err(format!("写入 ready 失败：{error}"));
    }
    notifier.end_hold();

    spawn_ticker(&service, &shutdown);

    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut response_writer = writer.clone();
    match drcom_ipc::serve_requests(&mut reader, &mut response_writer, &backend) {
        Ok(ConnectionEnd::Stopped) => {}
        Ok(ConnectionEnd::Closed) => {
            eprintln!("drcom-daemon: stdin 已关闭，执行下线后退出");
            shutdown_after_eof(&service, &notifier, &shutdown);
        }
        Err(error) => {
            eprintln!("drcom-daemon: 协议处理失败：{error}；执行下线后退出");
            shutdown_after_eof(&service, &notifier, &shutdown);
            wait_for_exit_ready(&shutdown);
            finish(&notifier, &shutdown, Duration::from_millis(50));
            return Err(format!("协议处理失败：{error}"));
        }
    }

    wait_for_exit_ready(&shutdown);
    finish(&notifier, &shutdown, Duration::from_millis(50));
    eprintln!("drcom-daemon: 已停止");
    Ok(())
}

// ---------------------------------------------------------------------------
// TCP transport
// ---------------------------------------------------------------------------

fn run_tcp(
    service: Service,
    shutdown: Arc<ShutdownState>,
    info: ServerInfo,
    address: SocketAddr,
) -> Result<(), String> {
    let listener =
        TcpListener::bind(address).map_err(|error| format!("无法监听 {address}: {error}"))?;
    let local = listener
        .local_addr()
        .map_err(|error| format!("无法读取实际监听地址：{error}"))?;
    eprintln!("drcom-daemon listening {local}");

    listener
        .set_nonblocking(true)
        .map_err(|error| format!("无法设置非阻塞监听：{error}"))?;

    let hub = Hub::new();
    let notifier = Arc::new(Notifier::hub(hub.clone()));
    let backend = Arc::new(DaemonBackend::new(
        service.clone(),
        Arc::clone(&notifier),
        Arc::clone(&shutdown),
        info,
    ));
    subscribe(&service, &notifier, &shutdown);
    spawn_ticker(&service, &shutdown);

    while !shutdown.requested.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                eprintln!("drcom-daemon: 客户端已连接 {peer}");
                let backend = Arc::clone(&backend);
                let hub = hub.clone();
                let shutdown = Arc::clone(&shutdown);
                thread::spawn(move || {
                    if let Err(error) = serve_tcp_client(stream, backend.as_ref(), &hub, &shutdown)
                    {
                        eprintln!("drcom-daemon: 客户端 {peer} 断开：{error}");
                    }
                });
            }
            Err(ref error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(format!("accept 失败：{error}")),
        }
    }

    drop(listener);
    wait_for_exit_ready(&shutdown);
    finish(&notifier, &shutdown, Duration::from_millis(200));
    eprintln!("drcom-daemon: 已停止");
    Ok(())
}

fn serve_tcp_client(
    stream: TcpStream,
    backend: &DaemonBackend,
    hub: &Hub,
    shutdown: &ShutdownState,
) -> io::Result<()> {
    // The listener is non-blocking so the accept loop can notice shutdown; make
    // sure the accepted socket itself is blocking again.
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let writer = SharedWriter::new(stream.try_clone()?);
    // Subscribe before the banner so no notification emitted during the
    // handshake is lost; the lines wait in the client queue.
    let (client_id, lines) = hub.subscribe();
    let mut reader = BufReader::new(stream);

    let mut handshake_writer = writer.clone();
    if let Err(error) = drcom_ipc::handshake(&mut handshake_writer, backend) {
        hub.unsubscribe(client_id);
        return Err(error);
    }

    // The writer thread owns socket writes for notifications; responses are
    // written by the serving thread through the same shared writer, so lines
    // never interleave.
    let pump_writer = writer.clone();
    thread::spawn(move || {
        while let Ok(line) = lines.recv() {
            let mut writer = pump_writer.clone();
            if writer
                .write_all(line.as_bytes())
                .and_then(|()| writer.flush())
                .is_err()
            {
                break;
            }
        }
    });

    let mut response_writer = writer.clone();
    let result = drcom_ipc::serve_requests(&mut reader, &mut response_writer, backend);
    if matches!(result, Ok(ConnectionEnd::Stopped)) {
        // Keep this client subscribed until the daemon emits its final
        // exit_ready, then let the writer thread flush it before closing.
        shutdown.wait_finished(Duration::from_secs(12));
    }
    hub.unsubscribe(client_id);
    result.map(|_| ())
}
