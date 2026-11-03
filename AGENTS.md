# AGENTS.md — Multi-Provider Music Player (MVP)

> Name: **Cadence**. Cargo package names use the `cadence-` prefix; directories stay unprefixed (see §3).
> Read this whole file before writing code. If something here conflicts with what the user tells you in chat, the user wins; then suggest updating this file.

---

## 1. What we're building

A fast, native desktop music player that plays music from **multiple sources** (local files, Subsonic/Navidrome, Spotify, YouTube Music, SoundCloud, others later) through **one unified library**.

- Language: **Rust** for the core. UI: **GPUI + gpui-component** (decided; gated by the M0 spike, see §11).
- Platform: **macOS first** (Apple Silicon primary, Intel secondary). Linux/Windows later; do not hard-code macOS assumptions into the core crates.
- Owner is a solo developer. Favor simple, reviewable code over clever abstractions.

### Why this exists (positioning)

Sonora (github.com/sonorahq/sonora; Rust + GPUI; Apple Music, Spotify, YouTube Music, Deezer, Tidal, Subsonic, local; GPL-3.0) already covers "native + many providers". We will not win on provider count. Our angle:

> **A music library that uses streaming services as sources**, instead of a browser for each service.

Differentiators (priority order):
1. **Unified library.** The same song on Spotify / YouTube Music / local files is *one* track with multiple playable sources. Play from the best available source; fall back automatically if one disappears.
2. **Local-first data.** Play history, ratings, tags, playlists live in a local SQLite DB the user owns, with open export. Switching or cancelling a service costs nothing.
3. **Headless core.** The engine is a library (and later a daemon); the GUI is a thin client. TUI/CLI/remote can come later with no rewrite.

Anything not serving one of these is probably out of MVP scope.

---

## 2. MVP definition

**The MVP is done when** a user can:
1. Point the app at a local music folder and see it in a library.
2. Connect **one remote provider** (Subsonic/Navidrome) and search/play from it.
3. Play tracks gaplessly with seek, volume, queue, and media keys / Now Playing.
4. See a track that exists in **both** local files and the remote provider shown as **one** entry with two sources, and play it from either (manual source preference is enough).
5. Quit and relaunch with all state (library, queue, history) intact.

### Non-goals for MVP (do not build unless asked)
- Smart playlists, recommendations, audio analysis (BPM/key/embeddings), stats dashboards
- Plugin system, scripting, command palette, local HTTP API
- Social features, listen-along, collaborative playlists, sharing links
- EQ, crossfade, bit-perfect/exclusive mode (gapless + normalization only)
- Spotify / YouTube Music / SoundCloud providers (post-MVP; unofficial-API risk)
- Windows/Linux builds, Mac App Store distribution
- Downloading or caching of protected provider streams (see §9)

---

## 3. Architecture overview

Cargo workspace; all crates are flat siblings under `crates/`. Dependencies declared once in `[workspace.dependencies]`; members use `dep.workspace = true`. **Directories are unprefixed; Cargo package names are `cadence-<dir>`** (e.g. `crates/core` → `cadence-core`).

**Create a crate only when it gets code.** M0 creates exactly: `core`, `providers`, `provider-local`, `audio`, `engine`, `app`. `platform-macos` is added at M3 and `provider-subsonic` at M4. No empty scaffolding crates.

```
Cargo.toml                 # workspace root
crates/
  core/                    # domain model, SQLite store, matching/dedup, library service        (M0)
  providers/               # Provider trait + shared types (no concrete providers)              (M0)
  provider-local/          # local files (tags via lofty/symphonia metadata)                     (M0)
  audio/                   # decode, buffering, gapless, output, loudness                        (M0)
  engine/                  # player state machine, queue, glues core + providers + audio;
                           #   defines SecretStore and other platform-facing traits              (M0)
  app/                     # GPUI shell - thin, no business logic                                (M0)
  cli/                     # headless harness: scan/search the library from a terminal            (M1)
  platform-macos/          # Now Playing, media keys, Keychain-backed SecretStore, hotkeys      (M3)
  provider-subsonic/       # Subsonic/Navidrome provider                                         (M4)
```

Dependency direction (never reverse):

```
app -> engine -> { core, providers, audio }
engine -> platform-* (through traits defined in engine)
provider-* -> providers (trait crate) + core types
core -> (nothing internal except small shared types)
```

