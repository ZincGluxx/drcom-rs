//! Physical adapter selection for the authentication socket.
//!
//! The login frame carries the adapter's IPv4 address and MAC, and the socket
//! binds to that address so authentication traffic cannot leave through a
//! virtual interface. Hyper-V, VMware, WSL, VPN, TAP and loopback adapters all
//! appear in the Windows adapter list, so they are filtered out by description
//! and by requiring an operational status, a usable IPv4 address and a real MAC.
//!
//! Ranking follows the reference C# client's `ScoreCandidate`, including its
//! weights: the interface the OS *would* use to reach the authentication server
//! dominates, then the presence of a gateway, then the media type. Asking the
//! routing table first is what keeps a second, unrelated wired NIC from being
//! chosen just because it is also wired — a guess the old "wired wins" rule
//! could not avoid. [`select_for`] performs that query; [`select`] is the same
//! choice without a destination.
//!
//! The preferred-route lookup itself lives in [`crate::route`], because that is
//! also where the original installs its `/32` route for the same server.

use std::net::{IpAddr, Ipv4Addr};

use crate::route;

/// MAC used when no adapter can be queried, matching the C# client's default.
pub const DEFAULT_MAC: u64 = 0x8888_8888_8888;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterInfo {
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
    let preferred_route = destination.and_then(route::best_interface_index);
    let mut adapters = list();
    adapters.sort_by_key(|adapter| std::cmp::Reverse(score(adapter, preferred_route)));
    adapters.into_iter().next()
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
            if let Some(adapter) = unsafe { read_adapter(current) } {
                adapters.push(adapter);
            }
            current = unsafe { (*current).Next };
        }
        adapters
    }

    unsafe fn read_adapter(current: *const IP_ADAPTER_ADDRESSES_LH) -> Option<AdapterInfo> {
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

        let mut ipv4 = None;
        let mut subnet_mask = None;
        let mut unicast = adapter.FirstUnicastAddress;
        while !unicast.is_null() {
            let entry = unsafe { &*unicast };
            if let Some(address) = unsafe { ipv4_from_socket_address(entry.Address.lpSockaddr) }
                && usable_ipv4(address)
            {
                ipv4 = Some(address);
                let prefix = entry.OnLinkPrefixLength;
                if prefix > 0 && prefix <= 32 {
                    subnet_mask = Some(Ipv4Addr::from(u32::MAX << (32 - prefix)));
                }
                break;
            }
            unicast = entry.Next;
        }

        let mut details = super::NetworkDetails {
            subnet_mask,
            ..Default::default()
        };
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

        Some(AdapterInfo {
            friendly_name,
            description,
            ipv4: ipv4?,
            mac,
            is_wireless: adapter.IfType == IF_TYPE_IEEE80211,
            has_gateway: !adapter.FirstGatewayAddress.is_null(),
            // `IfIndex` lives in the header's leading union, next to `Length`;
            // it is the same index the routing APIs report.
            interface_index: unsafe { adapter.Anonymous1.Anonymous.IfIndex },
            details,
        })
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

#[cfg(not(windows))]
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
