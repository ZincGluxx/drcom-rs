//! Pure parsers for macOS system output, tested on every build host.
use std::net::IpAddr;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct HardwarePort {
    pub name: String,
    pub device: String,
    pub wireless: bool,
}

pub(super) fn hardware_ports(text: &str) -> Vec<HardwarePort> {
    let mut name = None;
    let mut result = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("Hardware Port: ") {
            name = Some(value.to_owned());
        } else if let Some(device) = line.strip_prefix("Device: ") {
            let Some(name) = name.take() else {
                continue;
            };
            if !super::command::valid_interface_name(device) {
                continue;
            }
            // These are explicit OS hardware-port types, not substring guesses
            // based on a user-controlled interface name.
            if matches!(
                name.as_str(),
                "Bluetooth PAN" | "Thunderbolt Bridge" | "FireWire"
            ) {
                continue;
            }
            let wireless = matches!(name.as_str(), "Wi-Fi" | "AirPort");
            result.push(HardwarePort {
                name,
                device: device.to_owned(),
                wireless,
            });
        }
    }
    result
}

/// netstat -rn -f inet[6]. Keep default next hops with their actual BSD device.
pub(super) fn default_gateways(text: &str, name: &str) -> Vec<IpAddr> {
    let mut result = Vec::new();
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.first() != Some(&"default") || !fields.iter().skip(2).any(|field| *field == name)
        {
            continue;
        }
        if let Some(address) = fields
            .get(1)
            .and_then(|value| value.split('%').next()?.parse::<IpAddr>().ok())
            && !address.is_unspecified()
            && !result.contains(&address)
        {
            result.push(address);
        }
    }
    result
}

/// scutil --dns lists scoped and unscoped resolvers. Prefer this interface's
/// scoped entries, otherwise expose the system's unscoped resolver entries.
pub(super) fn dns(text: &str, index: u32) -> Vec<IpAddr> {
    let mut global = Vec::new();
    let mut scoped = Vec::new();
    for block in text.split("resolver #").skip(1) {
        let ifindex = block.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == "if_index")
                .then(|| value.split_whitespace().next()?.parse::<u32>().ok())
                .flatten()
        });
        let target = match ifindex {
            Some(value) if value == index => &mut scoped,
            None => &mut global,
            _ => continue,
        };
        for line in block.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            if !key.trim().starts_with("nameserver[") {
                continue;
            }
            if let Some(address) = value
                .trim()
                .split('%')
                .next()
                .and_then(|v| v.parse::<IpAddr>().ok())
                && !target.contains(&address)
            {
                target.push(address);
            }
        }
    }
    if scoped.is_empty() { global } else { scoped }
}

pub(super) fn active(text: &str) -> bool {
    text.lines().any(|line| line.trim() == "status: active")
}

pub(super) fn ready_ipv6(text: &str, address: IpAddr) -> bool {
    text.lines().any(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        fields.first() == Some(&"inet6")
            && fields
                .get(1)
                .and_then(|value| value.split('%').next()?.parse::<IpAddr>().ok())
                == Some(address)
            && !fields.iter().any(|value| {
                matches!(
                    *value,
                    "tentative" | "duplicated" | "deprecated" | "detached"
                )
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hardware_ports_only_accept_os_mapping_and_known_media() {
        let ports = hardware_ports(
            "Hardware Port: Wi-Fi\nDevice: en0\nEthernet Address: aa:bb:cc:dd:ee:ff\n\nHardware Port: USB 10/100/1000 LAN\nDevice: en8\n\nHardware Port: Thunderbolt Bridge\nDevice: bridge0\n\nHardware Port: Bluetooth PAN\nDevice: en3\nHardware Port: Ethernet\nDevice: ../bad\n",
        );
        assert_eq!(ports.len(), 2);
        assert!(ports[0].wireless);
        assert!(!ports[1].wireless);
        assert_eq!(ports[1].device, "en8");
    }
    #[test]
    fn dns_keeps_only_matching_scope_and_falls_back_to_global() {
        let text = "DNS configuration\nresolver #1\n nameserver[0] : 1.1.1.1\nresolver #2\n nameserver[0] : 10.0.0.1\n if_index : 4 (en0)\nresolver #3\n nameserver[0] : fe80::1%en8\n if_index : 8 (en8)\n";
        assert_eq!(dns(text, 4), vec!["10.0.0.1".parse::<IpAddr>().unwrap()]);
        assert_eq!(dns(text, 8), vec!["fe80::1".parse::<IpAddr>().unwrap()]);
        assert_eq!(dns(text, 9), vec!["1.1.1.1".parse::<IpAddr>().unwrap()]);
    }
    #[test]
    fn scoped_gateway_and_ipv6_state_are_not_guessed() {
        let text = "Destination Gateway Flags Netif Expire\ndefault 10.0.0.1 UGScg en0\ndefault fe80::1%en8 UGcIg en8\ndefault link#4 UCS en0\n";
        assert_eq!(
            default_gateways(text, "en0"),
            vec!["10.0.0.1".parse::<IpAddr>().unwrap()]
        );
        assert_eq!(
            default_gateways(text, "en8"),
            vec!["fe80::1".parse::<IpAddr>().unwrap()]
        );
        let text = "en0: flags=8863<UP,BROADCAST>\n inet6 fe80::1%en0 prefixlen 64 secured\n inet6 2001:db8::1 prefixlen 64 tentative\n status: active\n";
        assert!(active(text));
        assert!(ready_ipv6(text, "fe80::1".parse().unwrap()));
        assert!(!ready_ipv6(text, "2001:db8::1".parse().unwrap()));
        assert!(!active("status: inactive"));
    }
}
