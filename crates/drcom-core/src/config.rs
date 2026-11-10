//! Client configuration file.
//!
//! The original client keeps its own settings in `config`, `DrConfigure`,
//! `DrLinkConfigure`, `DrPluginsConfig` and `<account>\<account>`, all of which
//! are base16 or raw ciphertext. Their key lives inside the VMProtect section of
//! `DrAuthSvr.dll`, whose `.text`/`.rdata`/`.data` sections carry no raw bytes on
//! disk at all, so no program that avoids linking that DLL can read them (see
//! `docs/original-client-analysis.md`). This module therefore defines a plain
//! INI file for the Rust client, reusing the key spellings the original's own
//! binaries contain (`auth_server`, `svr_port`) so that a reader of both stays
//! oriented.
//!
//! Format:
//!
//! ```text
//! ; 注释
//! [drcom]
//! auth_server = 10.100.61.3, 10.100.61.4
//! svr_port    = 61440
//! account     = 20230001
//! ```
//!
//! A `[section]` header is accepted and ignored. A line whose first non-blank
//! character is `;` or `#` is a comment, matching the original's own
//! `;svr_ip=` style. Values run verbatim to the end of the line, so a password
//! may contain `;`. Unknown keys are an error rather than a silent no-op: this
//! is a hand-edited file, and a typo that quietly disabled `auth_route` would be
//! worse than a startup failure.

use std::fmt;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use crate::reference_login::{ACCOUNT_FIELD_LEN, MAX_PASSWORD_LEN};
use crate::route::RouteMode;

/// File name searched for next to the executable and in the working directory.
pub const DEFAULT_FILE_NAME: &str = "drcom.ini";

/// Sub-directory of `%APPDATA%` searched after the executable's own directory.
///
/// The installer targets Program Files, which an ordinary user cannot write to,
/// so without this a per-user override would require administrative rights.
pub const APP_DATA_DIR: &str = "DrComCampus";

/// Environment variable that overrides the server list, kept for the GUI's
/// back-compatibility with the prototype.
pub const SERVER_ENV: &str = "DRCOM_SERVER";

/// Authentication server used when neither `drcom.ini` nor the environment
/// names one.
///
/// The C# client hard-codes the same address as `AuthServer` and hides the
/// field from its window, and the deployment's own `auth_log.txt` records
/// `LGI OK … 10.100.61.3`. Without a fallback a fresh install has no server at
/// all, and the window can do nothing but report that it is unconfigured.
pub const BUILTIN_SERVERS: [Ipv4Addr; 1] = [Ipv4Addr::new(10, 100, 61, 3)];

/// Settings the client understands, all optional until a session is built.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientConfig {
    pub servers: Vec<Ipv4Addr>,
    pub account: String,
    pub password: String,
    pub local_ip: Option<Ipv4Addr>,
    pub mac: Option<u64>,
    pub hostname: Option<String>,
    pub dns: Option<Ipv4Addr>,
    pub dhcp: Option<Ipv4Addr>,
    pub server_port: Option<u16>,
    pub local_port: Option<u16>,
    pub auth_route: Option<RouteMode>,
}

/// A rejected configuration line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(formatter, "{}", self.message)
        } else {
            write!(formatter, "第 {} 行：{}", self.line, self.message)
        }
    }
}

impl std::error::Error for ConfigError {}

impl ClientConfig {
    /// Checks the values a session actually needs, so failures name the field
    /// instead of surfacing later as a frame-building error.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.account.is_empty() {
            return Err(fail(0, "缺少 account（账号）"));
        }
        if self.password.is_empty() {
            return Err(fail(0, "缺少 password（密码）"));
        }
        if self.account.len() > ACCOUNT_FIELD_LEN {
            return Err(fail(
                0,
                format!(
                    "account 最多 {ACCOUNT_FIELD_LEN} 字节，当前 {}",
                    self.account.len()
                ),
            ));
        }
        if self.password.len() > MAX_PASSWORD_LEN {
            return Err(fail(
                0,
                format!(
                    "password 最多 {MAX_PASSWORD_LEN} 字节，当前 {}",
                    self.password.len()
                ),
            ));
        }
        if self.servers.is_empty() {
            return Err(fail(0, "缺少 auth_server（认证服务器）"));
        }
        Ok(())
    }

    /// Server list, preferring the file over the environment so that an explicit
    /// configuration wins, then falling back to the built-in campus address so
    /// that a fresh install can authenticate with no file present. Used by the
    /// GUI, which has no command line.
    pub fn servers_or_env(&self) -> Vec<Ipv4Addr> {
        if !self.servers.is_empty() {
            return self.servers.clone();
        }
        if let Ok(value) = std::env::var(SERVER_ENV) {
            let parsed = parse_servers(&value).unwrap_or_default();
            if !parsed.is_empty() {
                return parsed;
            }
        }
        BUILTIN_SERVERS.to_vec()
    }
}

