//! LRCLIB's lookup API, answered from the store: `GET /api/get`, `GET /api/get/{id}` and `GET /api/search`, with
//! the parameters, response bodies and errors of LRCLIB's server.
//!
//! The behavior is that of LRCLIB (<https://github.com/tranxuanthang/lrclib>, MIT, Copyright (c) 2024
//! tranxuanthang): `server/src/routes/*.rs`, `server/src/repositories/track_repository.rs`, `server/src/errors.rs`.

use std::borrow::Cow;

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::store::{Scratch, Store, StoreError, with_scratch};
use crate::text::normalize_param;

#[derive(Deserialize)]
pub struct GetParams {
    track_name: String,
    artist_name: String,
    album_name: Option<String>,
    duration: Option<f64>,
}

#[derive(Deserialize)]
pub struct SearchParams {
    q: Option<String>,
    track_name: Option<String>,
    artist_name: Option<String>,
    album_name: Option<String>,
}

/// A response body, borrowing from the store's buffers.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TrackResponse<'a> {
    id: i64,
    name: Option<&'a str>,
    track_name: Option<&'a str>,
    artist_name: Option<&'a str>,
    album_name: Option<&'a str>,
    duration: Option<f64>,
    instrumental: bool,
    has_word_sync: bool,
    plain_lyrics: Option<Cow<'a, str>>,
    synced_lyrics: Option<Cow<'a, str>>,
    lyricsfile: Option<Cow<'a, str>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorBody {
    message: String,
    name: &'static str,
    status_code: u16,
}

#[derive(Debug)]
pub enum ApiError {
    NotFound,
    Validation(String),
    Unknown(String),
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        Self::Unknown(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, name, message) = match self {
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                "TrackNotFound",
                "Failed to find specified track".to_owned(),
            ),
            Self::Validation(message) => (StatusCode::BAD_REQUEST, "ValidationError", message),
            Self::Unknown(error) => {
                tracing::error!(error, "request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "UnknownError",
                    "Something bad happened when processing your request".to_owned(),
                )
            }
        };
        (
            status,
            Json(ErrorBody {
                message,
                name,
                status_code: status.as_u16(),
            }),
        )
            .into_response()
    }
}

/// A JSON body.
pub struct JsonBody(Vec<u8>);

impl IntoResponse for JsonBody {
    fn into_response(self) -> Response {
        (
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )],
            self.0,
        )
            .into_response()
    }
}

/// Writes the track with this ordinal and its lyrics to `out` as JSON; an error when LRCLIB's server couldn't read
/// it.
fn write_track(
    store: &Store,
    ordinal: u32,
    scratch: &mut Scratch,
    out: &mut Vec<u8>,
) -> Result<(), ApiError> {
    let Scratch { tracks, lyrics } = scratch;
    let track = store.track(ordinal, tracks)?;
    if track.broken {
        return Err(ApiError::Unknown(format!(
            "track {} can't be read",
            track.id
        )));
    }
    let lyrics = store.lyrics(ordinal, &track, lyrics)?;
    let body = TrackResponse {
        id: i64::from(track.id),
        name: track.name,
        track_name: track.name,
        artist_name: track.artist,
        album_name: track.album,
        duration: track.duration,
        instrumental: lyrics.instrumental,
        has_word_sync: lyrics.word_sync,
        plain_lyrics: lyrics.plain,
        synced_lyrics: lyrics.synced,
        lyricsfile: lyrics.lyricsfile,
    };
    serde_json::to_writer(out, &body).map_err(|e| ApiError::Unknown(e.to_string()))
}

/// A single track as the body.
fn track_body(store: &Store, ordinal: u32, scratch: &mut Scratch) -> Result<JsonBody, ApiError> {
    let mut out = Vec::with_capacity(8 << 10);
    write_track(store, ordinal, scratch, &mut out)?;
    Ok(JsonBody(out))
}

/// Field validation as LRCLIB's server does it (`validator`): one line per invalid field.
fn validate(params: &GetParams) -> Result<(), ApiError> {
    let mut errors = Vec::new();
    if params.track_name.is_empty() {
        errors.push("track_name: cannot be empty");
    }
    if params.artist_name.is_empty() {
        errors.push("artist_name: cannot be empty");
    }
    if params.duration.is_some_and(|d| d < 1.0 || d > 3600.0) {
        errors.push("duration: must be between 1 and 3600");
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ApiError::Validation(errors.join("\n")))
    }
}

pub fn get_by_metadata(store: &Store, params: &GetParams) -> Result<JsonBody, ApiError> {
    validate(params)?;
    let (Some(name), Some(artist)) = (
        normalize_param(Some(&params.track_name)),
        normalize_param(Some(&params.artist_name)),
    ) else {
        return Err(ApiError::NotFound);
    };
    let album = normalize_param(params.album_name.as_deref());
    with_scratch(|scratch| {
        let ordinal = store
            .find(
                &name,
                &artist,
                album.as_deref(),
                params.duration,
                &mut scratch.tracks,
            )?
            .ok_or(ApiError::NotFound)?;
        track_body(store, ordinal, scratch)
    })
}

pub fn get_by_id(store: &Store, id: i64) -> Result<JsonBody, ApiError> {
    let ordinal = store.ordinal(id)?.ok_or(ApiError::NotFound)?;
    with_scratch(|scratch| track_body(store, ordinal, scratch))
}

pub fn search(store: &Store, params: &SearchParams) -> Result<JsonBody, ApiError> {
    let q = normalize_param(params.q.as_deref());
    let track_name = normalize_param(params.track_name.as_deref());
    let artist_name = normalize_param(params.artist_name.as_deref());
    let album_name = normalize_param(params.album_name.as_deref());
    let ids = store
        .search
        .search(
            q.as_deref(),
            track_name.as_deref(),
            artist_name.as_deref(),
            album_name.as_deref(),
        )
        .map_err(|e| ApiError::Unknown(e.to_string()))?;
    let mut out = Vec::with_capacity(ids.len() * (6 << 10) + 2);
    out.push(b'[');
    with_scratch(|scratch| {
        let mut first = true;
        for id in ids {
            // A track the index has and the store doesn't is left out, as LRCLIB's server does.
            let Some(ordinal) = store.ordinal(id)? else {
                continue;
            };
            if !first {
                out.push(b',');
            }
            first = false;
            write_track(store, ordinal, scratch, &mut out)?;
        }
        Ok::<(), ApiError>(())
    })?;
    out.push(b']');
    Ok(JsonBody(out))
}
