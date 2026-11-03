//! Ingest: turn a provider's [`TrackInput`] into a canonical track plus a source link.
//!
//! The ladder is AGENTS.md §7, in order: an already-linked source is reused (rescans are
//! idempotent), then user overrides, then ISRC, then MusicBrainz, then fuzzy. Wrong merges
//! are worse than missed ones, so the fuzzy band caps at "suggestion".

use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;
use crate::matching::{Candidate, ParsedTitle, Query, parse_title, score};
use crate::store::{Store, now_unix};

/// Auto-merge threshold (AGENTS.md §7.4).
pub const AUTO_MERGE: f64 = 0.90;
/// Below this the candidate is ignored; between the two it becomes a suggestion.
pub const SUGGEST_MERGE: f64 = 0.70;

/// What a provider hands ingest. Remote identity only — core owns the canonical track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackInput {
    pub provider: String,
    pub provider_track_id: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub year: Option<i32>,
    pub duration_ms: Option<i64>,
    pub isrc: Option<String>,
    pub musicbrainz_id: Option<String>,
    pub quality: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMethod {
    ExistingSource,
    Override,
    Isrc,
    MusicBrainz,
    Fuzzy,
    New,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ingested {
    pub track_id: i64,
    pub source_id: i64,
    pub method: MatchMethod,
    pub confidence: f64,
}

impl Store {
    pub fn ingest(&mut self, input: &TrackInput) -> Result<Ingested> {
        let tx = self.conn.transaction()?;
        let parsed = parse_title(&input.title);
        let now = now_unix();

        let (track_id, method, confidence) = if let Some(track_id) = linked_track(&tx, input)? {
            (track_id, MatchMethod::ExistingSource, 1.0)
        } else if let Some(override_target) = override_target(&tx, input)? {
            let track_id = match override_target {
                Some(track_id) if track_exists(&tx, track_id)? => track_id,
                _ => create_track(&tx, input, &parsed, now)?,
            };
            (track_id, MatchMethod::Override, 1.0)
        } else if let Some(track_id) = by_isrc(&tx, input, parsed.flags)? {
            (track_id, MatchMethod::Isrc, 1.0)
        } else if let Some(track_id) = by_musicbrainz(&tx, input)? {
            (track_id, MatchMethod::MusicBrainz, 1.0)
        } else {
            match best_candidate(&tx, input, &parsed)? {
                Some((track_id, score)) if score >= AUTO_MERGE => {
                    (track_id, MatchMethod::Fuzzy, score)
                }
                suggestion => {
                    let track_id = create_track(&tx, input, &parsed, now)?;
                    if let Some((candidate_id, score)) = suggestion {
                        add_suggestion(&tx, input, candidate_id, score)?;
                    }
                    (track_id, MatchMethod::New, 1.0)
                }
            }
        };

        let source_id = if method == MatchMethod::ExistingSource {
            refresh_source(&tx, input, now)?
        } else {
            insert_source(&tx, track_id, input, confidence, now)?
        };

        if method != MatchMethod::ExistingSource {
            Store::reindex(&tx, track_id)?;
        }

        tx.commit()?;
        Ok(Ingested {
            track_id,
            source_id,
            method,
            confidence,
        })
    }
}

/// A source we have already ingested (same provider + remote id) keeps its canonical track,
/// so rescans never duplicate.
fn linked_track(conn: &Connection, input: &TrackInput) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT track_id FROM track_sources WHERE provider = ?1 AND provider_track_id = ?2",
            params![input.provider, input.provider_track_id],
            |row| row.get(0),
        )
        .optional()?)
}

fn override_target(conn: &Connection, input: &TrackInput) -> Result<Option<Option<i64>>> {
    Ok(conn
        .query_row(
            "SELECT track_id FROM match_overrides WHERE provider = ?1 AND provider_track_id = ?2",
            params![input.provider, input.provider_track_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?)
}

fn track_exists(conn: &Connection, track_id: i64) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM tracks WHERE id = ?1", [track_id], |_| Ok(()))
        .optional()?
        .is_some())
}