Rules:
- **No UI types in `core`, `engine`, `audio`, or `providers`.**
- **No provider-specific logic outside `provider-*` crates.** The engine only knows the trait.
- **The audio thread never blocks** on I/O, locks held by UI code, or allocation-heavy work. Communicate with it via lock-free channels / ring buffers.
- Platform code sits behind traits so `core`/`engine` compile and test on any OS.

---

## 4. Domain model

### Core concepts
- **Track** — a canonical song entity (one per distinct recording as we've matched it).
- **TrackSource** — a concrete playable instance of a Track on one provider (`provider`, `provider_track_id`, quality, duration, availability).
- **Artist / Album** — canonical, linked to tracks; provider IDs stored alongside.
- **Play** — a history row (what, when, from which source, how long, skipped or completed).
- **Playlist** — ordered list of Tracks (canonical), *not* of provider IDs. This is what makes fallback work.
- **ProviderAccount** — connection state for a provider; secrets live in the Keychain, never in the DB.

### SQLite schema (initial; use migrations, e.g. `refinery` or `sqlx migrate`)

```sql
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
  version_flags INTEGER NOT NULL DEFAULT 0, -- bitflags: live, remaster, remix, acoustic, instrumental, radio_edit, ...
  created_at INTEGER NOT NULL
);
CREATE INDEX idx_tracks_isrc ON tracks(isrc);

CREATE TABLE track_sources (
  id INTEGER PRIMARY KEY,
  track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
  provider TEXT NOT NULL,                -- 'local', 'spotify', 'ytmusic', ...
  provider_track_id TEXT NOT NULL,       -- or file path for local
  duration_ms INTEGER,
  quality TEXT,                          -- provider-reported, free-form for MVP
  available INTEGER NOT NULL DEFAULT 1,
  last_checked_at INTEGER,
  match_confidence REAL NOT NULL,        -- 0.0..1.0, 1.0 = exact ISRC or manual
  UNIQUE(provider, provider_track_id)
);

CREATE TABLE match_overrides (           -- user manual merge/split decisions always win
  id INTEGER PRIMARY KEY,
  provider TEXT NOT NULL,
  provider_track_id TEXT NOT NULL,
  track_id INTEGER REFERENCES tracks(id) ON DELETE CASCADE, -- NULL = "never merge"
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
  -- NO secrets here. Tokens go in the macOS Keychain.
);

CREATE TABLE queue_items (                -- persisted play queue (MVP item 5: restore on relaunch)
  position INTEGER PRIMARY KEY,
  track_id INTEGER NOT NULL REFERENCES tracks(id)
);
-- Queue cursor (current index + position_ms) lives in the typed settings/state struct, not here.

CREATE TABLE match_suggestions (          -- "maybe the same track" pairs awaiting user confirmation (§7.4)
  provider TEXT NOT NULL,
  provider_track_id TEXT NOT NULL,
  candidate_track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
  score REAL NOT NULL,
  PRIMARY KEY (provider, provider_track_id, candidate_track_id)
);

-- Fast fuzzy search across the library.
-- Plain (NOT contentless) FTS5 table keyed by rowid = tracks.id. Artist/album names live in other
-- tables, so external-content FTS5 doesn't map cleanly; instead the ingest code (same transaction)
-- inserts/updates/deletes the FTS row whenever a track is created, changed, or removed, and
-- re-indexes affected tracks when an artist/album name changes.
-- Do NOT use content='' (contentless): updates/deletes need manual 'delete' commands and rescans desync.
CREATE VIRTUAL TABLE tracks_fts USING fts5(title, artist, album);
```

Treat this as a starting point: propose changes via migrations, don't silently diverge.

---

## 5. Decisions

**Decided**

| Decision | Choice | Notes |
|---|---|---|
| Name | Cadence | Package prefix `cadence-`. |
| UI framework | GPUI + gpui-component (`gpui` 0.2.2, `gpui-component` 0.9) | **Gated by the M0 spike** (§11): must render a window on stable Rust, else toolchain pin + §12 note before any real code. |
| First remote provider | Subsonic/Navidrome (M4) | Stable and open. Spotify/YouTube Music are post-MVP. |
| `librespot` / Spotify playback | **Out** | DRM. See §9. |
| Project license | MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`) | User choice, Nov 3 2026. `cargo-deny` allowlist checks deps. |
| DB access | `rusqlite` behind one writer task | Migrations hand-rolled via `PRAGMA user_version` + numbered SQL migrations (no `refinery`/`sqlx`). |
| Async runtime | `tokio` | One runtime, in-process; no daemon yet. |

**Open (do NOT decide silently; ask the user)**

(Nothing open right now. Re-open an entry here if a settled choice proves wrong.)

If a task is blocked on an open item, say so and propose a default instead of guessing.

---

## 6. Provider abstraction

Providers are **fragile** (APIs change, break, get blocked). Isolate them completely.

```rust
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;                 // "local", "spotify", ...
    fn capabilities(&self) -> Capabilities;     // bitflags below

    async fn authenticate(&self, ctx: &AuthContext) -> Result<(), ProviderError>;
    async fn search(&self, q: &str, kind: SearchKind) -> Result<SearchResults, ProviderError>;
    async fn track(&self, id: &ProviderTrackId) -> Result<RemoteTrack, ProviderError>;
    async fn library_tracks(&self, cursor: Option<Cursor>) -> Result<Page<RemoteTrack>, ProviderError>;
    async fn playlists(&self) -> Result<Vec<RemotePlaylist>, ProviderError>;
    async fn open_stream(&self, id: &ProviderTrackId) -> Result<AudioSource, ProviderError>;
}

