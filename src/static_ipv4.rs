//! Validated static IPv4 edits, executed only after the user presses Apply.
//!
//! Windows runs `netsh` from an elevated PowerShell script. Linux has no
//! equivalent per-interface call that works without privileges, and a desktop
//! session owns its network through NetworkManager, so the Unix build edits the
//! active connection for the chosen device with `nmcli`. Every argument is
//! passed as a separate argument-vector entry — never through a shell — so an
//! adapter or connection name cannot inject commands.
#[cfg(windows)]
use base64::{Engine, engine::general_purpose::STANDARD};
use std::net::Ipv4Addr;

pub struct Configuration {
    address: Ipv4Addr,
    mask: Ipv4Addr,
    gateway: Ipv4Addr,
    dns: Ipv4Addr,
}

impl Configuration {
    pub fn parse(address: &str, mask: &str, gateway: &str, dns: &str) -> Result<Self, String> {
        fn parse(value: &str, label: &str) -> Result<Ipv4Addr, String> {
            value
                .trim()
                .parse()
                .map_err(|_| format!("{label} 格式不正确"))
        }
        fn unicast(ip: Ipv4Addr) -> bool {
            !ip.is_unspecified()
                && !ip.is_loopback()
                && !ip.is_multicast()
                && !ip.is_broadcast()
                && ip.octets()[0] != 0
                && ip.octets()[0] < 224
        }
        let config = Self {
            address: parse(address, "IPv4 地址")?,
            mask: parse(mask, "子网掩码")?,
            gateway: parse(gateway, "网关")?,
            dns: parse(dns, "DNS")?,
        };
        let mask = u32::from(config.mask);
        let prefix = mask.leading_ones();
        if prefix == 0 || prefix > 30 || mask != u32::MAX << (32 - prefix) {
            return Err("子网掩码必须连续，支持 /1 到 /30".into());
        }
        if !unicast(config.address) || !unicast(config.gateway) || !unicast(config.dns) {
            return Err("地址、网关和 DNS 必须是有效单播 IPv4 地址".into());
        }
        let host = u32::from(config.address) & !mask;
        let gateway_host = u32::from(config.gateway) & !mask;
        if host == 0 || host == !mask || gateway_host == 0 || gateway_host == !mask {
            return Err("地址和网关不能使用子网的网络地址或广播地址".into());
        }
        if u32::from(config.address) & mask != u32::from(config.gateway) & mask
            || config.address == config.gateway
        {
            return Err("网关必须与地址处于同一子网，且不能相同".into());
        }
        Ok(config)
    }

    #[cfg(windows)]
    fn script(&self, adapter_name: &str) -> String {
        let adapter = STANDARD.encode(adapter_name.as_bytes());
        format!(
            r#"$ErrorActionPreference = 'Stop'
$adapter = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{adapter}'))
& "$env:SystemRoot\System32\netsh.exe" interface ipv4 set address "name=$adapter" static {} {} {} 1
if ($LASTEXITCODE -ne 0) {{ exit 10 }}
& "$env:SystemRoot\System32\netsh.exe" interface ipv4 set dnsservers "name=$adapter" static {} primary validate=no
if ($LASTEXITCODE -ne 0) {{ exit 11 }}
exit 0"#,
            self.address, self.mask, self.gateway, self.dns
        )
    }

    pub fn apply(&self, adapter_name: &str) -> Result<(), String> {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let encoded = STANDARD.encode(
                self.script(adapter_name)
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>(),
            );
            // Only fixed switches and a base64 payload reach the outer command.
            // Windows displays the standard UAC dialog at this point.
            let wrapper = format!(
                r#"try {{ $p = Start-Process -FilePath "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -Verb RunAs -WindowStyle Hidden -ArgumentList '-NoProfile -NonInteractive -WindowStyle Hidden -EncodedCommand {encoded}' -Wait -PassThru; exit $p.ExitCode }} catch {{ exit 12 }}"#
            );
            let output = std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", &wrapper])
                .creation_flags(0x08000000)
                .output()
                .map_err(|e| e.to_string())?;
            match output.status.code() {
                Some(0) => Ok(()),
                Some(10) => Err("IPv4 地址设置失败，请检查地址和网卡状态".into()),
                Some(11) => Err(
                    "IPv4 地址已修改，但 DNS 设置失败；请重试或打开 Windows 网络设置检查".into(),
                ),
                _ => Err("网络设置未完成，管理员授权可能已取消".into()),
            }
        }
        #[cfg(not(windows))]
        {
            self.apply_network_manager(adapter_name)
        }
    }
}

