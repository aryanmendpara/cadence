//! Canonical entities. Rows in the store map to these; providers never own identity.

/// Recording attributes that make two same-titled recordings different songs.
///
/// Stored on `tracks.version_flags`. Matching requires flags to be equal before two
/// sources may be auto-merged (AGENTS.md §7).
pub mod version_flags {
    pub const LIVE: u32 = 1 << 0;
    pub const REMASTER: u32 = 1 << 1;
    pub const REMIX: u32 = 1 << 2;
    pub const ACOUSTIC: u32 = 1 << 3;
    pub const INSTRUMENTAL: u32 = 1 << 4;
    pub const RADIO_EDIT: u32 = 1 << 5;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artist {
    pub id: i64,
    pub name: String,
    pub sort_name: Option<String>,
    pub musicbrainz_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Album {
    pub id: i64,
    pub title: String,
    pub album_artist_id: Option<i64>,
    pub year: Option<i32>,
    pub musicbrainz_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub id: i64,
    pub title: String,
    pub artist_id: i64,
    pub album_id: Option<i64>,
    pub duration_ms: Option<i64>,
    pub isrc: Option<String>,
    pub musicbrainz_id: Option<String>,
    pub version_flags: u32,
    pub created_at: i64,
}

/// One playable copy of a canonical [`Track`] on a provider.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackSource {
    pub id: i64,
    pub track_id: i64,
    pub provider: String,
    pub provider_track_id: String,
    pub duration_ms: Option<i64>,
    pub quality: Option<String>,
    pub available: bool,
    pub last_checked_at: Option<i64>,
    pub match_confidence: f64,
}
