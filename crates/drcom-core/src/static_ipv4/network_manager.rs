//! Adapted from origin/linux:src/static_ipv4.rs. An argument vector prevents
//! shell injection; UUID selectors also prevent connection-name option confusion.
use super::Configuration;
#[cfg(target_os = "linux")]
use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[cfg(target_os = "linux")]
fn nmcli() -> Option<PathBuf> {
    // No PATH lookup: this command can request polkit authorization. Do not run
    // an executable planted in the current directory or a user-supplied PATH.
    ["/usr/bin/nmcli", "/bin/nmcli"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

#[cfg(target_os = "linux")]
pub(super) fn unavailable_reason() -> Option<&'static str> {
    if nmcli().is_some() {
        None
    } else {
        Some("未找到系统 nmcli；请在系统网络设置中配置，或安装并启用 NetworkManager")
    }
}

fn modify_arguments(config: &Configuration, uuid: &str) -> Vec<String> {
    vec![
        "connection".into(),
        "modify".into(),
        "uuid".into(),
        uuid.into(),
        "ipv4.method".into(),
        "manual".into(),
        "ipv4.addresses".into(),
        format!(
            "{}/{}",
            config.address,
            u32::from(config.mask).leading_ones()
        ),
        "ipv4.gateway".into(),
        config.gateway.to_string(),
        "ipv4.dns".into(),
        config.dns.to_string(),
    ]
}

fn activate_arguments(uuid: &str) -> Vec<String> {
    vec!["connection".into(), "up".into(), "uuid".into(), uuid.into()]
}

fn terse_fields(line: &str) -> Option<(String, String)> {
    let mut first = String::new();
    let mut second = String::new();
    let mut target = &mut first;
    let mut separated = false;
    let mut chars = line.chars();
    while let Some(character) = chars.next() {
        match character {
            '\\' => target.push(chars.next()?),
            ':' if !separated => {
                separated = true;
                target = &mut second;
            }
            ':' => return None,
            other => target.push(other),
        }
    }
    separated.then_some((first, second))
}

fn valid_uuid(uuid: &str) -> bool {
    uuid.len() == 36
        && uuid.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn uuid_for_device(listing: &str, adapter: &str) -> Result<String, String> {
    let matches: Vec<String> = listing
        .lines()
        .filter_map(terse_fields)
        .filter_map(|(uuid, device)| (device == adapter && valid_uuid(&uuid)).then_some(uuid))
        .collect();
    match matches.as_slice() {
        [uuid] => Ok(uuid.clone()),
        [] => Err(format!(
            "未找到网卡“{adapter}”的活动 NetworkManager 连接；请先在系统网络设置中连接该网卡"
        )),
        _ => Err(format!(
            "网卡“{adapter}”对应多个活动连接；请在系统网络设置中选择并配置，未修改任何连接"
        )),
    }
}

#[cfg(target_os = "linux")]
fn run_nmcli(executable: &Path, arguments: &[String]) -> Result<String, String> {
    let output = Command::new(executable)
        .args(["--wait", "20"])
        .args(arguments)
        .env("LC_ALL", "C")
        .output()
        .map_err(|error| format!("无法执行系统 nmcli：{error}"))?;
    if output.status.success() {
        return String::from_utf8(output.stdout)
            .map_err(|_| "nmcli 返回了无效 UTF-8，未继续配置".into());
    }
    let details = String::from_utf8_lossy(&output.stderr);
    let lower = details.to_ascii_lowercase();
    if [
        "not authorized",
        "authorization",
        "polkit",
        "permission denied",
        "not allowed",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        return Err("NetworkManager 拒绝修改网络；需要桌面会话的 polkit 授权，请允许系统授权或在系统网络设置中配置".into());
    }
    if lower.contains("not running") || lower.contains("could not connect") {
        return Err("无法连接 NetworkManager；请确认服务和桌面会话 D-Bus 已启用".into());
    }
    Err(format!(
        "nmcli 操作失败（退出码 {:?}）：{}",
        output.status.code(),
        details.trim()
    ))
}

#[cfg(target_os = "linux")]
pub(super) fn apply(config: &Configuration, adapter: &str) -> Result<(), String> {
    if adapter.is_empty() || adapter.chars().any(char::is_control) {
        return Err("网卡名称无效，未修改任何连接".into());
    }
    let executable =
        nmcli().ok_or_else(|| unavailable_reason().unwrap_or("nmcli 不可用").to_string())?;
    let listing = run_nmcli(
        &executable,
        &[
            "--terse".into(),
            "--escape".into(),
            "yes".into(),
            "--fields".into(),
            "UUID,DEVICE".into(),
            "connection".into(),
            "show".into(),
            "--active".into(),
        ],
    )?;
    let uuid = uuid_for_device(&listing, adapter)?;
    run_nmcli(&executable, &modify_arguments(config, &uuid))
        .map_err(|error| format!("静态 IPv4 连接配置未完成：{error}"))?;
    run_nmcli(&executable, &activate_arguments(&uuid)).map_err(|error| {
        format!(
            "IPv4 配置已保存，但连接重新激活失败（可能短暂断网）；请在系统网络设置中检查：{error}"
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const UUID: &str = "01234567-89ab-cdef-0123-456789abcdef";
    #[test]
    fn nmcli_uuid_selectors_and_each_value_are_separate_arguments() {
        let config =
            Configuration::parse("10.0.0.8", "255.255.255.0", "10.0.0.1", "1.1.1.1").unwrap();
        assert_eq!(
            modify_arguments(&config, UUID),
            vec![
                "connection",
                "modify",
                "uuid",
                UUID,
                "ipv4.method",
                "manual",
                "ipv4.addresses",
                "10.0.0.8/24",
                "ipv4.gateway",
                "10.0.0.1",
                "ipv4.dns",
                "1.1.1.1"
            ]
        );
        assert_eq!(
            activate_arguments(UUID),
            vec!["connection", "up", "uuid", UUID]
        );
        assert!(!valid_uuid("--help"));
        assert!(!valid_uuid("'; touch /tmp/x"));
    }
    #[test]
    fn terse_output_handles_escaped_devices_and_rejects_ambiguity() {
        assert_eq!(
            terse_fields("uuid:eth\\:0"),
            Some(("uuid".into(), "eth:0".into()))
        );
        assert_eq!(
            terse_fields("uuid:back\\\\slash"),
            Some(("uuid".into(), "back\\slash".into()))
        );
        assert!(terse_fields("no separator").is_none());
        assert!(terse_fields("uuid:trailing\\").is_none());
        assert!(terse_fields("uuid:extra:field").is_none());
        assert_eq!(
            uuid_for_device(&format!("{UUID}:eth0\n"), "eth0").unwrap(),
            UUID
        );
        assert!(uuid_for_device(&format!("{UUID}:eth0\n{UUID}:eth0\n"), "eth0").is_err());
        assert!(uuid_for_device("--help:eth0", "eth0").is_err());
        assert!(uuid_for_device(&format!("{UUID}:eth0\n"), "eth1").is_err());
    }
}
