//! HTTP: LRCLIB's lookup API, answered from the current store, and the embedded web search.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tower_http::cors::{Any, CorsLayer};

use crate::api::{self, ApiError};
use crate::mirror::Mirror;
use crate::store::Store;

mod assets {
    include!(concat!(env!("OUT_DIR"), "/assets.rs"));
}

/// Only the lookups: publishing and flagging lyrics have no place on a copy that the next dump replaces.
pub fn router(mirror: Arc<Mirror>) -> Router {
    Router::new()
        .route("/api/get", get(get_by_metadata))
        .route("/api/get/:track_id", get(get_by_track_id))
        .route("/api/search", get(search))
        .route("/healthz", get(|| async { "ok" }))
        .fallback(static_file)
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET])
                .allow_headers([
                    header::CONTENT_TYPE,
                    header::HeaderName::from_static("x-user-agent"),
                    header::HeaderName::from_static("lrclib-client"),
                ])
                .expose_headers([header::RETRY_AFTER]),
        )
        .with_state(mirror)
}

/// While the first dump is imported there is nothing to answer from.
fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, "600")],
        axum::Json(serde_json::json!({
            "statusCode": 503,
            "name": "NoDatabase",
            "message": "The first database dump is still being imported",
        })),
    )
        .into_response()
}

/// Answers from the current store, on a blocking thread (reads are file reads).
async fn answer<F>(mirror: &Mirror, f: F) -> Response
where
    F: FnOnce(&Store) -> Result<api::JsonBody, ApiError> + Send + 'static,
{
    let Some(store) = mirror.current() else {
        return unavailable();
    };
    match tokio::task::spawn_blocking(move || f(&store)).await {
        Ok(Ok(body)) => body.into_response(),
        Ok(Err(e)) => e.into_response(),
        Err(e) => ApiError::Unknown(e.to_string()).into_response(),
    }
}

async fn get_by_metadata(
    State(mirror): State<Arc<Mirror>>,
    Query(params): Query<api::GetParams>,
) -> Response {
    answer(&mirror, move |store| api::get_by_metadata(store, &params)).await
}

async fn get_by_track_id(State(mirror): State<Arc<Mirror>>, Path(id): Path<i64>) -> Response {
    answer(&mirror, move |store| api::get_by_id(store, id)).await
}

async fn search(
    State(mirror): State<Arc<Mirror>>,
    Query(params): Query<api::SearchParams>,
) -> Response {
    answer(&mirror, move |store| api::search(store, &params)).await
}

/// The web search (a single page app: unknown paths get `index.html`).
async fn static_file(request: Request) -> Response {
    let path = request.uri().path().trim_start_matches('/');
    if path == "api" || path.starts_with("api/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let (name, body) = match find(path) {
        Some(file) => file,
        None => match find("index.html") {
            Some(file) => file,
            None => {
                return (
                    StatusCode::OK,
                    "lrclib-mirror: built without the web search",
                )
                    .into_response();
            }
        },
    };
    let cache = if name.starts_with("assets/") {
        "public, max-age=31536000, immutable" // file names carry a content hash
    } else {
        "no-cache"
    };
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static(content_type(name)),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static(cache)),
        ],
        body,
    )
        .into_response()
}

fn find(path: &str) -> Option<(&'static str, &'static [u8])> {
    let path = if path.is_empty() { "index.html" } else { path };
    assets::FILES
        .iter()
        .find(|(name, _)| *name == path)
        .copied()
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("svg") => "image/svg+xml",
        Some("json") => "application/json",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}
