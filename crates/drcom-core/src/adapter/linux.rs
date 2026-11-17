//! Linux enumeration: the kernel state lives in `ip -j` output and sysfs.
//! There is no adapter GUID table, so the interface name is the stable id —
//! predictable network names (`enp46s0`, `wlp0s20f3`) are persistent across
//! reboots and DHCP leases, which is the property the remembered adapter
//! needs, and they are also what the rest of this module reports.

// Ported from origin/linux:src/adapter.rs (9735565), with strict address
// state, trusted command paths and per-link DNS added here.
use super::command;
use super::{AdapterInfo, NetworkDetails, mac_from_bytes, usable_ipv4};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

/// Where Linux exposes per-interface state.
const SYS_CLASS_NET: &str = "/sys/class/net";
/// The configured resolver, shared by every interface on Linux.
const RESOLV_CONF: &str = "/etc/resolv.conf";

/// One interface of `ip -j addr show`. Unknown fields are ignored.
#[derive(Debug, serde::Deserialize)]
struct InterfaceRow {
    #[serde(default)]
    ifindex: u32,
    #[serde(default)]
    ifname: String,
    #[serde(default)]
    flags: Vec<String>,
    #[serde(default)]
    link_type: String,
    #[serde(default)]
    operstate: String,
    /// The current (possibly randomized) MAC, as `aa:bb:cc:dd:ee:ff`.
    #[serde(default)]
    address: String,
    #[serde(default)]
    addr_info: Vec<AddressRow>,
}

/// One `inet`/`inet6` entry of `addr_info`.
#[derive(Debug, serde::Deserialize)]
struct AddressRow {
    #[serde(default)]
    family: String,
    #[serde(default)]
    local: String,
    #[serde(default)]
    prefixlen: u8,
    #[serde(default)]
    flags: Vec<String>,
    #[serde(default)]
    tentative: bool,
    #[serde(default)]
    dadfailed: bool,
    #[serde(default)]
    deprecated: bool,
    #[serde(default)]
    preferred_life_time: Option<serde_json::Value>,
}

/// One row of `ip -j route show default`.
#[derive(Debug, serde::Deserialize)]
struct DefaultRouteRow {
    #[serde(default)]
    dev: Option<String>,
    #[serde(default)]
    gateway: Option<String>,
}

/// A `usable_ipv4` address with the prefix length the kernel reported.
#[derive(Debug, Clone, Copy)]
struct UsableAddress {
    ipv4: Ipv4Addr,
    prefixlen: u8,
}

pub fn list() -> Vec<AdapterInfo> {
    // Missing iproute2 or malformed output means enumeration is unavailable.
    // Return no automatic candidates; session discovery fails safely before
    // credentials, rather than inventing an adapter or MAC/source binding.
    let Some(interfaces) = ip_json::<InterfaceRow>(&["addr", "show"]) else {
        return Vec::new();
    };
    // A default route is a ranking signal, not a requirement: an interface
    // with only an on-link campus network can still carry the login.
    let mut default_routes =
        ip_json::<DefaultRouteRow>(&["-4", "route", "show", "default"]).unwrap_or_default();
    default_routes.extend(
        ip_json::<DefaultRouteRow>(&["-6", "route", "show", "default"]).unwrap_or_default(),
    );
    let dns = read_dns();
    let mut adapters = Vec::new();
    for interface in &interfaces {
        if !is_physical_interface(interface) {
            continue;
        }
        let Some(mac) = parse_mac(&interface.address) else {
            continue;
        };
        let friendly_name = interface.ifname.clone();
        let description =
            driver_description(&friendly_name).unwrap_or_else(|| friendly_name.clone());
        let addresses = usable_addresses(&interface.addr_info);
        if addresses.is_empty() {
            continue;
        }
        let ipv6: Vec<IpAddr> = interface
            .addr_info
            .iter()
            .filter_map(ipv6_address)
            .collect();
        let routes: Vec<&DefaultRouteRow> = default_routes
            .iter()
            .filter(|route| route.dev.as_deref() == Some(interface.ifname.as_str()))
            .collect();
        let gateways: Vec<IpAddr> = routes
            .iter()
            .filter_map(|route| route.gateway.as_deref()?.parse().ok())
            .collect();
        let shared = NetworkDetails {
            subnet_mask: None,
            ipv6,
            gateways,
            // resolvectl is per-link; /etc/resolv.conf is the actual system
            // resolver fallback (possibly a local stub), not fabricated DHCP.
            dns: command::read("resolvectl", &["dns", &friendly_name])
                .map(|text| parse_link_dns(&text))
                .filter(|ips| !ips.is_empty())
                .unwrap_or_else(|| dns.clone()),
        };
        // One AdapterInfo per usable address, the same shape the Windows
        // path produces for an adapter with several IPv4 addresses.
        for address in addresses {
            adapters.push(AdapterInfo {
                stable_id: friendly_name.clone(),
                friendly_name: friendly_name.clone(),
                description: description.clone(),
                ipv4: address.ipv4,
                mac,
                is_wireless: is_wireless(&friendly_name),
                has_gateway: !routes.is_empty(),
                interface_index: interface.ifindex,
                details: NetworkDetails {
                    subnet_mask: mask_from_prefix(address.prefixlen),
                    ..shared.clone()
                },
            });
        }
    }
    // Deterministic order, independent of how `ip` happened to walk the
    // kernel's interface list.
    adapters.sort_by_key(|adapter| (adapter.interface_index, adapter.ipv4));
    adapters
}

