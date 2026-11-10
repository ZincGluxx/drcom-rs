//! Unit tests for the wire envelopes, framing, in-memory server and client.

use std::io::{BufReader, Cursor};
use std::net::TcpListener;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use drcom_service::{
    API_VERSION, Command, CommandOutcome, Notification, ProbeKind, Service, ServiceOptions,
    Snapshot,
};

use crate::client::Client;
use crate::protocol::{
    MAX_LINE_BYTES, PROTOCOL_NAME, PROTOCOL_VERSION, ReadLine, Ready, Request, RequestBody,
    Response, ServerInfo, ServerMessage, encode_json_line, parse_request, read_line_limited,
};
use crate::server::{Backend, ConnectionEnd, serve_connection};

fn sample_snapshot() -> Snapshot {
    Service::new(ServiceOptions::for_test()).snapshot()
}

fn sample_info() -> ServerInfo {
    ServerInfo::new(API_VERSION, "0.0.0-test")
}

fn json_lines(bytes: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8(bytes.to_vec())
        .expect("server output is UTF-8")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("server line is JSON"))
        .collect()
}

fn find_response(lines: &[serde_json::Value], id: u64) -> &serde_json::Value {
    lines
        .iter()
        .find(|line| line.get("id").and_then(serde_json::Value::as_u64) == Some(id))
        .expect("response with the requested id")
}

// ---------------------------------------------------------------------------
// Envelopes
// ---------------------------------------------------------------------------

#[test]
fn request_round_trips_every_command_variant() {
    let commands = vec![
        Command::Login {
            username: "20230001".to_owned(),
            password: "s3cret".to_owned(),
            auto_reconnect: true,
        },
        Command::Logout,
        Command::RefreshAdapter,
        Command::Probe {
            kind: ProbeKind::All,
        },
        Command::OpenNetworkEditor,
        Command::CloseNetworkEditor,
        Command::ApplyNetwork {
            ip: "10.0.0.2".to_owned(),
            mask: "255.255.255.0".to_owned(),
            gateway: "10.0.0.1".to_owned(),
            dns: "10.10.10.10".to_owned(),
        },
        Command::SaveSettings {
            username: "20230001".to_owned(),
            password: "s3cret".to_owned(),
            auto_login: true,
            auto_reconnect: true,
            autostart: false,
            minimize_to_tray: true,
        },
        Command::ClearSavedPassword {
            username: "20230001".to_owned(),
            auto_reconnect: false,
            minimize_to_tray: true,
        },
        Command::SetMinimizeToTray { enabled: false },
        Command::DiagnosticsText,
        Command::Shutdown,
    ];

    for (index, command) in commands.into_iter().enumerate() {
        let request = Request {
            id: index as u64 + 1,
            body: RequestBody::Command {
                command: command.clone(),
            },
        };
        let line = encode_json_line(&request).expect("serialise request");
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
        let parsed: Request = serde_json::from_str(line.trim_end()).expect("parse request");
        assert_eq!(parsed, request, "round trip of {command:?}");
    }
}

#[test]
fn request_shapes_match_the_documented_envelopes() {
    let ping = Request {
        id: 7,
        body: RequestBody::Ping,
    };
    assert_eq!(
        serde_json::to_value(&ping).expect("serialise"),
        serde_json::json!({"id": 7, "type": "ping"})
    );

    let snapshot = Request {
        id: 8,
        body: RequestBody::Snapshot,
    };
    assert_eq!(
        serde_json::to_value(&snapshot).expect("serialise"),
        serde_json::json!({"id": 8, "type": "snapshot"})
    );

    let login = Request {
        id: 9,
        body: RequestBody::Command {
            command: Command::Login {
                username: "u".to_owned(),
                password: "p".to_owned(),
                auto_reconnect: false,
            },
        },
    };
    assert_eq!(
        serde_json::to_value(&login).expect("serialise"),
        serde_json::json!({
            "id": 9,
            "type": "command",
            "command": {"command": "login", "username": "u", "password": "p", "auto_reconnect": false}
        })
    );

    let shutdown = Request {
        id: 10,
        body: RequestBody::Shutdown,
    };
    assert_eq!(
        serde_json::to_value(&shutdown).expect("serialise"),
        serde_json::json!({"id": 10, "type": "shutdown"})
    );
}

