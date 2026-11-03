#![forbid(unsafe_code)]
//! Player state machine, queue, playback orchestration.
//!
//! The engine owns the store and the audio handle. The UI calls intent methods (`toggle`,
//! `next`, …) and reads state; [`Engine::poll`] pumps the audio thread's events. No UI types
//! cross this boundary (AGENTS.md §3).
//!
//! Source selection is deliberately blunt for the MVP: [`cadence_core::store::Store::playback_source`]
//! picks the preferred available source, and a track with none is skipped. Per-source fallback
//! on failure arrives with the second provider (M5).

mod queue;

pub use queue::Queue;

use std::path::Path;

use cadence_audio::{Command, Event, Player, TrackRequest};
use cadence_core::store::{Store, TrackHit};

/// What playback is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Nothing loaded.
    Idle,
    /// A track was requested and has not started yet.
    Loading,
    Playing,
    Paused,
}

/// Errors from the engine's own operations. Audio and UI errors are reported through
/// [`Engine::last_error`] instead, so a bad file never takes the app down.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Core(#[from] cadence_core::Error),
    #[error(transparent)]
    Audio(#[from] cadence_audio::Error),
    #[error("the audio thread stopped")]
    AudioThreadGone,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A play row that has been opened but not yet closed.
struct OpenPlay {
    row_id: i64,
    duration_ms: Option<i64>,
}

pub struct Engine {
    store: Store,
    player: Player,
    queue: Queue,
    state: State,
    now_playing: Option<TrackHit>,
    position_ms: u64,
    open_play: Option<OpenPlay>,
    last_error: Option<String>,
}

impl Engine {
    /// Open (or create) the library at `db_path` and start the audio thread.
    pub fn open(db_path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::new(Store::open(db_path)?, Player::new()?))
    }

    /// Build an engine around an existing store and player.
    pub fn new(store: Store, player: Player) -> Self {
        Self {
            store,
            player,
            queue: Queue::new(),
            state: State::Idle,
            now_playing: None,
            position_ms: 0,
            open_play: None,
            last_error: None,
        }
    }

    /// Load the persisted queue. Does not start playback.
    pub fn restore_queue(&mut self) -> Result<()> {
        let ids = self.store.load_queue()?;
        self.queue.set(&ids);
        Ok(())
    }

    /// Replace the queue and persist it.
    pub fn set_queue(&mut self, track_ids: &[i64]) -> Result<()> {
        self.queue.set(track_ids);
        self.store.save_queue(track_ids)?;
        // Whatever was pre-loaded belongs to the old queue.
        self.player.send(Command::SetNext(None));
        Ok(())
    }

    /// The queue, for display.
    pub fn queue(&self) -> &Queue {
        &self.queue
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// Metadata for the track that is playing or paused.
    pub fn now_playing(&self) -> Option<&TrackHit> {
        self.now_playing.as_ref()
    }

    /// How far into the current track playback is.
    pub fn position_ms(&self) -> u64 {
        self.position_ms
    }

    /// The last thing that went wrong, until the next successful action.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Pause, resume, or start the current queue position.
    pub fn toggle(&mut self) {
        match self.state {
            State::Playing => {
                self.player.send(Command::Pause);
            }
            State::Paused => {
                self.player.send(Command::Resume);
            }
            State::Idle => {
                let start = self.queue.cursor().unwrap_or(0);
                self.start_forward(start);
            }
            State::Loading => {}
        }
    }

    /// Play the track at `index`.
    pub fn play_index(&mut self, index: usize) {
        self.start_forward(index);
    }

    /// Move to the next queue item.
    pub fn next(&mut self) {
        let start = self.queue.cursor().map_or(0, |cursor| cursor + 1);
        self.start_forward(start);
    }

    /// Move to the previous queue item (staying put at the start).
    pub fn previous(&mut self) {
        let start = self
            .queue
            .cursor()
            .map_or(0, |cursor| cursor.saturating_sub(1));
        self.start_forward(start);
    }

    /// Jump inside the current track.
    pub fn seek(&mut self, ms: u64) {
        if self.state == State::Playing || self.state == State::Paused {
            self.position_ms = ms;
            self.player.send(Command::Seek(ms));
        }
    }

    /// 0.0..=1.0.
    pub fn set_volume(&mut self, volume: f32) {
        self.player.send(Command::SetVolume(volume));
    }

    /// Drain audio events and update the state. Call this from the UI's frame loop.
    pub fn poll(&mut self) {
        while let Some(event) = self.player.try_recv() {
            self.handle(event);
        }
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::Playing { id, duration_ms } => {
                self.close_play(false);
                let index = id as usize;
                self.queue.set_cursor(index);
                let track_id = self.queue.at(index);
                self.now_playing = track_id.and_then(|id| self.store.track_hit(id).ok().flatten());
                self.position_ms = 0;
                self.state = State::Playing;
                if let Some(track_id) = track_id
                    && let Ok(row_id) = self.store.start_play(track_id, None)
                {
                    self.open_play = Some(OpenPlay {
                        row_id,
                        duration_ms: duration_ms.map(|ms| ms as i64),
                    });
                }
                // The worker just consumed its prepared track; prepare the one after it.
                self.preload(index + 1);
            }
            Event::Paused => self.state = State::Paused,
            Event::Resumed => self.state = State::Playing,
            Event::Stopped => {
                self.close_play(false);
                self.state = State::Idle;
                self.position_ms = 0;
            }
            Event::Ended => {
                self.close_play(true);
                self.state = State::Idle;
            }
            Event::Position { ms } => self.position_ms = ms,
            Event::Error(message) => {
                self.close_play(false);
                self.last_error = Some(message);
                self.state = State::Idle;
            }
        }
    }

    /// Start at `from` and walk forward until a track actually has a playable source.
    fn start_forward(&mut self, from: usize) {
        for index in from..self.queue.len() {
            if !self.queue.set_cursor(index) {
                break;
            }
            if self.start(index) {
                return;
            }
        }
        self.state = State::Idle;
    }

    fn start(&mut self, index: usize) -> bool {
        let Some(track_id) = self.queue.at(index) else {
            return false;
        };
        let source = match self.store.playback_source(track_id) {
            Ok(Some(source)) => source,
            Ok(None) => {
                self.last_error = Some(format!("track {track_id} has no playable source"));
                return false;
            }
            Err(error) => {
                self.last_error = Some(error.to_string());
                return false;
            }
        };

        self.last_error = None;
        self.position_ms = 0;
        self.state = State::Loading;
        let sent = self.player.send(Command::Play(TrackRequest {
            path: source.uri.into(),
            id: index as u64,
        }));
        if !sent {
            self.last_error = Some(Error::AudioThreadGone.to_string());
            self.state = State::Idle;
        }
        sent
    }

    /// Hand the worker the track after `index` so the swap is gapless.
    fn preload(&self, index: usize) {
        let request = self.queue.at(index).and_then(|track_id| {
            let source = self.store.playback_source(track_id).ok().flatten()?;
            Some(TrackRequest {
                path: source.uri.into(),
                id: index as u64,
            })
        });
        self.player.send(Command::SetNext(request));
    }

    fn close_play(&mut self, ended: bool) {
        let Some(open) = self.open_play.take() else {
            return;
        };
        let played = self.position_ms.min(i64::MAX as u64) as i64;
        let completed = ended || counts_as_played(played, open.duration_ms);
        if let Err(error) = self.store.finish_play(open.row_id, played, completed) {
            self.last_error = Some(error.to_string());
        }
    }
}