/// Runs `ip -j <arguments>` and decodes the JSON array it prints.
///
/// Values are passed as separate arguments, never through a shell.
fn ip_json<T: serde::de::DeserializeOwned>(arguments: &[&str]) -> Option<Vec<T>> {
    let mut args = vec!["-j"];
    args.extend_from_slice(arguments);
    let output = command::output("ip", &args, std::time::Duration::from_secs(3)).ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

/// A real, administratively up interface that is not loopback.
fn is_physical_interface(interface: &InterfaceRow) -> bool {
    if interface.ifindex == 0
        || !command::valid_interface_name(&interface.ifname)
        || interface.link_type != "ether"
        || !matches!(interface.operstate.as_str(), "UP" | "UNKNOWN")
        || interface.link_type == "loopback"
        || interface.ifname == "lo"
        || interface.flags.iter().any(|flag| flag == "LOOPBACK")
        || !interface.flags.iter().any(|flag| flag == "UP")
    {
        return false;
    }
    // Bridges, veth pairs, tun/tap and docker interfaces are backed by no
    // device, so they have neither `device` nor `phy80211` in sysfs.
    sysfs_path(&interface.ifname, "device").exists()
        || sysfs_path(&interface.ifname, "phy80211").exists()
}

fn sysfs_path(ifname: &str, entry: &str) -> PathBuf {
    Path::new(SYS_CLASS_NET).join(ifname).join(entry)
}

fn is_wireless(ifname: &str) -> bool {
    sysfs_path(ifname, "wireless").exists() || sysfs_path(ifname, "phy80211").exists()
}

/// Every distinct usable IPv4 address of one interface, with its prefix.
fn usable_addresses(rows: &[AddressRow]) -> Vec<UsableAddress> {
    let mut addresses: Vec<UsableAddress> = Vec::new();
    for row in rows {
        if row.family != "inet" || !address_ready(row) {
            continue;
        }
        let Ok(ipv4) = row.local.parse::<Ipv4Addr>() else {
            continue;
        };
        if !usable_ipv4(ipv4) || addresses.iter().any(|existing| existing.ipv4 == ipv4) {
            continue;
        }
        addresses.push(UsableAddress {
            ipv4,
            prefixlen: row.prefixlen,
        });
    }
    addresses
}

fn address_ready(row: &AddressRow) -> bool {
    !row.tentative
        && !row.dadfailed
        && !row.deprecated
        && !row
            .flags
            .iter()
            .any(|flag| matches!(flag.as_str(), "tentative" | "dadfailed" | "deprecated"))
        && !row
            .preferred_life_time
            .as_ref()
            .is_some_and(|value| value.as_u64() == Some(0))
}

fn ipv6_address(row: &AddressRow) -> Option<IpAddr> {
    if row.family != "inet6" || !address_ready(row) {
        return None;
    }
    row.local.parse::<IpAddr>().ok().filter(IpAddr::is_ipv6)
}

/// IPv4 mask from the prefix length `ip` reports. A zero prefix means "no
/// mask was reported", matching the Windows path's `None` for invalid data.
fn mask_from_prefix(prefixlen: u8) -> Option<Ipv4Addr> {
    (1..=32)
        .contains(&prefixlen)
        .then(|| Ipv4Addr::from(u32::MAX << (32 - prefixlen)))
}

/// Parses the `aa:bb:cc:dd:ee:ff` form `ip` prints. `mac_from_bytes`
/// rejects the all-zero MAC of unconfigured or virtual interfaces.
fn parse_mac(value: &str) -> Option<u64> {
    let mut octets = [0u8; 6];
    let mut parts = value.trim().split(':');
    for octet in &mut octets {
        let part = parts.next()?;
        if part.len() != 2 {
            return None;
        }
        *octet = u8::from_str_radix(part, 16).ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    mac_from_bytes(&octets)
}

/// Driver and PCI/USB product of the backing device, e.g. `r8169 10EC:8168`.
fn driver_description(ifname: &str) -> Option<String> {
    let text = std::fs::read_to_string(sysfs_path(ifname, "device/uevent")).ok()?;
    uevent_description(&text)
}

fn uevent_description(text: &str) -> Option<String> {
    let driver = uevent_field(text, "DRIVER");
    let product = uevent_field(text, "PRODUCT").or_else(|| uevent_field(text, "PCI_ID"));
    match (driver, product) {
        (Some(driver), Some(product)) => Some(format!("{driver} {product}")),
        (Some(driver), None) => Some(driver.to_string()),
        (None, Some(product)) => Some(product.to_string()),
        (None, None) => None,
    }
}

fn uevent_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix(key)?
            .strip_prefix('=')?
            .split_whitespace()
            .next()
    })
}

