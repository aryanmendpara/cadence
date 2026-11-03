#![forbid(unsafe_code)]
//! Cadence core: the canonical library — domain model, SQLite store, matching.
//!
//! The store is deliberately synchronous and has no internal locking. One writer owns a
//! [`store::Store`] (the engine, on its own thread); readers get query results, never the
//! connection.

pub mod model;
pub mod store;

/// Errors surfaced by the core crates.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("database schema v{found} is newer than this build (v{supported}); update Cadence")]
    SchemaTooNew { found: i64, supported: i64 },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
