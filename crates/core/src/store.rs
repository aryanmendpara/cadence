//! SQLite store: schema migrations, FTS5 search.
//!
//! `Store` is not `Sync`-friendly by design — one writer thread owns it. Reads handed to
//! the UI are plain row structs.

use std::path::Path;

use rusqlite::Connection;

use crate::{Error, Result};

/// Applied in order; `PRAGMA user_version` records how many have run.
const MIGRATIONS: &[&str] = &[V1];

const V1: &str = r#"
CREATE TABLE artists (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  sort_name TEXT,
  musicbrainz_id TEXT
);

CREATE TABLE albums (
  id INTEGER PRIMARY KEY,
  title TEXT NOT NULL,
  album_artist_id INTEGER REFERENCES artists(id),
  year INTEGER,
  musicbrainz_id TEXT
);

CREATE TABLE tracks (
  id INTEGER PRIMARY KEY,
  title TEXT NOT NULL,
  artist_id INTEGER NOT NULL REFERENCES artists(id),
  album_id INTEGER REFERENCES albums(id),
  duration_ms INTEGER,
  isrc TEXT,
  musicbrainz_id TEXT,
  version_flags INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
);
CREATE INDEX idx_tracks_isrc ON tracks(isrc);

CREATE TABLE track_sources (
  id INTEGER PRIMARY KEY,
  track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
  provider TEXT NOT NULL,
  provider_track_id TEXT NOT NULL,
  duration_ms INTEGER,
  quality TEXT,
  available INTEGER NOT NULL DEFAULT 1,
  last_checked_at INTEGER,
  match_confidence REAL NOT NULL,
  UNIQUE(provider, provider_track_id)
);

CREATE TABLE match_overrides (
  id INTEGER PRIMARY KEY,
  provider TEXT NOT NULL,
  provider_track_id TEXT NOT NULL,
  track_id INTEGER REFERENCES tracks(id) ON DELETE CASCADE,
  UNIQUE(provider, provider_track_id)
);

CREATE TABLE plays (
  id INTEGER PRIMARY KEY,
  track_id INTEGER NOT NULL REFERENCES tracks(id),
  source_id INTEGER REFERENCES track_sources(id),
  started_at INTEGER NOT NULL,
  played_ms INTEGER NOT NULL,
  completed INTEGER NOT NULL
);

CREATE TABLE playlists (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE playlist_items (
  playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
  position INTEGER NOT NULL,
  track_id INTEGER NOT NULL REFERENCES tracks(id),
  PRIMARY KEY (playlist_id, position)
);

CREATE TABLE provider_accounts (
  provider TEXT PRIMARY KEY,
  display_name TEXT,
  connected_at INTEGER
);

CREATE TABLE queue_items (
  position INTEGER PRIMARY KEY,
  track_id INTEGER NOT NULL REFERENCES tracks(id)
);

CREATE TABLE match_suggestions (
  provider TEXT NOT NULL,
  provider_track_id TEXT NOT NULL,
  candidate_track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
  score REAL NOT NULL,
  PRIMARY KEY (provider, provider_track_id, candidate_track_id)
);

-- Plain (not contentless) FTS5, rowid = tracks.id. Ingest keeps it in sync in the same
-- transaction as the track write (AGENTS.md §4).
CREATE VIRTUAL TABLE tracks_fts USING fts5(title, artist, album);
"#;

/// Hits returned by [`Store::search`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackHit {
    pub track_id: i64,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_ms: Option<i64>,
}