fn fail(line: usize, message: impl Into<String>) -> ConfigError {
    ConfigError {
        line,
        message: message.into(),
    }
}

/// Parses a comma- or whitespace-separated IPv4 list.
fn parse_servers(value: &str) -> Result<Vec<Ipv4Addr>, String> {
    let mut servers = Vec::new();
    for part in value.split(|character: char| character == ',' || character.is_whitespace()) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        servers.push(
            part.parse::<Ipv4Addr>()
                .map_err(|_| format!("认证服务器地址无效：{part}"))?,
        );
    }
    Ok(servers)
}

fn parse_mac(value: &str) -> Result<u64, String> {
    let text = value.trim();
    let digits: String = text
        .trim_start_matches("0x")
        .trim_start_matches("0X")
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .collect();
    if digits.is_empty() {
        return Err("mac 不能为空".to_string());
    }
    // The frame stores the MAC as six bytes, so anything wider cannot be sent.
    if digits.len() > 12 {
        return Err(format!("mac 最多 12 位十六进制，当前 {}", digits.len()));
    }
    u64::from_str_radix(&digits, 16).map_err(|_| format!("mac 无效：{text}"))
}

fn parse_port(value: &str, field: &str, allow_zero: bool) -> Result<u16, String> {
    let port: u16 = value
        .trim()
        .parse()
        .map_err(|_| format!("{field} 不是有效端口：{value}"))?;
    if port == 0 && !allow_zero {
        return Err(format!("{field} 不能为 0"));
    }
    Ok(port)
}

fn parse_route_mode(value: &str) -> Result<RouteMode, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" | "none" | "0" => Ok(RouteMode::Off),
        "dry-run" | "dryrun" | "check" => Ok(RouteMode::DryRun),
        "manage" | "on" | "1" => Ok(RouteMode::Manage),
        other => Err(format!(
            "auth_route 只能是 off / dry-run / manage，当前 {other}"
        )),
    }
}

/// Parses configuration text.
pub fn parse(text: &str) -> Result<ClientConfig, ConfigError> {
    let mut config = ClientConfig::default();
    for (index, raw) in text.lines().enumerate() {
        // A UTF-8 BOM on the first line would otherwise become part of the key.
        let line = raw.trim_start_matches('\u{feff}');
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(';') || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') {
            if !trimmed.ends_with(']') {
                return Err(fail(index + 1, "节区标题缺少右括号"));
            }
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            return Err(fail(index + 1, "缺少 “=”"));
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        let result: Result<(), String> = match key.as_str() {
            "auth_server" | "server" | "servers" | "server_list" => {
                parse_servers(value).map(|servers| config.servers.extend(servers))
            }
            "account" | "user" | "username" => {
                config.account = value.to_string();
                Ok(())
            }
            "password" | "pass" => {
                config.password = value.to_string();
                Ok(())
            }
            "local_ip" | "local-ip" | "ip" => value
                .parse::<Ipv4Addr>()
                .map(|address| config.local_ip = Some(address))
                .map_err(|_| format!("local_ip 不是有效的 IPv4 地址：{value}")),
            "mac" => parse_mac(value).map(|mac| config.mac = Some(mac)),
            "hostname" | "host_name" | "host" => {
                config.hostname = Some(value.to_string());
                Ok(())
            }
            "dns" | "primary_dns" => value
                .parse::<Ipv4Addr>()
                .map(|address| config.dns = Some(address))
                .map_err(|_| format!("dns 不是有效的 IPv4 地址：{value}")),
            "dhcp" | "dhcp_server" => value
                .parse::<Ipv4Addr>()
                .map(|address| config.dhcp = Some(address))
                .map_err(|_| format!("dhcp 不是有效的 IPv4 地址：{value}")),
            "svr_port" | "server_port" | "port" => {
                parse_port(value, "svr_port", false).map(|port| config.server_port = Some(port))
            }
            "local_port" => {
                parse_port(value, "local_port", true).map(|port| config.local_port = Some(port))
            }
            "auth_route" | "route" => {
                parse_route_mode(value).map(|mode| config.auth_route = Some(mode))
            }
            other => Err(format!("未知配置项 {other}")),
        };
        if let Err(message) = result {
            return Err(fail(index + 1, message));
        }
    }
    Ok(config)
}

/// Error from [`load`], keeping I/O and syntax failures apart.
#[derive(Debug)]
pub enum LoadError {
    Io(std::io::Error),
    Invalid(ConfigError),
}

