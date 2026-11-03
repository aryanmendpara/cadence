#![forbid(unsafe_code)]
//! Audio: decode with symphonia, resample with rubato, output with cpal.
//!
//! Threading contract (AGENTS.md §8): the cpal callback only copies from a lock-free ring
//! buffer. Decoding, resampling and command handling happen on the player thread. The UI
//! never touches either.

mod decode;
mod output;
mod player;
mod resample;

pub use decode::{Chunk, Decoder};
pub use player::{Command, Event, Player, TrackRequest};
pub use resample::Resampler;

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot open {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("decode error: {0}")]
    Decode(#[from] symphonia::core::errors::Error),
    #[error("unsupported audio: {0}")]
    Unsupported(String),
    #[error("no output device")]
    NoOutputDevice,
    #[error("audio device: {0}")]
    Device(#[from] cpal::Error),
    #[error("resample: {0}")]
    Resample(String),
    #[error("cannot start the audio thread: {0}")]
    Thread(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
