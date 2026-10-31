//! On-demand connectivity checks. Ping results do not prove login success.
use std::net::{IpAddr, ToSocketAddrs};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family {
    V4,
    V6,
}

const CAMPUS_HOST: &str = "jlu.edu.cn";
// Independent domestic endpoints. HTTPS verifies DNS, TCP, TLS certificate and
// an HTTP response; a single successful endpoint establishes reachability.
const PUBLIC_HOSTS: [&str; 2] = ["www.baidu.com", "www.taobao.com"];

pub fn internet_probe(
    family: Family,
    adapter: Option<&drcom_campus::adapter::AdapterInfo>,
) -> String {
    let Some(adapter) = adapter else {
        return "未找到网络连接｜请插好网线或连接校园 Wi-Fi 后重试".into();
    };
    let source = match family {
        Family::V4 => IpAddr::V4(adapter.ipv4),
        Family::V6 => match adapter
            .details
            .ipv6
            .iter()
            .find(|ip| matches!(ip, IpAddr::V6(v6) if v6.segments()[0] & 0xe000 == 0x2000))
        {
            Some(ip) => *ip,
            None => return "暂无 IPv6 地址｜当前校园网网卡尚未分配可访问外网的 IPv6 地址".into(),
        },
    };
    let mut failures = Vec::new();
    for host in PUBLIC_HOSTS {
        match https_probe(host, family, source) {
            Ok(result) => return result,
            Err(reason) => failures.push(format!("{host}：{reason}")),
        }
    }
    format!(
        "暂时无法确认可达｜{}；检测网卡 {}，源地址 {}。请检查网络，或稍后重试",
        failures.join("；"),
        adapter.friendly_name,
        source
    )
}

fn https_probe(host: &str, family: Family, source: IpAddr) -> Result<String, String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let curl = std::path::PathBuf::from(
            std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()),
        )
        .join("System32")
        .join("curl.exe");
        let output = std::process::Command::new(curl)
            // -q must be first: ignore any user curlrc (proxy/TLS overrides).
            .args([
                "-q",
                if family == Family::V6 { "-6" } else { "-4" },
                "--noproxy",
                "*",
                "--interface",
                &source.to_string(),
                "--connect-timeout",
                "3",
                "--max-time",
                "4",
                "--silent",
                "--head",
                "--output",
                "NUL",
                "--write-out",
                "%{http_code} %{remote_ip} %{time_total}",
                &format!("https://{host}/"),
            ])
            .creation_flags(0x08000000)
            .output()
            .map_err(|_| "系统网络检测组件 curl.exe 不可用".to_string())?;
        https_result(
            output.status.code().unwrap_or(-1),
            &String::from_utf8_lossy(&output.stdout),
            family,
            host,
            source,
        )
    }
    #[cfg(not(windows))]
    {
        let _ = (host, family, source);
        Err("当前系统暂不支持 HTTPS 检测".into())
    }
}

fn https_result(
    exit: i32,
    output: &str,
    family: Family,
    host: &str,
    source: IpAddr,
) -> Result<String, String> {
    if exit != 0 {
        return Err(match exit {
            6 => "域名解析失败或没有对应地址",
            7 => "无法建立连接",
            28 => "连接超时",
            35 | 60 => "HTTPS 证书或安全连接验证失败，请检查系统时间或网络拦截",
            45 => "当前网卡地址不可用，请刷新网络",
            _ => "未完成 HTTPS 访问",
        }
        .to_string()
            + &format!("（检测代码 {exit}）"));
    }
    let fields: Vec<_> = output.split_whitespace().collect();
    let status = fields
        .first()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(0);
    let remote = fields.get(1).and_then(|value| value.parse::<IpAddr>().ok());
    let Some(remote) = remote.filter(|ip| {
        ip.is_ipv6() == (family == Family::V6) && !ip.is_loopback() && !ip.is_unspecified()
    }) else {
        return Err("未确认目标地址，无法判断这条线路".into());
    };
    if !(100..600).contains(&status) {
        return Err("未收到有效网站响应".into());
    }
    let ms = fields
        .get(2)
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.0)
        * 1000.0;
    Ok(format!(
        "可用 · {ms:.0} ms｜HTTPS {host}，HTTP {status}，目标 {remote}，源地址 {source}；已验证安全连接和网站响应"
    ))
}

pub fn campus_probe() -> String {
    ping_host(CAMPUS_HOST, Family::V4)
}

fn resolve_target(host: &str, family: Family) -> Result<IpAddr, String> {
    let addresses = (host, 0)
        .to_socket_addrs()
        .map_err(|_| format!("无法解析 {host}，请检查 DNS 设置或稍后重试"))?;
    addresses
        .into_iter()
        .find(|ip| ip.is_ipv6() == (family == Family::V6))
        .map(|address| address.ip())
        .ok_or_else(|| {
            format!(
                "{host} 没有 {} 地址，暂时无法检测这条线路",
                if family == Family::V6 { "IPv6" } else { "IPv4" }
            )
        })
}

