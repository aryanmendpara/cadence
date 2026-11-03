//! Decoding: symphonia packets in, stereo f32 frames out.
//!
//! Every chunk leaves here as **stereo interleaved f32** so nothing downstream has to think
//! about channel layouts. Downmixing is deliberately blunt (see [`to_stereo`]).

use std::fs::File;
use std::path::Path;

use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, TimeBase};

use crate::{Error, Result};

/// A block of decoded audio: stereo, interleaved, sample rate from the file.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    /// Stereo interleaved samples.
    pub samples: Vec<f32>,
    /// Sample rate of this chunk, in Hz.
    pub rate: u32,
}

impl Chunk {
    /// Frames (not samples) in this chunk.
    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }
}

/// One open file. Sequential reads only; seeking resets the decoder and trims to the exact
/// requested frame.
pub struct Decoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    duration_ms: Option<u64>,
    rate: u32,
    /// Frames to drop from the next chunk so a seek lands exactly on the requested time.
    trim_frames: usize,
}

impl Decoder {
    /// Open `path` and prepare its first audio track for decoding.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());

        let mut hint = Hint::new();
        if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(extension);
        }

        let format = symphonia::default::get_probe().probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )?;

        let (track_id, time_base, duration_ms, params) = {
            let track = format.first_track(TrackType::Audio).ok_or_else(|| {
                Error::Unsupported(format!("no audio track in {}", path.display()))
            })?;
            let params = match track.codec_params.as_ref() {
                Some(CodecParameters::Audio(params)) => params.clone(),
                _ => {
                    return Err(Error::Unsupported(format!(
                        "audio track in {} has no codec parameters",
                        path.display()
                    )));
                }
            };
            let duration_ms = track
                .duration
                .zip(track.time_base)
                .and_then(|(duration, base)| base.calc_duration(duration))
                .and_then(time_to_ms);
            (track.id, track.time_base, duration_ms, params)
        };

        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(&params, &AudioDecoderOptions::default())?;
        let rate = params.sample_rate.ok_or_else(|| {
            Error::Unsupported(format!("unknown sample rate in {}", path.display()))
        })?;

        Ok(Self {
            format,
            decoder,
            track_id,
            time_base,
            duration_ms,
            rate,
            trim_frames: 0,
        })
    }

    /// Sample rate declared by the container.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Total length, when the container states it.
    pub fn duration_ms(&self) -> Option<u64> {
        self.duration_ms
    }

    /// The next decoded block, or `None` at end of file.
    ///
    /// Undecodable packets are skipped: one bad frame is not the end of the track.
    pub fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => return Ok(None),
                Err(SymphoniaError::IoError(error))
                    if error.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(error.into()),
            };

            if packet.track_id != self.track_id {
                continue;
            }

            let buffer = match self.decoder.decode(&packet) {
                Ok(buffer) => buffer,
                // Recoverable: drop the packet, keep going.
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(error) => return Err(error.into()),
            };

            if buffer.frames() == 0 {
                continue;
            }

            let rate = buffer.spec().rate();
            let channels = buffer.spec().channels().count();
            let mut interleaved = vec![0.0f32; buffer.samples_interleaved()];
            buffer.copy_to_slice_interleaved(&mut interleaved[..]);
            let mut samples = to_stereo(&interleaved, channels);

            if self.trim_frames > 0 {
                let drop = self.trim_frames.min(samples.len() / 2);
                samples.drain(..drop * 2);
                self.trim_frames -= drop;
                if samples.is_empty() {
                    continue;
                }
            }

            return Ok(Some(Chunk { samples, rate }));
        }
    }

    /// Seek to `ms` from the start. The next chunk begins at exactly that frame where the
    /// container allows it.
    pub fn seek(&mut self, ms: u64) -> Result<()> {
        let seconds = ms as f64 / 1000.0;
        let time = Time::try_from_secs_f64(seconds)
            .ok_or_else(|| Error::Unsupported(format!("seek to {ms} ms is out of range")))?;
        let seeked = self.format.seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time,
                track_id: Some(self.track_id),
            },
        )?;
        self.decoder.reset();

        // Accurate seeks land on a packet boundary at or before the target; skip the frames in
        // between. Frames, not milliseconds: sub-millisecond rounding is audible as a click.
        let rate = u128::from(self.rate);
        let target = (u128::from(ms) * rate).div_ceil(1000);
        let actual = match self.time_base {
            Some(base) => {
                // seconds = ts * numer / denom, then frames = seconds * rate.
                let frames = i128::from(seeked.actual_ts.get())
                    * i128::from(base.numer.get())
                    * i128::from(self.rate)
                    / i128::from(base.denom.get());
                u128::try_from(frames.max(0)).unwrap_or(0)
            }
            None => target,
        };
        self.trim_frames = usize::try_from(target.saturating_sub(actual)).unwrap_or(0);
        Ok(())
    }
}

