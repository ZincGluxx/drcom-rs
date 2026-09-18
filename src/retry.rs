//! The Dr.COM login timeout/server failover rule from `0x1003d410`.

use std::net::Ipv4Addr;

const MAX_LOGIN_SENDS_PER_SERVER: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginTimeout {
    RetrySameServer(Ipv4Addr),
    SwitchServer(Ipv4Addr),
    AllServersExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryConfigError {
    NoServers,
    InvalidServer(Ipv4Addr),
}

/// Tracks actual login sends, including the initial send. The original counts
/// up before issuing a new challenge and changes server after three sends.
#[derive(Debug)]
pub struct LoginRetry {
    servers: Vec<Ipv4Addr>,
    active_index: usize,
    sends_on_active_server: u8,
}

impl LoginRetry {
    pub fn new(servers: Vec<Ipv4Addr>) -> Result<Self, RetryConfigError> {
        Self::build(servers, false)
    }

    /// Same as [`LoginRetry::new`] but additionally accepts `127.0.0.1`, so the
    /// whole session can be exercised against a loopback mock authentication
    /// server. A campus deployment only ever lists routable servers, so
    /// production callers keep using [`LoginRetry::new`] and its sentinel check.
    pub fn new_accepting_loopback(servers: Vec<Ipv4Addr>) -> Result<Self, RetryConfigError> {
        Self::build(servers, true)
    }

    fn build(servers: Vec<Ipv4Addr>, accept_loopback: bool) -> Result<Self, RetryConfigError> {
        if servers.is_empty() {
            return Err(RetryConfigError::NoServers);
        }
        for server in &servers {
            // The original treats these sentinel values as absent servers.
            let sentinel = *server == Ipv4Addr::UNSPECIFIED
                || *server == Ipv4Addr::BROADCAST
                || (*server == Ipv4Addr::LOCALHOST && !accept_loopback);
            if sentinel {
                return Err(RetryConfigError::InvalidServer(*server));
            }
        }
        Ok(Self {
            servers,
            active_index: 0,
            sends_on_active_server: 1,
        })
    }

    pub fn current_server(&self) -> Ipv4Addr {
        self.servers[self.active_index]
    }

    pub fn on_login_timeout(&mut self) -> LoginTimeout {
        if self.sends_on_active_server < MAX_LOGIN_SENDS_PER_SERVER {
            self.sends_on_active_server += 1;
            return LoginTimeout::RetrySameServer(self.current_server());
        }
        if self.active_index + 1 >= self.servers.len() {
            self.active_index = 0;
            self.sends_on_active_server = 1;
            return LoginTimeout::AllServersExhausted;
        }
        self.active_index += 1;
        self.sends_on_active_server = 1;
        LoginTimeout::SwitchServer(self.current_server())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_sends_then_failover_then_exhaustion() {
        let first = Ipv4Addr::new(10, 0, 0, 1);
        let second = Ipv4Addr::new(10, 0, 0, 2);
        let mut retry = LoginRetry::new(vec![first, second]).unwrap();
        assert_eq!(retry.current_server(), first);
        assert_eq!(
            retry.on_login_timeout(),
            LoginTimeout::RetrySameServer(first)
        );
        assert_eq!(
            retry.on_login_timeout(),
            LoginTimeout::RetrySameServer(first)
        );
        assert_eq!(retry.on_login_timeout(), LoginTimeout::SwitchServer(second));
        assert_eq!(
            retry.on_login_timeout(),
            LoginTimeout::RetrySameServer(second)
        );
        assert_eq!(
            retry.on_login_timeout(),
            LoginTimeout::RetrySameServer(second)
        );
        assert_eq!(retry.on_login_timeout(), LoginTimeout::AllServersExhausted);
        assert_eq!(retry.current_server(), first);
    }

    #[test]
    fn loopback_is_only_accepted_for_tests() {
        assert_eq!(
            LoginRetry::new(vec![Ipv4Addr::LOCALHOST]).unwrap_err(),
            RetryConfigError::InvalidServer(Ipv4Addr::LOCALHOST)
        );
        let retry = LoginRetry::new_accepting_loopback(vec![Ipv4Addr::LOCALHOST]).unwrap();
        assert_eq!(retry.current_server(), Ipv4Addr::LOCALHOST);
        assert_eq!(
            LoginRetry::new_accepting_loopback(vec![Ipv4Addr::UNSPECIFIED]).unwrap_err(),
            RetryConfigError::InvalidServer(Ipv4Addr::UNSPECIFIED)
        );
    }
}
