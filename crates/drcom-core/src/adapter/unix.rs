//! Shared Unix ABI operations; no guessed sockaddr layouts.
use std::ffi::CStr;
#[cfg(any(target_os = "macos", test))]
use std::ffi::CString;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub(crate) fn interface_name(index: u32) -> io::Result<String> {
    if index == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface index must not be zero",
        ));
    }
    let mut name = [0 as libc::c_char; libc::IF_NAMESIZE];
    // SAFETY: the buffer has IF_NAMESIZE bytes, as required by if_indextoname.
    if unsafe { libc::if_indextoname(index, name.as_mut_ptr()) }.is_null() {
        return Err(io::Error::last_os_error());
    }
    let text = unsafe { CStr::from_ptr(name.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    if !super::command::valid_interface_name(&text) {
        return Err(io::Error::other("invalid interface name returned by OS"));
    }
    Ok(text)
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn interface_index(name: &str) -> Option<u32> {
    if !super::command::valid_interface_name(name) {
        return None;
    }
    let name = CString::new(name).ok()?;
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    (index != 0).then_some(index)
}

pub(crate) struct Addresses(*mut libc::ifaddrs);
impl Addresses {
    pub(crate) fn get() -> io::Result<Self> {
        let mut head = std::ptr::null_mut();
        if unsafe { libc::getifaddrs(&mut head) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(head))
    }
    pub(crate) fn iter(&self) -> impl Iterator<Item = &libc::ifaddrs> {
        let mut next = self.0;
        std::iter::from_fn(move || {
            if next.is_null() {
                return None;
            }
            // SAFETY: getifaddrs owns this linked list until self is dropped.
            let entry = unsafe { &*next };
            next = entry.ifa_next;
            Some(entry)
        })
    }
}
impl Drop for Addresses {
    fn drop(&mut self) {
        unsafe { libc::freeifaddrs(self.0) };
    }
}

pub(crate) unsafe fn ip(address: *const libc::sockaddr) -> Option<IpAddr> {
    if address.is_null() {
        return None;
    }
    match unsafe { (*address).sa_family as i32 } {
        libc::AF_INET => {
            let value = unsafe { &*address.cast::<libc::sockaddr_in>() };
            Some(IpAddr::V4(Ipv4Addr::from(
                value.sin_addr.s_addr.to_ne_bytes(),
            )))
        }
        libc::AF_INET6 => {
            let value = unsafe { &*address.cast::<libc::sockaddr_in6>() };
            Some(IpAddr::V6(Ipv6Addr::from(value.sin6_addr.s6_addr)))
        }
        _ => None,
    }
}

/// Validate the requested source/interface pair even before pinning. If the
/// interface disappeared or its DHCP address changed, never send credentials.
pub(crate) fn validate_source(index: u32, source: Ipv4Addr) -> io::Result<()> {
    if source.is_unspecified() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an interface-pinned socket requires an explicit source IPv4 address",
        ));
    }
    let name = interface_name(index)?;
    let addresses = Addresses::get()?;
    for entry in addresses.iter() {
        if entry.ifa_name.is_null() {
            continue;
        }
        let current = unsafe { CStr::from_ptr(entry.ifa_name) };
        if current.to_bytes() == name.as_bytes()
            && entry.ifa_flags & libc::IFF_UP as u32 != 0
            && unsafe { ip(entry.ifa_addr) } == Some(IpAddr::V4(source))
        {
            return Ok(());
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AddrNotAvailable,
        format!("source {source} is not assigned to interface {name}; refresh adapters"),
    ))
}
