//! One Dr.COM UDP challenge exchange. No credentials or login frame are sent.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::protocol::{ChallengeError, ChallengeResponse, challenge_request};
use crate::reference_login::{BuildError, ReferenceLoginConfig, build_login_packet};
use crate::retry::{LoginRetry, LoginTimeout};
use crate::transport::UdpTransport;

#[derive(Debug)]
pub enum ChallengeExchangeError {
    Io(io::Error),
    TimedOut,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChallengeReceipt {
    /// The original records the source IP of an accepted initial challenge.
    pub source: SocketAddr,
    pub challenge: [u8; 4],
    pub system_auth_option: u16,
    /// Preserved for the login extensions that have not yet been decoded.
    pub datagram: Vec<u8>,
}

/// Builds the post-challenge login frame without sending it. Keeping this
/// boundary explicit prevents credentials from reaching the network until the
/// original packet profile is selected and reviewed.
pub fn build_reference_login(
    receipt: &ChallengeReceipt,
    username: &[u8],
    password: &[u8],
    config: &ReferenceLoginConfig,
) -> Result<Vec<u8>, BuildError> {
    build_login_packet(&receipt.challenge, username, password, config)
}

#[derive(Debug)]
pub enum ChallengeConnectError {
    Exchange(ChallengeExchangeError),
    AllServersTimedOut,
}

/// The original calls its C runtime `time` function and puts the low 16 bits
/// of Unix seconds into each challenge request (`0x1002ecf0`, `0x100026dd`).
pub fn current_challenge_nonce() -> u16 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as u16
}

/// Sends the 20-byte challenge request and waits for a matching type `0x02`
/// response. The original initial response path checks the echoed sequence
/// and nonce, then remembers the source IP. Unexpected or stale datagrams do
/// not become authenticated state. Type `0x05` is the original failure branch.
pub fn exchange_challenge(
    transport: &UdpTransport,
    destination: SocketAddrV4,
    sequence: u8,
    nonce: u16,
    client_option: u8,
    timeout: Duration,
) -> Result<ChallengeReceipt, ChallengeExchangeError> {
    exchange_challenge_from(
        transport,
        destination,
        sequence,
        nonce,
        client_option,
        timeout,
        None,
    )
}

/// Original proactive logout order at `0x1002ec10`: send a client Ping,
/// wait 500 ms, then request a fresh challenge. Unlike initial login, the
/// original logout handler accepts only the current online server IP
/// (`0x10030660`). The caller supplies the appropriate Ping variant.
pub fn exchange_logout_challenge(
    transport: &UdpTransport,
    online_server: SocketAddrV4,
    ping_datagram: &[u8],
    sequence: u8,
    nonce: u16,
    client_option: u8,
    timeout: Duration,
) -> Result<ChallengeReceipt, ChallengeExchangeError> {
    transport
        .send(online_server, ping_datagram)
        .map_err(ChallengeExchangeError::Io)?;
    thread::sleep(Duration::from_millis(500));
    exchange_challenge_from(
        transport,
        online_server,
        sequence,
        nonce,
        client_option,
        timeout,
        Some(*online_server.ip()),
    )
}

