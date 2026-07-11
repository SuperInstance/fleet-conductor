//! Integration test: exercises the fleet-conductor HTTP server over a real
//! TCP socket.
//!
//! This test starts the server on a real OS-assigned port and connects as a
//! genuine TCP client — every request and response traverses the kernel TCP
//! stack. Nothing is mocked.
//!
//! Two test modes:
//! 1. `full_lifecycle_over_real_tcp_socket` — server in a background thread
//!    with a real bound socket (same OS process, real TCP).
//! 2. `two_process_coordination_via_subprocess` — server as an actual OS
//!    subprocess (two real processes talking over a real socket).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};

// ---------------------------------------------------------------------------
// Minimal HTTP client (std only — no reqwest/ureq dependency)
// ---------------------------------------------------------------------------

/// Send an HTTP request to `addr` and return `(status_code, json_body)`.
///
/// Each call opens a fresh TCP connection, sends the request, reads until the
/// server closes the connection (`Connection: close`), and parses the
/// response. This is a real TCP round-trip, not an in-memory function call.
fn http_request(addr: SocketAddr, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("TCP connect failed");
    stream.set_nodelay(true).ok();

    let request = match body {
        Some(b) => format!(
            "{method} {path} HTTP/1.1\r\n\
             Host: localhost\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {clen}\r\n\
             Connection: close\r\n\
             \r\n\
             {b}",
            clen = b.len()
        ),
        None => format!(
            "{method} {path} HTTP/1.1\r\n\
             Host: localhost\r\n\
             Connection: close\r\n\
             \r\n"
        ),
    };

    stream.write_all(request.as_bytes()).expect("write failed");

    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read failed");
    let response = String::from_utf8(response).expect("non-utf8 response");

    // Parse status code from the first line ("HTTP/1.1 200 OK").
    let status: u16 = response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .expect("no status code in response");

    // Body is everything after the first \r\n\r\n.
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("").to_string();

    (status, body)
}

// ---------------------------------------------------------------------------
// Test 1: Full lifecycle over a real TCP socket (background thread)
// ---------------------------------------------------------------------------

/// The README's example conservation config: gamma in [-0.5, 0.5], eta >= 0.3.
const README_SPEC: &str = r#"{"agents":[{"kind":"inference","count":4,"layer":0}],"conservation":{"gamma_min":-0.5,"gamma_max":0.5,"eta_floor":0.3}}"#;

