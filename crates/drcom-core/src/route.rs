//! Authentication-server host route, matching the original client's own
//! routing work.
//!
//! Before the campus deployment authenticates a host, the only thing it is
//! allowed to reach is the authentication server itself. The original
//! `DrAuthSvr.dll` therefore installs a `/32` route for the authentication
//! server through the local gateway on the interface that already carries the
//! adapter's address, and removes it again during shutdown. Its own shutdown log
//! records both halves of that contract:
//!
//! ```text
//! Delete route for 10.100.61.3,255.255.255.255
//! DeleteIpForwardEntry,找不到元素。
//! drcom run:cmd.exe /c route DELETE 10.100.61.3 MASK 255.255.255.255 49.140.185.254 IF 7
//! ```
//!
//! Two details are taken from that evidence rather than invented here:
//!
//! * the IP Helper call is tried first and the `route.exe` form is the
//!   fallback, because the API refuses a row whose metric differs from the one
//!   already installed;
//! * the fallback argument order is exactly `DELETE <server> MASK
//!   255.255.255.255 <gateway> IF <index>`, which [`RoutePlan::arguments`]
//!   reproduces so a reviewer can diff it against the log line above.
//!
//! The reference C# client never installs the route; it only asks the OS which
//! interface *would* carry authentication traffic (`GetBestInterface`) in order
//! to score adapters. [`best_interface_index`] exposes the same query, so the
//! adapter ranking can agree with the routing table instead of guessing.
//!
//! Nothing in this module runs unless a caller asks for it: [`RouteMode::Off`]
//! is the default and every test here only exercises the pure helpers.

use std::net::Ipv4Addr;

/// What the session is allowed to do to the routing table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RouteMode {
    /// Never touch the routing table. This is the default.
    #[default]
    Off,
    /// Resolve the route and report it, but change nothing.
    DryRun,
    /// Install the host route before login and remove it after logout.
    Manage,
}

impl RouteMode {
    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Whether a resolved plan may actually be applied.
    pub fn applies(self) -> bool {
        matches!(self, Self::Manage)
    }
}

/// Which direction of the `route.exe` contract a plan is being rendered for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteAction {
    Add,
    Delete,
}

impl RouteAction {
    fn verb(self) -> &'static str {
        match self {
            Self::Add => "ADD",
            Self::Delete => "DELETE",
        }
    }
}

/// A resolved `/32` route for one authentication server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoutePlan {
    pub server: Ipv4Addr,
    pub gateway: Ipv4Addr,
    pub interface_index: u32,
}

impl RoutePlan {
    /// Arguments for `route.exe`, mirroring the original's own command line.
    pub fn arguments(&self, action: RouteAction) -> Vec<String> {
        vec![
            action.verb().to_string(),
            self.server.to_string(),
            "MASK".to_string(),
            Ipv4Addr::BROADCAST.to_string(),
            self.gateway.to_string(),
            "IF".to_string(),
            self.interface_index.to_string(),
        ]
    }

