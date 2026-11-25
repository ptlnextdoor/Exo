//! The facade's HTTP face: Gmail's paths, behind the guard that keeps other local software, and
//! web pages, out.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tower::ServiceBuilder;

use crate::facade::{Draft, Facade, Failed};
use crate::mailstore::Mailstore;

/// Largest JSON body of a write the facade reads (a modify or a batch of IDs).
const JSON_LIMIT: usize = 4 << 20;

/// How long a list request waits for the server's text search.
const SEARCH_BUDGET: Duration = Duration::from_secs(10);

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
/// A request that changes the mailbox reaches the server first; what the facade cannot do
/// (labels that are not folders, filters, settings) is refused with 403.
pub fn router<S: Mailstore + 'static>(facade: Arc<Facade<S>>, token: &str) -> Router {
    let token: Arc<str> = token.into();
    facade.engine.router().layer(
        ServiceBuilder::new()
            .layer(from_fn_with_state(token, guard))
            .layer(from_fn_with_state(facade.clone(), sync_first))
            .layer(from_fn_with_state(facade.clone(), load_bodies))
            .layer(from_fn_with_state(facade, writes)),
    )
}

/// Only the app.
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
    if request.method() != Method::GET {
        return next.run(request).await;
    }
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
        Some(["messages"] | ["threads"]) => {
            let query =
                url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
                    .find_map(|(name, value)| (name == "q").then(|| value.into_owned()));
            match query {
                Some(query) => {
                    match tokio::time::timeout(SEARCH_BUDGET, facade.text_hits(&query)).await {
                        Ok(Ok(ids)) => ids,
                        Ok(Err(error)) => {
                            log::warn!("server search failed: {error:#}");
                            Vec::new()
                        }
                        Err(_) => {
                            log::warn!("server search took over {SEARCH_BUDGET:?}");
                            Vec::new()
                        }
                    }
                }
                None => Vec::new(),
            }
        }
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

/// What a write is about.
enum Target {
    Message(String),
    Thread(String),
}

enum Write {
    Modify(Target),
    BatchModify,
    Trash(Target),
    Untrash(Target),
    Delete(Target),
    BatchDelete,
}

fn unsupported() -> Response {
    failure(
        StatusCode::FORBIDDEN,
        "PERMISSION_DENIED",
        "This mailbox does not support that.",
    )
}

fn refused(failed: Failed) -> Response {
    match failed {
        Failed::Request(message) => failure(StatusCode::BAD_REQUEST, "INVALID_ARGUMENT", &message),
        Failed::Server(error) => {
            log::warn!("the mail server refused a change: {error:#}");
            failure(
                StatusCode::BAD_GATEWAY,
                "UNAVAILABLE",
                "The mail server did not accept the change.",
            )
        }
    }
}

