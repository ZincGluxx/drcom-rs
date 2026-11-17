//! Linux routing through `iproute2`. The OS preference comes from
//! `ip route get` and the host route is installed with
//! `ip route add|del <server>/32 [via <gateway>] dev <interface>`, the exact
//! equivalent of the Windows IP Helper row the original writes.

use super::{Ipv4Addr, RouteAction, RouteError, RoutePlan};
use crate::adapter::command;
use std::path::Path;

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
    let mut args = vec!["-j", "-4"];
    args.extend_from_slice(arguments);
    let output = command::output("ip", &args, std::time::Duration::from_secs(3)).ok()?;
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

pub fn default_interface_index() -> Option<u32> {
    ip_json(&["route", "show", "default"])?
        .iter()
        .filter_map(|row| {
            Some((
                row.metric.unwrap_or(0),
                interface_index(row.dev.as_deref()?)?,
            ))
        })
        .min_by_key(|(metric, _)| *metric)
        .map(|(_, index)| index)
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
            // Linux omits an explicit metric when it is zero.
            Some((row.metric.unwrap_or(0), gateway))
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
    let args = route_arguments(plan, action, &ifname);
    let refs: Vec<_> = args.iter().map(String::as_str).collect();
    let output =
        command::output("ip", &refs, std::time::Duration::from_secs(3)).map_err(|error| {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                RouteError::AccessDenied
            } else {
                RouteError::Unsupported
            }
        })?;
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
            // An EEXIST race must not become InstalledRoute: cleanup would
            // otherwise delete a route installed by a different owner.
            if text.contains("cannot find device") || text.contains("no such device") {
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
    if !command::valid_interface_name(ifname) {
        return None;
    }
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
        // A racing ADD must not claim someone else's route. Deleting an
        // already absent route remains idempotent success.
        assert_eq!(
            classify_failure(RouteAction::Add, Some(2), "RTNETLINK answers: File exists"),
            Err(RouteError::Refused(2))
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