#[cfg(not(windows))]
impl Configuration {
    /// The NetworkManager argument vector for `connection modify`, kept separate
    /// from execution so the "one argument per field" property is testable.
    fn modify_arguments(&self, connection: &str) -> Vec<String> {
        let prefix = u32::from(self.mask).leading_ones();
        vec![
            "connection".to_string(),
            "modify".to_string(),
            connection.to_string(),
            "ipv4.method".to_string(),
            "manual".to_string(),
            "ipv4.addresses".to_string(),
            format!("{}/{}", self.address, prefix),
            "ipv4.gateway".to_string(),
            self.gateway.to_string(),
            "ipv4.dns".to_string(),
            self.dns.to_string(),
        ]
    }

    fn apply_network_manager(&self, adapter_name: &str) -> Result<(), String> {
        let connection = active_connection(adapter_name)?;
        run_nmcli(&self.modify_arguments(&connection))
            .map_err(|error| format!("修改连接“{connection}”的静态 IPv4 设置失败：{error}"))?;
        run_nmcli(&activate_arguments(&connection))
            .map_err(|error| format!("重新启用连接“{connection}”失败：{error}"))?;
        Ok(())
    }
}

/// The active NetworkManager connection serving `adapter_name`.
#[cfg(not(windows))]
fn active_connection(adapter_name: &str) -> Result<String, String> {
    let listing = run_nmcli(&["-t", "-f", "NAME,DEVICE", "connection", "show", "--active"])?;
    connection_for_device(&listing, adapter_name).ok_or_else(|| {
        format!(
            "没有找到网卡“{adapter_name}”上已激活的 NetworkManager 连接；请先在系统设置中连接该网络，再设置静态 IPv4"
        )
    })
}

#[cfg(not(windows))]
fn activate_arguments(connection: &str) -> Vec<String> {
    vec![
        "connection".to_string(),
        "up".to_string(),
        connection.to_string(),
    ]
}

/// One `-t` record as `(connection name, device)`.
///
/// Terse mode escapes a colon that belongs to a field as `\:` and a backslash
/// as `\\`, so the field separator is the first colon that is not escaped.
#[cfg(not(windows))]
fn terse_name_and_device(line: &str) -> Option<(String, String)> {
    let mut name = String::new();
    let mut device = String::new();
    let mut target = &mut name;
    let mut separated = false;
    let mut characters = line.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => target.push(characters.next()?),
            ':' if !separated => {
                separated = true;
                target = &mut device;
            }
            _ => target.push(character),
        }
    }
    separated.then_some((name, device))
}

#[cfg(not(windows))]
fn connection_for_device(listing: &str, adapter_name: &str) -> Option<String> {
    let wanted = adapter_name.trim();
    listing
        .lines()
        .filter_map(terse_name_and_device)
        .find_map(|(name, device)| (device.trim() == wanted).then_some(name))
}

/// Runs `nmcli` with a real argument vector. No shell is involved.
#[cfg(not(windows))]
fn run_nmcli<S: AsRef<std::ffi::OsStr>>(arguments: &[S]) -> Result<String, String> {
    let output = std::process::Command::new("nmcli")
        .args(arguments)
        .output()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "未找到 nmcli（NetworkManager 命令行工具）；请先安装并启用 NetworkManager"
                    .to_string()
            } else {
                format!("无法执行 nmcli：{error}")
            }
        })?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    Err(describe_nmcli_failure(&output))
}

