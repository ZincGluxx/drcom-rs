//! Integration test for the stdio transport of `drcom-daemon`.
//!
//! Spawns the real binary with the side-effect-free flags (no startup probe, no
//! auto-login), then exercises the handshake, one read-only request, one command,
//! diagnostics and a graceful shutdown. Every read is bounded by a timeout.

mod common;

use std::time::Duration;

use common::Daemon;
use serde_json::json;

const ARGS: [&str; 3] = ["--stdio", "--no-startup-probe", "--no-auto-login"];

#[test]
fn stdio_handshake_state_requests_diagnostics_and_shutdown() {
    let mut daemon = Daemon::spawn(&ARGS);

    // 1. The banner is the first line, with both version lines.
    let ready = daemon.next_json("ready banner");
    assert_eq!(ready["event"], "ready");
    assert_eq!(ready["protocol"], "drcom-ipc");
    assert_eq!(ready["protocol_version"], 1);
    assert_eq!(ready["api_version"], 1);
    assert_eq!(ready["app_version"], env!("CARGO_PKG_VERSION"));

    // 2. The second line is the initial full snapshot.
    let state = daemon.read_event("state");
    let snapshot = &state["snapshot"];
    assert_eq!(snapshot["api_version"], 1);
    assert_eq!(snapshot["app_version"], env!("CARGO_PKG_VERSION"));
    for section in [
        "connection",
        "settings",
        "adapter",
        "failure",
        "probe",
        "network_editor",
    ] {
        assert!(snapshot[section].is_object(), "missing snapshot.{section}");
    }
    assert!(snapshot["campus_page"].is_string());
    assert!(snapshot["tooltip"].is_string());
    // Credentials never cross IPC: snapshots have no password field at all.
    assert!(snapshot["settings"].get("password").is_none());

    // 3. `snapshot` is read-only and returns the same API version.
    daemon.send(&json!({"id": 1, "type": "snapshot"}));
    let response = daemon.read_response(1);
    assert_eq!(response["ok"], true);
    assert_eq!(response["snapshot"]["api_version"], 1);
    assert!(response["snapshot"]["adapter"].is_object());
    assert!(response["snapshot"]["settings"].is_object());

    // 4. `ping` reports protocol and build information.
    daemon.send(&json!({"id": 2, "type": "ping"}));
    let response = daemon.read_response(2);
    assert_eq!(response["ok"], true);
    assert_eq!(response["info"]["protocol"], "drcom-ipc");
    assert_eq!(response["info"]["protocol_version"], 1);
    assert_eq!(response["info"]["api_version"], 1);
    assert_eq!(response["info"]["app_version"], env!("CARGO_PKG_VERSION"));

    // 5. Commands are forwarded one-to-one; diagnostics returns text instead of
    //    a state change.
    daemon.send(&json!({
        "id": 3,
        "type": "command",
        "command": {"command": "diagnostics_text"}
    }));
    let response = daemon.read_response(3);
    assert_eq!(response["ok"], true);
    let text = response["text"]
        .as_str()
        .expect("diagnostics response carries text");
    assert!(!text.is_empty(), "diagnostics text should not be empty");
    assert!(
        text.contains("DrCom"),
        "unexpected diagnostics text: {text}"
    );

    // 6. Shutdown: the response comes first, then exit_ready, then exit code 0.
    daemon.send(&json!({"id": 4, "type": "shutdown"}));
    let response = daemon.read_response(4);
    assert_eq!(response["ok"], true);
    let exit_ready = daemon.read_event("exit_ready");
    assert_eq!(exit_ready["event"], "exit_ready");
    let status = daemon.wait_exit(Duration::from_secs(10));
    assert_eq!(status.code(), Some(0), "graceful shutdown exits 0");

    // 7. Logs go to stderr only and never look like protocol JSON.
    let stderr = daemon.stderr_text();
    assert!(
        stderr.contains("drcom-daemon"),
        "expected daemon status on stderr, got: {stderr:?}"
    );
    assert!(
        !stderr
            .lines()
            .any(|line| line.trim_start().starts_with('{')),
        "protocol JSON leaked into stderr: {stderr}"
    );
}

#[test]
fn closing_stdin_is_a_graceful_shutdown() {
    let mut daemon = Daemon::spawn(&ARGS);

    let ready = daemon.next_json("ready banner");
    assert_eq!(ready["event"], "ready");
    let _state = daemon.read_event("state");

    daemon.close_stdin();

    let exit_ready = daemon.read_event("exit_ready");
    assert_eq!(exit_ready["event"], "exit_ready");
    let status = daemon.wait_exit(Duration::from_secs(10));
    assert_eq!(status.code(), Some(0));
}
