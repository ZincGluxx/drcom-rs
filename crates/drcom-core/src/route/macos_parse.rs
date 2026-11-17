use std::net::Ipv4Addr;

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Lookup {
    pub interface: Option<String>,
    pub gateway: Option<Ipv4Addr>,
    pub destination: Option<Ipv4Addr>,
    pub host: bool,
}

/// route -n get -inet output. A cached destination alone is NOT a /32 route;
/// only the kernel's HOST flag establishes that ownership-relevant fact.
pub(super) fn lookup(text: &str) -> Lookup {
    let mut result = Lookup::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "interface" if crate::adapter::command::valid_interface_name(value) => {
                result.interface = Some(value.into())
            }
            "gateway" => result.gateway = value.parse().ok(),
            "destination" => result.destination = value.parse().ok(),
            "flags" => {
                result.host = value
                    .trim_matches(['<', '>'])
                    .split(',')
                    .any(|flag| flag == "HOST")
            }
            _ => {}
        }
    }
    result
}

pub(super) fn failure(
    action: super::RouteAction,
    code: Option<i32>,
    stderr: &str,
) -> Result<(), super::RouteError> {
    use super::{RouteAction, RouteError};
    let text = stderr.to_ascii_lowercase();
    if text.contains("not permitted")
        || text.contains("permission denied")
        || text.contains("must be root")
    {
        return Err(RouteError::AccessDenied);
    }
    if action == RouteAction::Delete
        && (text.contains("not in table") || text.contains("no such process"))
    {
        return Ok(());
    }
    if text.contains("no such device") || text.contains("bad address") {
        return Err(RouteError::NoInterface);
    }
    // File exists must remain a failure: the caller must never own someone
    // else's route merely because it raced this process's add.
    Err(RouteError::Refused(code.unwrap_or(-1) as u32))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_resolved_destination_does_not_imply_an_owned_host_route() {
        let route = lookup(
            " route to: 10.100.61.3\ndestination: default\n gateway: 10.0.0.1\n interface: en0\n flags: <UP,GATEWAY,DONE,STATIC,PRCLONING,GLOBAL>\n",
        );
        assert_eq!(route.interface.as_deref(), Some("en0"));
        assert!(!route.host);
        let route = lookup(
            "destination: 10.100.61.3\n gateway: 10.0.0.1\n interface: en8\n flags: <UP,GATEWAY,HOST,DONE,STATIC>\n",
        );
        assert!(route.host);
        assert_eq!(route.destination, Some(Ipv4Addr::new(10, 100, 61, 3)));
    }
    #[test]
    fn permission_and_existing_routes_are_not_owned_successes() {
        use super::super::{RouteAction, RouteError};
        assert_eq!(
            failure(
                RouteAction::Add,
                Some(1),
                "route: writing to routing socket: Operation not permitted"
            ),
            Err(RouteError::AccessDenied)
        );
        assert_eq!(
            failure(RouteAction::Add, Some(1), "route: File exists"),
            Err(RouteError::Refused(1))
        );
        assert_eq!(
            failure(RouteAction::Delete, Some(1), "route: not in table"),
            Ok(())
        );
        assert_eq!(lookup("interface: ../bad").interface, None);
    }
}
