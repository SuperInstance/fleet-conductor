//! Standalone binary entry point for the fleet-conductor HTTP server.
//!
//! Run with:
//! ```bash
//! cargo run --bin conductor-server -- 127.0.0.1:7878
//! ```
//!
//! If no address is given, defaults to `127.0.0.1:7878`.

use std::io::Write;

use fleet_conductor::ConductorServer;

fn main() {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:7878".to_string());

    let server = ConductorServer::new(&addr).unwrap_or_else(|e| {
        eprintln!("failed to bind to {addr}: {e}");
        std::process::exit(1);
    });

    println!(
        "fleet-conductor server listening on {}",
        server.local_addr()
    );
    let _ = std::io::stdout().flush();

    server.serve();
}
