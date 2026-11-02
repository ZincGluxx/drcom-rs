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
//! Linux expresses the same contract with iproute2: the host route is
//! `ip route add|del <server>/32 via <gateway> dev <interface>` and the OS
//! preference comes from `ip route get`. The `route.exe` helpers stay because
//! they document and test the Windows form the original logs.
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
    /// Routing information is unavailable on this platform.
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

#[cfg(target_os = "linux")]
mod platform {
    //! Linux routing through `iproute2`. The OS preference comes from
    //! `ip route get` and the host route is installed with
    //! `ip route add|del <server>/32 [via <gateway>] dev <interface>`, the exact
    //! equivalent of the Windows IP Helper row the original writes.

    use super::{Ipv4Addr, RouteAction, RouteError, RoutePlan};
    use std::path::Path;
    use std::process::Command;

    /// Where the kernel lists interfaces by name.
    const SYS_CLASS_NET: &str = "/sys/class/net";

    /// One row of `ip -j route ...`. `ip` prints many more fields; serde
    /// ignores them, and every field here is absent from some subcommand, so
    /// all of them are optional.
    #[derive(Debug, serde::Deserialize)]
    struct RouteRow {
        #[serde(default)]
        dst: Option<String>,
        #[serde(default)]
        gateway: Option<String>,
        #[serde(default)]
        dev: Option<String>,
        #[serde(default)]
        metric: Option<u32>,
    }

    /// Runs `ip -j <arguments>` and decodes the JSON array it prints.
    ///
    /// Values are passed as separate arguments, never through a shell.
    fn ip_json(arguments: &[&str]) -> Option<Vec<RouteRow>> {
        let output = Command::new("ip").arg("-j").args(arguments).output().ok()?;
        if !output.status.success() {
            return None;
        }
        serde_json::from_slice(&output.stdout).ok()
    }

    pub fn best_interface_index(destination: Ipv4Addr) -> Option<u32> {
        let queried = destination.to_string();
        let rows = ip_json(&["route", "get", &queried])?;
        interface_index(rows.first()?.dev.as_deref()?)
    }

    pub fn gateway_for_interface(interface_index: u32) -> Option<Ipv4Addr> {
        ip_json(&["route", "show", "default"])?
            .iter()
            .filter_map(|row| {
                let device = row.dev.as_deref()?;
                if self::interface_index(device)? != interface_index {
                    return None;
                }
                let gateway = row.gateway.as_deref()?.parse::<Ipv4Addr>().ok()?;
                // A default route without a metric sorts last, matching the
                // Windows code's "lowest metric wins" rule.
                Some((row.metric.unwrap_or(u32::MAX), gateway))
            })
            .min_by_key(|(metric, _)| *metric)
            .map(|(_, gateway)| gateway)
    }

    pub fn existing_host_route(server: Ipv4Addr) -> Option<RoutePlan> {
        let query = format!("{server}/32");
        let rows = ip_json(&["route", "show", &query])?;
        let row = rows.iter().find(|row| {
            row.dst
                .as_deref()
                .is_some_and(|dst| is_exact_host_route(dst, server))
        })?;
        Some(RoutePlan {
            server,
            gateway: row
                .gateway
                .as_deref()
                .and_then(|value| value.parse().ok())
                // A link route has no next hop; the plan carries "unspecified"
                // so the command renders without `via`.
                .unwrap_or(Ipv4Addr::UNSPECIFIED),
            interface_index: interface_index(row.dev.as_deref()?)?,
        })
    }

    pub fn apply(plan: &RoutePlan, action: RouteAction) -> Result<(), RouteError> {
        let ifname = ifname_for_index(plan.interface_index).ok_or(RouteError::NoInterface)?;
        let output = Command::new("ip")
            .args(route_arguments(plan, action, &ifname))
            .output()
            .map_err(|_| RouteError::NoInterface)?;
        if output.status.success() {
            return Ok(());
        }
        classify_failure(
            action,
            output.status.code(),
            &String::from_utf8_lossy(&output.stderr),
        )
    }