#[test]
fn responses_round_trip_through_json() {
    let snapshot = Response::snapshot(1, sample_snapshot());
    let info = Response::info(2, sample_info());
    let text = Response::text(3, "diagnostics");
    let error = Response::error(4, "boom");

    for response in [snapshot, info, text, error] {
        let line = encode_json_line(&response).expect("serialise response");
        assert!(line.ends_with('\n'));
        let parsed: Response = serde_json::from_str(line.trim_end()).expect("parse response");
        assert_eq!(parsed, response);
    }
}

#[test]
fn ready_and_notifications_share_the_server_message_envelope() {
    let ready = Ready::from_info(&sample_info());
    let ready_line = encode_json_line(&ready).expect("serialise ready");
    assert!(ready_line.starts_with("{\"event\":\"ready\""));
    let parsed: ServerMessage = serde_json::from_str(ready_line.trim_end()).expect("parse ready");
    assert_eq!(parsed, ServerMessage::Ready(ready));

    let notifications = [
        Notification::State {
            snapshot: sample_snapshot(),
        },
        Notification::HideToTray {
            note: "已连接".to_owned(),
        },
        Notification::ExitReady,
    ];
    for notification in notifications {
        let line = encode_json_line(&notification).expect("serialise notification");
        let parsed: ServerMessage =
            serde_json::from_str(line.trim_end()).expect("parse notification");
        assert_eq!(parsed, ServerMessage::Notification(notification));
    }
}

