//! The Gmail API over a [`Mailbox`] that another crate keeps in step with a real store (an IMAP
//! server, say): the mock's mail routes and nothing else, with no OAuth, quota, faults, calendar,
//! people or `/__mock/` admin endpoints.

use std::sync::Arc;

use axum::Router;
use axum::extract::Request;
use axum::middleware::{Next, from_fn};
use axum::response::Response;

pub use crate::mail::history::{Change, HistoryLog, Record};
pub use crate::mail::{Mailbox, NewMessage, Restored, StoredMessage, Threading};
use crate::oauth::Identity;
use crate::world::{App, Config, World};

/// A mailbox served as Gmail v1. Cloning shares the mailbox.
#[derive(Clone)]
pub struct Engine {
    app: Arc<App>,
}

impl Engine {
    pub fn new(mailbox: Mailbox) -> Self {
        // The world's other halves are never reachable through `router`, so the cheapest world
        // to build around the mailbox will do.
        let mut world: World = crate::fixtures::world(&Config::default());
        world.mail = mailbox;
        Self {
            app: Arc::new(App::from_world(world)),
        }
    }

    /// Runs `f` on the mailbox, which no request is using meanwhile.
    pub fn with<R>(&self, f: impl FnOnce(&mut Mailbox) -> R) -> R {
        f(&mut self.app.lock().mail)
    }

    /// Gmail's paths (`/gmail/v1/users/me/...`). Every request is the mailbox owner's, so the
    /// caller authenticates them.
    pub fn router(&self) -> Router {
        crate::mail::routes::router(self.app.clone())
            .layer(from_fn(as_owner))
            .layer(from_fn(crate::http::format_json))
            .with_state(self.app.clone())
    }
}

async fn as_owner(mut request: Request, next: Next) -> Response {
    request.extensions_mut().insert(Identity::default());
    next.run(request).await
}
