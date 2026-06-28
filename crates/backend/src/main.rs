//! Lattice backend binary entry point.
//!
//! Parses a repository path from the first CLI argument (default `.`), starts the
//! backend via [`lattice_backend::run`], prints the bound WebSocket address, and
//! runs until Ctrl-C.

use std::net::SocketAddr;
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let addr: SocketAddr = match "127.0.0.1:0".parse() {
        Ok(addr) => addr,
        Err(error) => {
            eprintln!("lattice: invalid listen address: {error}");
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
