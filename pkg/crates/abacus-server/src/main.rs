//! Abacus HTTP Server — binary entry point
//!
//! ## Usage
//! ```bash
//! cargo run --bin abacus-server
//! ABACUS_SERVER_TOKEN=secret cargo run --bin abacus-server
//! ```

use std::net::SocketAddr;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    abacus_server::logging::init();

    let addr = std::env::var("ABACUS_SERVER_ADDR")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 8080)));

    let server = abacus_server::AbacusServer::new(addr).await;
    server.serve().await
}
