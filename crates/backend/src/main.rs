//! Lattice backend binary entry point.
//!
//! Parses a repository path from the first CLI argument (default `.`), starts the
//! backend via [`lattice_backend::run`] on a fixed WebSocket address (default
//! `127.0.0.1:7000`, override with the `LATTICE_ADDR` env var), prints the bound
//! address, and runs until Ctrl-C.

use std::path::PathBuf;

#[tokio::main]
async fn main() {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let raw_addr = std::env::var("LATTICE_ADDR").ok();
    let addr = match lattice_backend::app::resolve_listen_addr(raw_addr.as_deref()) {
        Ok(addr) => addr,
        Err(message) => {
            eprintln!("lattice: {message}");
            return;
        }
    };

    let handle = match lattice_backend::run(root, addr).await {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("lattice: failed to start: {error}");
            return;
        }
    };

    println!("lattice listening on {}", handle.addr);

    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("lattice: signal error: {error}");
    }
    handle.shutdown().await;
}
