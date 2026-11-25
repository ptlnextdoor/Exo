#![deny(
    clippy::correctness,
    clippy::suspicious,
    clippy::complexity,
    clippy::perf
)]
//! A mock of the Google APIs Exo uses (Gmail v1, Calendar v3, People v1, OAuth 2.0), serving
//! Google's paths and wire format from one origin, backed by in-memory state seeded from a
//! deterministic fixture mailbox.
//!
//! ```no_run
//! # async fn demo() -> std::io::Result<()> {
//! let server = gmail_mock::MockServer::start(gmail_mock::Config::default()).await?;
//! let client = gmail::Client::new(
//!     server.endpoints(),
//!     gmail::StaticToken(gmail_mock::DEV_ACCESS_TOKEN.into()),
//! );
//! # Ok(()) }
//! ```

mod admin;
mod calendar;
pub mod engine;
mod error;
mod fixtures;
mod http;
mod mail;
mod oauth;
mod people;
mod world;

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::task::JoinHandle;

pub use oauth::DEV_ACCESS_TOKEN;
pub use world::{Config, parse_time};

/// The mock as an axum router, for embedding in another server.
pub fn router(config: &Config) -> axum::Router {
    http::router(Arc::new(world::App::new(config)))
}

/// Serves the mock on `listener` until the task is dropped or fails.
///
/// # Errors
/// Fails when the listener does.
pub async fn serve(listener: TcpListener, config: &Config) -> std::io::Result<()> {
    let config = served_at(config, listener.local_addr()?);
    axum::serve(listener, router(&config)).await
}

/// `config`, served at `address` unless it says where it's served.
fn served_at(config: &Config, address: SocketAddr) -> Config {
    let mut config = config.clone();
    config.origin.get_or_insert_with(|| {
        url::Url::parse(&format!("http://{address}/")).expect("socket addresses form URLs")
    });
    config
}

/// A mock running in the background, for tests. Stops when dropped.
pub struct MockServer {
    address: SocketAddr,
    router: axum::Router,
    /// `None` while [disconnected](Self::disconnect).
    serving: Option<Serving>,
}

impl MockServer {
    /// Starts the mock on an ephemeral port on 127.0.0.1.
    ///
    /// # Errors
    /// Fails if the port can't be bound.
    pub async fn start(config: Config) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let router = router(&served_at(&config, address));
        Ok(Self {
            address,
            serving: Some(Serving::start(listener, router.clone())),
            router,
        })
    }

    /// The origin every API is served from, e.g. `http://127.0.0.1:52011/`.
    pub fn url(&self) -> url::Url {
        url::Url::parse(&format!("http://{}/", self.address)).expect("socket addresses form URLs")
    }

    /// Client endpoints pointing at this server.
    pub fn endpoints(&self) -> gmail::Endpoints {
        gmail::Endpoints::mock(&self.url())
    }

    /// Goes away, as Google does when the network drops. The port closes, and so does every
    /// connection to it once its request is answered, so what's sent meanwhile gets no answer.
    /// The mailbox stays as it is.
    pub async fn disconnect(&mut self) {
        if let Some(serving) = self.serving.take() {
            serving.stop().await;
        }
    }

    /// Comes back after [`disconnect`](Self::disconnect), on the same port, with the mailbox as
    /// it was.
    ///
    /// # Errors
    /// Fails if the port can't be bound again.
    pub async fn reconnect(&mut self) -> std::io::Result<()> {
        if self.serving.is_none() {
            let listener = TcpListener::bind(self.address).await?;
            self.serving = Some(Serving::start(listener, self.router.clone()));
        }
        Ok(())
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        if let Some(serving) = &self.serving {
            serving.task.abort();
        }
    }
}

/// The mock answering on a listener until told to stop.
struct Serving {
    stop: tokio::sync::oneshot::Sender<()>,
    task: JoinHandle<std::io::Result<()>>,
}

impl Serving {
    fn start(listener: TcpListener, router: axum::Router) -> Self {
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
        });
        Self { stop, task }
    }

    /// Stops listening, closes idle connections and waits for the rest to finish.
    async fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.task.await;
    }
}