impl fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "读取配置文件失败：{error}"),
            Self::Invalid(error) => write!(formatter, "配置文件无效：{error}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// Reads and parses a configuration file.
pub fn load(path: &Path) -> Result<ClientConfig, LoadError> {
    let text = std::fs::read_to_string(path).map_err(LoadError::Io)?;
    parse(&text).map_err(LoadError::Invalid)
}

/// Looks for `drcom.ini` next to the executable, then in the working directory,
/// then under `%APPDATA%\DrComCampus`.
///
/// The search stops at the first file that exists, so a per-user copy is only
/// consulted when no machine-wide one is present. Returns `None` when no
/// candidate exists, which is the normal case for a client driven entirely by
/// command-line flags.
pub fn discover() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(executable) = std::env::current_exe()
        && let Some(directory) = executable.parent()
    {
        candidates.push(directory.join(DEFAULT_FILE_NAME));
    }
    candidates.push(PathBuf::from(DEFAULT_FILE_NAME));
    if let Some(appdata) = std::env::var_os("APPDATA") {
        candidates.push(
            PathBuf::from(appdata)
                .join(APP_DATA_DIR)
                .join(DEFAULT_FILE_NAME),
        );
    }
    candidates.into_iter().find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_keys_the_original_client_uses() {
        let config = parse(
            "; 认证服务器，对应原版配置里的 auth_server\n\
             [drcom]\n\
             auth_server = 10.100.61.3, 10.100.61.4\n\
             svr_port=61440\n\
             account = 20230001\n\
             password = p@ss;word\n\
             auth_route = dry-run\n",
        )
        .expect("valid");
        assert_eq!(
            config.servers,
            vec![Ipv4Addr::new(10, 100, 61, 3), Ipv4Addr::new(10, 100, 61, 4)]
        );
        assert_eq!(config.server_port, Some(61440));
        assert_eq!(config.account, "20230001");
        // A semicolon only starts a comment at the beginning of a line, so a
        // password keeps it.
        assert_eq!(config.password, "p@ss;word");
        assert_eq!(config.auth_route, Some(RouteMode::DryRun));
    }

    #[test]
    fn servers_accept_whitespace_and_repeats() {
        let config =
            parse("auth_server=10.0.0.1\n_auth_server_is_unknown=1").expect_err("unknown key");
        assert_eq!(config.line, 2);

        let config = parse("auth_server=10.0.0.1 10.0.0.2\n").expect("valid");
        assert_eq!(config.servers.len(), 2);

        let config = parse("auth_server=10.0.0.1\nauth_server=10.0.0.2\n").expect("valid");
        assert_eq!(config.servers.len(), 2);
    }

    #[test]
    fn comment_and_blank_lines_are_skipped() {
        let config = parse("\n; a\n# b\n   \naccount=u\n").expect("valid");
        assert_eq!(config.account, "u");
        assert_eq!(config.servers.len(), 0);
    }

    #[test]
    fn bad_values_report_their_line() {
        let error = parse("account=u\nsvr_port=70000\n").expect_err("port out of range");
        assert_eq!(error.line, 2);

        let error = parse("auth_server=10.0.0.1,999.1.1.1\n").expect_err("bad address");
        assert_eq!(error.line, 1);

        let error = parse("no_equals_here\n").expect_err("missing =");
        assert_eq!(error.line, 1);

        let error = parse("auth_route=maybe\n").expect_err("bad mode");
        assert_eq!(error.line, 1);
    }

    #[test]
    fn zero_is_allowed_for_the_local_port_only() {
        let config = parse("local_port=0\n").expect("valid");
        assert_eq!(config.local_port, Some(0));
        assert!(
            parse("svr_port=0\n").is_err(),
            "the server port cannot be 0"
        );
    }

    #[test]
    fn validation_names_the_missing_field() {
        let config = parse("auth_server=10.0.0.1\n").expect("valid");
        let error = config.validate().expect_err("no account");
        assert!(error.message.contains("account"), "{error}");

        let config = parse("auth_server=10.0.0.1\naccount=u\npassword=p\n").expect("valid");
        config.validate().expect("complete");
    }

    #[test]
    fn validation_rejects_fields_the_frame_cannot_hold() {
        let long_account = "a".repeat(ACCOUNT_FIELD_LEN + 1);
        let config = parse(&format!(
            "account={long_account}\npassword=p\nauth_server=10.0.0.1\n"
        ))
        .expect("syntax is fine");
        assert!(
            config
                .validate()
                .expect_err("too long")
                .message
                .contains("account")
        );

        let long_password = "p".repeat(MAX_PASSWORD_LEN + 1);
        let config = parse(&format!(
            "account=u\npassword={long_password}\nauth_server=10.0.0.1\n"
        ))
        .expect("syntax is fine");
        assert!(
            config
                .validate()
                .expect_err("too long")
                .message
                .contains("password")
        );
    }

    #[test]
    fn file_servers_win_over_the_environment() {
        let config = parse("auth_server=10.0.0.9\n").expect("valid");
        assert_eq!(config.servers_or_env(), vec![Ipv4Addr::new(10, 0, 0, 9)]);
    }
}
