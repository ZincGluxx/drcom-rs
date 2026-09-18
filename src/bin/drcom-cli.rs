//! Command-line Dr.COM client.
//!
//! The GUI binary only opens the Slint window, so this entry point exists to
//! make the authentication engine usable and testable without a desktop:
//!
//! ```text
//! drcom-cli --server 10.100.61.3 --user 20230001 --pass secret
//!           --local-ip 10.100.61.20 --mac aa:bb:cc:dd:ee:ff
//! ```
//!
//! Settings may also come from a `drcom.ini` next to the executable; explicit
//! flags always win over it. It prints every session event, stops on Enter (or
//! `q` + Enter) or after `--duration`, and never prints the password.

use std::net::Ipv4Addr;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use drcom_campus::adapter;
use drcom_campus::config;
use drcom_campus::event_log;
use drcom_campus::route::RouteMode;
use drcom_campus::session::{
    LoginSession, SessionConfig, SessionError, SessionEvent, SessionPhase,
};

const USAGE: &str = "\
drcom-cli — Dr.COM 校园网命令行认证客户端

用法：
  drcom-cli --user <账号> --pass <密码> [选项]

必填：
  --user <账号>           校园网账号

可选：
  --server <ip[,ip...]>   认证服务器地址，可用逗号分隔多个；省略时使用配置、环境变量或内置地址
  --config <文件>         读取配置文件（INI）；不指定时按顺序查找
                          可执行文件旁的 drcom.ini 与当前目录的 drcom.ini
  --pass <密码>           密码；也可用环境变量 DRCOM_PASSWORD，避免进入命令行历史
  --local-ip <ip>         本机网卡 IPv4；省略时自动检测
  --mac <mac>             网卡 MAC，支持 aa:bb:cc:dd:ee:ff、aabbccddeeff、0xaabbccddeeff
  --hostname <name>       计算机名，默认取环境变量 COMPUTERNAME
  --dns <ip>              主 DNS，默认 10.10.10.10
  --dhcp <ip>             DHCP 服务器，默认 0.0.0.0
  --port <n>              认证服务器端口，默认 61440
  --local-port <n>        本地端口，默认 61440；0 表示由系统分配
  --duration <秒>         保活时长，0 表示持续到手动停止，默认 0
  --once                  只认证一次，成功后立即下线
  --auth-route            解析认证服务器主机路由并打印计划，但不修改路由表
  --auth-route-apply      登录前添加认证服务器 /32 主机路由，下线后移除（需管理员）
  --quiet                 只输出阶段变化与错误
  --help                  显示本帮助

配置文件（INI）示例：
  ; 认证服务器，对应原版配置里的 auth_server
  [drcom]
  auth_server = 10.100.61.3, 10.100.61.4
  svr_port    = 61440
  account     = 20230001
  password    = secret
  auth_route  = dry-run
";

