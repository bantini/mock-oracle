//! Speaks Oracle's TNS/TTC wire protocol so node-oracledb (Thin mode) and other
//! thin drivers can connect to a [`mock_oracle_core::Database`].

mod auth;
mod oranum;
mod session;
mod tns;
mod ttc;

use std::net::SocketAddr;
use std::sync::Arc;

use mock_oracle_core::Database;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Server settings.
#[derive(Debug, Clone)]
pub struct Config {
    /// The password every user logs in with. Any user name is accepted.
    pub password: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            password: DEFAULT_PASSWORD.into(),
        }
    }
}

pub const DEFAULT_PASSWORD: &str = "oracle";

/// A running server. Dropping the handle does not stop it; call [`Server::stop`].
pub struct Server {
    addr: SocketAddr,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl Server {
    /// Binds `addr` (port 0 picks a free port) and starts accepting connections.
    pub async fn start(
        addr: SocketAddr,
        db: Arc<Database>,
        config: Config,
    ) -> std::io::Result<Self> {
        let config = Arc::new(config);
        let listener = TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        let (shutdown, mut stop) = oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stop => break,
                    accepted = listener.accept() => match accepted {
                        Ok((socket, peer)) => {
                            tracing::debug!(%peer, "connection accepted");
                            let _ = socket.set_nodelay(true);
                            let session = session::Session::new(socket, Arc::clone(&db), Arc::clone(&config));
                            tokio::spawn(async move {
                                if let Err(err) = session.run().await {
                                    tracing::warn!(%peer, %err, "connection ended with an error");
                                }
                            });
                        }
                        Err(err) => tracing::warn!(%err, "accept failed"),
                    },
                }
            }
        });
        Ok(Self {
            addr,
            shutdown,
            task,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// The connect string to hand to a driver, e.g. `localhost:1521/FREEPDB1`.
    pub fn connect_string(&self) -> String {
        format!("localhost:{}/FREEPDB1", self.addr.port())
    }

    pub async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = self.task.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn starts_on_a_free_port_and_stops() {
        let server = Server::start(
            "127.0.0.1:0".parse().unwrap(),
            Arc::new(Database::new()),
            Config::default(),
        )
        .await
        .unwrap();
        assert_ne!(server.local_addr().port(), 0);
        assert!(server.connect_string().ends_with("/FREEPDB1"));
        server.stop().await;
    }
}
