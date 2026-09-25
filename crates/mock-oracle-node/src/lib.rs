//! `require('mock-oracle')`: starts the mock inside the Node process so
//! node-oracledb can connect to `db.connectString`.

use std::net::SocketAddr;
use std::sync::Arc;

use mock_oracle_core::Database;
use mock_oracle_server::Server;
use napi::bindgen_prelude::*;
use napi_derive::napi;
use tokio::sync::Mutex;

#[napi(object)]
pub struct StartOptions {
    /// Port to listen on; 0 (the default) picks a free one.
    pub port: Option<u32>,
}

#[napi]
pub struct MockOracle {
    server: Mutex<Option<Server>>,
    connect_string: String,
}

#[napi]
impl MockOracle {
    #[napi(factory)]
    pub async fn start(options: Option<StartOptions>) -> Result<MockOracle> {
        let port = options.and_then(|o| o.port).unwrap_or(0);
        let port = u16::try_from(port).map_err(|_| Error::from_reason("port must be 0-65535"))?;
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let server = Server::start(addr, Arc::new(Database::new()))
            .await
            .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(MockOracle {
            connect_string: server.connect_string(),
            server: Mutex::new(Some(server)),
        })
    }

    #[napi(getter)]
    pub fn connect_string(&self) -> String {
        self.connect_string.clone()
    }

    #[napi]
    pub async fn stop(&self) -> Result<()> {
        if let Some(server) = self.server.lock().await.take() {
            server.stop().await;
        }
        Ok(())
    }
}
