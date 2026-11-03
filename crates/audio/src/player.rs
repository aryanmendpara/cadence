//! The playback worker: owns the decoder, the resampler, the output stream and the clock.
//!
//! Everything runs on one thread; the caller talks to it with [`Command`]s and reads [`Event`]s.
//! Nothing here blocks the audio callback.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crate::decode::Decoder;
use crate::output::Output;
use crate::resample::Resampler;
use crate::{Error, Result};

/// How often position updates are emitted while playing.
const POSITION_INTERVAL: Duration = Duration::from_millis(200);
/// Don't decode while the ring is this full (samples).
const MIN_FREE_SAMPLES: usize = 8_192;

/// What the caller asks the playback thread to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Load `path` and play it from the start.
    Play(PathBuf),
    Pause,
    Resume,
    Stop,
    /// Seek to a position inside the current track.
    Seek(u64),
    /// 0.0..=1.0; applied on the worker thread, not in the callback.
    SetVolume(f32),
}

/// What the playback thread reports back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A track opened successfully and is now playing.
    Playing {
        duration_ms: Option<u64>,
    },
    Paused,
    Resumed,
    Stopped,
    /// The track reached its end; playback has stopped.
    Ended,
    Position {
        ms: u64,
    },
    Error(String),
}

/// Handle to the playback thread. Dropping it stops playback.
pub struct Player {
    commands: Sender<Command>,
    events: Receiver<Event>,
}

impl Player {
    /// Spawn the audio thread. The device is opened on first playback, so this cannot fail on
    /// a machine without audio hardware.
    pub fn new() -> Result<Self> {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        thread::Builder::new()
            .name("cadence-audio".into())
            .spawn(move || run(command_rx, event_tx))
            .map_err(|error| Error::Thread(error.to_string()))?;
        Ok(Self {
            commands: command_tx,
            events: event_rx,
        })
    }

    /// Queue a command. Returns `false` if the audio thread is gone.
    pub fn send(&self, command: Command) -> bool {
        self.commands.send(command).is_ok()
    }

    /// The next queued event, if any.
    pub fn try_recv(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    /// Wait up to `timeout` for the next event.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Event> {
        self.events.recv_timeout(timeout).ok()
    }
}

