#![forbid(unsafe_code)]
//! Headless harness for the core: scan a folder, search the library.
//!
//! This is the M1/M2 test surface from AGENTS.md — the GPUI app comes later and calls the
//! same `cadence-core` API.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cadence_audio::{Command, Event, Player, TrackRequest};
use cadence_core::store::Store;
use directories::ProjectDirs;

const USAGE: &str = "\
cadence <command>

  scan <folder>     index audio files into the library
  search <terms>    full-text search over title, artist and album
  play <file>...    play files back to back, gaplessly
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
        Some("play") => {
            if args.len() < 2 {
                bail!("play needs at least one audio file path");
            }
            play(args[1..].iter().map(PathBuf::from).collect())
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

/// The M2 harness: play files back to back through the real audio path.
///
/// A gapless run reports `Playing` for every file and a single `Ended` at the very end; a
/// broken hand-off would end the first track instead.
fn play(paths: Vec<PathBuf>) -> Result<()> {
    let player = Player::new().context("starting the audio thread")?;
    let total = paths.len();

    let first = TrackRequest {
        path: paths[0].clone(),
        id: 0,
    };
    if !player.send(Command::Play(first)) {
        bail!("the audio thread stopped");
    }
    preload(&player, &paths, 1);

    let mut duration_ms = None;
    loop {
        match player.recv_timeout(Duration::from_millis(250)) {
            Some(Event::Playing {
                id,
                duration_ms: length,
            }) => {
                duration_ms = length;
                let label = paths.get(id as usize).map(|p| p.display().to_string());
                println!(
                    "[{}/{total}] {}{}",
                    id + 1,
                    label.unwrap_or_default(),
                    length
                        .map(|ms| format!(" ({})", clock(ms)))
                        .unwrap_or_default()
                );
                preload(&player, &paths, id as usize + 1);
            }
            Some(Event::Position { ms }) => {
                let total = duration_ms.map(clock).unwrap_or_else(|| "--:--".into());
                print!("\r{} / {total}  ", clock(ms));
                std::io::stdout().flush().ok();
            }
            Some(Event::Paused) => println!("\rpaused        "),
            Some(Event::Ended | Event::Stopped) => {
                println!("\rdone           ");
                return Ok(());
            }
            Some(Event::Error(error)) => {
                println!();
                bail!("{error}");
            }
            Some(_) | None => {}
        }
    }
}

/// Hand the worker the file after `index`; `None` clears a stale one at the end of the list.
fn preload(player: &Player, paths: &[PathBuf], index: usize) {
    let request = paths.get(index).map(|path| TrackRequest {
        path: path.clone(),
        id: index as u64,
    });
    player.send(Command::SetNext(request));
}

fn clock(ms: u64) -> String {
    let seconds = ms / 1_000;
    format!("{}:{:02}", seconds / 60, seconds % 60)
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