#[test]
fn malformed_requests_report_the_recovered_id() {
    let error = parse_request("{\"id\": 42, \"type\": \"warp\"}").expect_err("unknown type");
    assert_eq!(error.id, 42);
    assert!(error.message.contains("invalid request"));

    let error = parse_request("not json").expect_err("invalid JSON");
    assert_eq!(error.id, 0);
    assert!(error.message.contains("invalid JSON"));

    let error = parse_request("{\"id\": 3}").expect_err("missing type");
    assert_eq!(error.id, 3);
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

#[test]
fn reads_multiple_lines_crlf_and_a_final_line_without_newline() {
    let input = b"{\"id\":1,\"type\":\"ping\"}\r\n{\"id\":2,\"type\":\"snapshot\"}\nlast";
    let mut reader = BufReader::new(Cursor::new(input.to_vec()));

    assert_eq!(
        read_line_limited(&mut reader).expect("read first"),
        ReadLine::Line("{\"id\":1,\"type\":\"ping\"}".to_owned())
    );
    assert_eq!(
        read_line_limited(&mut reader).expect("read second"),
        ReadLine::Line("{\"id\":2,\"type\":\"snapshot\"}".to_owned())
    );
    assert_eq!(
        read_line_limited(&mut reader).expect("read final"),
        ReadLine::Line("last".to_owned())
    );
    assert_eq!(
        read_line_limited(&mut reader).expect("read eof"),
        ReadLine::Eof
    );
}

#[test]
fn rejects_an_oversized_line_and_recovers_at_the_next_one() {
    let mut input = vec![b'x'; MAX_LINE_BYTES + 10];
    input.push(b'\n');
    input.extend_from_slice(b"{\"id\":1,\"type\":\"ping\"}\n");
    let mut reader = BufReader::new(Cursor::new(input));

    match read_line_limited(&mut reader).expect("read oversized") {
        ReadLine::Rejected { recovered, error } => {
            assert!(recovered);
            assert!(error.contains("1048576"));
        }
        other => panic!("expected rejection, got {other:?}"),
    }
    assert_eq!(
        read_line_limited(&mut reader).expect("read next line"),
        ReadLine::Line("{\"id\":1,\"type\":\"ping\"}".to_owned())
    );
}

#[test]
fn rejects_invalid_utf8_and_keeps_the_stream_in_sync() {
    let input = b"\xff\xfe\n{\"id\":1,\"type\":\"ping\"}\n";
    let mut reader = BufReader::new(Cursor::new(input.to_vec()));
    match read_line_limited(&mut reader).expect("read invalid") {
        ReadLine::Rejected { recovered, error } => {
            assert!(recovered);
            assert!(error.contains("UTF-8"));
        }
        other => panic!("expected rejection, got {other:?}"),
    }
    assert_eq!(
        read_line_limited(&mut reader).expect("read next line"),
        ReadLine::Line("{\"id\":1,\"type\":\"ping\"}".to_owned())
    );
}

#[test]
fn the_protocol_constants_are_stable() {
    assert_eq!(PROTOCOL_NAME, "drcom-ipc");
    assert_eq!(PROTOCOL_VERSION, 1);
    assert_eq!(MAX_LINE_BYTES, 1024 * 1024);
    assert_eq!(
        crate::server::Backend::info(&Service::new(ServiceOptions::for_test())).api_version,
        API_VERSION
    );
}

// ---------------------------------------------------------------------------
// In-memory server
// ---------------------------------------------------------------------------

struct FakeBackend {
    snapshot: Snapshot,
    executed: Mutex<Vec<Command>>,
    stopped: AtomicBool,
}

impl FakeBackend {
    fn new() -> Self {
        Self {
            snapshot: sample_snapshot(),
            executed: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
        }
    }
}

impl Backend for FakeBackend {
    fn info(&self) -> ServerInfo {
        sample_info()
    }

    fn snapshot(&self) -> Snapshot {
        self.snapshot.clone()
    }

    fn execute(&self, command: Command) -> CommandOutcome {
        self.executed
            .lock()
            .expect("command log")
            .push(command.clone());
        match command {
            Command::DiagnosticsText => CommandOutcome::Text("diagnostics text".to_owned()),
            _ => CommandOutcome::Snapshot(self.snapshot.clone()),
        }
    }

    fn after_stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}

#[test]
fn in_memory_server_answers_requests_and_shuts_down() {
    let backend = FakeBackend::new();
    let input = concat!(
        "{\"id\":1,\"type\":\"ping\"}\n",
        "{\"id\":2,\"type\":\"snapshot\"}\n",
        "{\"id\":3,\"type\":\"command\",\"command\":{\"command\":\"diagnostics_text\"}}\n",
        "{\"id\":4,\"type\":\"command\",\"command\":{\"command\":\"refresh_adapter\"}}\n",
        "not json\n",
        "{\"id\":5,\"type\":\"shutdown\"}\n",
        "{\"id\":6,\"type\":\"ping\"}\n",
    );
    let mut reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
    let mut writer = Vec::new();

    let end = serve_connection(&mut reader, &mut writer, &backend).expect("serve");
    assert_eq!(end, ConnectionEnd::Stopped);
    assert!(backend.stopped.load(Ordering::SeqCst));

    let lines = json_lines(&writer);
    // Banner first, then the initial state, then one response per request. The
    // ping after `shutdown` must not be answered.
    assert_eq!(lines[0]["event"], "ready");
    assert_eq!(lines[0]["protocol"], PROTOCOL_NAME);
    assert_eq!(lines[0]["protocol_version"], PROTOCOL_VERSION);
    assert_eq!(lines[0]["api_version"], API_VERSION);
    assert_eq!(lines[0]["app_version"], "0.0.0-test");
    assert_eq!(lines[1]["event"], "state");
    assert_eq!(lines[1]["snapshot"]["api_version"], API_VERSION);
    assert!(lines[1]["snapshot"]["adapter"].is_object());
    assert!(lines[1]["snapshot"]["settings"].is_object());

    let ping = find_response(&lines, 1);
    assert_eq!(ping["ok"], true);
    assert_eq!(ping["info"]["protocol"], PROTOCOL_NAME);
    assert_eq!(ping["info"]["api_version"], API_VERSION);

    let snapshot = find_response(&lines, 2);
    assert_eq!(snapshot["ok"], true);
    assert_eq!(snapshot["snapshot"]["api_version"], API_VERSION);
    assert!(snapshot["snapshot"]["adapter"].is_object());
    assert!(snapshot["snapshot"]["settings"].is_object());

    let diagnostics = find_response(&lines, 3);
    assert_eq!(diagnostics["ok"], true);
    assert_eq!(diagnostics["text"], "diagnostics text");

    let refresh = find_response(&lines, 4);
    assert_eq!(refresh["ok"], true);
    assert!(refresh["snapshot"].is_object());

    let malformed = find_response(&lines, 0);
    assert_eq!(malformed["ok"], false);
    assert!(
        malformed["error"]
            .as_str()
            .expect("error text")
            .contains("invalid JSON")
    );

    let shutdown = find_response(&lines, 5);
    assert_eq!(shutdown["ok"], true);
    assert!(shutdown["snapshot"].is_object());

    assert!(
        lines
            .iter()
            .all(|line| line.get("id").and_then(serde_json::Value::as_u64) != Some(6)),
        "no requests may be served after shutdown"
    );

    let executed = backend.executed.lock().expect("command log");
    assert!(executed.contains(&Command::DiagnosticsText));
    assert!(executed.contains(&Command::RefreshAdapter));
    assert_eq!(executed.last(), Some(&Command::Shutdown));
}

#[test]
fn server_reports_an_oversized_line_and_continues() {
    let backend = FakeBackend::new();
    let mut input = vec![b'x'; MAX_LINE_BYTES + 1];
    input.push(b'\n');
    input.extend_from_slice(b"{\"id\":1,\"type\":\"ping\"}\n{\"id\":2,\"type\":\"shutdown\"}\n");
    let mut reader = BufReader::new(Cursor::new(input));
    let mut writer = Vec::new();

    let end = serve_connection(&mut reader, &mut writer, &backend).expect("serve");
    assert_eq!(end, ConnectionEnd::Stopped);

    let lines = json_lines(&writer);
    let oversized = find_response(&lines, 0);
    assert_eq!(oversized["ok"], false);
    assert!(
        oversized["error"]
            .as_str()
            .expect("error text")
            .contains("1048576")
    );
    assert_eq!(find_response(&lines, 1)["ok"], true);
    assert_eq!(find_response(&lines, 2)["ok"], true);
}

// ---------------------------------------------------------------------------
// Blocking client over TCP
// ---------------------------------------------------------------------------

#[test]
fn tcp_client_round_trips_ping_snapshot_diagnostics_and_shutdown() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
    let address = listener.local_addr().expect("listener address");
    let server = thread::spawn(move || {
        let (stream, _peer) = listener.accept().expect("accept one client");
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut writer = stream;
        let backend = FakeBackend::new();
        serve_connection(&mut reader, &mut writer, &backend).expect("serve one client")
    });

    let mut client = Client::connect_tcp(address).expect("connect");
    assert_eq!(client.ready().protocol, PROTOCOL_NAME);
    assert_eq!(client.ready().protocol_version, PROTOCOL_VERSION);
    assert_eq!(client.ready().api_version, API_VERSION);

    let initial = client
        .next_notification(Duration::from_secs(2))
        .expect("initial notification")
        .expect("initial state");
    assert!(matches!(initial, Notification::State { .. }));

    let ping = client.ping().expect("ping");
    assert!(ping.ok);
    assert_eq!(ping.info.expect("ping info").app_version, "0.0.0-test");

    let snapshot = client.snapshot().expect("snapshot");
    let snapshot = snapshot.snapshot.expect("snapshot payload");
    assert_eq!(snapshot.api_version, API_VERSION);
    let value = serde_json::to_value(&snapshot).expect("snapshot JSON");
    assert!(value["adapter"].is_object());
    assert!(value["settings"].is_object());
    assert!(value["connection"].is_object());

    let diagnostics = client
        .command(Command::DiagnosticsText)
        .expect("diagnostics");
    assert_eq!(diagnostics.text.as_deref(), Some("diagnostics text"));

    let shutdown = client.shutdown().expect("shutdown");
    assert!(shutdown.ok);

    assert_eq!(
        server.join().expect("server thread"),
        ConnectionEnd::Stopped
    );
}
