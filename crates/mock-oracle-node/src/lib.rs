//! `require('mock-oracle')`: starts the mock inside the Node process so
//! node-oracledb can connect to `db.connectString`.

use std::net::SocketAddr;
use std::sync::Arc;

use mock_oracle_core::{Database, Snapshot as CoreSnapshot};
use mock_oracle_server::{Config, Server, DEFAULT_PASSWORD};
use napi::bindgen_prelude::*;
use napi_derive::napi;
use tokio::sync::Mutex;

#[napi(object)]
pub struct StartOptions {
    /// Port to listen on; 0 (the default) picks a free one.
    pub port: Option<u32>,
    /// Password for every user; defaults to "oracle".
    pub password: Option<String>,
    /// SQL run before the first connection, for example CREATE TABLE and INSERT
    /// statements. Statements are separated by `;` or a line holding only `/`.
    pub seed: Option<String>,
}

/// A saved copy of every table, from [`MockOracle::snapshot`].
#[napi]
pub struct Snapshot {
    inner: CoreSnapshot,
}

#[napi]
pub struct MockOracle {
    server: Mutex<Option<Server>>,
    connect_string: String,
    db: Arc<Database>,
    /// The state right after seeding, restored by `restore()` without an argument.
    initial: CoreSnapshot,
}

fn sql_error(e: mock_oracle_core::OraError) -> Error {
    Error::from_reason(e.to_string())
}

#[napi]
impl MockOracle {
    #[napi(factory)]
    pub async fn start(options: Option<StartOptions>) -> Result<MockOracle> {
        let (port, password, seed) = match options {
            Some(o) => (o.port.unwrap_or(0), o.password, o.seed),
            None => (0, None, None),
        };
        let db = Arc::new(Database::new());
        if let Some(seed) = seed {
            db.run_script(&seed).map_err(sql_error)?;
        }
        let initial = db.snapshot();
        let config = Config {
            password: password.unwrap_or_else(|| DEFAULT_PASSWORD.into()),
        };
        let port = u16::try_from(port).map_err(|_| Error::from_reason("port must be 0-65535"))?;
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let server = Server::start(addr, Arc::clone(&db), config)
            .await
            .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(MockOracle {
            connect_string: server.connect_string(),
            server: Mutex::new(Some(server)),
            db,
            initial,
        })
    }

    #[napi(getter)]
    pub fn connect_string(&self) -> String {
        self.connect_string.clone()
    }

    /// Runs SQL statements (separated by `;` or `/`) and commits them.
    #[napi]
    pub fn run_script(&self, sql: String) -> Result<()> {
        self.db.run_script(&sql).map_err(sql_error)
    }

    /// Captures the committed contents of every table.
    #[napi]
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            inner: self.db.snapshot(),
        }
    }

    /// Puts every table back as it was in `snapshot`, or as it was right after
    /// start (and seeding) when no snapshot is given. Uncommitted work in open
    /// connections is not discarded.
    #[napi]
    pub fn restore(&self, snapshot: Option<&Snapshot>) {
        self.db
            .restore(snapshot.map_or(&self.initial, |s| &s.inner));
    }

    #[napi]
    pub async fn stop(&self) -> Result<()> {
        if let Some(server) = self.server.lock().await.take() {
            server.stop().await;
        }
        Ok(())
    }
}