#[cfg(not(windows))]
fn describe_nmcli_failure(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = stderr.trim();
    let stdout = stdout.trim();
    let details = match (stderr.is_empty(), stdout.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stderr.to_string(),
        (true, false) => stdout.to_string(),
        (false, false) => format!("{stderr} {stdout}"),
    };
    let lower = details.to_lowercase();
    if lower.contains("not authorized")
        || lower.contains("authorization")
        || lower.contains("polkit")
        || lower.contains("permission denied")
        || lower.contains("not allowed")
    {
        return format!(
            "NetworkManager 拒绝修改网络：需要管理员授权（polkit）。请在弹出的授权框中允许；若没有出现授权框，请让管理员为当前用户配置 NetworkManager 权限。原始错误：{details}"
        );
    }
    if lower.contains("could not connect to networkmanager")
        || lower.contains("networkmanager is not running")
        || lower.contains("networkmanager not running")
    {
        return format!(
            "无法连接到 NetworkManager 服务；请确认 NetworkManager 正在运行。原始错误：{details}"
        );
    }
    if details.is_empty() {
        format!("nmcli 执行失败（退出码 {:?}）", output.status.code())
    } else {
        format!("nmcli 执行失败：{details}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_invalid_subnets_and_command_text() {
        assert!(Configuration::parse("10.0.0.8", "255.255.255.0", "10.0.0.1", "1.1.1.1").is_ok());
        assert!(
            Configuration::parse("10.0.0.8; exit", "255.255.255.0", "10.0.0.1", "1.1.1.1").is_err()
        );
        assert!(Configuration::parse("10.0.0.8", "255.0.255.0", "10.0.0.1", "1.1.1.1").is_err());
        assert!(Configuration::parse("10.0.0.8", "255.255.255.0", "10.1.0.1", "1.1.1.1").is_err());
        assert!(
            Configuration::parse("10.0.0.255", "255.255.255.0", "10.0.0.1", "1.1.1.1").is_err()
        );
    }

    #[cfg(windows)]
    #[test]
    fn adapter_names_cannot_inject_powershell() {
        let config =
            Configuration::parse("10.0.0.8", "255.255.255.0", "10.0.0.1", "1.1.1.1").unwrap();
        let script = config.script("Ethernet'; exit 99; #");
        assert!(!script.contains("exit 99"));
        assert!(script.contains("static 10.0.0.8 255.255.255.0 10.0.0.1"));
    }

    #[cfg(not(windows))]
    #[test]
    fn nmcli_arguments_keep_the_connection_name_as_one_argument() {
        let config =
            Configuration::parse("10.0.0.8", "255.255.255.0", "10.0.0.1", "1.1.1.1").unwrap();
        // A connection name with spaces must reach nmcli as a single argument;
        // passing it through a shell would split it and edit the wrong thing.
        let connection = "Wired connection 1";
        assert_eq!(
            config.modify_arguments(connection),
            vec![
                "connection",
                "modify",
                connection,
                "ipv4.method",
                "manual",
                "ipv4.addresses",
                "10.0.0.8/24",
                "ipv4.gateway",
                "10.0.0.1",
                "ipv4.dns",
                "1.1.1.1",
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        );
        assert_eq!(
            activate_arguments(connection),
            vec![
                "connection".to_string(),
                "up".to_string(),
                connection.to_string()
            ]
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn terse_nmcli_output_unescapes_colons() {
        assert_eq!(
            terse_name_and_device("Wired connection 1:eth0"),
            Some(("Wired connection 1".to_string(), "eth0".to_string()))
        );
        assert_eq!(
            terse_name_and_device("Wired\\: work:eth0"),
            Some(("Wired: work".to_string(), "eth0".to_string()))
        );
        assert_eq!(terse_name_and_device("no separator"), None);
        assert_eq!(
            connection_for_device("one:eth0\nWired\\: work:eth1\n", "eth1"),
            Some("Wired: work".to_string())
        );
        assert_eq!(connection_for_device("one:eth0\n", "eth9"), None);
    }
}