/// Every resolver named in `/etc/resolv.conf`, deduplicated in file order.
fn read_dns() -> Vec<IpAddr> {
    std::fs::read_to_string(RESOLV_CONF)
        .map(|text| parse_nameservers(&text))
        .unwrap_or_default()
}

fn parse_nameservers(text: &str) -> Vec<IpAddr> {
    let mut servers = Vec::new();
    for line in text.lines() {
        // Comments start with '#'; ';' is the legacy form.
        let line = line.split(['#', ';']).next().unwrap_or("").trim();
        let mut fields = line.split_whitespace();
        if fields.next() != Some("nameserver") {
            continue;
        }
        let Some(value) = fields.next() else {
            continue;
        };
        let Some(address) = parse_nameserver(value) else {
            continue;
        };
        if !servers.contains(&address) {
            servers.push(address);
        }
    }
    servers
}

/// A `nameserver` value, tolerating a `%interface` zone and a port suffix.
fn parse_nameserver(value: &str) -> Option<IpAddr> {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix('[') {
        let (inner, suffix) = rest.split_once(']')?;
        if !suffix.is_empty() {
            suffix.strip_prefix(':')?.parse::<u16>().ok()?;
        }
        return inner.split('%').next()?.parse().ok();
    }
    let address = value.split('%').next().unwrap_or(value);
    if let Ok(ip) = address.parse::<IpAddr>() {
        return Some(ip);
    }
    // `[2001:db8::1]:53` — brackets are the unambiguous IPv6 port form.
    if let Some(inner) = address
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        return inner.parse().ok();
    }
    // `1.1.1.1:53` — only IPv4 can carry an unbracketed port, because a
    // bare trailing `:port` is also a valid IPv6 address.
    let (host, port) = address.rsplit_once(':')?;
    port.parse::<u16>().ok()?;
    host.parse::<Ipv4Addr>().ok().map(IpAddr::V4)
}