fn run(commands: Receiver<Command>, events: Sender<Event>) {
    let mut worker = Worker::new(events);
    loop {
        let mut busy = false;
        loop {
            match commands.try_recv() {
                Ok(command) => {
                    worker.handle(command);
                    busy = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        busy |= worker.pump();
        worker.emit_position();
        if busy {
            continue;
        }

        // Nothing to do: sleep until a command arrives. Short timeout while playing so the
        // ring gets topped up as it drains.
        let timeout = if worker.playing {
            Duration::from_millis(10)
        } else {
            Duration::from_millis(100)
        };
        match commands.recv_timeout(timeout) {
            Ok(command) => worker.handle(command),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

struct Worker {
    events: Sender<Event>,
    output: Option<Output>,
    decoder: Option<Decoder>,
    resampler: Option<Resampler>,
    /// Samples decoded but not yet accepted by the ring.
    pending: Vec<f32>,
    volume: f32,
    /// Track position at the moment the output clock was last reset.
    base_ms: u64,
    /// Frames handed to the output since that reset.
    pushed_frames: u64,
    playing: bool,
    finished: bool,
    last_position: Instant,
}

impl Worker {
    fn new(events: Sender<Event>) -> Self {
        Self {
            events,
            output: None,
            decoder: None,
            resampler: None,
            pending: Vec::new(),
            volume: 1.0,
            base_ms: 0,
            pushed_frames: 0,
            playing: false,
            finished: false,
            last_position: Instant::now(),
        }
    }

    fn send(&self, event: Event) {
        let _ = self.events.send(event);
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Play(path) => self.play(&path),
            Command::Pause => {
                if self.playing {
                    self.playing = false;
                    if let Some(output) = &self.output
                        && let Err(error) = output.pause()
                    {
                        self.send(Event::Error(error.to_string()));
                    }
                    self.send(Event::Paused);
                }
            }
            Command::Resume => {
                if !self.playing && self.decoder.is_some() && !self.finished {
                    self.playing = true;
                    if let Some(output) = &self.output
                        && let Err(error) = output.resume()
                    {
                        self.send(Event::Error(error.to_string()));
                    }
                    self.send(Event::Resumed);
                }
            }
            Command::Stop => {
                self.reset();
                self.playing = false;
                self.send(Event::Stopped);
            }
            Command::Seek(ms) => self.seek(ms),
            Command::SetVolume(volume) => self.volume = volume.clamp(0.0, 1.0),
        }
    }

    fn play(&mut self, path: &Path) {
        self.reset();
        self.playing = false;

        let decoder = match Decoder::open(path) {
            Ok(decoder) => decoder,
            Err(error) => {
                self.send(Event::Error(error.to_string()));
                return;
            }
        };
        let duration_ms = decoder.duration_ms();

        if let Err(error) = self.ensure_output() {
            self.send(Event::Error(error.to_string()));
            return;
        }
        let (device_rate, resume_error) = match self.output.as_ref() {
            Some(output) => (output.rate(), output.resume().err()),
            None => {
                self.send(Event::Error("no output device".into()));
                return;
            }
        };
        if let Some(error) = resume_error {
            self.send(Event::Error(error.to_string()));
            return;
        }

        self.resampler = if decoder.rate() == device_rate {
            None
        } else {
            match Resampler::new(decoder.rate(), device_rate) {
                Ok(resampler) => Some(resampler),
                Err(error) => {
                    self.send(Event::Error(error.to_string()));
                    return;
                }
            }
        };

        if let Some(output) = self.output.as_mut() {
            output.flush();
        }

        self.base_ms = 0;
        self.pushed_frames = 0;
        self.finished = false;
        self.decoder = Some(decoder);
        self.playing = true;
        self.last_position = Instant::now();
        self.send(Event::Playing { duration_ms });
    }

    fn seek(&mut self, ms: u64) {
        let Some(decoder) = self.decoder.as_mut() else {
            return;
        };
        if let Err(error) = decoder.seek(ms) {
            self.send(Event::Error(error.to_string()));
            return;
        }
        if let Some(resampler) = self.resampler.as_mut() {
            resampler.reset();
        }
        self.pending.clear();
        self.base_ms = ms;
        self.pushed_frames = 0;
        self.finished = false;
        if let Some(output) = self.output.as_mut() {
            output.flush();
        }
        self.last_position = Instant::now();
        self.send(Event::Position { ms });
    }

    fn ensure_output(&mut self) -> Result<()> {
        if self.output.is_none() {
            self.output = Some(Output::open()?);
        }
        Ok(())
    }

    /// Decode and enqueue as much as the ring accepts. Returns `true` if anything moved.
    fn pump(&mut self) -> bool {
        let mut busy = false;
        if self.output.is_none() {
            return busy;
        }

        loop {
            if self.pending.is_empty() {
                if self
                    .output
                    .as_ref()
                    .is_some_and(|output| output.free_slots() < MIN_FREE_SAMPLES)
                {
                    return busy;
                }
                match self.next_samples() {
                    Some(samples) => self.pending = samples,
                    None => {
                        if self.finished {
                            self.finish_if_drained();
                        }
                        return busy;
                    }
                }
                if self.pending.is_empty() {
                    return busy;
                }
            }

            let Some(output) = self.output.as_mut() else {
                return busy;
            };
            let written = output.write(&self.pending).min(self.pending.len());
            if written == 0 {
                return busy;
            }
            busy = true;
            self.pushed_frames += (written / 2) as u64;
            self.pending.drain(..written);
            if !self.pending.is_empty() {
                return busy;
            }
        }
    }

    /// One chunk of stereo samples, resampled and volume-adjusted.
    fn next_samples(&mut self) -> Option<Vec<f32>> {
        if self.finished {
            return None;
        }
        let chunk = match self.decoder.as_mut()?.next_chunk() {
            Ok(Some(chunk)) => chunk,
            Ok(None) => return self.tail(),
            Err(error) => {
                self.send(Event::Error(error.to_string()));
                self.finished = true;
                return None;
            }
        };

        let samples = match self.resampler.as_mut() {
            Some(resampler) => match resampler.push(&chunk.samples) {
                Ok(samples) => samples,
                Err(error) => {
                    self.send(Event::Error(error.to_string()));
                    self.finished = true;
                    return None;
                }
            },
            None => chunk.samples,
        };
        Some(self.apply_volume(samples))
    }

    /// End of file: hand over whatever the resampler still holds, then mark the track finished.
    fn tail(&mut self) -> Option<Vec<f32>> {
        self.finished = true;
        let samples = match self.resampler.as_mut() {
            Some(resampler) => match resampler.flush() {
                Ok(samples) => samples,
                Err(error) => {
                    self.send(Event::Error(error.to_string()));
                    return None;
                }
            },
            None => Vec::new(),
        };
        (!samples.is_empty()).then(|| self.apply_volume(samples))
    }

    fn apply_volume(&self, samples: Vec<f32>) -> Vec<f32> {
        if (self.volume - 1.0).abs() < f32::EPSILON {
            return samples;
        }
        samples
            .into_iter()
            .map(|sample| (sample * self.volume).clamp(-1.0, 1.0))
            .collect()
    }

    fn finish_if_drained(&mut self) {
        let drained = self
            .output
            .as_ref()
            .is_some_and(|output| output.played_frames() >= self.pushed_frames);
        if !drained {
            return;
        }
        self.playing = false;
        self.send(Event::Ended);
        self.reset();
    }

    fn emit_position(&mut self) {
        if !self.playing || self.last_position.elapsed() < POSITION_INTERVAL {
            return;
        }
        self.last_position = Instant::now();
        let Some(output) = self.output.as_ref() else {
            return;
        };
        let played_ms = output.played_frames() * 1000 / u64::from(output.rate().max(1));
        self.send(Event::Position {
            ms: self.base_ms + played_ms,
        });
    }

    /// Drop the decoder and buffered audio, keeping the device open.
    fn reset(&mut self) {
        self.decoder = None;
        self.resampler = None;
        self.pending.clear();
        self.pushed_frames = 0;
        self.base_ms = 0;
        self.finished = false;
        if let Some(output) = self.output.as_mut() {
            output.flush();
        }
    }
}