fn exchange_challenge_from(
    transport: &UdpTransport,
    destination: SocketAddrV4,
    sequence: u8,
    nonce: u16,
    client_option: u8,
    timeout: Duration,
    accepted_source: Option<Ipv4Addr>,
) -> Result<ChallengeReceipt, ChallengeExchangeError> {
    transport
        .send(
            destination,
            &challenge_request(sequence, nonce, client_option),
        )
        .map_err(ChallengeExchangeError::Io)?;

    let deadline = Instant::now() + timeout;
    let mut buffer = [0u8; 2048];
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|value| !value.is_zero())
            .ok_or(ChallengeExchangeError::TimedOut)?;
        transport
            .set_read_timeout(Some(remaining))
            .map_err(ChallengeExchangeError::Io)?;
        let (len, source) = match transport.receive(&mut buffer) {
            Ok(result) => result,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err(ChallengeExchangeError::TimedOut);
            }
            // A latched ICMP error from an earlier datagram says nothing about
            // this reply, so keep waiting instead of failing the exchange.
            Err(error) if crate::transport::is_transient_icmp(&error) => continue,
            Err(error) => return Err(ChallengeExchangeError::Io(error)),
        };
        if let Some(expected) = accepted_source
            && source.ip() != expected
        {
            continue;
        }
        let datagram = &buffer[..len];
        if datagram.first() == Some(&0x05) {
            return Err(ChallengeExchangeError::Rejected);
        }
        match ChallengeResponse::parse(datagram, sequence, nonce) {
            Ok(parsed) => {
                return Ok(ChallengeReceipt {
                    source,
                    challenge: parsed.challenge,
                    system_auth_option: parsed.system_auth_option,
                    datagram: datagram.to_vec(),
                });
            }
            Err(
                ChallengeError::UnexpectedType
                | ChallengeError::SequenceMismatch
                | ChallengeError::NonceMismatch
                | ChallengeError::TooShort,
            ) => continue,
        }
    }
}

/// Reuses the original login retry counter rather than starting a new budget
/// after the challenge succeeds. The caller must keep `retry` for the later
/// full login stage. No incomplete login packet is sent by this function.
pub fn connect_challenge(
    transport: &UdpTransport,
    retry: &mut LoginRetry,
    server_port: u16,
    sequence: &mut u8,
    client_option: u8,
    timeout: Duration,
) -> Result<ChallengeReceipt, ChallengeConnectError> {
    connect_challenge_with(
        transport,
        retry,
        server_port,
        sequence,
        client_option,
        timeout,
        current_challenge_nonce,
    )
}

/// Same as [`connect_challenge`] but with a caller-supplied nonce source, so
/// the reference profile can use its `Unix seconds % 0xffff` derivation instead
/// of the original binary's truncated low 16 bits.
pub fn connect_challenge_with<N>(
    transport: &UdpTransport,
    retry: &mut LoginRetry,
    server_port: u16,
    sequence: &mut u8,
    client_option: u8,
    timeout: Duration,
    next_nonce: N,
) -> Result<ChallengeReceipt, ChallengeConnectError>
where
    N: FnMut() -> u16,
{
    run_challenge_retries(retry, sequence, next_nonce, |server, sequence, nonce| {
        exchange_challenge(
            transport,
            SocketAddrV4::new(server, server_port),
            sequence,
            nonce,
            client_option,
            timeout,
        )
    })
}

