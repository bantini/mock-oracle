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
    let db = Arc::new(Database::new());
    for file in seed_files()? {
        let sql = std::fs::read_to_string(&file)?;
        db.run_script(&sql)
            .map_err(|e| std::io::Error::other(format!("{}: {e}", file.display())))?;
        tracing::info!(file = %file.display(), "ran seed SQL");
    }
    let server = Server::start(addr, db, Config { password }).await?;
    tracing::info!(addr = %server.local_addr(), "mock-oracle listening");

    tokio::signal::ctrl_c().await?;
    server.stop().await;
    Ok(())
}

/// SQL files to run at startup: `MOCK_ORACLE_SEED` (a file, or a directory whose
/// `.sql` files run in name order), else `/docker-entrypoint-initdb.d` if it exists.
fn seed_files() -> std::io::Result<Vec<std::path::PathBuf>> {
    let path = match std::env::var_os("MOCK_ORACLE_SEED") {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let default = std::path::PathBuf::from("/docker-entrypoint-initdb.d");
            if !default.is_dir() {
                return Ok(Vec::new());
            }
            default
        }
    };
    if path.is_file() {
        return Ok(vec![path]);
    }
    let mut files: Vec<_> = std::fs::read_dir(&path)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("sql")))
        .collect();
    files.sort();
    Ok(files)
}
