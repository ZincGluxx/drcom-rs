//! macOS hardware ports + getifaddrs. Unknown media is not called physical.
use super::{AdapterInfo, NetworkDetails, command, mac_from_bytes, macos_parse, unix, usable_ipv4};
use std::ffi::CStr;
use std::net::IpAddr;

pub fn list() -> Vec<AdapterInfo> {
    let Some(hardware) = command::read("networksetup", &["-listallhardwareports"]) else {
        return Vec::new();
    };
    let Ok(addresses) = unix::Addresses::get() else {
        return Vec::new();
    };
    let routes4 = command::read("netstat", &["-rn", "-f", "inet"]).unwrap_or_default();
    let routes6 = command::read("netstat", &["-rn", "-f", "inet6"]).unwrap_or_default();
    let dns = command::read("scutil", &["--dns"]).unwrap_or_default();
    let mut result = Vec::new();
    for port in macos_parse::hardware_ports(&hardware) {
        let Some(index) = unix::interface_index(&port.device) else {
            continue;
        };
        let Some(state) = command::read("ifconfig", &[&port.device]) else {
            continue;
        };
        if !macos_parse::active(&state) {
            continue;
        }
        let entries: Vec<_> = addresses
            .iter()
            .filter(|entry| {
                !entry.ifa_name.is_null()
                    && unsafe { CStr::from_ptr(entry.ifa_name) }.to_bytes()
                        == port.device.as_bytes()
                    && entry.ifa_flags & libc::IFF_UP as u32 != 0
                    && entry.ifa_flags & libc::IFF_LOOPBACK as u32 == 0
            })
            .collect();
        let mac = entries.iter().find_map(|entry| {
            if entry.ifa_addr.is_null()
                || unsafe { (*entry.ifa_addr).sa_family as i32 } != libc::AF_LINK
            {
                return None;
            }
            // sockaddr_dl is supplied by getifaddrs and valid for AF_LINK.
            let link = unsafe { &*entry.ifa_addr.cast::<libc::sockaddr_dl>() };
            // IFT_ETHER, from net/if_types.h. Other AF_LINK media do not carry
            // the six-octet Ethernet identifier the authentication frame needs.
            if link.sdl_type != 6 || link.sdl_alen != 6 {
                return None;
            }
            let offset = std::mem::offset_of!(libc::sockaddr_dl, sdl_data) + link.sdl_nlen as usize;
            // sockaddr_dl is variable-length: sdl_data's declared array may
            // be shorter than the kernel's sdl_len for long interface names.
            if offset.checked_add(6)? > link.sdl_len as usize {
                return None;
            }
            let bytes =
                unsafe { std::slice::from_raw_parts(entry.ifa_addr.cast::<u8>().add(offset), 6) };
            mac_from_bytes(bytes)
        });
        let Some(mac) = mac else {
            continue;
        };
        let ipv6: Vec<_> = entries
            .iter()
            .filter_map(|entry| unsafe { unix::ip(entry.ifa_addr) })
            .filter(|ip| ip.is_ipv6() && macos_parse::ready_ipv6(&state, *ip))
            .collect();
        let mut gateways = macos_parse::default_gateways(&routes4, &port.device);
        gateways.extend(macos_parse::default_gateways(&routes6, &port.device));
        let details = NetworkDetails {
            subnet_mask: None,
            ipv6,
            gateways,
            dns: macos_parse::dns(&dns, index),
        };
        for entry in entries {
            let Some(IpAddr::V4(ipv4)) = (unsafe { unix::ip(entry.ifa_addr) }) else {
                continue;
            };
            if !usable_ipv4(ipv4)
                || result
                    .iter()
                    .any(|a: &AdapterInfo| a.interface_index == index && a.ipv4 == ipv4)
            {
                continue;
            }
            let subnet_mask = match unsafe { unix::ip(entry.ifa_netmask) } {
                Some(IpAddr::V4(ip)) => Some(ip),
                _ => None,
            };
            result.push(AdapterInfo {
                // Preserve the OS persistent BSD name, not a Wi-Fi MAC that can
                // rotate or an ifindex that can change after unplugging USB.
                stable_id: port.device.clone(),
                friendly_name: port.name.clone(),
                description: format!("{} ({})", port.name, port.device),
                ipv4,
                mac,
                is_wireless: port.wireless,
                has_gateway: !details.gateways.is_empty(),
                interface_index: index,
                details: NetworkDetails {
                    subnet_mask,
                    ..details.clone()
                },
            });
        }
    }
    result.sort_by_key(|a| (a.interface_index, a.ipv4));
    result
}
