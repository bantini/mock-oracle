use std::net::SocketAddr;
use std::sync::Arc;

use mock_oracle_core::Database;
use mock_oracle_server::{Config, Server, DEFAULT_PASSWORD};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let addr: SocketAddr = std::env::var("MOCK_ORACLE_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:1521".into())
        .parse()
        .expect("MOCK_ORACLE_ADDR must be host:port");

    let password =
        std::env::var("MOCK_ORACLE_PASSWORD").unwrap_or_else(|_| DEFAULT_PASSWORD.into());
    let server = Server::start(addr, Arc::new(Database::new()), Config { password }).await?;
    tracing::info!(addr = %server.local_addr(), "mock-oracle listening");

    tokio::signal::ctrl_c().await?;
    server.stop().await;
    Ok(())
}