    /// The same arguments as one readable command, for logs and diagnostics.
    pub fn command_line(&self, action: RouteAction) -> String {
        format!("route {}", self.arguments(action).join(" "))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteError {
    /// The destination could not be reached through any interface.
    NoInterface,
    /// No default route on that interface, so there is no gateway to use.
    NoGateway,
    /// `ERROR_ACCESS_DENIED`: installing a route needs an elevated process.
    AccessDenied,
    /// The IP Helper call and the `route.exe` fallback both refused.
    Refused(u32),
    /// Routing information is only available on Windows.
    Unsupported,
}

/// Interface the OS would use to reach `destination`, as `GetBestInterface`
/// reports it. The reference C# client uses the same call to score adapters.
pub fn best_interface_index(destination: Ipv4Addr) -> Option<u32> {
    platform::best_interface_index(destination)
}

/// Next hop of the default route installed on `interface_index`.
pub fn gateway_for_interface(interface_index: u32) -> Option<Ipv4Addr> {
    platform::gateway_for_interface(interface_index)
}

/// The `/32` route currently installed for `server`, if any.
pub fn existing_host_route(server: Ipv4Addr) -> Option<RoutePlan> {
    platform::existing_host_route(server)
}

/// Resolves the plan the original would install for `server`: the interface the
/// OS prefers plus that interface's default gateway.
pub fn plan(server: Ipv4Addr) -> Result<RoutePlan, RouteError> {
    let interface_index = best_interface_index(server).ok_or(RouteError::NoInterface)?;
    let gateway = gateway_for_interface(interface_index).ok_or(RouteError::NoGateway)?;
    Ok(RoutePlan {
        server,
        gateway,
        interface_index,
    })
}

/// Applies `action` for `plan`, IP Helper first and `route.exe` second, exactly
/// as the original does. Callers must gate this on user intent: it changes the
/// machine's routing table and normally needs an elevated process.
pub fn apply(plan: &RoutePlan, action: RouteAction) -> Result<(), RouteError> {
    platform::apply(plan, action)
}

/// A plan that was actually installed, so it can be removed again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstalledRoute {
    pub plan: RoutePlan,
}

impl InstalledRoute {
    /// Removes the route this value stands for.
    pub fn remove(&self) -> Result<(), RouteError> {
        apply(&self.plan, RouteAction::Delete)
    }
}

/// Result of one [`install_for`] pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteOutcome {
    /// Routes this call installed, in the order it installed them.
    pub installed: Vec<InstalledRoute>,
    /// How many servers already have a `/32` route, so nothing had to be done.
    pub already_present: usize,
    /// Servers that could not be given a route, with the reason.
    pub failed: Vec<(Ipv4Addr, RouteError)>,
}

impl RouteOutcome {
    /// Every server ended up routable, either because this call installed the
    /// route or because one was already there.
    pub fn is_satisfied(&self, requested: usize) -> bool {
        self.failed.is_empty() && self.installed.len() + self.already_present == requested
    }
}

/// Resolves a plan for every server and installs it, skipping servers that
/// already have a host route. Returns what was installed so the caller can undo
/// exactly that, and never the routes the system or another client owns.
///
/// `report` sees every plan the pass considered. It exists so a caller can log
/// the route without this module deciding on a log format.
pub fn install_for(
    servers: &[Ipv4Addr],
    mode: RouteMode,
    mut report: impl FnMut(RoutePlan, RouteAction, Result<(), RouteError>),
) -> RouteOutcome {
    let mut outcome = RouteOutcome::default();
    if !mode.is_enabled() {
        return outcome;
    }
    for server in servers {
        let plan = match plan(*server) {
            Ok(plan) => plan,
            Err(error) => {
                outcome.failed.push((*server, error));
                continue;
            }
        };
        if existing_host_route(*server).is_some() {
            // Another component already owns this route; leaving it alone keeps
            // removal from tearing down state this process never created.
            outcome.already_present += 1;
            report(plan, RouteAction::Add, Ok(()));
            continue;
        }
        if !mode.applies() {
            report(plan, RouteAction::Add, Ok(()));
            continue;
        }
        let result = apply(&plan, RouteAction::Add);
        report(plan, RouteAction::Add, result);
        match result {
            Ok(()) => outcome.installed.push(InstalledRoute { plan }),
            Err(error) => outcome.failed.push((*server, error)),
        }
    }
    outcome
}

/// Address conversions shared by the platform code and its tests.
///
/// The Windows v1 forwarding table stores each address as a `DWORD` whose
/// *memory* holds the address in network order — the same convention
/// `inet_addr` uses. Converting through `from_be_bytes`/`to_be_bytes` therefore
/// byte-swaps the address on a little-endian host. A live smoke run is what
/// caught that: the campus gateway `49.140.185.254` was read back as
/// `254.185.140.49`, while the interface index `7` was correct, which pointed
/// straight at the conversion rather than at the API.
///
/// `from_ne_bytes`/`to_ne_bytes` expresses the actual contract — the value's
/// bytes are the address's bytes — and is self-inverse on either endianness.
mod bytes {
    use std::net::Ipv4Addr;

