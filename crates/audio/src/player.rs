//! The playback worker: owns the decoder, the resampler, the output stream and the clock.
//!
//! Everything runs on one thread; the caller talks to it with [`Command`]s and reads [`Event`]s.
//! Nothing here blocks the audio callback.
//!
//! Gapless: the caller pre-loads the next track with [`Command::SetNext`]; when the current file
//! ends, the worker swaps decoders and keeps feeding the same ring buffer, so there is no silence
//! between tracks.

use std::path::PathBuf;
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

/// One track to play, tagged with the caller's own id so events can refer to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackRequest {
    pub path: PathBuf,
    /// Opaque to the audio crate; echoed back in [`Event::Playing`].
    pub id: u64,
}

/// What the caller asks the playback thread to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Load this track and play it from the start, discarding anything already loaded.
    Play(TrackRequest),
    /// Pre-load the track that follows. The worker keeps it open and switches to it at the
    /// exact end of the current one. `None` clears it.
    SetNext(Option<TrackRequest>),
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
    /// A track started (or, mid-stream, that the next one took over).
    Playing {
        id: u64,
        duration_ms: Option<u64>,
    },
    Paused,
    Resumed,
    Stopped,
    /// The queue ran dry; playback has stopped.
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

/// A track waiting to take over.
struct Prepared {
    request: TrackRequest,
    decoder: Decoder,
}

struct Worker {
    events: Sender<Event>,
    output: Option<Output>,
    decoder: Option<Decoder>,
    resampler: Option<Resampler>,
    next: Option<Prepared>,
    /// Samples decoded but not yet accepted by the ring.
    pending: Vec<f32>,
    volume: f32,
    /// Track position of the first frame currently in flight.
    anchor_ms: u64,
    /// Output clock reading when that frame was queued.
    anchor_frames: u64,
    /// Frames handed to the output since the last flush.
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
            next: None,
            pending: Vec::new(),
            volume: 1.0,
            anchor_ms: 0,
            anchor_frames: 0,
            pushed_frames: 0,
            playing: false,
            finished: false,
            last_position: Instant::now(),
        }
    }

    fn send(&self, event: Event) {
        let _ = self.events.send(event);
    }

    fn fail(&mut self, message: String) {
        self.send(Event::Error(message));
        self.finished = true;
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Play(request) => self.play(request),
            Command::SetNext(request) => self.set_next(request),
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

    fn play(&mut self, request: TrackRequest) {
        self.reset();
        self.playing = false;

        let decoder = match Decoder::open(&request.path) {
            Ok(decoder) => decoder,
            Err(error) => {
                self.send(Event::Error(error.to_string()));
                return;
            }
        };
        if let Err(error) = self.ensure_output() {
            self.send(Event::Error(error.to_string()));
            return;
        }
        let resume_error = match self.output.as_ref() {
            Some(output) => output.resume().err(),
            None => {
                self.send(Event::Error("no output device".into()));
                return;
            }
        };
        if let Some(error) = resume_error {
            self.send(Event::Error(error.to_string()));
            return;
        }
        if let Some(output) = self.output.as_mut() {
            output.flush();
        }

        self.playing = true;
        self.last_position = Instant::now();
        self.install(Prepared { request, decoder });
    }

    fn set_next(&mut self, request: Option<TrackRequest>) {
        self.next = None;
        let Some(request) = request else {
            return;
        };
        if self.decoder.is_none() {
            return;
        }
        match Decoder::open(&request.path) {
            Ok(decoder) => self.next = Some(Prepared { request, decoder }),
            Err(error) => self.send(Event::Error(error.to_string())),
        }
    }

    /// Make `prepared` the current track. The consumer needs no gap: the ring is untouched.
    fn install(&mut self, prepared: Prepared) -> bool {
        let Prepared { request, decoder } = prepared;
        let device_rate = match self.output.as_ref() {
            Some(output) => output.rate(),
            None => return false,
        };

        self.resampler = if decoder.rate() == device_rate {
            None
        } else {
            match Resampler::new(decoder.rate(), device_rate) {
                Ok(resampler) => Some(resampler),
                Err(error) => {
                    self.fail(error.to_string());
                    return false;
                }
            }
        };

        let duration_ms = decoder.duration_ms();
        self.decoder = Some(decoder);
        self.anchor_ms = 0;
        self.anchor_frames = self
            .output
            .as_ref()
            .map_or(0, crate::output::Output::played_frames);
        self.finished = false;
        self.send(Event::Playing {
            id: request.id,
            duration_ms,
        });
        true
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
        self.finished = false;
        if let Some(output) = self.output.as_mut() {
            output.flush();
        }
        self.pushed_frames = 0;
        self.anchor_ms = ms;
        self.anchor_frames = 0;
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
                if self.finished {
                    self.finish_if_drained();
                    return busy;
                }
                if self
                    .output
                    .as_ref()
                    .is_some_and(|output| output.free_slots() < MIN_FREE_SAMPLES)
                {
                    return busy;
                }
                if !self.produce() {
                    if self.finished {
                        self.finish_if_drained();
                    }
                    return busy;
                }
                if self.pending.is_empty() {
                    continue;
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

    /// Decode one chunk into `pending`, or hand over to the next track at end of file.
    /// Returns `true` if it made progress.
    fn produce(&mut self) -> bool {
        if self.finished || self.decoder.is_none() {
            return false;
        }
        let chunk = match self.decoder.as_mut() {
            Some(decoder) => decoder.next_chunk(),
            None => return false,
        };

        match chunk {
            Ok(Some(chunk)) => {
                let samples = match self.resampler.as_mut() {
                    Some(resampler) => match resampler.push(&chunk.samples) {
                        Ok(samples) => samples,
                        Err(error) => {
                            self.fail(error.to_string());
                            return false;
                        }
                    },
                    None => chunk.samples,
                };
                self.push_samples(samples);
                true
            }
            Ok(None) => {
                // Flush the resampler's delay line before the next track takes over.
                if let Some(tail) = self.take_tail() {
                    self.push_samples(tail);
                }
                match self.next.take() {
                    Some(prepared) => self.install(prepared),
                    None => {
                        self.finished = true;
                        false
                    }
                }
            }
            Err(error) => {
                self.fail(error.to_string());
                false
            }
        }
    }

    fn take_tail(&mut self) -> Option<Vec<f32>> {
        let resampler = self.resampler.as_mut()?;
        match resampler.flush() {
            Ok(samples) => (!samples.is_empty()).then_some(samples),
            Err(error) => {
                self.send(Event::Error(error.to_string()));
                None
            }
        }
    }

    /// Append samples and apply gain to just those samples.
    fn push_samples(&mut self, samples: Vec<f32>) {
        if samples.is_empty() {
            return;
        }
        let start = self.pending.len();
        self.pending.extend(samples);
        if (self.volume - 1.0).abs() >= f32::EPSILON {
            for sample in &mut self.pending[start..] {
                *sample = (*sample * self.volume).clamp(-1.0, 1.0);
            }
        }
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
        let played = output.played_frames().saturating_sub(self.anchor_frames);
        let ms = self.anchor_ms + played * 1000 / u64::from(output.rate().max(1));
        self.send(Event::Position { ms });
    }

    /// Drop the decoder and buffered audio, keeping the device open.
    fn reset(&mut self) {
        self.decoder = None;
        self.resampler = None;
        self.next = None;
        self.pending.clear();
        self.pushed_frames = 0;
        self.anchor_ms = 0;
        self.anchor_frames = 0;
        self.finished = false;
        if let Some(output) = self.output.as_mut() {
            output.flush();
        }
    }
}
