//! Serve the application under test in-process, over real TCP.
//!
//! Tests go through the full ingress path (socket, HTTP parsing, middleware,
//! routing, extractors) exactly like an external client. Nothing calls handlers
//! directly. Each server binds an ephemeral port on 127.0.0.1, so many servers
//! (one per suite or per test) can run side by side.

use std::future::Future;
use std::net::SocketAddr;

use anyhow::{Context as _, Result};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// A running in-process server. Shuts down gracefully on [`TestServer::shutdown`] or drop.
pub struct TestServer {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<()>>>,
}

/// Resolves when the server should stop.
pub type ShutdownSignal = std::pin::Pin<Box<dyn Future<Output = ()> + Send>>;

impl TestServer {
    /// Framework-agnostic: you get a bound listener and a shutdown signal, and return
    /// the serve future. Works for axum, actix (via `from_std`), hyper, poem, ...
    ///
    /// ```ignore
    /// let srv = TestServer::spawn(|listener, shutdown| async move {
    ///     axum::serve(listener, app()).with_graceful_shutdown(shutdown).await?;
    ///     Ok(())
    /// }).await?;
    /// ```
    pub async fn spawn<F, Fut>(serve: F) -> Result<TestServer>
    where
        F: FnOnce(TcpListener, ShutdownSignal) -> Fut,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("binding test server")?;
        let addr = listener.local_addr()?;
        let (tx, rx) = oneshot::channel::<()>();
        let signal: ShutdownSignal = Box::pin(async move {
            let _ = rx.await;
        });
        let task = tokio::spawn(serve(listener, signal));
        Ok(TestServer {
            addr,
            shutdown: Some(tx),
            task: Some(task),
        })
    }

    /// Serves an axum `Router`.
    #[cfg(feature = "axum")]
    pub async fn axum(router: axum::Router) -> Result<TestServer> {
        TestServer::spawn(|listener, shutdown| async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown)
                .await?;
            Ok(())
        })
        .await
    }

    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `http://127.0.0.1:<port>`; use as [`crate::RunConfig::base_url`].
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Stops accepting connections and waits for in-flight requests to finish.
    pub async fn shutdown(mut self) -> Result<()> {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        match self.task.take() {
            Some(t) => t.await.context("server task panicked")?,
            None => Ok(()),
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}
