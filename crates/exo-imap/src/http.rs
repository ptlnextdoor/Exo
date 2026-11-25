//! The facade's HTTP face: Gmail's paths, behind the guard that keeps other local software, and
//! web pages, out.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tower::ServiceBuilder;

use crate::facade::Facade;
use crate::mailstore::Mailstore;

/// How long `profile` and `history.list` wait for the server before answering from what the
/// facade has.
const SYNC_BUDGET: Duration = Duration::from_secs(2);

/// Google's error envelope.
fn failure(code: StatusCode, status: &str, message: &str) -> Response {
    let body = json!({ "error": { "code": code.as_u16(), "message": message, "status": status } });
    (code, axum::Json(body)).into_response()
}

/// `a == b` in time that depends on their length only.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |diff, (a, b)| diff | (a ^ b)) == 0
}

/// The Gmail API of `facade` for the client that holds `token`.
///
/// Reads only: a request that would change the mailbox gets 403 until writes reach the server.
pub fn router<S: Mailstore + 'static>(facade: Arc<Facade<S>>, token: &str) -> Router {
    let token: Arc<str> = token.into();
    facade.engine.router().layer(
        ServiceBuilder::new()
            .layer(from_fn_with_state(token, guard))
            .layer(from_fn_with_state(facade.clone(), sync_first))
            .layer(from_fn_with_state(facade, load_bodies)),
    )
}

/// Only the app, and only to read.
///
/// A page in Exo's own mail view can reach loopback, so the token is what keeps it out of the
/// mailbox, and a browser request (which carries `Origin`) is refused even with the token.
async fn guard(State(token): State<Arc<str>>, request: Request, next: Next) -> Response {
    if request.headers().contains_key(header::ORIGIN) {
        return failure(
            StatusCode::FORBIDDEN,
            "PERMISSION_DENIED",
            "Cross-origin requests are refused.",
        );
    }
    let given = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if !given.is_some_and(|given| constant_time_eq(given.as_bytes(), token.as_bytes())) {
        return failure(
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
            "Invalid credentials.",
        );
    }
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return failure(
            StatusCode::FORBIDDEN,
            "PERMISSION_DENIED",
            "This mailbox is read-only for now.",
        );
    }
    next.run(request).await
}

/// Exo asks `profile` and `history.list` to learn what changed, so that is when to ask the server.
async fn sync_first<S: Mailstore + 'static>(
    State(facade): State<Arc<Facade<S>>>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    if path.ends_with("/history") || path.ends_with("/profile") {
        match tokio::time::timeout(SYNC_BUDGET, facade.sync_if_stale()).await {
            Ok(Ok(report)) if report.changed() => log::info!("synced: {report:?}"),
            Ok(Ok(_)) => {}
            Ok(Err(error)) => log::warn!("sync failed: {error:#}"),
            Err(_) => log::warn!("sync took over {SYNC_BUDGET:?}; answering from what is indexed"),
        }
    }
    next.run(request).await
}

/// Reads of a message's text, its thread's, or an attachment's need the whole message, which the
/// facade holds back until asked.
async fn load_bodies<S: Mailstore + 'static>(
    State(facade): State<Arc<Facade<S>>>,
    request: Request,
    next: Next,
) -> Response {
    let wants_text = !request
        .uri()
        .query()
        .unwrap_or_default()
        .split('&')
        .any(|pair| matches!(pair, "format=metadata" | "format=minimal"));
    let segments: Vec<&str> = request
        .uri()
        .path()
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    // gmail / v1 / users / {user} / ...
    let ids = match segments.get(4..) {
        Some(["messages", id, ..]) if wants_text || segments.len() > 6 => vec![(*id).to_owned()],
        Some(["threads", id]) if wants_text => facade
            .engine
            .with(|mailbox| mailbox.thread_message_ids(id).unwrap_or_default()),
        _ => Vec::new(),
    };
    if let Err(error) = facade.load_bodies(&ids).await {
        log::warn!("fetching message text failed: {error:#}");
        return failure(
            StatusCode::BAD_GATEWAY,
            "UNAVAILABLE",
            "The mail server did not answer.",
        );
    }
    next.run(request).await
}
