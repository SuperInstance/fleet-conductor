//! HTTP server exposing the in-memory [`Conductor`] over a real TCP socket.
//!
//! This module adds a thin network layer on top of the existing orchestration
//! core. It does not modify the conductor's logic — it wraps it behind a
//! minimal HTTP/1.1 server built on `std::net::TcpListener`, allowing two
//! separate OS processes (or threads) to coordinate fleet state through a
//! real network interface.
//!
//! ## Endpoints
//!
//! | Method | Path         | Body             | Action                          |
//! |--------|--------------|------------------|---------------------------------|
//! | GET    | `/health`    | —                | Liveness probe.                 |
//! | GET    | `/fleet`     | —                | Observe current fleet state.    |
//! | POST   | `/reconcile` | `FleetSpec` JSON | Reconcile toward desired state. |
//! | POST   | `/advance`   | —                | Advance all agent lifecycles.   |
//! | POST   | `/drain/:id` | —                | Drain an agent (guard-gated).   |
//! | GET    | `/agent/:id` | —                | Get an agent's current state.   |
//!
//! All responses are JSON. The server is synchronous (thread-per-connection)
//! and has no async runtime dependency, matching the existing crate's style.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use crate::{AgentId, Conductor, FleetSpec};

/// A minimal HTTP server wrapping a [`Conductor`] behind a real TCP socket.
///
/// Created with [`ConductorServer::new`] or
/// [`ConductorServer::with_conductor`].  Call [`ConductorServer::serve`] to
/// start accepting connections (blocks the calling thread).
pub struct ConductorServer {
    conductor: Arc<Mutex<Conductor>>,
    listener: TcpListener,
}

impl ConductorServer {
    /// Create a new server bound to `addr` with a fresh default
    /// [`Conductor`].
    ///
    /// Use `"127.0.0.1:0"` to let the OS assign an available port; retrieve
    /// it with [`Self::local_addr`].
    pub fn new(addr: &str) -> std::io::Result<Self> {
        Ok(Self {
            conductor: Arc::new(Mutex::new(Conductor::new())),
            listener: TcpListener::bind(addr)?,
        })
    }

    /// Create a new server with a pre-configured [`Conductor`].
    pub fn with_conductor(addr: &str, conductor: Conductor) -> std::io::Result<Self> {
        Ok(Self {
            conductor: Arc::new(Mutex::new(conductor)),
            listener: TcpListener::bind(addr)?,
        })
    }

    /// The actual address the server is listening on.
    pub fn local_addr(&self) -> SocketAddr {
        self.listener.local_addr().expect("listener is bound")
    }

    /// Accept connections forever, handling each in its own thread.
    ///
    /// Blocks the calling thread.  For integration tests, spawn this in a
    /// background thread and connect as a separate client.
    pub fn serve(&self) {
        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => {
                    let conductor = Arc::clone(&self.conductor);
                    std::thread::spawn(move || {
                        let _ = handle_connection(stream, conductor);
                    });
                }
                Err(e) => eprintln!("accept error: {e}"),
            }
        }
    }
}

// ===========================================================================
// HTTP parsing (minimal — just enough for the endpoints above)
// ===========================================================================

/// A parsed HTTP request (only the parts we need).
struct HttpRequest {
    method: String,
    path: String,
    body: String,
}

fn parse_request(stream: &mut TcpStream) -> std::io::Result<HttpRequest> {
    let mut reader = BufReader::new(stream);

    // Request line: "METHOD /path HTTP/1.1"
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "malformed request line",
        ));
    }
    let method = parts[0].to_string();
    let path = parts[1].to_string();

    // Headers — we only need Content-Length.
    let mut content_length: usize = 0;
    loop {
        let mut header = String::new();
        let n = reader.read_line(&mut header)?;
        if n == 0 {
            break; // EOF before blank line
        }
        let header = header.trim();
        if header.is_empty() {
            break; // blank line = end of headers
        }
        // Case-insensitive prefix match.
        let lower = header.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }

    // Body (if any).
    let mut body = String::new();
    if content_length > 0 {
        reader
            .take(content_length as u64)
            .read_to_string(&mut body)?;
    }

    Ok(HttpRequest { method, path, body })
}

