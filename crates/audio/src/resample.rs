//! Sample-rate conversion (rubato FFT) with an input buffer, so callers can hand over
//! whatever chunk sizes the decoder produces.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{FixedSync, Resampler as _};

use crate::{Error, Result};

const CHANNELS: usize = 2;
/// Frames per resampler call; ~256 frames per FFT sub-chunk is rubato's sweet spot.
const CHUNK: usize = 1024;

/// Stereo-only resampler. Feed it [`push`](Self::push); it returns whichever whole chunks
/// became available.
pub struct Resampler {
    inner: rubato::Fft<f32>,
    pending: Vec<f32>,
}

impl Resampler {
    /// Build a converter from `from_rate` to `to_rate` for stereo audio.
    pub fn new(from_rate: u32, to_rate: u32) -> Result<Self> {
        let inner = rubato::Fft::<f32>::new(
            from_rate as usize,
            to_rate as usize,
            CHUNK,
            CHANNELS,
            FixedSync::Input,
        )
        .map_err(|error| Error::Resample(error.to_string()))?;
        Ok(Self {
            inner,
            pending: Vec::with_capacity(CHUNK * CHANNELS * 2),
        })
    }

    /// Resample `input` (stereo interleaved). Returns zero or more output blocks; leftover
    /// input is kept for next time.
    pub fn push(&mut self, input: &[f32]) -> Result<Vec<f32>> {
        self.pending.extend_from_slice(input);
        let needed = CHUNK * CHANNELS;
        let mut out = Vec::with_capacity(self.pending.len() / 2 + CHUNK);

        while self.pending.len() >= needed {
            let chunk: Vec<f32> = self.pending.drain(..needed).collect();
            self.run(&chunk, None, &mut out)?;
        }
        Ok(out)
    }

    /// Discard buffered input and filter state; used after a seek.
    pub fn reset(&mut self) {
        self.pending.clear();
        self.inner.reset();
    }

    /// End of stream: process whatever is buffered (padded with silence) so the last few
    /// milliseconds are not dropped.
    pub fn flush(&mut self) -> Result<Vec<f32>> {
        let frames = self.pending.len() / CHANNELS;
        if frames == 0 {
            return Ok(Vec::new());
        }
        let mut chunk = std::mem::take(&mut self.pending);
        chunk.resize(CHUNK * CHANNELS, 0.0);
        let mut out = Vec::with_capacity(CHUNK * CHANNELS);
        self.run(&chunk, Some(frames), &mut out)?;
        // The resampler always emits a whole chunk; keep only what the real input produced.
        let expected = (frames as f64 * self.inner.resample_ratio()).ceil() as usize;
        out.truncate(expected * CHANNELS);
        Ok(out)
    }

    fn run(&mut self, chunk: &[f32], partial: Option<usize>, out: &mut Vec<f32>) -> Result<()> {
        let input = InterleavedSlice::new(chunk, CHANNELS, CHUNK)
            .map_err(|error| Error::Resample(error.to_string()))?;
        let frames = self.inner.output_frames_next();
        let mut scratch = vec![0.0f32; frames * CHANNELS];
        let mut output = InterleavedSlice::new_mut(&mut scratch, CHANNELS, frames)
            .map_err(|error| Error::Resample(error.to_string()))?;
        let indexing = partial.map(|frames| rubato::Indexing::default().partial_len(frames));
        let (_, produced) = self
            .inner
            .process_into_buffer(&input, &mut output, indexing.as_ref())
            .map_err(|error| Error::Resample(error.to_string()))?;
        out.extend_from_slice(&scratch[..produced * CHANNELS]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_second_at_44k_becomes_one_second_at_48k() {
        let mut resampler = Resampler::new(44_100, 48_000).unwrap();

        // Feed a 440 Hz tone in decoder-sized blocks.
        let mut phase = 0.0f32;
        let step = std::f32::consts::TAU * 440.0 / 44_100.0;
        let mut produced = 0usize;
        for _ in 0..(44_100 / 1_152) {
            let mut block = Vec::with_capacity(1_152 * 2);
            for _ in 0..1_152 {
                let sample = phase.sin() * 0.5;
                block.extend_from_slice(&[sample, sample]);
                phase = (phase + step) % std::f32::consts::TAU;
            }
            produced += resampler.push(&block).unwrap().len() / 2;
        }

        // 38 full blocks of 1152 frames = 43776 frames in; allow the filter delay.
        let expected = 43_776 * 48_000 / 44_100;
        let error = (produced as i64 - expected as i64).abs();
        assert!(
            error < expected as i64 / 20,
            "produced {produced} frames, expected about {expected}"
        );

        // The tail flush must emit the buffered remainder, not drop it.
        let tail = resampler.flush().unwrap().len() / 2;
        assert!(tail > 0, "flush produced nothing");
        let total = produced + tail;
        let one_second = 44_100 * 48_000 / 44_100;
        assert!(
            (total as i64 - one_second as i64).abs() < one_second as i64 / 50,
            "flushed total {total} frames, expected about {one_second}"
        );
        assert!(
            resampler.flush().unwrap().is_empty(),
            "second flush repeats data"
        );
    }
}
