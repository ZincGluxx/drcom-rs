//! Validated static IPv4 edits, executed only after the user presses Apply.
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
            let _ = adapter_name;
            Err("当前平台不支持静态 IPv4 设置".into())
        }
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
    #[test]
    fn adapter_names_cannot_inject_powershell() {
        let config =
            Configuration::parse("10.0.0.8", "255.255.255.0", "10.0.0.1", "1.1.1.1").unwrap();
        let script = config.script("Ethernet'; exit 99; #");
        assert!(!script.contains("exit 99"));
        assert!(script.contains("static 10.0.0.8 255.255.255.0 10.0.0.1"));
    }
}
