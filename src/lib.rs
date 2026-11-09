//! lrclib-mirror as a library: the store (import and reading), the search index and the API, for the binary and for
//! tools comparing it with LRCLIB's server.

pub mod api;
pub mod config;
pub mod dump;
pub mod lyricsfile;
pub mod mirror;
pub mod search;
pub mod sqlite;
pub mod store;
pub mod text;
pub mod web;
pub mod yaml;

/// The version.
pub const VERSION: &str = env!("LRCLIB_MIRROR_VERSION");