/// Changes to messages go to the server before the engine hears of them: labels as flags and
/// folders, deletes, and sends. Drafts stay in the facade.
async fn writes<S: Mailstore + 'static>(
    State(facade): State<Arc<Facade<S>>>,
    request: Request,
    next: Next,
) -> Response {
    if matches!(*request.method(), Method::GET | Method::HEAD) {
        return next.run(request).await;
    }
    let path = request.uri().path().to_owned();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // [upload /] gmail / v1 / users / {user} / ...
    let Some(at) = segments.iter().position(|s| *s == "users") else {
        return unsupported();
    };
    let rest = segments.get(at + 2..).unwrap_or_default();
    let message = |id: &&str| Target::Message((*id).to_owned());
    let thread = |id: &&str| Target::Thread((*id).to_owned());
    let write = match (request.method().clone(), rest) {
        (Method::POST, ["messages", "send"]) => return send(facade, request, next, false).await,
        (Method::POST, ["drafts", "send"]) => return send(facade, request, next, true).await,
        (_, ["drafts", ..]) => return next.run(request).await,
        (Method::POST, ["messages", "batchModify"]) => Write::BatchModify,
        (Method::POST, ["messages", "batchDelete"]) => Write::BatchDelete,
        (Method::POST, ["messages", id, "modify"]) => Write::Modify(message(id)),
        (Method::POST, ["threads", id, "modify"]) => Write::Modify(thread(id)),
        (Method::POST, ["messages", id, "trash"]) => Write::Trash(message(id)),
        (Method::POST, ["threads", id, "trash"]) => Write::Trash(thread(id)),
        (Method::POST, ["messages", id, "untrash"]) => Write::Untrash(message(id)),
        (Method::POST, ["threads", id, "untrash"]) => Write::Untrash(thread(id)),
        (Method::DELETE, ["messages", id]) => Write::Delete(message(id)),
        (Method::DELETE, ["threads", id]) => Write::Delete(thread(id)),
        _ => return unsupported(),
    };

    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, JSON_LIMIT).await else {
        return failure(
            StatusCode::PAYLOAD_TOO_LARGE,
            "INVALID_ARGUMENT",
            "The request is too large.",
        );
    };
    let json = |text: &Bytes| serde_json::from_slice::<serde_json::Value>(text).unwrap_or_default();
    let list = |value: &serde_json::Value, key: &str| -> Vec<String> {
        value[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    };
    let ids_of = |target: &Target| match target {
        Target::Message(id) => vec![id.clone()],
        Target::Thread(id) => facade
            .engine
            .with(|mailbox| mailbox.thread_message_ids(id).unwrap_or_default()),
    };
    let body = json(&bytes);
    // `(ids, labels to add, labels to remove)`, or `None` for a delete.
    let (ids, change) = match &write {
        Write::Modify(target) => (
            ids_of(target),
            Some((list(&body, "addLabelIds"), list(&body, "removeLabelIds"))),
        ),
        Write::BatchModify => (
            list(&body, "ids"),
            Some((list(&body, "addLabelIds"), list(&body, "removeLabelIds"))),
        ),
        Write::Trash(target) => (
            ids_of(target),
            Some((vec!["TRASH".into()], vec!["INBOX".into(), "SPAM".into()])),
        ),
        Write::Untrash(target) => (ids_of(target), Some((Vec::new(), vec!["TRASH".into()]))),
        Write::Delete(target) => (ids_of(target), None),
        Write::BatchDelete => (list(&body, "ids"), None),
    };
    let modifies = matches!(write, Write::Modify(_) | Write::BatchModify);
    let request = Request::from_parts(parts, Body::from(bytes));

    let mut writer = facade.writer().await;
    let done = match change {
        Some((add, remove)) => {
            // The engine words its own refusal of labels it will not change.
            if modifies
                && facade
                    .engine
                    .with(|m| m.check_modifiable(&add, &remove))
                    .is_err()
            {
                drop(writer);
                return next.run(request).await;
            }
            writer.labels(&ids, &add, &remove).await
        }
        None => writer.delete(&ids).await,
    };
    let done = match done {
        Ok(done) => done,
        Err(failed) => return refused(failed),
    };
    let response = next.run(request).await;
    if response.status().is_success()
        && let Err(error) = writer.commit(done)
    {
        log::warn!("saving a change failed; the next sync repairs it: {error:#}");
    }
    response
}

/// `messages.send` and `drafts.send`: the engine makes the message, the outgoing server takes
/// it, and only then does Exo hear that it went.
async fn send<S: Mailstore + 'static>(
    facade: Arc<Facade<S>>,
    request: Request,
    next: Next,
    from_draft: bool,
) -> Response {
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return failure(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            "The request could not be read.",
        );
    };
    let draft_id = from_draft
        .then(|| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .flatten()
        .and_then(|value| value["id"].as_str().map(str::to_owned));
    let request = Request::from_parts(parts, Body::from(bytes));

    let mut writer = facade.writer().await;
    let draft = draft_id.and_then(|id| {
        facade.engine.with(|mailbox| {
            let message = mailbox.draft(&id).ok()?;
            Some(Draft {
                thread_id: (message.thread_id != message.id).then(|| message.thread_id.clone()),
                raw: message.raw.clone(),
                id,
            })
        })
    });
    let response = next.run(request).await;
    if !response.status().is_success() {
        return response;
    }
    let (parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return failure(StatusCode::BAD_GATEWAY, "UNAVAILABLE", "No reply.");
    };
    let Some(id) = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|value| value["id"].as_str().map(str::to_owned))
    else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    match writer.sent(&id, draft).await {
        Ok(()) => Response::from_parts(parts, Body::from(bytes)),
        Err(failed) => refused(failed),
    }
}