fn write_response(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        len = body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

// ===========================================================================
// Routing
// ===========================================================================

fn handle_connection(
    mut stream: TcpStream,
    conductor: Arc<Mutex<Conductor>>,
) -> std::io::Result<()> {
    let req = match parse_request(&mut stream) {
        Ok(r) => r,
        Err(_) => return Ok(()), // client disconnected or sent garbage
    };

    let (status, body) = route(&req, &conductor);
    write_response(&mut stream, status, &body)
}

fn route(req: &HttpRequest, conductor: &Arc<Mutex<Conductor>>) -> (u16, String) {
    match (req.method.as_str(), req.path.as_str()) {
        // --- Health ---
        ("GET", "/health") => (200, r#"{"status":"ok"}"#.to_string()),

        // --- Observe fleet ---
        ("GET", "/fleet") => {
            let c = conductor.lock().unwrap();
            let state = c.observe();
            json_ok(&state)
        }

        // --- Reconcile toward desired state ---
        ("POST", "/reconcile") => match serde_json::from_str::<FleetSpec>(&req.body) {
            Ok(spec) => {
                let mut c = conductor.lock().unwrap();
                c.reconcile(spec);
                (200, r#"{"status":"reconciled"}"#.to_string())
            }
            Err(e) => (400, format!(r#"{{"error":"invalid FleetSpec: {e}"}}"#)),
        },

        // --- Advance all agent lifecycles ---
        ("POST", "/advance") => {
            let mut c = conductor.lock().unwrap();
            let n = c.advance_lifecycle();
            (200, format!(r#"{{"advanced":{n}}}"#))
        }

        // --- Drain an agent (conservation-guarded) ---
        ("POST", path) if path.starts_with("/drain/") => {
            let id_str = &path["/drain/".len()..];
            match id_str.parse::<u64>() {
                Ok(id) => {
                    let mut c = conductor.lock().unwrap();
                    let outcome = c.drain_agent(AgentId(id));
                    json_ok(&outcome)
                }
                Err(_) => (400, r#"{"error":"invalid agent id"}"#.to_string()),
            }
        }

        // --- Get a single agent's state ---
        ("GET", path) if path.starts_with("/agent/") => {
            let id_str = &path["/agent/".len()..];
            match id_str.parse::<u64>() {
                Ok(id) => {
                    let c = conductor.lock().unwrap();
                    match c.agent_state(AgentId(id)) {
                        Some(state) => (200, format!(r#"{{"state":"{state}"}}"#)),
                        None => (404, r#"{"error":"agent not found"}"#.to_string()),
                    }
                }
                Err(_) => (400, r#"{"error":"invalid agent id"}"#.to_string()),
            }
        }

        // --- Not found ---
        _ => (404, r#"{"error":"not found"}"#.to_string()),
    }
}

/// Serialise a `T: Serialize` to JSON, returning `(200, json)` or
/// `(500, error_json)`.
fn json_ok<T: serde::Serialize>(value: &T) -> (u16, String) {
    match serde_json::to_string(value) {
        Ok(s) => (200, s),
        Err(e) => (500, format!(r#"{{"error":"serialize error: {e}"}}"#)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the server binds to a real port and responds to /health.
    #[test]
    fn server_binds_and_serves_health() {
        let server = ConductorServer::new("127.0.0.1:0").unwrap();
        let addr = server.local_addr();
        std::thread::spawn(move || server.serve());

        // Give the listener a moment to enter its accept loop.
        std::thread::sleep(std::time::Duration::from_millis(50));

        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.contains("200 OK"));
        assert!(response.contains(r#""status":"ok""#));
    }
}
