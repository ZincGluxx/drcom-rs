//! User-triggered probes.
use std::{
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

/// Which half of dual-stack a public-internet probe should exercise.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family {
    V4,
    V6,
}

/// The hosts the public-internet probes resolve.
///
/// Both are domestic dual-stack names that answer on 443 from anywhere in
/// mainland China. The obvious literals — `1.1.1.1` and
/// `2606:4700:4700::1111` — are filtered inside China: they time out on a
/// perfectly healthy connection, so probing them turns this health check into a
/// false alarm that reports *both* families broken while the user browses
/// normally. It also misleads in the other direction: ICMPv6 to those two
/// addresses still answers, so an ICMP-only check reports success for a path
/// that carries no traffic. Reachability has to be measured against a target
/// that is supposed to answer from here.
const PUBLIC_V4_HOST: &str = "www.baidu.com";
const PUBLIC_V6_HOST: &str = "www.taobao.com";

pub fn tcp_probe(address: SocketAddr) -> String {
    let start = Instant::now();
    match TcpStream::connect_timeout(&address, Duration::from_secs(3)) {
        // The resolved address is part of the answer: for a dual-stack host it
        // is the only way for the user to see which of the two paths ran.
        Ok(_) => format!(
            "可达 · {} · {} ms",
            address.ip(),
            start.elapsed().as_millis()
        ),
        Err(error) => format!("未连通 · {} · {:?}", address.ip(), error.kind()),
    }
}

/// Resolves a domestic host in one address family and connects to it.
///
/// Resolving first is what makes the answer trustworthy: a family that resolves
/// but cannot carry a connection is a different fault from one that never
/// resolves at all, and users need to be able to tell those apart. It is also
/// what a browser does, so "this works" here means the site opens.
pub fn internet_probe(family: Family) -> String {
    use std::net::ToSocketAddrs;

    let host = match family {
        Family::V4 => PUBLIC_V4_HOST,
        Family::V6 => PUBLIC_V6_HOST,
    };
    let addresses = match (host, 443u16).to_socket_addrs() {
        Ok(found) => found.collect::<Vec<_>>(),
        Err(error) => return format!("域名解析失败 · {:?}", error.kind()),
    };
    let wanted = match family {
        Family::V4 => false,
        Family::V6 => true,
    };
    match addresses.iter().find(|item| item.is_ipv6() == wanted) {
        Some(target) => {
            let result = tcp_probe(*target);
            match family {
                // A failed IPv6 probe has two very different causes: no routable
                // address on this machine, or a routable address on a path that
                // is blocked. The address and error kind alone cannot tell them
                // apart, and only the second is worth waiting to be fixed.
                Family::V6 if !result.starts_with("可达") => {
                    format!("{result} ｜ {}", local_ipv6_state())
                }
                _ => result,
            }
        }
        None => match family {
            Family::V4 => "该域名没有 IPv4 记录".into(),
            Family::V6 => "该域名没有 IPv6 记录".into(),
        },
    }
}

/// Describes this machine's own IPv6 addresses, to pair with a failed probe.
fn local_ipv6_state() -> String {
    let adapters = drcom_campus::adapter::list();
    let global = drcom_campus::adapter::global_ipv6_count(&adapters);
    if global > 0 {
        return format!("本机有 {global} 个全局 IPv6 地址");
    }
    if adapters
        .iter()
        .any(|adapter| !adapter.details.ipv6.is_empty())
    {
        "本机只有链路本地 IPv6 地址".into()
    } else {
        "本机没有 IPv6 地址".into()
    }
}

pub fn campus_probe(server: std::net::Ipv4Addr) -> String {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        match std::process::Command::new("ping.exe")
            .args(["-4", "-n", "1", "-w", "2000", &server.to_string()])
            .creation_flags(0x08000000)
            .output()
        {
            Ok(output) if output.status.success() => "ICMP 可达".into(),
            Ok(_) => "ICMP 未响应（不代表认证不可用）".into(),
            Err(error) => format!("测试失败：{error}"),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = server;
        "当前平台不支持校内 ICMP 测试".into()
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
    fn the_public_probes_never_point_at_targets_blocked_inside_china() {
        // This is the regression that made a healthy connection report both
        // families as broken. ICMPv6 to these addresses still answers while TCP
        // 443 does not, so the mistake is invisible in a ping and only shows up
        // as a user being told their working network is down.
        for host in [PUBLIC_V4_HOST, PUBLIC_V6_HOST] {
            for banned in ["1.1.1.1", "8.8.8.8", "2606:4700", "cloudflare", "google"] {
                assert!(
                    !host.contains(banned),
                    "{host} is unreachable from mainland China and cannot be a probe target"
                );
            }
            assert!(
                host.ends_with(".com") || host.ends_with(".cn"),
                "{host} should be a resolvable public name"
            );
        }
        assert_ne!(PUBLIC_V4_HOST, PUBLIC_V6_HOST);
    }

    #[test]
    fn internet_probe_always_returns_something_the_user_can_read() {
        // Deliberately tolerant of being offline: the point is that every branch
        // -- resolved, unresolved, no record for the family -- produces a
        // message rather than panicking or coming back empty.
        for family in [Family::V4, Family::V6] {
            let text = internet_probe(family);
            assert!(!text.is_empty(), "{family:?} produced nothing");
            assert!(
                text.contains('·') || text.contains("记录"),
                "{family:?} produced an unreadable result: {text}"
            );
        }
    }
}