fn ping_host(host: &str, family: Family) -> String {
    let target = match resolve_target(host, family) {
        Ok(target) => target,
        Err(message) => return message,
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let flag = if family == Family::V6 { "-6" } else { "-4" };
        match std::process::Command::new("ping.exe")
            .args([flag, "-n", "2", "-w", "1500", &target.to_string()])
            .creation_flags(0x08000000)
            .output()
        {
            // TTL is present only in an IPv4 echo reply; time=/时间= belongs to
            // an echo reply in both supported Windows languages. Exit status
            // alone can also be zero for an ICMP destination-unreachable reply.
            Ok(output) => ping_result(&String::from_utf8_lossy(&output.stdout), target),
            Err(_) => "无法启动网络检测，请检查系统中的 ping.exe".into(),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = target;
        "当前系统暂不支持 Ping 检测".into()
    }
}

fn ping_result(output: &str, target: IpAddr) -> String {
    let lower = output.to_ascii_lowercase();
    // Match ASCII units too: Chinese Windows emits legacy-encoded Chinese,
    // but the reply latency's '=…ms' / '<…ms' and TTL survive decoding.
    let reply = lower.lines().any(|line| {
        line.contains("ttl=")
            || (line.contains("ms")
                && (line.contains('=') || line.contains('<'))
                && line.contains(&target.to_string()))
    });
    if reply {
        format!("可以连通（{target}）")
    } else {
        format!("未收到回复（{target}）；可能断网或对方不回应 Ping，不能据此判断登录失败")
    }
}

pub fn open_network_settings() -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Delegate privileged IPv4/DNS edits to the standard Windows adapter UI.
        std::process::Command::new("control.exe")
            .arg("ncpa.cpl")
            .creation_flags(0x08000000)
            .spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "请打开系统网络设置",
        ))
    }
}

pub fn copy(text: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        clipboard_win::set_clipboard_string(text).map_err(std::io::Error::other)
    }
    #[cfg(not(windows))]
    {
        let _ = text;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "剪贴板暂不支持当前平台",
        ))
    }
}

/// Writes the diagnostics text to a timestamped file beside the event log and
/// shows it in Explorer, so a user can attach it to a report instead of trying to
/// paste a wall of text into a chat window.
pub fn export(report: &str) -> std::io::Result<std::path::PathBuf> {
    let directory = drcom_campus::preferences::data_directory()?;
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!(
        "drcom-诊断-{}.txt",
        drcom_campus::event_log::compact_timestamp()
    ));
    // Prefixed so Notepad — which still guesses the code page from the bytes on
    // older builds — opens the Chinese text correctly instead of as mojibake.
    let mut bytes = Vec::with_capacity(report.len() + 3);
    bytes.extend_from_slice(&[0xef, 0xbb, 0xbf]);
    bytes.extend_from_slice(report.as_bytes());
    std::fs::write(&path, bytes)?;
    let _ = reveal(&directory);
    Ok(path)
}

/// Opens a folder in Explorer. Failure is not worth reporting: the file is
/// already written, and the path is shown in the window either way.
pub fn reveal(directory: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("explorer.exe")
            .arg(directory)
            .creation_flags(0x08000000)
            .spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = directory;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "当前平台无法打开文件夹",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_requires_a_valid_response_in_the_requested_family() {
        let source = "10.0.0.2".parse().unwrap();
        assert!(
            https_result(0, "200 1.2.3.4 0.125", Family::V4, "example.com", source)
                .unwrap()
                .starts_with("可用 · 125 ms")
        );
        assert!(https_result(0, "200 1.2.3.4 0.125", Family::V6, "example.com", source).is_err());
        assert!(https_result(0, "000 1.2.3.4 0", Family::V4, "example.com", source).is_err());
        assert!(
            https_result(60, "000 1.2.3.4 0", Family::V4, "example.com", source)
                .unwrap_err()
                .contains("证书")
        );
        assert!(
            https_result(28, "", Family::V4, "example.com", source)
                .unwrap_err()
                .contains("超时")
        );
    }

    #[test]
    fn unreachable_reply_is_not_success() {
        let target = "1.2.3.4".parse().unwrap();
        assert!(
            ping_result("Reply from 1.2.3.1: Destination host unreachable.", target)
                .starts_with("未收到回复")
        );
        assert!(
            ping_result("Reply from 1.2.3.4: bytes=32 time<1ms TTL=55", target)
                .starts_with("可以连通")
        );
    }

    #[test]
    fn ipv6_reply_and_timeout_are_distinct() {
        let target = "2001:db8::1".parse().unwrap();
        assert!(ping_result("Reply from 2001:db8::1: time=20ms", target).starts_with("可以连通"));
        assert!(ping_result("来自 2001:db8::1 的回复: 时间=20ms", target).starts_with("可以连通"));
        assert!(ping_result("Request timed out.", target).starts_with("未收到回复"));
    }
}