struct Args {
    servers: Vec<Ipv4Addr>,
    user: String,
    pass: String,
    local_ip: Option<Ipv4Addr>,
    mac: u64,
    hostname: String,
    dns: Ipv4Addr,
    dhcp: Ipv4Addr,
    port: u16,
    local_port: u16,
    duration: Duration,
    once: bool,
    quiet: bool,
    auth_route: RouteMode,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let parsed = match parse_args(&args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("参数错误：{message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(parsed) {
        Ok(()) => {
            event_log::record("cli 结束：正常");
            ExitCode::SUCCESS
        }
        // Stopping on Enter, on stdin EOF or when --duration elapses is what the
        // user asked for, so it is not a failure. The session still logs out
        // first; only the exit status and the wording change.
        Err(SessionError::Stopped) => {
            println!("已按要求停止");
            event_log::record("cli 结束：按要求停止");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("认证结束：{error}");
            // The exit code says *that* it failed; the log is what says why, which
            // matters when the terminal output has already scrolled away.
            event_log::record(&format!("cli 失败：{error}"));
            ExitCode::FAILURE
        }
    }
}

/// Loads the file named by `--config`, or else the first `drcom.ini` found next
/// to the executable or in the working directory.
fn load_config(args: &[String]) -> Result<config::ClientConfig, String> {
    let explicit = args
        .iter()
        .position(|arg| arg == "--config")
        .and_then(|index| args.get(index + 1))
        .cloned();
    match explicit {
        Some(path) => {
            let path = Path::new(&path);
            if !path.is_file() {
                return Err(format!("--config 指定的文件不存在：{}", path.display()));
            }
            config::load(path).map_err(|error| error.to_string())
        }
        None => match config::discover() {
            Some(path) => {
                let loaded = config::load(&path).map_err(|error| error.to_string())?;
                eprintln!("已读取配置 {}", path.display());
                Ok(loaded)
            }
            None => Ok(config::ClientConfig::default()),
        },
    }
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    // The configuration file seeds the defaults and explicit flags override it,
    // so `drcom-cli --server ...` behaves exactly as before while a bare
    // invocation can now run from drcom.ini alone.
    let file = load_config(args)?;
    let mut servers: Vec<Ipv4Addr> = file.servers_or_env();
    let mut servers_from_flag = false;
    let mut user = file.account.clone();
    let mut pass = if file.password.is_empty() {
        std::env::var("DRCOM_PASSWORD").unwrap_or_default()
    } else {
        file.password.clone()
    };
    let mut local_ip = file.local_ip;
    let mut mac = file.mac;
    let mut hostname = file
        .hostname
        .clone()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_default();
    let mut dns = file.dns.unwrap_or(Ipv4Addr::new(10, 10, 10, 10));
    let mut dhcp = file.dhcp.unwrap_or(Ipv4Addr::UNSPECIFIED);
    let mut port = file.server_port.unwrap_or(61440);
    let mut local_port = file.local_port.unwrap_or(61440);
    let mut duration = Duration::ZERO;
    let mut once = false;
    let mut quiet = false;
    let mut auth_route = file.auth_route.unwrap_or(RouteMode::Off);

    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        let mut value = || -> Result<String, String> {
            index += 1;
            args.get(index)
                .cloned()
                .ok_or_else(|| format!("{flag} 缺少取值"))
        };
        match flag {
            "--config" => {
                // Already applied by load_config; consume the value so it is
                // not mistaken for an unknown operand.
                value()?;
            }
            "--server" => {
                if !servers_from_flag {
                    // A flag replaces the file's list rather than appending to
                    // it, which is what "explicit wins" has to mean here.
                    servers.clear();
                    servers_from_flag = true;
                }
                for part in value()?.split(',') {
                    let parsed = part
                        .trim()
                        .parse::<Ipv4Addr>()
                        .map_err(|_| format!("服务器地址无效：{part}"))?;
                    servers.push(parsed);
                }
            }
            "--user" => user = value()?,
            "--pass" => pass = value()?,
            "--local-ip" => {
                local_ip = Some(
                    value()?
                        .parse::<Ipv4Addr>()
                        .map_err(|_| "--local-ip 不是有效的 IPv4 地址".to_string())?,
                )
            }
            "--mac" => mac = Some(parse_mac(&value()?)?),
            "--hostname" => hostname = value()?,
            "--dns" => {
                dns = value()?
                    .parse()
                    .map_err(|_| "--dns 不是有效的 IPv4 地址".to_string())?
            }
            "--dhcp" => {
                dhcp = value()?
                    .parse()
                    .map_err(|_| "--dhcp 不是有效的 IPv4 地址".to_string())?
            }
            "--port" => {
                port = value()?
                    .parse()
                    .map_err(|_| "--port 不是有效端口".to_string())?
            }
            "--local-port" => {
                local_port = value()?
                    .parse()
                    .map_err(|_| "--local-port 不是有效端口".to_string())?
            }
            "--duration" => {
                let seconds: u64 = value()?
                    .parse()
                    .map_err(|_| "--duration 需要秒数".to_string())?;
                duration = Duration::from_secs(seconds);
            }
            "--once" => once = true,
            "--auth-route" => auth_route = RouteMode::DryRun,
            "--auth-route-apply" => auth_route = RouteMode::Manage,
            "--quiet" => quiet = true,
            other => return Err(format!("未知参数 {other}")),
        }
        index += 1;
    }

    if user.is_empty() {
        return Err("必须用 --user 或配置文件里的 account 指定账号".to_string());
    }
    if pass.is_empty() {
        return Err(
            "必须用 --pass、配置文件里的 password 或环境变量 DRCOM_PASSWORD 提供密码".to_string(),
        );
    }

    Ok(Args {
        servers,
        user,
        pass,
        local_ip,
        mac: mac.unwrap_or(adapter::DEFAULT_MAC),
        hostname,
        dns,
        dhcp,
        port,
        local_port,
        duration,
        once,
        quiet,
        auth_route,
    })
}

fn parse_mac(text: &str) -> Result<u64, String> {
    let digits: String = text
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X")
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .collect();
    if digits.is_empty() || digits.len() > 12 {
        return Err(format!("MAC 无效：{text}"));
    }
    u64::from_str_radix(&digits, 16)
        .map_err(|_| format!("MAC 无效：{text}"))
        .map(|value| {
            if value == 0 {
                adapter::DEFAULT_MAC
            } else {
                value
            }
        })
}

