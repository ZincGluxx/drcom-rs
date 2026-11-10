//! Integration tests for the TCP transport and the command-line surface.

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::Daemon;
use drcom_ipc::Client;
use drcom_service::Notification;

fn wait_for_exit_ready(client: &mut Client) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match client.next_notification(remaining) {
            Ok(Some(Notification::ExitReady)) => return true,
            Ok(Some(_)) => {}
            Ok(None) => return false,
            Err(_) => return false,
        }
    }
}

#[test]
fn tcp_mode_serves_two_clients_and_broadcasts_exit_ready() {
    let mut daemon = Daemon::spawn(&[
        "--listen",
        "127.0.0.1:0",
        "--no-startup-probe",
        "--no-auto-login",
    ]);
    let address = daemon.listen_address();
    assert!(address.ip().is_loopback());
    assert_ne!(address.port(), 0, "ephemeral port must be reported");

    let mut first = Client::connect_tcp(address).expect("first client connects");
    assert_eq!(first.ready().protocol, "drcom-ipc");
    assert_eq!(first.ready().api_version, 1);
    let initial = first
        .next_notification(Duration::from_secs(5))
        .expect("initial notification")
        .expect("initial state");
    assert!(matches!(initial, Notification::State { .. }));

    let mut second = Client::connect_tcp(address).expect("second client connects");
    let second_initial = second
        .next_notification(Duration::from_secs(5))
        .expect("second initial notification")
        .expect("second initial state");
    assert!(matches!(second_initial, Notification::State { .. }));

    let response = first.snapshot().expect("snapshot over TCP");
    assert!(response.ok);
    assert_eq!(response.snapshot.expect("snapshot payload").api_version, 1);

    // A shutdown from one client ends the daemon for all clients; both get the
    // exit_ready notification before the sockets close.
    let response = first.shutdown().expect("shutdown response");
    assert!(response.ok);
    assert!(
        wait_for_exit_ready(&mut first),
        "first client got exit_ready"
    );
    assert!(
        wait_for_exit_ready(&mut second),
        "second client got exit_ready"
    );

    let status = daemon.wait_exit(Duration::from_secs(10));
    assert_eq!(status.code(), Some(0));
    let stderr = daemon.stderr_text();
    assert!(
        stderr.contains("drcom-daemon listening"),
        "missing listening banner: {stderr:?}"
    );
}

#[test]
fn version_and_help_are_plain_text_and_do_not_start_a_service() {
    let output = Command::new(env!("CARGO_BIN_EXE_drcom-daemon"))
        .arg("--version")
        .output()
        .expect("run --version");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert!(stdout.starts_with("drcom-daemon "), "stdout: {stdout:?}");
    assert!(stdout.contains("protocol drcom-ipc"), "stdout: {stdout:?}");
    assert!(stdout.contains("api 1"), "stdout: {stdout:?}");
    assert!(output.stderr.is_empty());

    let output = Command::new(env!("CARGO_BIN_EXE_drcom-daemon"))
        .arg("--help")
        .output()
        .expect("run --help");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert!(stdout.contains("用法"), "stdout: {stdout:?}");
    assert!(stdout.contains("--listen"));
}

#[test]
fn refuses_non_loopback_binds_and_conflicting_arguments() {
    let output = Command::new(env!("CARGO_BIN_EXE_drcom-daemon"))
        .args(["--listen", "0.0.0.0:0"])
        .output()
        .expect("run non-loopback bind");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(
        stderr.contains("非回环"),
        "expected a non-loopback refusal, got: {stderr:?}"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_drcom-daemon"))
        .args(["--stdio", "--listen", "127.0.0.1:0"])
        .output()
        .expect("run conflicting transports");
    assert_eq!(output.status.code(), Some(2));

    let output = Command::new(env!("CARGO_BIN_EXE_drcom-daemon"))
        .arg("--not-a-flag")
        .output()
        .expect("run unknown flag");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("未知参数"), "stderr: {stderr:?}");
}
