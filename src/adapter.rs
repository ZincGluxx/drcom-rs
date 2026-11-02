//! Physical adapter selection for the authentication socket.
//!
//! The login frame carries the adapter's IPv4 address and MAC, and the socket
//! binds both that address and the outbound interface. Hyper-V, VMware, WSL,
//! VPN, TAP and loopback adapters all
//! appear in the Windows adapter list, so they are filtered out by description
//! and by requiring an operational status, a usable IPv4 address and a real MAC.
//!
//! The last successfully authenticated adapter GUID is tried first. Remaining
//! candidates follow the OS route, gateway and media scores. Ranking only sets
//! the probe order: the session validates a challenge on each candidate before
//! sending credentials. Every preferred IPv4 address is a separate candidate.
//!
//! Linux has no adapter GUID table, so the platform module below fills the same
//! fields from `ip -j addr show`, `ip -j route show default` and sysfs. The
//! interface name (`enp46s0`, `wlp0s20f3`) is already persistent under
//! predictable network names, so it is the stable id there.
//!
//! The preferred-route lookup itself lives in [`crate::route`], because that is
//! also where the original installs its `/32` route for the same server.

use std::net::{IpAddr, Ipv4Addr};

use crate::route;

/// MAC used when no adapter can be queried, matching the C# client's default.
pub const DEFAULT_MAC: u64 = 0x8888_8888_8888;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterInfo {
    /// Windows adapter GUID; unlike IfIndex it survives reboot and DHCP changes.
    pub stable_id: String,
    pub friendly_name: String,
    pub description: String,
    pub ipv4: Ipv4Addr,
    pub mac: u64,
    pub is_wireless: bool,
    pub has_gateway: bool,
    /// IPv4 interface index, comparable with [`route::best_interface_index`].
    pub interface_index: u32,
    pub details: NetworkDetails,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkDetails {
    pub subnet_mask: Option<Ipv4Addr>,
    pub ipv6: Vec<IpAddr>,
    pub gateways: Vec<IpAddr>,
    pub dns: Vec<IpAddr>,
}

/// Is this the `fe80::/10` link-local range?
///
/// A link-local address is how IPv6 looks when it is running but not routable.
/// Everything that reports IPv6 state has to separate it from a global address,
/// otherwise "IPv6 is up" and "IPv6 can reach nothing" read identically — and
/// the second is what users describe as "IPv6 does not work".
pub fn is_link_local(address: &IpAddr) -> bool {
    match address {
        IpAddr::V6(value) => {
            let octets = value.octets();
            octets[0] == 0xfe && (octets[1] & 0xc0) == 0x80
        }
        IpAddr::V4(_) => false,
    }
}

/// How many routable IPv6 addresses the given adapters hold between them.
pub fn global_ipv6_count(adapters: &[AdapterInfo]) -> usize {
    adapters
        .iter()
        .flat_map(|adapter| adapter.details.ipv6.iter())
        .filter(|address| !is_link_local(address))
        .count()
}

/// Weights from the reference C# `ScoreCandidate`. Reaching the authentication
/// server is worth more than every other signal combined, so an adapter on the
/// real authentication path always wins over a better-looking idle one.
const SCORE_PREFERRED_ROUTE: u32 = 1000;
const SCORE_HAS_GATEWAY: u32 = 300;
const SCORE_REAL_MEDIA: u32 = 200;

fn score(adapter: &AdapterInfo, preferred_route: Option<u32>) -> u32 {
    let mut score = 0;
    if preferred_route.is_some_and(|index| index == adapter.interface_index) {
        score += SCORE_PREFERRED_ROUTE;
    }
    if adapter.has_gateway {
        score += SCORE_HAS_GATEWAY;
    }
    if !adapter.is_wireless {
        score += SCORE_REAL_MEDIA;
    }
    score
}

/// The adapter that should carry authentication traffic, best first. When
/// `destination` is given, the interface the OS would use to reach it is
/// preferred, which is the signal the original gets from its own route work.
pub fn select_for(destination: Option<Ipv4Addr>) -> Option<AdapterInfo> {
    candidates_for(destination).into_iter().next()
}

/// Ranking is only a probe order; the session must verify a challenge before login.
pub fn candidates_for(destination: Option<Ipv4Addr>) -> Vec<AdapterInfo> {
    let preferred_route = destination.and_then(route::best_interface_index);
    let mut adapters = list();
    rank_candidates(
        &mut adapters,
        preferred_route,
        remembered_adapter().as_deref(),
    );
    adapters
}

fn rank_candidates(adapters: &mut [AdapterInfo], route: Option<u32>, remembered: Option<&str>) {
    adapters.sort_by_key(|adapter| {
        (
            std::cmp::Reverse(
                remembered.is_some_and(|id| id.eq_ignore_ascii_case(&adapter.stable_id)),
            ),
            std::cmp::Reverse(score(adapter, route)),
            adapter.stable_id.clone(),
            adapter.ipv4,
        )
    });
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RememberedAdapter {
    stable_id: String,
}

/// Contains only the successful interface GUID, never addresses or credentials.
pub fn remembered_adapter() -> Option<String> {
    let path = crate::preferences::data_directory()
        .ok()?
        .join("adapter.json");
    let value: RememberedAdapter = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (!value.stable_id.is_empty()).then_some(value.stable_id)
}

pub(crate) fn remember_adapter(adapter: &AdapterInfo) -> std::io::Result<()> {
    if adapter.stable_id.is_empty() {
        return Err(std::io::Error::other("网卡缺少稳定标识"));
    }
    let directory = crate::preferences::data_directory()?;
    std::fs::create_dir_all(&directory)?;
    let path = directory.join("adapter.json");
    let temporary = directory.join(format!("adapter.{}.tmp", std::process::id()));
    let value = RememberedAdapter {
        stable_id: adapter.stable_id.clone(),
    };
    std::fs::write(&temporary, serde_json::to_vec(&value)?)?;
    let result = crate::preferences::replace(&temporary, &path);
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Adapters that can carry the campus authentication traffic, best first.
pub fn select() -> Option<AdapterInfo> {
    select_for(None)
}

/// Every adapter that looks physically present and usable.
pub fn list() -> Vec<AdapterInfo> {
    platform::list()
}

/// Descriptions that never carry campus authentication traffic.
const VIRTUAL_KEYWORDS: [&str; 24] = [
    "hyper-v",
    "vethernet",
    "vmware",
    "virtualbox",
    "vbox",
    "wsl",
    "loopback",
    "tap-",
    "tap ",
    "tun",
    "vpn",
    "bluetooth",
    "npcap",
    "winpcap",
    "tailscale",
    "zerotier",
    "wan miniport",
    "wi-fi direct",
    "wifi direct",
    "teredo",
    "isatap",
    "docker",
    "forticlient",
    "wireguard",
];

fn looks_virtual(friendly_name: &str, description: &str) -> bool {
    let haystack = format!("{friendly_name} {description}").to_lowercase();
    VIRTUAL_KEYWORDS
        .iter()
        .any(|keyword| haystack.contains(keyword))
}

fn mac_from_bytes(bytes: &[u8]) -> Option<u64> {
    if bytes.len() < 6 || bytes[..6].iter().all(|byte| *byte == 0) {
        return None;
    }
    let mut value = 0u64;
    for byte in &bytes[..6] {
        value = (value << 8) | u64::from(*byte);
    }
    Some(value)
}

/// Addresses that mean "no usable address yet".
fn usable_ipv4(address: Ipv4Addr) -> bool {
    !address.is_unspecified() && !address.is_loopback() && !address.is_link_local()
}

#[cfg(windows)]
mod platform {
    use super::{AdapterInfo, looks_virtual, mac_from_bytes, usable_ipv4};
    use std::net::Ipv4Addr;
    use windows_sys::Win32::Foundation::ERROR_BUFFER_OVERFLOW;
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_MULTICAST,
        GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6, AF_UNSPEC};

    /// `IfType` values from `iptypes.h`.
    const IF_TYPE_ETHERNET_CSMACD: u32 = 6;
    const IF_TYPE_IEEE80211: u32 = 71;
    /// `IfOperStatusUp`.
    const IF_OPER_STATUS_UP: i32 = 1;
    /// `MAX_ADAPTER_ADDRESS_LENGTH`.
    const MAX_ADAPTER_ADDRESS_LENGTH: usize = 8;

    pub fn list() -> Vec<AdapterInfo> {
        let mut size = 16 * 1024u32;
        // The API returns aligned structs with pointers, not a byte-aligned payload.
        let mut buffer: Vec<u64> = Vec::new();
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_INCLUDE_GATEWAYS;
        let mut succeeded = false;
        for _ in 0..3 {
            buffer.resize((size as usize).div_ceil(8), 0);
            let result = unsafe {
                GetAdaptersAddresses(
                    AF_UNSPEC as u32,
                    flags,
                    std::ptr::null_mut(),
                    buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
                    &mut size,
                )
            };
            if result == 0 {
                succeeded = true;
                break;
            }
            if result != ERROR_BUFFER_OVERFLOW {
                // Any other failure means enumeration is unavailable; the
                // caller then falls back to the default MAC and bind-all.
                return Vec::new();
            }
        }
        if !succeeded {
            return Vec::new();
        }

        let mut adapters = Vec::new();
        let mut current = buffer.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !current.is_null() {
            // SAFETY: `current` walks the list the API wrote into `buffer`,
            // which outlives this loop.
            if let Some(mut found) = unsafe { read_adapter(current) } {
                adapters.append(&mut found);
            }
            current = unsafe { (*current).Next };
        }
        adapters
    }

    unsafe fn read_adapter(current: *const IP_ADAPTER_ADDRESSES_LH) -> Option<Vec<AdapterInfo>> {
        let adapter = unsafe { &*current };
        if adapter.OperStatus != IF_OPER_STATUS_UP {
            return None;
        }
        if adapter.IfType != IF_TYPE_ETHERNET_CSMACD && adapter.IfType != IF_TYPE_IEEE80211 {
            return None;
        }
        let friendly_name = unsafe { wide_to_string(adapter.FriendlyName) };
        let description = unsafe { wide_to_string(adapter.Description) };
        if looks_virtual(&friendly_name, &description) {
            return None;
        }
        let length = adapter.PhysicalAddressLength as usize;
        if length == 0 || length > MAX_ADAPTER_ADDRESS_LENGTH {
            return None;
        }
        let mac = mac_from_bytes(&adapter.PhysicalAddress[..length])?;

        let mut addresses = Vec::new();
        let mut unicast = adapter.FirstUnicastAddress;
        while !unicast.is_null() {
            let entry = unsafe { &*unicast };
            if let Some(address) = unsafe { ipv4_from_socket_address(entry.Address.lpSockaddr) }
                && usable_ipv4(address)
                && entry.DadState == 4
            {
                let prefix = entry.OnLinkPrefixLength;
                let mask = if prefix > 0 && prefix <= 32 {
                    Some(Ipv4Addr::from(u32::MAX << (32 - prefix)))
                } else {
                    None
                };
                if !addresses.iter().any(|(ip, _)| *ip == address) {
                    addresses.push((address, mask));
                }
            }
            unicast = entry.Next;
        }

        let mut details = super::NetworkDetails::default();
        let mut unicast = adapter.FirstUnicastAddress;
        while !unicast.is_null() {
            let entry = unsafe { &*unicast };
            if let Some(address @ std::net::IpAddr::V6(_)) =
                unsafe { ip_from_socket(entry.Address.lpSockaddr) }
            {
                details.ipv6.push(address);
            }
            unicast = entry.Next;
        }
        let mut gateway = adapter.FirstGatewayAddress;
        while !gateway.is_null() {
            let entry = unsafe { &*gateway };
            if let Some(address) = unsafe { ip_from_socket(entry.Address.lpSockaddr) } {
                details.gateways.push(address);
            }
            gateway = entry.Next;
        }
        let mut dns = adapter.FirstDnsServerAddress;
        while !dns.is_null() {
            let entry = unsafe { &*dns };
            if let Some(address) = unsafe { ip_from_socket(entry.Address.lpSockaddr) } {
                details.dns.push(address);
            }
            dns = entry.Next;
        }

        if adapter.AdapterName.is_null() {
            return None;
        }
        // SAFETY: GetAdaptersAddresses owns a terminated adapter GUID string.
        let stable_id = unsafe { std::ffi::CStr::from_ptr(adapter.AdapterName.cast()) }
            .to_string_lossy()
            .into_owned();
        Some(
            addresses
                .into_iter()
                .map(|(ipv4, subnet_mask)| AdapterInfo {
                    stable_id: stable_id.clone(),
                    friendly_name: friendly_name.clone(),
                    description: description.clone(),
                    ipv4,
                    mac,
                    is_wireless: adapter.IfType == IF_TYPE_IEEE80211,
                    has_gateway: !adapter.FirstGatewayAddress.is_null(),
                    // `IfIndex` lives in the header's leading union, next to `Length`;
                    // it is the same index the routing APIs report.
                    interface_index: unsafe { adapter.Anonymous1.Anonymous.IfIndex },
                    details: super::NetworkDetails {
                        subnet_mask,
                        ..details.clone()
                    },
                })
                .collect(),
        )
    }

    unsafe fn ip_from_socket(
        address: *const windows_sys::Win32::Networking::WinSock::SOCKADDR,
    ) -> Option<std::net::IpAddr> {
        if address.is_null() {
            return None;
        }
        let family = unsafe { std::ptr::read_unaligned(address.cast::<u16>()) };
        if family == AF_INET6 {
            // SOCKADDR_IN6 stores its 16 address octets at offset 8.
            let bytes =
                unsafe { std::ptr::read_unaligned(address.cast::<u8>().add(8).cast::<[u8; 16]>()) };
            Some(std::net::IpAddr::V6(std::net::Ipv6Addr::from(bytes)))
        } else {
            unsafe { ipv4_from_socket_address(address) }.map(std::net::IpAddr::V4)
        }
    }

    unsafe fn ipv4_from_socket_address(
        address: *const windows_sys::Win32::Networking::WinSock::SOCKADDR,
    ) -> Option<Ipv4Addr> {
        if address.is_null() {
            return None;
        }
        // `sockaddr_in` is a fixed ABI: family at 0, port at 2, address at 4.
        // Reading the raw bytes avoids depending on the `IN_ADDR` union layout.
        let family = unsafe { std::ptr::read_unaligned(address as *const u16) };
        if family != AF_INET {
            return None;
        }
        let bytes =
            unsafe { std::ptr::read_unaligned((address as *const u8).add(4) as *const [u8; 4]) };
        Some(Ipv4Addr::from(bytes))
    }

    unsafe fn wide_to_string(value: *const u16) -> String {
        if value.is_null() {
            return String::new();
        }
        let mut length = 0usize;
        while unsafe { *value.add(length) } != 0 {
            length += 1;
            if length > 512 {
                break;
            }
        }
        let slice = unsafe { std::slice::from_raw_parts(value, length) };
        String::from_utf16_lossy(slice)
    }
}

#[cfg(target_os = "linux")]
mod platform {
    //! Linux enumeration: the kernel state lives in `ip -j` output and sysfs.
    //! There is no adapter GUID table, so the interface name is the stable id —
    //! predictable network names (`enp46s0`, `wlp0s20f3`) are persistent across
    //! reboots and DHCP leases, which is the property the remembered adapter
    //! needs, and they are also what the rest of this module reports.

    use super::{AdapterInfo, NetworkDetails, looks_virtual, mac_from_bytes, usable_ipv4};
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
        // A missing `ip` binary or unparsable output means enumeration is
        // unavailable, not that the machine has no network: return nothing and
        // let the caller fall back to the default MAC and bind-all, exactly as
        // the Windows path does when GetAdaptersAddresses fails.
        let Some(interfaces) = ip_json::<InterfaceRow>(&["addr", "show"]) else {
            return Vec::new();
        };
        // A default route is a ranking signal, not a requirement: an interface
        // with only an on-link campus network can still carry the login.
        let default_routes =
            ip_json::<DefaultRouteRow>(&["route", "show", "default"]).unwrap_or_default();
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
            if looks_virtual(&friendly_name, &description) {
                continue;
            }
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
                dns: dns.clone(),
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
        let output = std::process::Command::new("ip")
            .arg("-j")
            .args(arguments)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        serde_json::from_slice(&output.stdout).ok()
    }

    /// A real, administratively up interface that is not loopback.
    fn is_physical_interface(interface: &InterfaceRow) -> bool {
        if interface.ifindex == 0
            || interface.ifname.is_empty()
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
            if row.family != "inet" {
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

    fn ipv6_address(row: &AddressRow) -> Option<IpAddr> {
        if row.family != "inet6" {
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
            *octet = u8::from_str_radix(parts.next()?, 16).ok()?;
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
            let uevent =
                "DRIVER=r8169\nPCI_CLASS=20000\nPCI_ID=10EC:8168\nPCI_SUBSYS_ID=1D72:2135\n";
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
                address: "00:00:00:00:00:00".into(),
                addr_info: Vec::new(),
            };
            assert!(!is_physical_interface(&loopback));
        }
    }
}

#[cfg(all(not(windows), not(target_os = "linux")))]
mod platform {
    use super::AdapterInfo;

    pub fn list() -> Vec<AdapterInfo> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembered_guid_wins_even_when_index_and_dhcp_address_change() {
        let make = |id: &str, index, ip| AdapterInfo {
            stable_id: id.into(),
            friendly_name: "wired".into(),
            description: "physical".into(),
            ipv4: ip,
            mac: 0x1122_3344_5566,
            is_wireless: false,
            has_gateway: true,
            interface_index: index,
            details: Default::default(),
        };
        let mut items = vec![
            make("other", 1, Ipv4Addr::new(192, 168, 1, 2)),
            make("CAMPUS", 99, Ipv4Addr::new(49, 140, 1, 3)),
        ];
        rank_candidates(&mut items, Some(1), Some("campus"));
        assert_eq!(items[0].stable_id, "CAMPUS");
        // An unplugged/replaced remembered adapter must not prevent fallback.
        rank_candidates(&mut items, Some(1), Some("missing"));
        assert_eq!(items[0].stable_id, "other");
        let encoded = serde_json::to_string(&RememberedAdapter {
            stable_id: "campus".into(),
        })
        .unwrap();
        assert_eq!(encoded, r#"{"stable_id":"campus"}"#);
    }

    #[test]
    fn virtual_adapters_are_rejected() {
        assert!(looks_virtual(
            "vEthernet (WSL)",
            "Hyper-V Virtual Ethernet Adapter"
        ));
        assert!(looks_virtual(
            "Ethernet 2",
            "VMware Virtual Ethernet Adapter for VMnet8"
        ));
        assert!(looks_virtual(
            "Local Area Connection",
            "TAP-Windows Adapter V9"
        ));
        assert!(looks_virtual(
            "Ethernet",
            "Sangfor SSL VPN CS Support System VNIC"
        ));
        assert!(!looks_virtual(
            "以太网",
            "Realtek PCIe GbE Family Controller"
        ));
        assert!(!looks_virtual("WLAN", "Intel(R) Wi-Fi 6 AX201 160MHz"));
    }

    #[test]
    fn mac_conversion_requires_six_non_zero_bytes() {
        assert_eq!(mac_from_bytes(&[0, 1, 2, 3, 4, 5]), Some(0x0001_0203_0405));
        assert_eq!(
            mac_from_bytes(&[0x11, 0x22, 0x88, 0x77, 0x66, 0x55]),
            Some(0x1122_8877_6655)
        );
        assert_eq!(mac_from_bytes(&[0; 6]), None, "all-zero MAC is not usable");
        assert_eq!(
            mac_from_bytes(&[0, 1, 2, 3]),
            None,
            "short MAC is not usable"
        );
    }

    #[test]
    fn apipa_and_loopback_addresses_are_not_usable() {
        assert!(!usable_ipv4(Ipv4Addr::UNSPECIFIED));
        assert!(!usable_ipv4(Ipv4Addr::new(127, 0, 0, 1)));
        assert!(!usable_ipv4(Ipv4Addr::new(169, 254, 1, 1)));
        assert!(usable_ipv4(Ipv4Addr::new(10, 100, 61, 20)));
    }

    #[test]
    fn ranking_prefers_wired_adapters_with_a_gateway() {
        let wired = AdapterInfo {
            stable_id: "wired".into(),
            friendly_name: "以太网".into(),
            description: "Realtek PCIe GbE Family Controller".into(),
            ipv4: Ipv4Addr::new(10, 100, 61, 20),
            mac: 0x1122_3344_5566,
            is_wireless: false,
            has_gateway: true,
            interface_index: 7,
            details: NetworkDetails::default(),
        };
        let wireless = AdapterInfo {
            stable_id: "wireless".into(),
            friendly_name: "WLAN".into(),
            description: "Intel(R) Wi-Fi 6 AX201".into(),
            ipv4: Ipv4Addr::new(10, 100, 61, 21),
            mac: 0xaabb_ccdd_eeff,
            is_wireless: true,
            has_gateway: true,
            interface_index: 12,
            details: NetworkDetails::default(),
        };
        assert!(score(&wired, None) > score(&wireless, None));
    }

    #[test]
    fn the_route_the_os_would_use_beats_every_other_signal() {
        // The C# reference gives the preferred route index +1000, more than the
        // gateway (+300) and media type (+200) combined. That matters because a
        // wireless adapter on the authentication path is still the right one.
        let preferred_wireless = AdapterInfo {
            stable_id: "wireless".into(),
            friendly_name: "WLAN".into(),
            description: "Intel(R) Wi-Fi 6 AX201".into(),
            ipv4: Ipv4Addr::new(10, 100, 61, 21),
            mac: 0xaabb_ccdd_eeff,
            is_wireless: true,
            has_gateway: true,
            interface_index: 12,
            details: NetworkDetails::default(),
        };
        let idle_wired = AdapterInfo {
            stable_id: "wired".into(),
            friendly_name: "以太网 2".into(),
            description: "Realtek PCIe GbE Family Controller".into(),
            ipv4: Ipv4Addr::new(192, 168, 1, 20),
            mac: 0x1122_3344_5566,
            is_wireless: false,
            has_gateway: true,
            interface_index: 7,
            details: NetworkDetails::default(),
        };
        assert!(
            score(&preferred_wireless, Some(12)) > score(&idle_wired, None),
            "the adapter actually on the authentication path must win"
        );
        assert!(
            score(&idle_wired, Some(7)) > score(&preferred_wireless, None),
            "and the preference must follow the destination, not the media"
        );
        // Without a destination the previous wired-first order still holds.
        assert!(score(&idle_wired, None) > score(&preferred_wireless, None));
    }

    #[test]
    fn enumeration_returns_only_usable_adapters() {
        for adapter in list() {
            assert!(usable_ipv4(adapter.ipv4), "{adapter:?}");
            assert_ne!(adapter.mac, 0, "{adapter:?}");
            // Reading `IfIndex` out of the header union must agree with the
            // routing APIs, which never use index 0 for a real interface.
            assert_ne!(adapter.interface_index, 0, "{adapter:?}");
            assert!(!looks_virtual(&adapter.friendly_name, &adapter.description));
        }
    }
}