/// Duration in milliseconds, saturating rather than wrapping on absurd values.
fn time_to_ms(time: Time) -> Option<u64> {
    u64::try_from(time.as_millis()).ok()
}

/// Stereo interleaved, whatever came in.
///
/// Mono is duplicated, stereo passes through, anything wider keeps the first two channels.
/// `ponytail:` front L/R only — proper 5.1 downmix needs the channel mask; add it if surround
/// files start sounding wrong.
fn to_stereo(samples: &[f32], channels: usize) -> Vec<f32> {
    match channels {
        0 => Vec::new(),
        1 => samples.iter().flat_map(|&s| [s, s]).collect(),
        2 => samples.to_vec(),
        _ => samples
            .chunks_exact(channels)
            .flat_map(|frame| [frame[0], frame[1]])
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;

    /// 16-bit mono WAV whose sample value encodes its frame index, so a seek is checkable.
    fn write_ramp_wav(path: &Path, rate: u32, frames: usize) {
        let mut data = Vec::with_capacity(frames * 2);
        for frame in 0..frames {
            let value = (frame % 30_000) as i16;
            data.extend_from_slice(&value.to_le_bytes());
        }
        let mut file = fs::File::create(path).unwrap();
        file.write_all(b"RIFF").unwrap();
        file.write_all(&(36 + data.len() as u32).to_le_bytes())
            .unwrap();
        file.write_all(b"WAVEfmt ").unwrap();
        file.write_all(&16u32.to_le_bytes()).unwrap(); // fmt chunk size
        file.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        file.write_all(&1u16.to_le_bytes()).unwrap(); // mono
        file.write_all(&rate.to_le_bytes()).unwrap();
        file.write_all(&(rate * 2).to_le_bytes()).unwrap(); // byte rate
        file.write_all(&2u16.to_le_bytes()).unwrap(); // block align
        file.write_all(&16u16.to_le_bytes()).unwrap(); // bits
        file.write_all(b"data").unwrap();
        file.write_all(&(data.len() as u32).to_le_bytes()).unwrap();
        file.write_all(&data).unwrap();
    }

    fn temp_wav(name: &str, rate: u32, frames: usize) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("cadence-{name}-{unique}.wav"));
        write_ramp_wav(&path, rate, frames);
        path
    }

    #[test]
    fn decodes_the_whole_file_as_stereo() {
        let path = temp_wav("decode", 44_100, 4_410);
        let mut decoder = Decoder::open(&path).unwrap();

        assert_eq!(decoder.duration_ms(), Some(100));

        let mut frames = 0;
        while let Some(chunk) = decoder.next_chunk().unwrap() {
            assert_eq!(chunk.rate, 44_100);
            assert_eq!(chunk.samples.len() % 2, 0);
            frames += chunk.frames();
        }
        assert_eq!(frames, 4_410);
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn seek_lands_on_the_requested_frame() {
        let path = temp_wav("seek", 44_100, 44_100);
        let mut decoder = Decoder::open(&path).unwrap();
        decoder.seek(500).unwrap();

        let chunk = decoder.next_chunk().unwrap().unwrap();
        let expected = 22_050;
        let actual = chunk.samples[0] * 32_768.0;
        assert!(
            (actual - expected as f32).abs() < 2.0,
            "expected sample {expected}, got {actual}"
        );
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn mono_is_duplicated_to_both_channels() {
        assert_eq!(to_stereo(&[0.25, -0.5], 1), vec![0.25, 0.25, -0.5, -0.5]);
        assert_eq!(
            to_stereo(&[0.1, 0.2, 0.3, 0.4], 2),
            vec![0.1, 0.2, 0.3, 0.4]
        );
    }
}