fn run_challenge_retries<N, E>(
    retry: &mut LoginRetry,
    sequence: &mut u8,
    mut next_nonce: N,
    mut exchange: E,
) -> Result<ChallengeReceipt, ChallengeConnectError>
where
    N: FnMut() -> u16,
    E: FnMut(std::net::Ipv4Addr, u8, u16) -> Result<ChallengeReceipt, ChallengeExchangeError>,
{
    loop {
        *sequence = sequence.wrapping_add(1);
        match exchange(retry.current_server(), *sequence, next_nonce()) {
            Ok(receipt) => return Ok(receipt),
            Err(ChallengeExchangeError::TimedOut) => match retry.on_login_timeout() {
                LoginTimeout::RetrySameServer(_) | LoginTimeout::SwitchServer(_) => continue,
                LoginTimeout::AllServersExhausted => {
                    return Err(ChallengeConnectError::AllServersTimedOut);
                }
            },
            Err(error) => return Err(ChallengeConnectError::Exchange(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::thread;

    #[test]
    fn exchanges_challenge_and_ignores_stale_nonce_on_loopback() {
        let server = UdpTransport::bind(Ipv4Addr::LOCALHOST, 0).unwrap();
        let destination = match server.local_addr().unwrap() {
            SocketAddr::V4(addr) => addr,
            _ => unreachable!(),
        };
        let worker = thread::spawn(move || {
            let mut buffer = [0u8; 64];
            let (len, client) = server.receive(&mut buffer).unwrap();
            assert_eq!(len, 20);
            assert_eq!(&buffer[..5], &[1, 7, 0x34, 0x12, 9]);
            let client = match client {
                SocketAddr::V4(addr) => addr,
                _ => unreachable!(),
            };
            let mut reply = [0u8; 0x2c];
            reply[..4].copy_from_slice(&[2, 7, 0x35, 0x12]);
            server.send(client, &reply).unwrap();
            reply[2] = 0x34;
            reply[4..8].copy_from_slice(&[1, 2, 3, 4]);
            reply[0x2a..0x2c].copy_from_slice(&0x8001u16.to_le_bytes());
            server.send(client, &reply).unwrap();
        });

        let client = UdpTransport::bind(Ipv4Addr::LOCALHOST, 0).unwrap();
        let receipt =
            exchange_challenge(&client, destination, 7, 0x1234, 9, Duration::from_secs(1)).unwrap();
        assert_eq!(receipt.source, SocketAddr::V4(destination));
        assert_eq!(receipt.challenge, [1, 2, 3, 4]);
        assert_eq!(receipt.system_auth_option, 0x8001);
        worker.join().unwrap();
    }

    #[test]
    fn timeout_retries_three_times_then_uses_next_server() {
        let first = Ipv4Addr::new(10, 0, 0, 1);
        let second = Ipv4Addr::new(10, 0, 0, 2);
        let mut retry = LoginRetry::new(vec![first, second]).unwrap();
        let mut sequence = 250;
        let mut attempted = Vec::new();
        let receipt = run_challenge_retries(
            &mut retry,
            &mut sequence,
            || 0x1234,
            |server, seq, nonce| {
                attempted.push((server, seq, nonce));
                if server == first {
                    Err(ChallengeExchangeError::TimedOut)
                } else {
                    Ok(ChallengeReceipt {
                        source: SocketAddr::V4(SocketAddrV4::new(second, 61440)),
                        challenge: [1, 2, 3, 4],
                        system_auth_option: 0,
                        datagram: vec![],
                    })
                }
            },
        )
        .unwrap();
        assert_eq!(receipt.challenge, [1, 2, 3, 4]);
        assert_eq!(attempted.len(), 4);
        assert_eq!(attempted[0], (first, 251, 0x1234));
        assert_eq!(attempted[1], (first, 252, 0x1234));
        assert_eq!(attempted[2], (first, 253, 0x1234));
        assert_eq!(attempted[3], (second, 254, 0x1234));
        assert_eq!(retry.current_server(), second);
        assert_eq!(sequence, 254);
    }

    #[test]
    fn logout_preflight_sends_ping_before_new_challenge() {
        let server = UdpTransport::bind(Ipv4Addr::LOCALHOST, 0).unwrap();
        let destination = match server.local_addr().unwrap() {
            SocketAddr::V4(addr) => addr,
            _ => unreachable!(),
        };
        let worker = thread::spawn(move || {
            let mut buffer = [0u8; 64];
            let (ping_len, client) = server.receive(&mut buffer).unwrap();
            assert_eq!(&buffer[..ping_len], &[0xff, 1, 2, 3]);
            let (challenge_len, source) = server.receive(&mut buffer).unwrap();
            assert_eq!(source, client);
            assert_eq!(challenge_len, 20);
            assert_eq!(&buffer[..5], &[1, 8, 0x34, 0x12, 9]);
            let client = match client {
                SocketAddr::V4(addr) => addr,
                _ => unreachable!(),
            };
            let mut reply = [0u8; 0x2c];
            reply[..4].copy_from_slice(&[2, 8, 0x34, 0x12]);
            reply[4..8].copy_from_slice(&[4, 3, 2, 1]);
            server.send(client, &reply).unwrap();
        });
        let client = UdpTransport::bind(Ipv4Addr::LOCALHOST, 0).unwrap();
        let receipt = exchange_logout_challenge(
            &client,
            destination,
            &[0xff, 1, 2, 3],
            8,
            0x1234,
            9,
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(receipt.challenge, [4, 3, 2, 1]);
        worker.join().unwrap();
    }
}
