use super::{Ipv4Addr, RouteAction, RouteError, RoutePlan, macos_parse};
use crate::adapter::{command, unix};
use std::time::Duration;

fn lookup(destination: &str, interface: Option<&str>) -> Option<macos_parse::Lookup> {
    let mut args = vec!["-n", "get", "-inet"];
    if let Some(name) = interface {
        args.extend(["-ifscope", name]);
    }
    args.push(destination);
    command::read("route", &args).map(|text| macos_parse::lookup(&text))
}

pub fn best_interface_index(destination: Ipv4Addr) -> Option<u32> {
    unix::interface_index(
        lookup(&destination.to_string(), None)?
            .interface
            .as_deref()?,
    )
}

pub fn default_interface_index() -> Option<u32> {
    unix::interface_index(lookup("default", None)?.interface.as_deref()?)
}

pub fn gateway_for_interface(index: u32) -> Option<Ipv4Addr> {
    let name = unix::interface_name(index).ok()?;
    lookup("default", Some(&name))?.gateway
}

pub fn existing_host_route(server: Ipv4Addr) -> Option<RoutePlan> {
    let found = lookup(&server.to_string(), None)?;
    if !found.host || found.destination != Some(server) {
        return None;
    }
    Some(RoutePlan {
        server,
        gateway: found.gateway.unwrap_or(Ipv4Addr::UNSPECIFIED),
        interface_index: unix::interface_index(found.interface.as_deref()?)?,
    })
}

pub fn apply(plan: &RoutePlan, action: RouteAction) -> Result<(), RouteError> {
    let name = unix::interface_name(plan.interface_index).map_err(|_| RouteError::NoInterface)?;
    let server = plan.server.to_string();
    let gateway = plan.gateway.to_string();
    let mut args = vec![
        "-n",
        match action {
            RouteAction::Add => "add",
            RouteAction::Delete => "delete",
        },
        "-host",
        &server,
    ];
    if plan.gateway.is_unspecified() {
        args.extend(["-interface", &name]);
    } else {
        args.push(&gateway);
    }
    args.extend(["-ifscope", &name]);
    let output = command::output("route", &args, Duration::from_secs(3)).map_err(|error| {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            RouteError::AccessDenied
        } else {
            RouteError::Unsupported
        }
    })?;
    if output.status.success() {
        return Ok(());
    }
    macos_parse::failure(
        action,
        output.status.code(),
        &String::from_utf8_lossy(&output.stderr),
    )
}
