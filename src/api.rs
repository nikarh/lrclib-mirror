//! LRCLIB's lookup API, answered from the store: `GET /api/get`, `GET /api/get/{id}`, `GET /api/search` and
//! `GET /api/v2/search` (the search of LRCLIB's web client: tracks without their lyrics), with the parameters,
//! response bodies and errors of LRCLIB's server. And for the web search, what LRCLIB's server doesn't have:
//! `GET /api/mirror/search` (pages of search results) and `GET /api/mirror/artist` (an artist's tracks).
//!
//! The behavior is that of LRCLIB (<https://github.com/tranxuanthang/lrclib>, MIT, Copyright (c) 2024
//! tranxuanthang): `server/src/routes/*.rs`, `server/src/repositories/track_repository.rs`, `server/src/errors.rs`.

use std::borrow::Cow;

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::store::{Scratch, Store, StoreError, with_scratch};
use crate::text::{normalize, normalize_param};

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

/// `GET /api/mirror/search`: [`SearchParams`] and a page. (Not `#[serde(flatten)]`: query strings would then only
/// deserialize into strings.)
#[derive(Deserialize)]
pub struct SearchPageParams {
    q: Option<String>,
    track_name: Option<String>,
    artist_name: Option<String>,
    album_name: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
}

/// `GET /api/mirror/artist`.
#[derive(Deserialize)]
pub struct ArtistParams {
    artist_name: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

/// Results per page of the web search's own lookups, by default and at most.
pub const PAGE_LIMIT: usize = 20;
pub const MAX_PAGE_LIMIT: usize = 100;
/// How deep the search results can be paged.
pub const MAX_SEARCH_OFFSET: usize = 10_000;

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

/// A track without its lyrics, saying which it has (`GET /api/v2/search`).
#[derive(Serialize)]
#[allow(clippy::struct_excessive_bools)] // the body's fields
#[serde(rename_all = "camelCase")]
struct TrackSummary<'a> {
    id: i64,
    track_name: Option<&'a str>,
    artist_name: Option<&'a str>,
    album_name: Option<&'a str>,
    duration: Option<f64>,
    instrumental: bool,
    has_word_sync: bool,
    has_plain_lyrics: bool,
    has_synced_lyrics: bool,
    has_lyricsfile: bool,
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

impl JsonBody {
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

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

/// Writes the track with this ordinal to `out` as JSON, without its lyrics; an error when LRCLIB's server couldn't
/// read it.
fn write_summary(
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
    let flags = store.lyrics_flags(ordinal, lyrics)?;
    let body = TrackSummary {
        id: i64::from(track.id),
        track_name: track.name,
        artist_name: track.artist,
        album_name: track.album,
        duration: track.duration,
        instrumental: flags.instrumental,
        has_word_sync: flags.word_sync,
        has_plain_lyrics: flags.plain,
        has_synced_lyrics: flags.synced,
        has_lyricsfile: flags.lyricsfile,
    };
    serde_json::to_writer(out, &body).map_err(|e| ApiError::Unknown(e.to_string()))
}

/// The tracks with these ids as a JSON array, written by `write` (a track the store doesn't have is left out, as
/// LRCLIB's server does).
fn write_tracks(
    store: &Store,
    ids: &[i64],
    write: fn(&Store, u32, &mut Scratch, &mut Vec<u8>) -> Result<(), ApiError>,
    out: &mut Vec<u8>,
) -> Result<(), ApiError> {
    out.push(b'[');
    with_scratch(|scratch| {
        let mut first = true;
        for &id in ids {
            let Some(ordinal) = store.ordinal(id)? else {
                continue;
            };
            if !first {
                out.push(b',');
            }
            first = false;
            write(store, ordinal, scratch, out)?;
        }
        Ok::<(), ApiError>(())
    })?;
    out.push(b']');
    Ok(())
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

/// The ids of the tracks a search finds, best first: `limit` of them from `offset` on.
fn search_ids(
    store: &Store,
    params: &SearchParams,
    offset: usize,
    limit: usize,
) -> Result<Vec<i64>, ApiError> {
    let q = normalize_param(params.q.as_deref());
    let track_name = normalize_param(params.track_name.as_deref());
    let artist_name = normalize_param(params.artist_name.as_deref());
    let album_name = normalize_param(params.album_name.as_deref());
    store
        .search
        .page(
            q.as_deref(),
            track_name.as_deref(),
            artist_name.as_deref(),
            album_name.as_deref(),
            offset,
            limit,
        )
        .map_err(|e| ApiError::Unknown(e.to_string()))
}

pub fn search(store: &Store, params: &SearchParams) -> Result<JsonBody, ApiError> {
    let ids = search_ids(store, params, 0, crate::search::LIMIT)?;
    let mut out = Vec::with_capacity(ids.len() * (6 << 10) + 2);
    write_tracks(store, &ids, write_track, &mut out)?;
    Ok(JsonBody(out))
}

/// `GET /api/v2/search`: the tracks `GET /api/search` finds, without their lyrics.
pub fn search_v2(store: &Store, params: &SearchParams) -> Result<JsonBody, ApiError> {
    let ids = search_ids(store, params, 0, crate::search::LIMIT)?;
    let mut out = Vec::with_capacity(ids.len() * 256 + 2);
    write_tracks(store, &ids, write_summary, &mut out)?;
    Ok(JsonBody(out))
}

/// The page asked for: `(offset, limit)`.
fn page(
    offset: Option<usize>,
    limit: Option<usize>,
    max_offset: usize,
) -> Result<(usize, usize), ApiError> {
    let offset = offset.unwrap_or(0);
    let limit = limit.unwrap_or(PAGE_LIMIT);
    let mut errors = Vec::new();
    if offset > max_offset {
        errors.push(format!("offset: must be at most {max_offset}"));
    }
    if !(1..=MAX_PAGE_LIMIT).contains(&limit) {
        errors.push(format!("limit: must be between 1 and {MAX_PAGE_LIMIT}"));
    }
    if errors.is_empty() {
        Ok((offset, limit))
    } else {
        Err(ApiError::Validation(errors.join("\n")))
    }
}

/// `GET /api/mirror/search`: a page of what `GET /api/v2/search` answers, ranked the same, `limit` tracks from
/// `offset` on. Fewer than `limit` tracks: the last page.
pub fn search_page(store: &Store, params: &SearchPageParams) -> Result<JsonBody, ApiError> {
    let (offset, limit) = page(params.offset, params.limit, MAX_SEARCH_OFFSET)?;
    let search = SearchParams {
        q: params.q.clone(),
        track_name: params.track_name.clone(),
        artist_name: params.artist_name.clone(),
        album_name: params.album_name.clone(),
    };
    let ids = search_ids(store, &search, offset, limit)?;
    let mut out = Vec::with_capacity(ids.len() * 256 + 2);
    write_tracks(store, &ids, write_summary, &mut out)?;
    Ok(JsonBody(out))
}

/// `GET /api/mirror/artist`: the tracks of the artist with this name (once normalized, as `GET /api/get` matches
/// it), by name, album and id, as `{"total": n, "tracks": [...]}` with `limit` tracks from `offset` on.
pub fn artist(store: &Store, params: &ArtistParams) -> Result<JsonBody, ApiError> {
    let (offset, limit) = page(params.offset, params.limit, usize::MAX)?;
    let Some(artist) = normalize_param(Some(&params.artist_name)) else {
        return Err(ApiError::Validation(
            "artist_name: cannot be empty".to_owned(),
        ));
    };
    with_scratch(|scratch| {
        let mut tracks: Vec<(String, String, u32, u32)> = Vec::new();
        store.for_artist(&artist, &mut scratch.tracks, |ordinal, track| {
            // Tracks LRCLIB's server can't read aren't listed.
            if !track.broken {
                tracks.push((
                    normalize(track.name.unwrap_or_default()),
                    normalize(track.album.unwrap_or_default()),
                    track.id,
                    ordinal,
                ));
            }
        })?;
        tracks.sort_unstable();
        let mut out = Vec::with_capacity(limit.min(tracks.len()) * 256 + 32);
        out.extend_from_slice(format!("{{\"total\":{},\"tracks\":[", tracks.len()).as_bytes());
        for (i, (_, _, _, ordinal)) in tracks.iter().skip(offset).take(limit).enumerate() {
            if i > 0 {
                out.push(b',');
            }
            write_summary(store, *ordinal, scratch, &mut out)?;
        }
        out.extend_from_slice(b"]}");
        Ok(JsonBody(out))
    })
}