fn run(args: Args) -> Result<(), SessionError> {
    let stop = Arc::new(AtomicBool::new(false));
    let selected = adapter::select_for(args.servers.first().copied());
    if let Some(found) = &selected {
        println!(
            "网卡 {} ({})  IPv4 {}  MAC {:012x}  IF {}",
            found.friendly_name, found.description, found.ipv4, found.mac, found.interface_index
        );
    }

    let mut config = SessionConfig::new(
        args.servers.clone(),
        args.user.clone().into_bytes(),
        args.pass.clone().into_bytes(),
    );
    config.server_port = args.port;
    config.local_port = args.local_port;
    config.local_ipv4 = args
        .local_ip
        .or_else(|| selected.as_ref().map(|a| a.ipv4))
        .unwrap_or(Ipv4Addr::UNSPECIFIED);
    config.hostname = args.hostname.clone().into_bytes();
    config.mac = args.mac;
    config.primary_dns = args.dns.octets();
    config.dhcp_server = args.dhcp.octets();
    config.auth_route = args.auth_route;

    println!(
        "准备认证：服务器 {} 端口 {}，本机 {}，账号 {}",
        args.servers
            .iter()
            .map(|server| server.to_string())
            .collect::<Vec<_>>()
            .join(","),
        config.server_port,
        config.local_ipv4,
        args.user
    );
    if config.local_ipv4.is_unspecified() {
        println!("提示：未检测到网卡 IPv4，套接字将绑定 0.0.0.0，建议用 --local-ip 指定");
    }
    // The account is printed to the terminal because the user typed it and needs
    // to see which one is in play. It never goes into the log file, which is
    // written to be shared.
    event_log::record(&format!(
        "cli 启动：服务器 {} 端口 {}，本机 {}，网卡 {}",
        config
            .servers
            .iter()
            .map(|server| server.to_string())
            .collect::<Vec<_>>()
            .join(","),
        config.server_port,
        config.local_ipv4,
        selected
            .as_ref()
            .map(|found| found.friendly_name.as_str())
            .unwrap_or("未检测到")
    ));

    if args.once {
        if args.auth_route.is_enabled() {
            eprintln!(
                "提示：--auth-route / --auth-route-apply 只作用于完整会话，--once 已忽略它们"
            );
        }
        let mut session = LoginSession::new(config)?;
        let mut report = Reporter::new(args.quiet);
        session.login(&mut |event| report.handle(event))?;
        println!("认证成功，按 --once 要求立即下线");
        session.logout(&mut |event| report.handle(event))?;
        return Ok(());
    }

    let mut session = LoginSession::new(config)?;
    session.use_external_stop(Arc::clone(&stop));
    spawn_stdin_watcher(Arc::clone(&stop));
    if !args.duration.is_zero() {
        spawn_timer(args.duration, Arc::clone(&stop));
    } else {
        println!("保活中：按回车（或输入 q 回车）停止并下线");
    }

    let mut report = Reporter::new(args.quiet);
    let result = session.run(&mut |event| report.handle(event));
    if result.is_ok() {
        println!(
            "已下线，本次在线 {}",
            format_duration(session.connected_for())
        );
    }
    result
}

fn spawn_stdin_watcher(stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).unwrap_or(0) > 0 {
            stop.store(true, Ordering::Relaxed);
            return;
        }
        // EOF on stdin (for example when run from a script) also stops.
        stop.store(true, Ordering::Relaxed);
    });
}

fn spawn_timer(duration: Duration, stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        println!("已达 --duration {} 秒，正在下线", duration.as_secs());
        stop.store(true, Ordering::Relaxed);
    });
}

struct Reporter {
    quiet: bool,
    last_phase: Option<SessionPhase>,
    started: Instant,
}

impl Reporter {
    fn new(quiet: bool) -> Self {
        Self {
            quiet,
            last_phase: None,
            started: Instant::now(),
        }
    }

    fn handle(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::Phase(phase) => {
                if self.last_phase == Some(phase) {
                    return;
                }
                self.last_phase = Some(phase);
                self.report(&format!("[{:>5.1}s] {}", self.elapsed(), phase_text(phase)));
            }
            SessionEvent::Note(note) => {
                if !self.quiet {
                    self.report(&format!("[{:>5.1}s] {note}", self.elapsed()));
                }
            }
            SessionEvent::KeepAliveCycle { sequence, tail } => {
                let tail: String = tail.iter().map(|byte| format!("{byte:02x}")).collect();
                let line = format!(
                    "[{:>5.1}s] 保活完成，序号 {sequence}，尾码 {tail}",
                    self.elapsed()
                );
                if !self.quiet {
                    println!("{line}");
                }
                // The keep-alive line reaches the log even under `--quiet`: an
                // unattended run is exactly the case where nobody is watching the
                // terminal and the file is the only record left.
                event_log::record(&format!("cli {line}"));
            }
        }
    }

    /// Prints a line and mirrors it into the event log, so a run started from a
    /// script or a scheduled task leaves the same trail as the GUI.
    fn report(&self, line: &str) {
        println!("{line}");
        event_log::record(&format!("cli {line}"));
    }

    fn elapsed(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }
}

fn phase_text(phase: SessionPhase) -> &'static str {
    match phase {
        SessionPhase::Challenge => "请求挑战",
        SessionPhase::Authenticating => "发送登录报文",
        SessionPhase::Online => "认证成功，已上线",
        SessionPhase::KeepAlive => "保活中",
        SessionPhase::LoggedOut => "已下线",
    }
}

fn format_duration(duration: Option<Duration>) -> String {
    match duration {
        Some(value) => format!("{} 秒", value.as_secs()),
        None => "0 秒".to_string(),
    }
}