bitflags! {
    pub struct Capabilities: u32 {
        const SEARCH = 1; const STREAM = 2; const LIBRARY = 4;
        const PLAYLISTS = 8; const LYRICS = 16; const WRITE_PLAYLISTS = 32;
    }
}

/// What a provider hands the engine. One variant today; kept as an enum so adding a
/// variant later is non-breaking.
#[non_exhaustive]
pub enum AudioSource {
    /// A byte stream symphonia can decode (HTTP, file, etc.)
    Encoded { reader: Box<dyn MediaReader>, hint: Option<FormatHint> },
}
```

Notes:
- Do not add a second `AudioSource` variant without asking the user.
- `ProviderError` must distinguish: `Auth`, `RateLimited`, `NotFound`, `Unavailable` (region/licensing), `Network`, `Unsupported` (capability missing), `Broken` (upstream changed). The engine uses this to decide on fallback vs. surfacing an error.
- Every provider call has a timeout and is cancellable. One provider failing must never freeze the UI or the audio thread.
- Providers return **remote** types (`RemoteTrack`). Only `core`'s matching/ingest step turns them into canonical `Track` + `TrackSource` rows.

---

## 7. Matching / dedup (the heart of the product)

Ingest pipeline for each `RemoteTrack`:

1. **Override check:** if `match_overrides` has an entry, obey it.
2. **ISRC match:** exact ISRC → attach as a source to that Track (confidence 1.0), *unless* version flags differ.
3. **Fuzzy match** (no ISRC or no hit): candidates from FTS on normalized title + primary artist, then score:
   - Normalize: lowercase, strip punctuation/diacritics, remove "(feat. …)", "- Remastered 2011", etc. into **version flags** rather than discarding.
   - Duration must be within ±2 s (tunable) when both are known.
   - Album match is a bonus, not a requirement (singles vs. compilations).
   - **Version flags must be equal** to auto-merge: live ≠ studio, remix ≠ original, instrumental ≠ vocal.
4. Thresholds: `>= 0.90` auto-merge; `0.70..0.90` create a new Track and record it as a **suggested merge** (user can confirm); `< 0.70` new Track.
5. **Wrong merges are worse than missed merges.** When unsure, don't merge.
6. User actions (manual merge/split) are written to `match_overrides` and always win on later re-ingests.

Unit tests are mandatory here, and the matcher + fixtures are built in **M1** (local libraries already contain duplicate copies, so the whole ladder is testable with one provider). Build a fixture set of tricky cases (live vs studio, remaster, feat. variants, non-Latin titles, same title different artist, duration mismatch, missing ISRC).

**Source selection at play time** (MVP): user-preferred provider order (settings) → first `available` source → on `Unavailable`/`NotFound`/`Broken`, mark the source unavailable (`last_checked_at`) and try the next. If none work, report clearly which sources failed and why.

---

## 8. Audio pipeline

Crates (confirm versions before adding): `symphonia` (decode), `cpal` (output; CoreAudio on macOS), `rubato` (resampling), `ebur128` (loudness, optional for MVP — ReplayGain tags acceptable first).

Design:
- Dedicated **decoder thread** per active/next track → writes f32 PCM into a **lock-free ring buffer** → **cpal output callback** reads it. The callback does no allocation, no locking, no I/O.
- **Gapless:** pre-open and pre-decode the next queue item shortly before the current ends; swap at the sample boundary. Respect encoder delay/padding metadata where available.
- **Resampling** to the device rate only when needed.
- **Volume/normalization** applied in the pipeline before output; no clipping (soft limit or headroom).
- Engine ↔ audio communication via a command channel (play, pause, seek, set next, set volume) and an event channel back (position, ended, error). Position updates are throttled (e.g. ~10–20 Hz).
- **Seek** on both local and network sources; network sources may need range-request support. If a source is not seekable, say so in the state, don't fake it.

Engine state machine: `Stopped → Loading → Playing ⇄ Paused → (Ended → next)`, with `Error(reason)` carrying the failed source so the fallback logic can act.

---

## 9. Hard constraints (non-negotiable)

1. **No DRM circumvention.** Do not write code that extracts decryption keys, strips DRM, bypasses subscription/region checks, or rips/saves protected provider streams to disk. If a provider's official or established mechanism requires a paid account, we require it.
2. **No copying from Sonora or other GPL code.** Reading for ideas is fine; write our own implementations. Check every new dependency's license before adding it and flag anything copyleft. The Sonora checkout lives at `../sonora-ref` (its own nested git repo, outside this repo's history); never copy from it into this tree.
3. **Secrets:** tokens, cookies, and credentials go in the **macOS Keychain** (accessed only through the `SecretStore` trait, §10), never in the DB, config files, logs, or git. Add them to `.gitignore` patterns and redact in logs.
4. **Unofficial provider APIs** (e.g. YouTube Music wrappers) carry ToS and breakage risk. Always put them behind the `Provider` trait, document the risk in the provider crate's README, and make them easy to disable. `librespot` is out of scope (DRM).
5. **Do not fabricate API behavior.** If you're unsure how a provider/endpoint/crate works, say so and check the docs or source rather than guessing.
6. **No telemetry** or network calls beyond what a feature explicitly needs.

---

## 10. macOS integration (`platform-macos`)

- **Now Playing + media keys:** `souvlaki` crate (or `objc2` + `MPNowPlayingInfoCenter` / `MPRemoteCommandCenter` directly).
- **Secrets:** `engine` defines a `SecretStore` trait (get/set/delete by key). `platform-macos` implements it with the `keyring` crate (confirm current version, ~4.x, and license: MIT/Apache) using the macOS Keychain. The crate choice is therefore a one-file decision; `security-framework` is the fallback if `keyring` falls short.
- **Global hotkeys:** `global-hotkey`.
- Prevent playback throttling when backgrounded (App Nap / audio session behavior) — test with the window hidden.
- `platform-macos` is created at M3, not before.
- Distribution (later, not MVP): code signing + notarization require an Apple Developer account. Until then, local builds only; document any `xattr` workaround for testers.

---

## 11. Milestones (build in this order)

Each milestone must be runnable and tested before starting the next.

**M0 — Gate, then scaffolding**
0. **GPUI spike — passed (Nov 4 2026).** `gpui` 0.2.2 + `gpui-component` 0.9 build and render a window on stable Rust 1.99 (macOS 27.0, Xcode 27.2). Xcode needs the Metal Toolchain component (`xcodebuild -downloadComponent MetalToolchain`); without it the gpui build script fails compiling `shaders.metal`. No nightly pin needed.
1. `git init` (this repo); Sonora checkout moved to `../sonora-ref`.
2. Workspace with **only** these crates: `core`, `providers`, `provider-local`, `audio`, `engine`, `app` (package names `cadence-*`).
3. Licences check (`deny.toml`, cargo-deny), CI (`cargo fmt --check`, `clippy -D warnings`, `cargo test`), `.gitignore`.
- Done when: the spike result is recorded, and `cargo build` / `cargo test` pass on a clean checkout.

**M1 — Core + local provider + matcher**
- Migrations, DB actor, ingest pipeline, `provider-local` (scan folder, read tags, handle moved/deleted files), FTS search (plain FTS5 table kept in sync in-transaction, §4).
- **Matcher functions (§7) and the fixture suite land here**, exercised on duplicate copies within the local library (same song twice, remaster vs original, live vs studio, feat. variants, etc.).
- Done when: scanning a folder populates the DB; search is instant; re-scan is incremental and doesn't desync FTS (including deletes); the matching fixtures pass.

**M2 — Audio engine**
- Decode + output + queue + gapless + seek + volume, driven by a CLI/test harness (no UI needed).
- Done when: a folder of tracks plays gaplessly, seeks accurately, and survives pause/resume and device changes without crashing or glitching.

**M3 — Minimal UI + macOS integration**
- Add `platform-macos`. Library list, search box, now-playing bar, queue; media keys and Now Playing; queue/state restored on relaunch (`queue_items`).
- Done when: §2 items 1, 3, and 5 hold.

**M4 — First remote provider (Subsonic/Navidrome)**
- Add `provider-subsonic`. Auth with credentials in the Keychain via `SecretStore`, search, library import, `open_stream`, error mapping to `ProviderError`.
- Done when: user can connect, search, and play from the server; failures degrade gracefully.

**M5 — Cross-provider unification**
- The matcher already exists (M1). This milestone only wires: cross-provider ingest (local + Subsonic into the same canonical tracks), `match_suggestions` + manual merge/split UI (`match_overrides`), source-preference setting, and fallback on source failure.
- Done when: §2 item 4 holds with real data from both providers.

Post-MVP candidates (do not start unprompted): Spotify/YouTube Music providers, smart playlists, missing-track detective, playlist history, stats, command palette, plugin system, daemon/CLI split.

---

## 12. Engineering conventions

- **Rust:** toolchain pinned in `rust-toolchain.toml` (stable, unless the M0 GPUI spike proves nightly is required; record the result here); edition 2021+; `#![forbid(unsafe_code)]` on all our crates (dependencies' unsafe is not ours). If a crate genuinely can't comply (e.g. platform FFI), raise it with the user rather than relaxing it silently.
- **Naming:** directories under `crates/` are unprefixed; Cargo package names are `cadence-<dir>`.
- **Errors:** `thiserror` in libraries, `anyhow` only in binaries/tests. No `unwrap()`/`expect()` outside tests except for proven invariants with a message.
- **Logging:** `tracing` with structured fields; never log secrets or full URLs containing tokens.
- **Config:** a single typed settings struct, stored in the platform config dir (`directories` crate), versioned.
- **Testing:** unit tests for matching/normalization; integration tests for ingest + DB; provider tests use recorded fixtures, never live credentials in CI. Audio tests use short generated WAV/FLAC fixtures.
- **Commits/PRs:** small, one concern each; explain the *why*. Update this file when an open decision in §5 is resolved or architecture changes.
- **Dependencies:** minimal and justified; prefer well-maintained crates; pin via the workspace; run `cargo deny` and `cargo audit`.
- **Performance targets (soft):** cold start under ~1 s to a usable window; search results under ~50 ms for a 50k-track library; idle CPU near 0%; no audio underruns during UI interaction.

---

## 13. How to work with the user

- Ask before big architectural choices (§5) or adding major dependencies.
- When you make an assumption, state it in one line.
- Prefer a working vertical slice over broad scaffolding.
- If something would violate §9, stop and say why instead of finding a workaround.
- If a request seems outside MVP scope (§2 non-goals), mention it and ask whether to proceed.
- Keep explanations short; the user will ask for depth when needed.

---

## 14. Glossary

- **Canonical track:** our own deduplicated Track row; the unit users see and playlists reference.
- **Source:** one provider's playable copy of a canonical track.
- **Provider:** an integration that can search/list/stream (local, Subsonic, Spotify, ...).
- **Fallback:** automatically trying another source when the preferred one fails.
- **ISRC:** International Standard Recording Code; a recording-level ID, the best cross-provider match key when present.
- **Version flags:** attributes (live, remaster, remix...) that make two same-titled tracks *different* recordings.