fn parse_link_dns(text: &str) -> Vec<IpAddr> {
    let mut result = Vec::new();
    for token in text
        .lines()
        .filter_map(|line| line.split_once(": ").map(|(_, rhs)| rhs))
        .flat_map(str::split_whitespace)
    {
        if let Some(ip) = parse_nameserver(token)
            && !result.contains(&ip)
        {
            result.push(ip);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed from this host's `ip -j addr show` row for enp46s0.
    const INTERFACE: &str = r#"[{"ifindex":2,"ifname":"enp46s0","flags":["BROADCAST","MULTICAST","UP","LOWER_UP"],"operstate":"UP","link_type":"ether","address":"1c:83:41:c5:84:fd","addr_info":[{"family":"inet","local":"49.140.185.105","prefixlen":24,"scope":"global","label":"enp46s0"},{"family":"inet6","local":"2001:da8:b000:6703:3d57:4c84:b9d8:dee","prefixlen":64,"scope":"global"},{"family":"inet6","local":"fe80::43a5:f07d:92e8:c973","prefixlen":64,"scope":"link"}]}]"#;

    #[test]
    fn address_json_keeps_one_usable_address_per_family() {
        let interfaces: Vec<InterfaceRow> = serde_json::from_str(INTERFACE).unwrap();
        let interface = &interfaces[0];
        assert_eq!(interface.ifindex, 2);
        assert_eq!(interface.ifname, "enp46s0");
        assert!(interface.flags.iter().any(|flag| flag == "UP"));
        let addresses = usable_addresses(&interface.addr_info);
        assert_eq!(addresses.len(), 1);
        assert_eq!(addresses[0].ipv4, Ipv4Addr::new(49, 140, 185, 105));
        assert_eq!(addresses[0].prefixlen, 24);
        let ipv6: Vec<IpAddr> = interface
            .addr_info
            .iter()
            .filter_map(ipv6_address)
            .collect();
        assert_eq!(ipv6.len(), 2, "link-local IPv6 must not be dropped here");
        assert!(ipv6.iter().any(super::super::is_link_local));
    }

    #[test]
    fn only_usable_addresses_become_candidates() {
        let rows: Vec<AddressRow> = serde_json::from_str(
                r#"[{"family":"inet","local":"127.0.0.1","prefixlen":8},{"family":"inet","local":"169.254.10.20","prefixlen":16},{"family":"inet","local":"49.140.185.105","prefixlen":24},{"family":"inet","local":"49.140.185.105","prefixlen":24},{"family":"inet6","local":"fe80::1","prefixlen":64},{"family":"inet","local":"not-an-address","prefixlen":24}]"#,
            )
            .unwrap();
        let addresses = usable_addresses(&rows);
        assert_eq!(addresses.len(), 1);
        assert_eq!(addresses[0].ipv4, Ipv4Addr::new(49, 140, 185, 105));
    }

    #[test]
    fn tentative_failed_and_expired_addresses_are_never_credentials_sources() {
        let rows: Vec<AddressRow> = serde_json::from_str(
            r#"[
            {"family":"inet","local":"10.0.0.2","prefixlen":24,"tentative":true},
            {"family":"inet","local":"10.0.0.3","prefixlen":24,"flags":["dadfailed"]},
            {"family":"inet","local":"10.0.0.4","prefixlen":24,"preferred_life_time":0},
            {"family":"inet","local":"10.0.0.5","prefixlen":24,"deprecated":true},
            {"family":"inet6","local":"2001:db8::1","prefixlen":64,"dadfailed":true},
            {"family":"inet6","local":"2001:db8::2","prefixlen":64,"flags":["deprecated"]},
            {"family":"inet","local":"10.0.0.6","prefixlen":24,"preferred_life_time":4294967295}
        ]"#,
        )
        .unwrap();
        assert_eq!(
            usable_addresses(&rows)
                .iter()
                .map(|row| row.ipv4)
                .collect::<Vec<_>>(),
            vec![Ipv4Addr::new(10, 0, 0, 6)]
        );
        assert!(rows.iter().filter_map(ipv6_address).next().is_none());
    }

    #[test]
    fn link_dns_and_bracketed_zone_and_port_are_parsed_without_fake_resolvers() {
        assert_eq!(
            parse_link_dns("Link 2 (en0): 10.0.0.1 fe80::1%en0\nGlobal: 1.1.1.1\n"),
            vec![
                "10.0.0.1".parse::<IpAddr>().unwrap(),
                "fe80::1".parse().unwrap(),
                "1.1.1.1".parse().unwrap()
            ]
        );
        assert_eq!(
            parse_nameserver("[fe80::1%en0]:53"),
            Some("fe80::1".parse().unwrap())
        );
        assert_eq!(parse_nameserver("[2001:db8::1]:65536"), None);
        assert_eq!(parse_mac("1:83:41:c5:84:fd"), None);
        assert_eq!(parse_mac("1c:83:41:c5:84:fd:aa"), None);
    }

    #[test]
    fn prefix_lengths_become_ipv4_masks() {
        assert_eq!(mask_from_prefix(24), Some(Ipv4Addr::new(255, 255, 255, 0)));
        assert_eq!(mask_from_prefix(32), Some(Ipv4Addr::BROADCAST));
        assert_eq!(mask_from_prefix(8), Some(Ipv4Addr::new(255, 0, 0, 0)));
        assert_eq!(mask_from_prefix(0), None, "no prefix was reported");
        assert_eq!(mask_from_prefix(33), None, "invalid prefix");
    }

    #[test]
    fn sysfs_mac_is_parsed_strictly() {
        assert_eq!(parse_mac("1c:83:41:c5:84:fd"), Some(0x1c83_41c5_84fd));
        assert_eq!(parse_mac("1C:83:41:C5:84:FD"), Some(0x1c83_41c5_84fd));
        assert_eq!(parse_mac("00:00:00:00:00:00"), None, "unconfigured MAC");
        assert_eq!(parse_mac("1c:83:41:c5:84"), None);
        assert_eq!(parse_mac("1c-83-41-c5-84-fd"), None);
    }

    #[test]
    fn uevent_supplies_driver_and_product_description() {
        let uevent = "DRIVER=r8169\nPCI_CLASS=20000\nPCI_ID=10EC:8168\nPCI_SUBSYS_ID=1D72:2135\n";
        assert_eq!(
            uevent_description(uevent).as_deref(),
            Some("r8169 10EC:8168")
        );
        assert_eq!(
            uevent_description("DRIVER=iwlwifi\n").as_deref(),
            Some("iwlwifi")
        );
        assert_eq!(uevent_description("MODALIAS=foo\n"), None);
    }

    #[test]
    fn resolv_conf_keeps_the_configured_stub_resolver() {
        let text = "# managed by systemd-resolved\nnameserver 127.0.0.53\nnameserver fe80::1%eth0\nnameserver 1.1.1.1:53\nnameserver 127.0.0.53\nnameserver not-an-address\nsearch .\n";
        assert_eq!(
            parse_nameservers(text),
            vec![
                "127.0.0.53".parse::<IpAddr>().unwrap(),
                "fe80::1".parse::<IpAddr>().unwrap(),
                "1.1.1.1".parse::<IpAddr>().unwrap(),
            ]
        );
    }

    #[test]
    fn default_routes_carry_the_gateway_of_their_interface() {
        let rows: Vec<DefaultRouteRow> = serde_json::from_str(
                r#"[{"dst":"default","gateway":"49.140.185.254","dev":"enp46s0","metric":100},{"dst":"default","dev":"tun0","metric":50}]"#,
            )
            .unwrap();
        assert_eq!(rows[0].dev.as_deref(), Some("enp46s0"));
        assert_eq!(rows[0].gateway.as_deref(), Some("49.140.185.254"));
        assert!(rows[1].gateway.is_none(), "a link default has no next hop");
    }

    #[test]
    fn interfaces_without_a_backing_device_are_not_physical() {
        for ifname in ["docker0", "veth1a", "tun0", "br-0123"] {
            let row = InterfaceRow {
                ifindex: 9,
                ifname: ifname.into(),
                flags: vec!["UP".into()],
                link_type: "ether".into(),
                operstate: "UP".into(),
                address: "00:00:00:00:00:00".into(),
                addr_info: Vec::new(),
            };
            assert!(!is_physical_interface(&row), "{ifname}");
        }
        // `lo` is refused even when its flags do not spell out LOOPBACK.
        let loopback = InterfaceRow {
            ifindex: 1,
            ifname: "lo".into(),
            flags: vec!["UP".into()],
            link_type: "ether".into(),
            operstate: "UP".into(),
            address: "00:00:00:00:00:00".into(),
            addr_info: Vec::new(),
        };
        assert!(!is_physical_interface(&loopback));
    }
}
