//! Speaks Oracle's TNS/TTC wire protocol so node-oracledb (Thin mode) and other
//! thin drivers can connect. The protocol is not implemented yet: connections are
//! accepted and closed.

use std::net::SocketAddr;
use std::sync::Arc;

use mock_oracle_core::Database;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// A running server. Dropping the handle does not stop it; call [`Server::stop`].
pub struct Server {
    addr: SocketAddr,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl Server {
    /// Binds `addr` (port 0 picks a free port) and starts accepting connections.
    pub async fn start(addr: SocketAddr, db: Arc<Database>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        let (shutdown, mut stop) = oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stop => break,
                    accepted = listener.accept() => match accepted {
                        Ok((socket, peer)) => {
                            let _db = Arc::clone(&db);
                            tracing::info!(%peer, "connection accepted; TNS not implemented yet, closing");
                            drop(socket);
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
        let server = Server::start("127.0.0.1:0".parse().unwrap(), Arc::new(Database::new()))
            .await
            .unwrap();
        assert_ne!(server.local_addr().port(), 0);
        assert!(server.connect_string().ends_with("/FREEPDB1"));
        server.stop().await;
    }
}