#[derive(Debug)]
pub struct Store {
    pub(crate) conn: Connection,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_conn(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;",
        )?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    fn migrate(&mut self) -> Result<()> {
        let applied = self.schema_version()?;
        let supported = MIGRATIONS.len() as i64;
        if applied > supported {
            return Err(Error::SchemaTooNew {
                found: applied,
                supported,
            });
        }
        for (index, sql) in MIGRATIONS.iter().enumerate().skip(applied as usize) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", (index + 1) as i64)?;
            tx.commit()?;
        }
        Ok(())
    }

    /// Full-text search over title, artist and album, best match first.
    ///
    /// Every term must match somewhere (AND); terms are prefix-matched so search works
    /// while typing.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<TrackHit>> {
        let expression = fts_expression(query);
        if expression.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.title, ar.name, al.title, t.duration_ms
             FROM tracks_fts
             JOIN tracks t ON t.id = tracks_fts.rowid
             JOIN artists ar ON ar.id = t.artist_id
             LEFT JOIN albums al ON al.id = t.album_id
             WHERE tracks_fts MATCH ?1
             ORDER BY bm25(tracks_fts, 10.0, 5.0, 1.0), t.title
             LIMIT ?2",
        )?;
        let hits = stmt
            .query_map((expression, limit as i64), |row| {
                Ok(TrackHit {
                    track_id: row.get(0)?,
                    title: row.get(1)?,
                    artist: row.get(2)?,
                    album: row.get(3)?,
                    duration_ms: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(hits)
    }

    pub(crate) fn reindex(conn: &Connection, track_id: i64) -> Result<()> {
        conn.execute(
            "INSERT OR REPLACE INTO tracks_fts(rowid, title, artist, album)
             SELECT t.id, t.title, ar.name, COALESCE(al.title, '')
             FROM tracks t
             JOIN artists ar ON ar.id = t.artist_id
             LEFT JOIN albums al ON al.id = t.album_id
             WHERE t.id = ?1",
            [track_id],
        )?;
        Ok(())
    }
}

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is before 1970")
        .as_secs() as i64
}

/// Turn free text into a safe FTS5 query: alphanumeric terms, each prefix-matched.
pub(crate) fn fts_expression(query: &str) -> String {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(|term| format!("{term}*"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(store: &Store) {
        store
            .conn
            .execute_batch(
                "INSERT INTO artists (id, name) VALUES (1, 'The Harbour');
                 INSERT INTO albums (id, title, album_artist_id) VALUES (1, 'Low Tide', 1);
                 INSERT INTO tracks (id, title, artist_id, album_id, duration_ms, created_at)
                   VALUES (1, 'Night Ferry', 1, 1, 240000, 0);
                 INSERT INTO tracks (id, title, artist_id, duration_ms, created_at)
                   VALUES (2, 'Harbour Lights', 1, 180000, 0);
                 INSERT INTO tracks_fts(rowid, title, artist, album) VALUES
                   (1, 'Night Ferry', 'The Harbour', 'Low Tide'),
                   (2, 'Harbour Lights', 'The Harbour', '');",
            )
            .unwrap();
    }

    #[test]
    fn fresh_database_is_at_latest_schema() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len() as i64);
    }

    #[test]
    fn migrate_is_idempotent() {
        let mut store = Store::open_in_memory().unwrap();
        store.migrate().unwrap();
        store.migrate().unwrap();
        assert_eq!(store.schema_version().unwrap(), MIGRATIONS.len() as i64);
    }

    #[test]
    fn refuses_newer_schema() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", 999).unwrap();
        let err = Store::from_conn(conn).unwrap_err();
        assert!(matches!(
            err,
            Error::SchemaTooNew {
                found: 999,
                supported: 1
            }
        ));
    }

    #[test]
    fn search_matches_prefix_and_orders_by_relevance() {
        let store = Store::open_in_memory().unwrap();
        seed(&store);

        let hits = store.search("harb", 10).unwrap();
        assert_eq!(hits.len(), 2);

        let hits = store.search("harbour ferry", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Night Ferry");
    }

    #[test]
    fn search_ignores_punctuation_and_empty_queries() {
        let store = Store::open_in_memory().unwrap();
        seed(&store);

        assert_eq!(store.search("harbour!", 10).unwrap().len(), 2);
        assert!(store.search("   ", 10).unwrap().is_empty());
        assert!(store.search("harbour", 0).unwrap().is_empty());
    }

    #[test]
    fn reindex_follows_artist_rename_and_track_delete() {
        let store = Store::open_in_memory().unwrap();
        seed(&store);

        store
            .conn
            .execute_batch("UPDATE artists SET name = 'North Harbour' WHERE id = 1")
            .unwrap();
        Store::reindex(&store.conn, 1).unwrap();
        Store::reindex(&store.conn, 2).unwrap();

        let hits = store.search("north", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(store.search("the", 10).unwrap().is_empty());

        store
            .conn
            .execute_batch(
                "DELETE FROM tracks WHERE id = 2; DELETE FROM tracks_fts WHERE rowid = 2",
            )
            .unwrap();
        assert_eq!(store.search("north", 10).unwrap().len(), 1);
    }
}