    /// The argument vector for one host route, shared by the command and the
    /// tests so the executed command cannot drift from what is asserted.
    fn route_arguments(plan: &RoutePlan, action: RouteAction, ifname: &str) -> Vec<String> {
        let mut arguments = vec![
            "route".to_string(),
            match action {
                RouteAction::Add => "add",
                RouteAction::Delete => "del",
            }
            .to_string(),
            format!("{}/32", plan.server),
        ];
        if !plan.gateway.is_unspecified() {
            arguments.push("via".to_string());
            arguments.push(plan.gateway.to_string());
        }
        arguments.push("dev".to_string());
        arguments.push(ifname.to_string());
        arguments
    }

    /// `ip` prints a host destination either as `a.b.c.d` or `a.b.c.d/32`,
    /// depending on the subcommand and version; both mean the same row.
    fn is_exact_host_route(dst: &str, server: Ipv4Addr) -> bool {
        dst == server.to_string() || dst == format!("{server}/32")
    }

    /// Maps a failed `ip` invocation onto the same outcomes the Windows path
    /// reports, including its idempotent add/delete cases.
    fn classify_failure(
        action: RouteAction,
        status: Option<i32>,
        stderr: &str,
    ) -> Result<(), RouteError> {
        let text = stderr.to_ascii_lowercase();
        // `strerror` text follows the locale, so both languages are matched.
        if text.contains("operation not permitted")
            || text.contains("permission denied")
            || stderr.contains("权限")
            || stderr.contains("不允许")
        {
            return Err(RouteError::AccessDenied);
        }
        let code = status.unwrap_or(-1) as u32;
        match action {
            RouteAction::Add => {
                if text.contains("file exists") {
                    // Another component already installed the same route: the
                    // goal state is met, so this is success, not a failure.
                    Ok(())
                } else if text.contains("cannot find device") || text.contains("no such device") {
                    Err(RouteError::NoInterface)
                } else {
                    Err(RouteError::Refused(code))
                }
            }
            RouteAction::Delete => {
                if text.contains("no such process")
                    || text.contains("cannot find device")
                    || text.contains("no such device")
                    || stderr.contains("没有那个进程")
                    || stderr.contains("找不到设备")
                {
                    // Already gone, which is exactly what the caller asked for.
                    Ok(())
                } else {
                    Err(RouteError::Refused(code))
                }
            }
        }
    }