fn by_isrc(conn: &Connection, input: &TrackInput, flags: u32) -> Result<Option<i64>> {
    let Some(isrc) = input
        .isrc
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    Ok(conn
        .query_row(
            "SELECT id FROM tracks WHERE isrc = ?1 COLLATE NOCASE AND version_flags = ?2
             ORDER BY id LIMIT 1",
            params![isrc, flags],
            |row| row.get(0),
        )
        .optional()?)
}

fn by_musicbrainz(conn: &Connection, input: &TrackInput) -> Result<Option<i64>> {
    let Some(mbid) = input
        .musicbrainz_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    Ok(conn
        .query_row(
            "SELECT id FROM tracks WHERE musicbrainz_id = ?1 COLLATE NOCASE ORDER BY id LIMIT 1",
            [mbid],
            |row| row.get(0),
        )
        .optional()?)
}

struct CandidateRow {
    id: i64,
    title: String,
    artist: String,
    album: Option<String>,
    duration_ms: Option<i64>,
    flags: u32,
}

fn best_candidate(
    conn: &Connection,
    input: &TrackInput,
    parsed: &ParsedTitle,
) -> Result<Option<(i64, f64)>> {
    let expression = crate::store::fts_expression(&parsed.clean);
    if expression.is_empty() {
        return Ok(None);
    }

    let mut stmt = conn.prepare(
        "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.version_flags
         FROM tracks_fts
         JOIN tracks t ON t.id = tracks_fts.rowid
         JOIN artists ar ON ar.id = t.artist_id
         LEFT JOIN albums al ON al.id = t.album_id
         WHERE tracks_fts MATCH ?1
         LIMIT 50",
    )?;
    let rows = stmt
        .query_map([expression], |row| {
            Ok(CandidateRow {
                id: row.get(0)?,
                title: row.get(1)?,
                artist: row.get(2)?,
                album: row.get(3)?,
                duration_ms: row.get(4)?,
                flags: row.get::<_, i64>(5)? as u32,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let query = Query {
        title: &parsed.clean,
        artist: &input.artist,
        album: input.album.as_deref(),
        duration_ms: input.duration_ms,
        flags: parsed.flags,
    };

    let best = rows
        .iter()
        .map(|row| {
            let candidate = Candidate {
                title: &row.title,
                artist: &row.artist,
                album: row.album.as_deref(),
                duration_ms: row.duration_ms,
                flags: row.flags,
            };
            (row.id, score(&query, &candidate))
        })
        .filter(|(_, score)| *score > 0.0)
        .max_by(|a, b| a.1.total_cmp(&b.1));

    Ok(best)
}

fn create_track(
    conn: &Connection,
    input: &TrackInput,
    parsed: &ParsedTitle,
    now: i64,
) -> Result<i64> {
    let artist_id = find_or_create_artist(conn, input.artist.trim())?;
    let album_artist = input
        .album_artist
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(input.artist.trim());
    let album_artist_id = find_or_create_artist(conn, album_artist)?;
    let album_id = match input
        .album
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(title) => Some(find_or_create_album(
            conn,
            title,
            album_artist_id,
            input.year,
        )?),
        None => None,
    };

    let isrc = input
        .isrc
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let musicbrainz_id = input
        .musicbrainz_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    Ok(conn.query_row(
        "INSERT INTO tracks
           (title, artist_id, album_id, duration_ms, isrc, musicbrainz_id, version_flags, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         RETURNING id",
        params![
            input.title,
            artist_id,
            album_id,
            input.duration_ms,
            isrc,
            musicbrainz_id,
            parsed.flags,
            now
        ],
        |row| row.get(0),
    )?)
}

fn find_or_create_artist(conn: &Connection, name: &str) -> Result<i64> {
    let existing = conn
        .query_row(
            "SELECT id FROM artists WHERE name = ?1 COLLATE NOCASE ORDER BY id LIMIT 1",
            [name],
            |row| row.get(0),
        )
        .optional()?;
    match existing {
        Some(id) => Ok(id),
        None => Ok(conn.query_row(
            "INSERT INTO artists (name) VALUES (?1) RETURNING id",
            [name],
            |row| row.get(0),
        )?),
    }
}

fn find_or_create_album(
    conn: &Connection,
    title: &str,
    album_artist_id: i64,
    year: Option<i32>,
) -> Result<i64> {
    let existing = conn
        .query_row(
            "SELECT id FROM albums
             WHERE title = ?1 COLLATE NOCASE AND album_artist_id IS ?2
             ORDER BY id LIMIT 1",
            params![title, album_artist_id],
            |row| row.get(0),
        )
        .optional()?;
    match existing {
        Some(id) => Ok(id),
        None => Ok(conn.query_row(
            "INSERT INTO albums (title, album_artist_id, year) VALUES (?1, ?2, ?3) RETURNING id",
            params![title, album_artist_id, year],
            |row| row.get(0),
        )?),
    }
}

fn insert_source(
    conn: &Connection,
    track_id: i64,
    input: &TrackInput,
    confidence: f64,
    now: i64,
) -> Result<i64> {
    Ok(conn.query_row(
        "INSERT INTO track_sources
           (track_id, provider, provider_track_id, duration_ms, quality,
            available, last_checked_at, match_confidence)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7)
         RETURNING id",
        params![
            track_id,
            input.provider,
            input.provider_track_id,
            input.duration_ms,
            input.quality,
            now,
            confidence
        ],
        |row| row.get(0),
    )?)
}

fn refresh_source(conn: &Connection, input: &TrackInput, now: i64) -> Result<i64> {
    let id = conn.query_row(
        "SELECT id FROM track_sources WHERE provider = ?1 AND provider_track_id = ?2",
        params![input.provider, input.provider_track_id],
        |row| row.get(0),
    )?;
    conn.execute(
        "UPDATE track_sources
         SET duration_ms = ?1, quality = ?2, available = 1, last_checked_at = ?3
         WHERE id = ?4",
        params![input.duration_ms, input.quality, now, id],
    )?;
    Ok(id)
}

fn add_suggestion(
    conn: &Connection,
    input: &TrackInput,
    candidate_track_id: i64,
    score: f64,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO match_suggestions
           (provider, provider_track_id, candidate_track_id, score)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            input.provider,
            input.provider_track_id,
            candidate_track_id,
            score
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(provider: &str, id: &str, title: &str, artist: &str) -> TrackInput {
        TrackInput {
            provider: provider.into(),
            provider_track_id: id.into(),
            title: title.into(),
            artist: artist.into(),
            album: Some("Low Tide".into()),
            album_artist: None,
            year: Some(2021),
            duration_ms: Some(240_000),
            isrc: None,
            musicbrainz_id: None,
            quality: None,
        }
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn rescan_is_idempotent() {
        let mut store = Store::open_in_memory().unwrap();
        let track = input("local", "/music/ferry.flac", "Night Ferry", "The Harbour");

        let first = store.ingest(&track).unwrap();
        let second = store.ingest(&track).unwrap();

        assert_eq!(first.track_id, second.track_id);
        assert_eq!(second.method, MatchMethod::ExistingSource);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM tracks"), 1);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM track_sources"), 1);
        assert_eq!(store.search("ferry", 10).unwrap().len(), 1);
    }

    #[test]
    fn isrc_links_two_providers_to_one_track() {
        let mut store = Store::open_in_memory().unwrap();
        let local = TrackInput {
            isrc: Some("GBAYE0601498".into()),
            ..input("local", "/music/ferry.flac", "Night Ferry", "The Harbour")
        };
        let remote = TrackInput {
            isrc: Some("gbaye0601498".into()),
            title: "night ferry".into(),
            ..input("subsonic", "123", "night ferry", "The Harbour")
        };

        let a = store.ingest(&local).unwrap();
        let b = store.ingest(&remote).unwrap();

        assert_eq!(a.track_id, b.track_id);
        assert_eq!(b.method, MatchMethod::Isrc);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM tracks"), 1);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM track_sources"), 2);
    }

    #[test]
    fn isrc_does_not_merge_across_version_flags() {
        let mut store = Store::open_in_memory().unwrap();
        let studio = TrackInput {
            isrc: Some("GBAYE0601498".into()),
            ..input("local", "/music/ferry.flac", "Night Ferry", "The Harbour")
        };
        let live = TrackInput {
            isrc: Some("GBAYE0601498".into()),
            title: "Night Ferry (Live)".into(),
            ..input("subsonic", "123", "Night Ferry (Live)", "The Harbour")
        };

        let a = store.ingest(&studio).unwrap();
        let b = store.ingest(&live).unwrap();

        assert_ne!(a.track_id, b.track_id);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM tracks"), 2);
    }

    #[test]
    fn fuzzy_merges_near_identical_recordings() {
        let mut store = Store::open_in_memory().unwrap();
        let first = store
            .ingest(&input(
                "local",
                "/music/a.flac",
                "Night Ferry",
                "The Harbour",
            ))
            .unwrap();
        let near = TrackInput {
            duration_ms: Some(240_800),
            ..input("subsonic", "123", "Night Ferry", "The Harbour")
        };
        let second = store.ingest(&near).unwrap();

        assert_eq!(first.track_id, second.track_id);
        assert_eq!(second.method, MatchMethod::Fuzzy);
        assert!(second.confidence >= AUTO_MERGE);
    }

    #[test]
    fn uncertain_match_creates_track_and_suggestion() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .ingest(&input(
                "local",
                "/music/a.flac",
                "Night Ferry",
                "The Harbour",
            ))
            .unwrap();
        let other_artist = input("subsonic", "123", "Night Ferry", "Harbour Lights");
        let second = store.ingest(&other_artist).unwrap();

        assert_eq!(second.method, MatchMethod::New);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM tracks"), 2);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM match_suggestions"), 1);
    }

    #[test]
    fn override_can_pin_and_refuse_merges() {
        let mut store = Store::open_in_memory().unwrap();
        let base = store
            .ingest(&input(
                "local",
                "/music/a.flac",
                "Night Ferry",
                "The Harbour",
            ))
            .unwrap();

        store
            .conn
            .execute(
                "INSERT INTO match_overrides (provider, provider_track_id, track_id)
                 VALUES ('subsonic', 'pinned', ?1), ('subsonic', 'never', NULL)",
                [base.track_id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO match_overrides (provider, provider_track_id, track_id)
                 VALUES ('subsonic', 'pinned2', ?1)",
                [base.track_id],
            )
            .unwrap();

        let pinned = store
            .ingest(&input("subsonic", "pinned2", "Some Other Song", "Nobody"))
            .unwrap();
        assert_eq!(pinned.track_id, base.track_id);
        assert_eq!(pinned.method, MatchMethod::Override);

        let refused = store
            .ingest(&input("subsonic", "never", "Night Ferry", "The Harbour"))
            .unwrap();
        assert_ne!(refused.track_id, base.track_id);
        assert_eq!(refused.method, MatchMethod::Override);
    }

    #[test]
    fn track_source_unique_key_prevents_duplicates() {
        let mut store = Store::open_in_memory().unwrap();
        let track = input("local", "/music/a.flac", "Night Ferry", "The Harbour");
        store.ingest(&track).unwrap();
        store.ingest(&track).unwrap();
        assert_eq!(count(&store, "SELECT COUNT(*) FROM track_sources"), 1);
    }
}