/// A play counts when it covered most of the track (the usual scrobble rule).
fn counts_as_played(played_ms: i64, duration_ms: Option<i64>) -> bool {
    duration_ms.is_some_and(|duration| duration > 0 && played_ms.saturating_mul(10) >= duration * 9)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadence_core::ingest::TrackInput;

    #[test]
    fn a_play_counts_only_once_it_covers_most_of_the_track() {
        assert!(counts_as_played(100_000, Some(100_000)));
        assert!(counts_as_played(90_000, Some(100_000)));
        assert!(!counts_as_played(89_999, Some(100_000)));
        assert!(!counts_as_played(5_000, None));
        assert!(!counts_as_played(5_000, Some(0)));
    }

    fn seed_store() -> Store {
        let mut store = Store::open_in_memory().unwrap();
        for (title, duration_ms) in [("First", 1_000), ("Second", 2_000)] {
            store
                .ingest(&TrackInput {
                    provider: "local".into(),
                    provider_track_id: format!("/music/{title}.wav"),
                    title: title.into(),
                    artist: "Tester".into(),
                    album: None,
                    album_artist: None,
                    year: None,
                    duration_ms: Some(duration_ms),
                    isrc: None,
                    musicbrainz_id: None,
                    quality: None,
                })
                .unwrap();
        }
        store
    }

    #[test]
    fn audio_events_move_the_cursor_and_close_play_rows() {
        let store = seed_store();
        let ids: Vec<i64> = (1..=2).collect();
        let mut engine = Engine::new(store, Player::new().unwrap());
        engine.set_queue(&ids).unwrap();

        // The worker starts the first track (position 0).
        engine.handle(Event::Playing {
            id: 0,
            duration_ms: Some(1_000),
        });
        assert_eq!(engine.state(), State::Playing);
        assert_eq!(engine.queue().cursor(), Some(0));
        assert_eq!(engine.now_playing().unwrap().title, "First");

        // Gapless hand-off to the second track closes the first play row.
        engine.position_ms = 1_000;
        engine.handle(Event::Playing {
            id: 1,
            duration_ms: Some(2_000),
        });
        assert_eq!(engine.queue().cursor(), Some(1));
        assert_eq!(engine.now_playing().unwrap().title, "Second");

        engine.position_ms = 2_000;
        engine.handle(Event::Ended);
        assert_eq!(engine.state(), State::Idle);

        let plays: Vec<(i64, i64, bool)> = engine
            .store
            .history(10)
            .unwrap()
            .into_iter()
            .map(|play| (play.track_id, play.played_ms, play.completed))
            .collect();
        assert_eq!(plays, vec![(2, 2_000, true), (1, 1_000, true)]);
    }

    #[test]
    fn a_skipped_track_is_recorded_as_incomplete() {
        let store = seed_store();
        let mut engine = Engine::new(store, Player::new().unwrap());
        engine.set_queue(&[1, 2]).unwrap();

        engine.handle(Event::Playing {
            id: 0,
            duration_ms: Some(1_000),
        });
        engine.position_ms = 100;
        engine.handle(Event::Playing {
            id: 1,
            duration_ms: Some(2_000),
        });

        let plays = engine.store.history(10).unwrap();
        let play = plays.last().expect("the skipped track was recorded");
        assert_eq!(play.track_id, 1);
        assert_eq!(play.played_ms, 100);
        assert!(!play.completed);
    }
}