#[test]
fn full_lifecycle_over_real_tcp_socket() {
    // --- Start the server on a real, OS-assigned port ---
    let server = fleet_conductor::ConductorServer::new("127.0.0.1:0").expect("bind failed");
    let addr = server.local_addr();
    eprintln!("[test] server bound to real address {addr}");

    // Serve in a background thread — this is our "second process".
    // The client (this thread) communicates entirely over TCP.
    std::thread::spawn(move || {
        server.serve();
    });

    // Give the listener a moment to enter its accept loop.
    std::thread::sleep(std::time::Duration::from_millis(50));

    // ---- Health check ----
    let (status, body) = http_request(addr, "GET", "/health", None);
    assert_eq!(status, 200);
    assert!(body.contains(r#""status":"ok""#), "health body: {body}");
    eprintln!("[test] health check OK");

    // ---- Register: reconcile toward 4 inference agents ----
    let (status, body) = http_request(addr, "POST", "/reconcile", Some(README_SPEC));
    assert_eq!(status, 200, "reconcile failed: {body}");
    eprintln!("[test] reconcile OK — registered 4 inference agents");

    // ---- Observe: 4 Pending agents, eta = 0 (no active agents yet) ----
    let (status, body) = http_request(addr, "GET", "/fleet", None);
    assert_eq!(status, 200);
    let fleet: serde_json::Value = serde_json::from_str(&body).expect("invalid JSON");
    assert_eq!(fleet["total"].as_u64().unwrap(), 4);
    assert_eq!(fleet["live"].as_u64().unwrap(), 4); // Pending is live
    assert_eq!(
        fleet["by_kind"]["inference"]["observed"].as_u64().unwrap(),
        4
    );
    assert_eq!(
        fleet["by_kind"]["inference"]["by_state"]["Pending"]
            .as_u64()
            .unwrap(),
        4
    );
    assert_eq!(fleet["eta"].as_f64().unwrap(), 0.0); // no Healthy/Degraded agents yet
    eprintln!("[test] fleet observed: 4 Pending, eta=0.0");

    // ---- Advance: Pending → Starting ----
    let (status, body) = http_request(addr, "POST", "/advance", None);
    assert_eq!(status, 200);
    let resp: serde_json::Value = serde_json::from_str(&body).expect("invalid JSON");
    assert_eq!(resp["advanced"].as_u64().unwrap(), 4);

    // ---- Advance: Starting → Healthy ----
    let (status, body) = http_request(addr, "POST", "/advance", None);
    assert_eq!(status, 200);
    let resp: serde_json::Value = serde_json::from_str(&body).expect("invalid JSON");
    assert_eq!(resp["advanced"].as_u64().unwrap(), 4);
    eprintln!("[test] advanced 4 agents to Healthy");

    // ---- Observe: 4 Healthy, eta = 0.4 (4 * 0.1) ----
    let (status, body) = http_request(addr, "GET", "/fleet", None);
    assert_eq!(status, 200);
    let fleet: serde_json::Value = serde_json::from_str(&body).expect("invalid JSON");
    assert_eq!(
        fleet["by_kind"]["inference"]["by_state"]["Healthy"]
            .as_u64()
            .unwrap(),
        4
    );
    assert!((fleet["eta"].as_f64().unwrap() - 0.4).abs() < 1e-12);
    eprintln!("[test] fleet observed: 4 Healthy, eta=0.4");

    // ---- Drain agent 0: eta 0.4 → 0.3 (== floor, allowed) ----
    let (status, body) = http_request(addr, "POST", "/drain/0", None);
    assert_eq!(status, 200);
    let outcome: serde_json::Value = serde_json::from_str(&body).expect("invalid JSON");
    assert!(
        outcome["Drained"].is_object(),
        "expected Drained, got: {body}"
    );
    assert_eq!(outcome["Drained"]["id"].as_u64().unwrap(), 0);
    assert!((outcome["Drained"]["new_eta"].as_f64().unwrap() - 0.3).abs() < 1e-12);
    eprintln!("[test] drain agent-0: Drained (eta 0.4 → 0.3)");

    // ---- Drain agent 1: eta 0.3 → 0.2 (< floor, DEFERRED) ----
    let (status, body) = http_request(addr, "POST", "/drain/1", None);
    assert_eq!(status, 200);
    let outcome: serde_json::Value = serde_json::from_str(&body).expect("invalid JSON");
    assert!(
        outcome["Deferred"].is_object(),
        "expected Deferred, got: {body}"
    );
    assert_eq!(outcome["Deferred"]["reason"], "EtaBelowFloor");
    assert!((outcome["Deferred"]["would_be_eta"].as_f64().unwrap() - 0.2).abs() < 1e-12);
    eprintln!("[test] drain agent-1: DEFERRED (eta floor would be breached)");

    // ---- Observe: 1 Draining, 3 Healthy (agent 1 unchanged) ----
    let (status, body) = http_request(addr, "GET", "/fleet", None);
    assert_eq!(status, 200);
    let fleet: serde_json::Value = serde_json::from_str(&body).expect("invalid JSON");
    assert_eq!(
        fleet["by_kind"]["inference"]["by_state"]["Draining"]
            .as_u64()
            .unwrap(),
        1
    );
    assert_eq!(
        fleet["by_kind"]["inference"]["by_state"]["Healthy"]
            .as_u64()
            .unwrap(),
        3
    );
    assert_eq!(fleet["live"].as_u64().unwrap(), 3); // Draining is not live
    eprintln!("[test] fleet observed: 1 Draining, 3 Healthy, live=3");

    // ---- Advance: Draining → Terminated ----
    let (status, body) = http_request(addr, "POST", "/advance", None);
    assert_eq!(status, 200);
    let resp: serde_json::Value = serde_json::from_str(&body).expect("invalid JSON");
    assert_eq!(resp["advanced"].as_u64().unwrap(), 1);
    eprintln!("[test] advanced 1 agent: Draining → Terminated");

    // ---- Observe: 1 Terminated, 3 Healthy, eta = 0.3 ----
    let (status, body) = http_request(addr, "GET", "/fleet", None);
    assert_eq!(status, 200);
    let fleet: serde_json::Value = serde_json::from_str(&body).expect("invalid JSON");
    assert_eq!(
        fleet["by_kind"]["inference"]["by_state"]["Terminated"]
            .as_u64()
            .unwrap(),
        1
    );
    assert_eq!(
        fleet["by_kind"]["inference"]["by_state"]["Healthy"]
            .as_u64()
            .unwrap(),
        3
    );
    assert_eq!(fleet["live"].as_u64().unwrap(), 3);
    assert!((fleet["eta"].as_f64().unwrap() - 0.3).abs() < 1e-12); // 3 active * 0.1
    eprintln!("[test] fleet observed: 1 Terminated, 3 Healthy, eta=0.3");

    // ---- Get individual agent states ----
    let (status, body) = http_request(addr, "GET", "/agent/0", None);
    assert_eq!(status, 200);
    let resp: serde_json::Value = serde_json::from_str(&body).expect("invalid JSON");
    assert_eq!(resp["state"], "Terminated");

    let (status, body) = http_request(addr, "GET", "/agent/1", None);
    assert_eq!(status, 200);
    let resp: serde_json::Value = serde_json::from_str(&body).expect("invalid JSON");
    assert_eq!(resp["state"], "Healthy");

    // ---- 404 for unknown agent ----
    let (status, body) = http_request(addr, "GET", "/agent/999", None);
    assert_eq!(status, 404);
    assert!(body.contains(r#""error":"agent not found""#));

    // ---- 404 for unknown path ----
    let (status, _) = http_request(addr, "GET", "/nonexistent", None);
    assert_eq!(status, 404);

    eprintln!("[test] ✅ full lifecycle over real TCP socket: all assertions passed");
}

// ---------------------------------------------------------------------------
// Test 2: Two real OS processes coordinating over a real socket
// ---------------------------------------------------------------------------

#[test]
fn two_process_coordination_via_subprocess() {
    use std::io::BufRead;
    use std::process::{Command, Stdio};

    // Construct the path to the server binary built by `cargo test`.
    let bin_path = format!(
        "{}/target/debug/conductor-server{}",
        env!("CARGO_MANIFEST_DIR"),
        std::env::consts::EXE_SUFFIX
    );

    // Start the server as a genuine separate OS process.
    let mut child = Command::new(&bin_path)
        .arg("127.0.0.1:0")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn {bin_path}: {e}"));

    // Read the "listening on ADDR" line that the binary prints before serve().
    let stdout = child.stdout.take().expect("no stdout handle");
    let mut reader = std::io::BufReader::new(stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("failed to read server output");

    let addr: SocketAddr = line
        .split_whitespace()
        .last()
        .expect("no address in output")
        .parse()
        .expect("failed to parse address");

    eprintln!(
        "[test] server subprocess PID {} listening on {addr}",
        child.id()
    );

    // We are now a genuinely separate OS process talking to the server over TCP.
    // No shared memory, no in-process calls — pure network I/O.

    // Register 2 inference agents with a permissive conservation config.
    let spec = r#"{"agents":[{"kind":"inference","count":2,"layer":0}],"conservation":{"gamma_min":-1.0,"gamma_max":1.0,"eta_floor":0.0}}"#;
    let (status, body) = http_request(addr, "POST", "/reconcile", Some(spec));
    assert_eq!(status, 200, "reconcile failed: {body}");

    // Advance to Healthy.
    let _ = http_request(addr, "POST", "/advance", None);
    let _ = http_request(addr, "POST", "/advance", None);

    // Observe over the real socket.
    let (status, body) = http_request(addr, "GET", "/fleet", None);
    assert_eq!(status, 200);
    let fleet: serde_json::Value = serde_json::from_str(&body).expect("invalid JSON");
    assert_eq!(fleet["total"].as_u64().unwrap(), 2);
    assert_eq!(
        fleet["by_kind"]["inference"]["by_state"]["Healthy"]
            .as_u64()
            .unwrap(),
        2
    );

    // Drain agent 0 (permissive config → always allowed).
    let (status, body) = http_request(addr, "POST", "/drain/0", None);
    assert_eq!(status, 200);
    let outcome: serde_json::Value = serde_json::from_str(&body).expect("invalid JSON");
    assert!(outcome["Drained"].is_object(), "expected Drained: {body}");

    eprintln!("[test] ✅ two-process coordination over real socket: all assertions passed");

    // Clean up.
    let _ = child.kill();
    let _ = child.wait();
}
