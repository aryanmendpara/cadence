#![forbid(unsafe_code)]
//! Headless harness for the core: scan a folder, search the library.
//!
//! This is the M1/M2 test surface from AGENTS.md — the GPUI app comes later and calls the
//! same `cadence-core` API.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use cadence_core::store::Store;
use directories::ProjectDirs;

const USAGE: &str = "\
cadence <command>

  scan <folder>     index audio files into the library
  search <terms>    full-text search over title, artist and album
  help              show this message

The database location can be overridden with CADENCE_DB.";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("scan") => {
            let root = args.get(1).context("scan needs a folder path")?;
            scan(PathBuf::from(root))
        }
        Some("search") => {
            let query = args[1..].join(" ");
            if query.trim().is_empty() {
                bail!("search needs at least one term");
            }
            search(&query)
        }
        Some("help" | "-h" | "--help") | None => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => bail!("unknown command `{other}`\n\n{USAGE}"),
    }
}

fn scan(root: PathBuf) -> Result<()> {
    let mut store = open_store()?;
    let mut ingested = 0u32;
    let mut skipped = 0u32;

    for result in cadence_provider_local::scan(&root) {
        match result {
            Ok(input) => {
                store
                    .ingest(&input)
                    .with_context(|| format!("ingesting {}", input.provider_track_id))?;
                ingested += 1;
            }
            Err(error) => {
                eprintln!("skip: {error}");
                skipped += 1;
            }
        }
    }

    println!("{ingested} tracks ingested, {skipped} files skipped");
    Ok(())
}

fn search(query: &str) -> Result<()> {
    let store = open_store()?;
    let hits = store.search(query, 50)?;
    if hits.is_empty() {
        println!("no matches for `{query}`");
        return Ok(());
    }
    for hit in hits {
        let album = hit.album.as_deref().unwrap_or("");
        let minutes = hit.duration_ms.unwrap_or(0) / 60_000;
        let seconds = hit.duration_ms.unwrap_or(0) % 60_000 / 1_000;
        if album.is_empty() {
            println!("{} — {} [{minutes}:{seconds:02}]", hit.artist, hit.title);
        } else {
            println!(
                "{} — {} ({album}) [{minutes}:{seconds:02}]",
                hit.artist, hit.title
            );
        }
    }
    Ok(())
}

fn open_store() -> Result<Store> {
    let path = match std::env::var_os("CADENCE_DB") {
        Some(path) => PathBuf::from(path),
        None => {
            let dirs = ProjectDirs::from("dev", "cadence", "Cadence")
                .context("no home directory for the library database")?;
            dirs.data_dir().join("library.db")
        }
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    Store::open(&path).with_context(|| format!("opening {}", path.display()))
}
