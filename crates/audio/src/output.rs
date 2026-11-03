//! cpal output: one stream, one lock-free ring buffer, and a clock.
//!
//! The device callback does exactly three things: drain the ring, count delivered frames, and
//! honour a flush epoch. No allocation, no locks, no I/O (AGENTS.md §8).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::{Error, Result};

/// One second of stereo headroom. Enough to absorb a scheduling hiccup, short enough that a
/// seek does not feel laggy.
const BUFFER_SECONDS: usize = 1;
/// How long [`Output::flush`] waits for the device callback before giving up (paused stream,
/// dead device).
const FLUSH_ACK_TIMEOUT: Duration = Duration::from_millis(50);

/// Shared with the callback. `played` only grows; `flushed_at` marks the value at which the
/// current epoch began, so the player can read "frames heard since the last flush".
pub(crate) struct Clock {
    epoch: AtomicU64,
    /// Bumped by the callback once it has acted on `epoch`.
    acked: AtomicU64,
    played: AtomicU64,
    flushed_at: AtomicU64,
}

impl Clock {
    fn new() -> Self {
        Self {
            epoch: AtomicU64::new(0),
            acked: AtomicU64::new(0),
            played: AtomicU64::new(0),
            flushed_at: AtomicU64::new(0),
        }
    }
}

pub(crate) struct Output {
    stream: cpal::Stream,
    producer: rtrb::Producer<f32>,
    clock: Arc<Clock>,
    rate: u32,
}

impl Output {
    /// Open the default output device at its preferred rate and start the stream (silent until
    /// something is pushed).
    pub fn open() -> Result<Self> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or(Error::NoOutputDevice)?;
        let supported = device.default_output_config()?;
        let format = supported.sample_format();
        let config = supported.config();
        let rate = config.sample_rate;
        let channels = usize::from(config.channels);

        let capacity = rate as usize * channels * BUFFER_SECONDS;
        let (producer, consumer) = rtrb::RingBuffer::<f32>::new(capacity);
        let clock = Arc::new(Clock::new());

        let stream = match format {
            cpal::SampleFormat::F32 => build::<f32>(&device, &config, consumer, clock.clone()),
            cpal::SampleFormat::I16 => build::<i16>(&device, &config, consumer, clock.clone()),
            cpal::SampleFormat::U16 => build::<u16>(&device, &config, consumer, clock.clone()),
            other => Err(Error::Unsupported(format!(
                "output device wants unsupported sample format {other:?}"
            ))),
        }?;
        stream.play()?;

        Ok(Self {
            stream,
            producer,
            clock,
            rate,
        })
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Samples that fit in the ring right now.
    pub fn free_slots(&self) -> usize {
        self.producer.slots()
    }

    /// Push as much of `samples` as fits; returns how many samples were accepted.
    pub fn write(&mut self, samples: &[f32]) -> usize {
        let mut written = 0;
        for &sample in samples {
            if self.producer.push(sample).is_err() {
                break;
            }
            written += 1;
        }
        written
    }

    /// Drop buffered audio and restart the frame clock. The callback empties the ring when it
    /// observes the new epoch, so no device rebuild is needed for a seek or a new track.
    ///
    /// Waits for the callback to acknowledge, so audio pushed afterwards is never dropped.
    pub fn flush(&mut self) {
        let epoch = self.clock.epoch.fetch_add(1, Ordering::Release) + 1;
        let deadline = Instant::now() + FLUSH_ACK_TIMEOUT;
        while self.clock.acked.load(Ordering::Acquire) < epoch && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Frames actually delivered to the device since the last [`flush`](Self::flush).
    pub fn played_frames(&self) -> u64 {
        let played = self.clock.played.load(Ordering::Relaxed);
        let flushed_at = self.clock.flushed_at.load(Ordering::Relaxed);
        played.saturating_sub(flushed_at)
    }

    pub fn pause(&self) -> Result<()> {
        Ok(self.stream.pause()?)
    }

    pub fn resume(&self) -> Result<()> {
        Ok(self.stream.play()?)
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut consumer: rtrb::Consumer<f32>,
    clock: Arc<Clock>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let channels = usize::from(config.channels);
    let mut epoch = clock.epoch.load(Ordering::Acquire);

    let stream = device.build_output_stream(
        *config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let current = clock.epoch.load(Ordering::Acquire);
            if current != epoch {
                epoch = current;
                while consumer.pop().is_ok() {}
                clock
                    .flushed_at
                    .store(clock.played.load(Ordering::Relaxed), Ordering::Relaxed);
                clock.acked.store(epoch, Ordering::Release);
            }

            let mut frames = 0u64;
            for frame in data.chunks_mut(channels) {
                if consumer.slots() < channels {
                    for sample in frame.iter_mut() {
                        *sample = T::from_sample(0.0f32);
                    }
                    continue;
                }
                for sample in frame.iter_mut() {
                    match consumer.pop() {
                        Ok(value) => *sample = T::from_sample(value),
                        Err(_) => *sample = T::from_sample(0.0f32),
                    }
                }
                frames += 1;
            }
            clock.played.fetch_add(frames, Ordering::Relaxed);
        },
        |error| eprintln!("audio output error: {error}"),
        None,
    )?;
    Ok(stream)
}
