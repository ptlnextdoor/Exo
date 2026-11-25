//! Router assembly and the cross-cutting HTTP behavior of Google's front end: JSON formatting
//! (`prettyPrint`, `fields`), auth, the per-user quota, fault injection, CORS and request
//! logging.

mod batch;
pub mod extract;
mod fields;
pub mod quota;

use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::post;
use tower_http::cors::CorsLayer;

use crate::error::Failure;
use crate::world::App;
use extract::Params;

/// Gmail accepts uploads up to 35 MB. The limit leaves headroom for base64 in JSON bodies.
const BODY_LIMIT: usize = 64 * 1024 * 1024;

/// The whole mock: every Google API on one origin, plus `/__mock/` admin endpoints.
pub fn router(app: Arc<App>) -> Router {
    let api = Router::new()
        .merge(crate::mail::routes::router(app.clone()))
        .merge(crate::calendar::routes::router())
        .merge(crate::people::routes::router())
        .merge(crate::oauth::api_router())
        .layer(from_fn_with_state(app.clone(), inject_faults))
        .layer(from_fn_with_state(app.clone(), quota::enforce))
        .layer(from_fn_with_state(app.clone(), crate::oauth::authenticate))
        .layer(from_fn(format_json))
        .with_state(app.clone());

    Router::new()
        .merge(api.clone())
        .route("/batch/gmail/v1", post(batch::handle).with_state(api))
        .merge(crate::oauth::router().with_state(app.clone()))
        .merge(crate::admin::router().with_state(app.clone()))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(CorsLayer::very_permissive())
        .layer(from_fn_with_state(app, delay))
        .layer(from_fn(log_requests))
}

/// Holds every API request for the configured latency (admin requests answer at once). A
/// batch is one round trip, as with Google.
async fn delay(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let latency = app.lock().config.latency;
    if !latency.is_zero() && !request.uri().path().starts_with("/__mock/") {
        tokio::time::sleep(latency).await;
    }
    next.run(request).await
}

/// Google pretty-prints JSON unless `prettyPrint=false`, applies `fields` to successful
/// responses, and always labels JSON as UTF-8.
pub(crate) async fn format_json(request: Request, next: Next) -> Response {
    let params = Params::parse(request.uri().query().unwrap_or_default());
    let response = next.run(request).await;
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if !is_json {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, usize::MAX).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    if parts.status.is_success()
        && let Some(mask) = params.text("fields")
    {
        value = fields::Mask::parse(mask).apply(value);
    }
    let pretty = params.get("prettyPrint").is_none_or(|v| v != "false");
    let mut text = if pretty {
        serde_json::to_string_pretty(&value)
    } else {
        serde_json::to_string(&value)
    }
    .expect("JSON values serialize");
    text.push('\n');
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=UTF-8"),
    );
    parts.headers.remove(header::CONTENT_LENGTH);
    for vary in ["Origin", "X-Origin", "Referer"] {
        parts
            .headers
            .append(header::VARY, HeaderValue::from_static(vary));
    }
    Response::from_parts(parts, Body::from(text))
}

/// Answers matching requests with a canned error, as configured through `/__mock/faults`. The
/// error comes instead of handling them, or after it (`after`), so the change is made but the
/// answer is lost.
async fn inject_faults(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let fault = {
        let mut world = app.lock();
        let path = request.uri().path();
        let method = request.method().as_str();
        let query: Vec<(String, String)> =
            url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
                .into_owned()
                .collect();
        world
            .faults
            .iter_mut()
            .find(|fault| {
                fault.times > 0
                    && path.starts_with(&fault.path)
                    && fault
                        .method
                        .as_deref()
                        .is_none_or(|wanted| wanted.eq_ignore_ascii_case(method))
                    && fault.query.as_deref().is_none_or(|wanted| {
                        url::form_urlencoded::parse(wanted.as_bytes())
                            .into_owned()
                            .all(|pair| query.contains(&pair))
                    })
            })
            .map(|fault| {
                fault.times -= 1;
                fault.clone()
            })
    };
    let Some(fault) = fault else {
        return next.run(request).await;
    };
    let calendar = request.uri().path().starts_with("/calendar/");
    if fault.after {
        let _ = next.run(request).await;
    }
    let (message, reason) = canned(fault.status);
    let mut failure = Failure::new(
        fault.status,
        fault.message.as_deref().unwrap_or(message),
        fault.reason.as_deref().unwrap_or(reason),
    );
    if calendar {
        failure = failure.legacy();
    }
    failure.into_response()
}

/// The message and reason Google typically sends with each status.
fn canned(status: u16) -> (&'static str, &'static str) {
    match status {
        400 => ("Precondition check failed.", "failedPrecondition"),
        401 => ("Invalid Credentials", "authError"),
        403 => ("User-rate limit exceeded.", "rateLimitExceeded"),
        404 => ("Requested entity was not found.", "notFound"),
        410 => ("Resource has been deleted", "deleted"),
        429 => (
            "Too many concurrent requests for user.",
            "rateLimitExceeded",
        ),
        503 => ("The service is currently unavailable.", "backendError"),
        _ => ("Backend Error", "backendError"),
    }
}

/// Google's HTML 404 for paths no API serves.
async fn not_found(request: Request) -> Response {
    let path = request.uri().path().to_owned();
    (
        StatusCode::NOT_FOUND,
        Html(format!(
            "<!DOCTYPE html>\n<html lang=en>\n<title>Error 404 (Not Found)!!1</title>\n\
             <p><b>404.</b> <ins>That’s an error.</ins>\n<p>The requested URL <code>{path}</code> \
             was not found on this server.  <ins>That’s all we know.</ins>\n"
        )),
    )
        .into_response()
}

async fn log_requests(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().clone();
    let target = request
        .uri()
        .path_and_query()
        .map_or_else(|| "/".to_owned(), ToString::to_string);
    let response = next.run(request).await;
    log::info!(
        "{method} {target} -> {} ({:.1?})",
        response.status().as_u16(),
        started.elapsed()
    );
    response
}
