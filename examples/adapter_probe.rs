//! Credential-free per-interface Dr.COM challenge diagnostic.
use drcom_campus::{
    adapter,
    protocol::{ChallengeResponse, challenge_request},
    transport::UdpTransport,
};
use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    time::{Duration, Instant},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        let selected = adapter::select_for(Some(Ipv4Addr::new(10, 100, 61, 3)));
        let candidates: Vec<_> = adapter::list().iter().map(|a| serde_json::json!({"name":a.friendly_name,"ipv4":a.ipv4.to_string(),"index":a.interface_index})).collect();
        println!(
            "{}",
            serde_json::json!({"candidates":candidates,"current_selection":selected.map(|a|a.friendly_name)})
        );
        return Ok(());
    }
    if args.len() != 2 {
        return Err("usage: adapter_probe [source-ip interface-index]".into());
    }
    let source: Ipv4Addr = args[0].parse()?;
    let index: u32 = args[1].parse()?;
    let started = Instant::now();
    let result = probe(source, index);
    println!(
        "{}",
        serde_json::json!({"source":source.to_string(),"index":index,"challenge":result.is_ok(),"detail":result.unwrap_or_else(|e|e.to_string()),"elapsed_ms":started.elapsed().as_millis()})
    );
    Ok(())
}

fn probe(source: Ipv4Addr, index: u32) -> Result<String, Box<dyn std::error::Error>> {
    let socket = UdpTransport::bind_interface(source, 0, Some(index))?;
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    let server = SocketAddrV4::new(Ipv4Addr::new(10, 100, 61, 3), 61440);
    let sequence = 2;
    let nonce = drcom_campus::auth::current_challenge_nonce();
    socket.send(server, &challenge_request(sequence, nonce, 9))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut buffer = [0u8; 2048];
    while Instant::now() < deadline {
        match socket.receive(&mut buffer) {
            Ok((len, peer)) if peer == SocketAddr::V4(server) => {
                if ChallengeResponse::parse(&buffer[..len], sequence, nonce).is_ok() {
                    return Ok(format!(
                        "valid challenge from {peer}; local {}",
                        socket.local_addr()?
                    ));
                }
            }
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) || drcom_campus::transport::is_transient_icmp(&e) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Err("no matching challenge within 2s (temporary source port)".into())
}