    fn interface_index(ifname: &str) -> Option<u32> {
        std::fs::read_to_string(Path::new(SYS_CLASS_NET).join(ifname).join("ifindex"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    fn ifname_for_index(index: u32) -> Option<String> {
        std::fs::read_dir(SYS_CLASS_NET)
            .ok()?
            .filter_map(|entry| entry.ok())
            .find_map(|entry| {
                let current = std::fs::read_to_string(entry.path().join("ifindex")).ok()?;
                (current.trim().parse::<u32>().ok()? == index)
                    .then(|| entry.file_name().to_string_lossy().into_owned())
            })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn plan() -> RoutePlan {
            RoutePlan {
                server: Ipv4Addr::new(10, 100, 61, 3),
                gateway: Ipv4Addr::new(49, 140, 185, 254),
                interface_index: 2,
            }
        }

        #[test]
        fn route_arguments_are_passed_as_separate_tokens() {
            assert_eq!(
                route_arguments(&plan(), RouteAction::Add, "enp46s0"),
                [
                    "route",
                    "add",
                    "10.100.61.3/32",
                    "via",
                    "49.140.185.254",
                    "dev",
                    "enp46s0"
                ]
                .map(str::to_string)
            );
            assert_eq!(
                route_arguments(&plan(), RouteAction::Delete, "enp46s0")[1],
                "del"
            );
            // A link route has no next hop, so it must not render an empty `via`.
            let link = RoutePlan {
                gateway: Ipv4Addr::UNSPECIFIED,
                ..plan()
            };
            assert_eq!(
                route_arguments(&link, RouteAction::Add, "enp46s0"),
                ["route", "add", "10.100.61.3/32", "dev", "enp46s0"].map(str::to_string)
            );
        }

        #[test]
        fn host_route_destination_matches_both_output_forms() {
            let server = plan().server;
            assert!(is_exact_host_route("10.100.61.3", server));
            assert!(is_exact_host_route("10.100.61.3/32", server));
            assert!(!is_exact_host_route("10.100.61.0/24", server));
            assert!(!is_exact_host_route("default", server));
        }

        #[test]
        fn idempotent_and_privilege_failures_are_mapped() {
            // A second ADD finds its own route: success, like `File exists` on
            // Windows. Deleting an already gone route is success too.
            assert_eq!(
                classify_failure(RouteAction::Add, Some(2), "RTNETLINK answers: File exists"),
                Ok(())
            );
            assert_eq!(
                classify_failure(
                    RouteAction::Delete,
                    Some(2),
                    "RTNETLINK answers: No such process"
                ),
                Ok(())
            );
            assert_eq!(
                classify_failure(RouteAction::Delete, Some(2), "Cannot find device \"enp0\""),
                Ok(())
            );
            // An unprivileged `ip route add` is the expected failure for a
            // normal user; it must be reported as AccessDenied, not Refused.
            assert_eq!(
                classify_failure(
                    RouteAction::Add,
                    Some(2),
                    "RTNETLINK answers: Operation not permitted"
                ),
                Err(RouteError::AccessDenied)
            );
            assert_eq!(
                classify_failure(
                    RouteAction::Delete,
                    Some(2),
                    "RTNETLINK answers: Invalid argument"
                ),
                Err(RouteError::Refused(2))
            );
            assert_eq!(
                classify_failure(RouteAction::Add, Some(2), "Cannot find device \"gone0\""),
                Err(RouteError::NoInterface)
            );
        }

        #[test]
        fn route_json_parses_default_and_resolved_rows() {
            let rows: Vec<RouteRow> = serde_json::from_str(
                r#"[{"dst":"default","gateway":"49.140.185.254","dev":"enp46s0","protocol":"static","metric":100,"flags":[]}]"#,
            )
            .unwrap();
            assert_eq!(rows[0].dev.as_deref(), Some("enp46s0"));
            assert_eq!(rows[0].gateway.as_deref(), Some("49.140.185.254"));
            assert_eq!(rows[0].metric, Some(100));
            // `ip route get` reports the resolved address as `dst` and no
            // metric; the parser must accept that shape.
            let rows: Vec<RouteRow> = serde_json::from_str(
                r#"[{"dst":"49.140.185.254","dev":"enp46s0","prefsrc":"49.140.185.105","flags":[],"uid":1000,"cache":[]}]"#,
            )
            .unwrap();
            assert_eq!(rows[0].dst.as_deref(), Some("49.140.185.254"));
            assert!(rows[0].metric.is_none());
        }

        #[test]
        fn sysfs_lookup_rejects_unknown_interfaces() {
            // Read-only: no kernel can have these names or that index.
            assert!(interface_index("drcom-no-such-interface").is_none());
            assert!(ifname_for_index(u32::MAX).is_none());
        }

        #[test]
        fn apply_to_a_vanished_interface_fails_before_touching_the_table() {
            // A safe way to exercise the failure path without privileges: the
            // interface does not exist, so no `ip` command is ever spawned.
            let plan = RoutePlan {
                server: Ipv4Addr::new(203, 0, 113, 1),
                gateway: Ipv4Addr::new(198, 51, 100, 1),
                interface_index: u32::MAX,
            };
            assert_eq!(apply(&plan, RouteAction::Add), Err(RouteError::NoInterface));
        }

        /// The routing table of the machine actually running the tests. Written
        /// as a self-consistency check so it also holds on another Linux host,
        /// and skipped when the machine has no default route at all.
        #[test]
        fn live_route_resolution_agrees_with_the_kernel() {
            let Some(index) = best_interface_index(Ipv4Addr::new(8, 8, 8, 8)) else {
                eprintln!("skipped: the kernel has no route to 8.8.8.8");
                return;
            };
            let Some(gateway) = gateway_for_interface(index) else {
                eprintln!("skipped: interface {index} has no default gateway");
                return;
            };
            // The gateway sits on the same link, so the kernel routes to it
            // through the interface that owns the default route.
            assert_eq!(best_interface_index(gateway), Some(index));
        }
    }
}

#[cfg(all(not(windows), not(target_os = "linux")))]
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