    pub fn to_network_order(address: Ipv4Addr) -> u32 {
        u32::from_ne_bytes(address.octets())
    }

    pub fn from_network_order(value: u32) -> Ipv4Addr {
        Ipv4Addr::from(value.to_ne_bytes())
    }
}

pub use bytes::{from_network_order, to_network_order};

#[cfg(windows)]
mod platform {
    use super::{
        Ipv4Addr, RouteAction, RouteError, RoutePlan, from_network_order, to_network_order,
    };
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, NO_ERROR};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        CreateIpForwardEntry, DeleteIpForwardEntry, GetBestInterface, GetIpForwardTable,
        MIB_IPFORWARDROW, MIB_IPFORWARDROW_0, MIB_IPFORWARDROW_1, MIB_IPFORWARDTABLE,
    };

    /// `MIB_IPROUTE_TYPE_INDIRECT` from `ipmib.h`. `windows-sys` does not export
    /// these enum names, so the documented values are inlined.
    const MIB_IPROUTE_TYPE_INDIRECT: u32 = 4;
    /// `MIB_IPROUTE_PROTO_NETMGMT` from `ipmib.h`, same reasoning.
    const MIB_IPROUTE_PROTO_NETMGMT: u32 = 3;
    /// `MIB_IPROUTE_METRIC_UNUSED`.
    const METRIC_UNUSED: u32 = u32::MAX;
    /// The metric `route ADD` uses by default, which is what the original's
    /// fallback command line produces.
    const DEFAULT_METRIC: u32 = 1;

    pub fn best_interface_index(destination: Ipv4Addr) -> Option<u32> {
        let mut index = 0u32;
        // Same address convention as the forwarding table: the DWORD's bytes
        // are the address's bytes.
        // SAFETY: both arguments are pointers to locals that outlive the call.
        let result = unsafe {
            GetBestInterface(to_network_order(destination), std::ptr::addr_of_mut!(index))
        };
        (result == NO_ERROR).then_some(index)
    }

    /// Reads the forwarding table. The API sizes the buffer on the first call,
    /// which is expected to report `ERROR_INSUFFICIENT_BUFFER`.
    fn forwarding_table() -> Vec<MIB_IPFORWARDROW> {
        let mut size = 0u32;
        // SAFETY: a null table with a size pointer is the documented way to ask
        // for the required size; nothing is written through the null pointer.
        unsafe { GetIpForwardTable(std::ptr::null_mut(), std::ptr::addr_of_mut!(size), 0) };
        if size < std::mem::size_of::<MIB_IPFORWARDTABLE>() as u32 {
            return Vec::new();
        }
        let mut buffer = vec![0u8; size as usize];
        let table = buffer.as_mut_ptr() as *mut MIB_IPFORWARDTABLE;
        // SAFETY: `buffer` is at least `size` bytes, which is what the sizing
        // call asked for, and it outlives every read below.
        let result = unsafe { GetIpForwardTable(table, std::ptr::addr_of_mut!(size), 0) };
        if result != NO_ERROR {
            return Vec::new();
        }
        // SAFETY: the header declares a one-element array; the API writes
        // `dwNumEntries` rows into the trailing storage.
        let count = unsafe { (*table).dwNumEntries } as usize;
        if count == 0 {
            return Vec::new();
        }
        unsafe { std::slice::from_raw_parts((*table).table.as_ptr(), count) }.to_vec()
    }

    pub fn gateway_for_interface(interface_index: u32) -> Option<Ipv4Addr> {
        let rows = forwarding_table();
        let default_route = |row: &&MIB_IPFORWARDROW| {
            row.dwForwardDest == 0
                && row.dwForwardMask == 0
                && row.dwForwardIfIndex == interface_index
                && row.dwForwardNextHop != 0
        };
        rows.iter()
            .filter(default_route)
            // The lowest metric wins when an interface has several defaults.
            .min_by_key(|row| row.dwForwardMetric1)
            .map(|row| from_network_order(row.dwForwardNextHop))
    }

    pub fn existing_host_route(server: Ipv4Addr) -> Option<RoutePlan> {
        let destination = to_network_order(server);
        let host_mask = to_network_order(Ipv4Addr::BROADCAST);
        forwarding_table()
            .into_iter()
            .find(|row| row.dwForwardDest == destination && row.dwForwardMask == host_mask)
            .map(|row| RoutePlan {
                server,
                gateway: from_network_order(row.dwForwardNextHop),
                interface_index: row.dwForwardIfIndex,
            })
    }

    fn host_row(plan: &RoutePlan) -> MIB_IPFORWARDROW {
        MIB_IPFORWARDROW {
            dwForwardDest: to_network_order(plan.server),
            dwForwardMask: to_network_order(Ipv4Addr::BROADCAST),
            dwForwardPolicy: 0,
            dwForwardNextHop: to_network_order(plan.gateway),
            dwForwardIfIndex: plan.interface_index,
            Anonymous1: MIB_IPFORWARDROW_0 {
                dwForwardType: MIB_IPROUTE_TYPE_INDIRECT,
            },
            Anonymous2: MIB_IPFORWARDROW_1 {
                dwForwardProto: MIB_IPROUTE_PROTO_NETMGMT,
            },
            dwForwardAge: 0,
            dwForwardNextHopAS: 0,
            dwForwardMetric1: DEFAULT_METRIC,
            dwForwardMetric2: METRIC_UNUSED,
            dwForwardMetric3: METRIC_UNUSED,
            dwForwardMetric4: METRIC_UNUSED,
            dwForwardMetric5: METRIC_UNUSED,
        }
    }

    /// `route.exe` fallback, built from the same argument vector the plan
    /// reports so the log line and the executed command cannot drift apart.
    fn route_exe(plan: &RoutePlan, action: RouteAction) -> Result<(), RouteError> {
        let output = std::process::Command::new("route")
            .args(plan.arguments(action))
            .output()
            .map_err(|_| RouteError::NoInterface)?;
        if output.status.success() {
            return Ok(());
        }
        let code = output.status.code().unwrap_or(-1);
        if code == ERROR_ACCESS_DENIED as i32 {
            return Err(RouteError::AccessDenied);
        }
        Err(RouteError::Refused(code as u32))
    }

    pub fn apply(plan: &RoutePlan, action: RouteAction) -> Result<(), RouteError> {
        let row = host_row(plan);
        // SAFETY: `row` is fully initialised and borrowed only for the call.
        let result = unsafe {
            match action {
                RouteAction::Add => CreateIpForwardEntry(&row),
                RouteAction::Delete => DeleteIpForwardEntry(&row),
            }
        };
        if result == NO_ERROR {
            return Ok(());
        }
        if result == ERROR_ACCESS_DENIED {
            return Err(RouteError::AccessDenied);
        }
        // The original falls through to `route.exe` whenever the API refuses,
        // which is what happens when another client installed the same route
        // with a different metric.
        route_exe(plan, action).map_err(|error| match error {
            RouteError::Refused(_) => RouteError::Refused(result),
            other => other,
        })
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{Ipv4Addr, RouteAction, RouteError, RoutePlan};

    pub fn best_interface_index(_destination: Ipv4Addr) -> Option<u32> {
        None
    }

    pub fn gateway_for_interface(_interface_index: u32) -> Option<Ipv4Addr> {
        None
    }

    pub fn existing_host_route(_server: Ipv4Addr) -> Option<RoutePlan> {
        None
    }

    pub fn apply(_plan: &RoutePlan, _action: RouteAction) -> Result<(), RouteError> {
        Err(RouteError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: Ipv4Addr = Ipv4Addr::new(10, 100, 61, 3);
    const GATEWAY: Ipv4Addr = Ipv4Addr::new(49, 140, 185, 254);
    const INDEX: u32 = 7;

    fn plan() -> RoutePlan {
        RoutePlan {
            server: SERVER,
            gateway: GATEWAY,
            interface_index: INDEX,
        }
    }

    #[test]
    fn delete_arguments_match_the_original_log_line() {
        // The original logs, verbatim:
        //   route DELETE 10.100.61.3 MASK 255.255.255.255 49.140.185.254 IF 7
        assert_eq!(
            plan().command_line(RouteAction::Delete),
            "route DELETE 10.100.61.3 MASK 255.255.255.255 49.140.185.254 IF 7"
        );
    }

    #[test]
    fn add_uses_the_same_shape_as_delete() {
        assert_eq!(
            plan().arguments(RouteAction::Add),
            vec![
                "ADD",
                "10.100.61.3",
                "MASK",
                "255.255.255.255",
                "49.140.185.254",
                "IF",
                "7",
            ]
        );
    }

    #[test]
    fn host_mask_is_always_a_single_address() {
        // A /32 route is what keeps the rest of the campus network routed by
        // the system table; a wider mask would hijack unrelated traffic.
        let arguments = plan().arguments(RouteAction::Add);
        assert_eq!(arguments[2], "MASK");
        assert_eq!(arguments[3], "255.255.255.255");
    }

    #[test]
    fn addresses_keep_their_network_bytes_in_memory() {
        // The contract the forwarding table actually follows: the value's bytes
        // *are* the address's bytes. Asserting on `to_ne_bytes` pins this
        // independently of the host endianness.
        assert_eq!(to_network_order(GATEWAY).to_ne_bytes(), GATEWAY.octets());
        assert_eq!(to_network_order(SERVER).to_ne_bytes(), SERVER.octets());
        assert_eq!(from_network_order(to_network_order(GATEWAY)), GATEWAY);
        assert_eq!(from_network_order(to_network_order(SERVER)), SERVER);
        assert_eq!(
            from_network_order(to_network_order(Ipv4Addr::UNSPECIFIED)),
            Ipv4Addr::UNSPECIFIED
        );
    }

    #[test]
    #[cfg(target_endian = "little")]
    fn little_endian_readback_matches_the_live_smoke_run() {
        // On this host the campus gateway reads back as this integer, which is
        // exactly what a real `drcom-cli --auth-route` run printed once the
        // byte-swap was removed.
        assert_eq!(to_network_order(GATEWAY), 0xfeb9_8c31);
        assert_eq!(from_network_order(0xfeb9_8c31), GATEWAY);
        // The previous `from_be_bytes` conversion produced this instead.
        assert_ne!(
            from_network_order(0xfeb9_8c31),
            Ipv4Addr::new(254, 185, 140, 49)
        );
    }

    #[test]
    fn off_mode_never_touches_the_routing_table() {
        let mut reports = Vec::new();
        let outcome = install_for(&[SERVER], RouteMode::Off, |plan, action, outcome| {
            reports.push((plan, action, outcome));
        });
        assert_eq!(outcome, RouteOutcome::default());
        assert!(
            reports.is_empty(),
            "an off session must not even resolve a plan"
        );
    }

    #[test]
    fn satisfaction_needs_every_requested_server_to_be_routable() {
        let plan = plan();
        let satisfied = RouteOutcome {
            installed: vec![InstalledRoute { plan }],
            already_present: 1,
            failed: Vec::new(),
        };
        assert!(satisfied.is_satisfied(2));
        assert!(
            !satisfied.is_satisfied(3),
            "an unaccounted server is not satisfied"
        );

        let partial = RouteOutcome {
            installed: Vec::new(),
            already_present: 1,
            failed: vec![(SERVER, RouteError::NoGateway)],
        };
        assert!(!partial.is_satisfied(2));
    }

    #[test]
    fn mode_flags_gate_resolution_and_application_separately() {
        assert!(!RouteMode::Off.is_enabled());
        assert!(!RouteMode::Off.applies());
        assert!(RouteMode::DryRun.is_enabled());
        assert!(!RouteMode::DryRun.applies(), "dry run must not mutate");
        assert!(RouteMode::Manage.is_enabled());
        assert!(RouteMode::Manage.applies());
        assert_eq!(RouteMode::default(), RouteMode::Off);
    }
}
