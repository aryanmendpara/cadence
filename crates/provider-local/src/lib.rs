#![forbid(unsafe_code)]
//! Local file provider: folder scan, tag reading, path-derived fallback.
//!
//! Identity is the absolute path (`provider_track_id`), so rescans are stable while the
//! file stays put. TAG-derived metadata becomes a [`TrackInput`]; `core` owns matching.

use std::path::{Path, PathBuf};

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::prelude::{Accessor, ItemKey};
use lofty::tag::Tag;

use cadence_core::ingest::TrackInput;

pub const PROVIDER_ID: &str = "local";

/// Extensions handed to `lofty`; everything else in the folder is ignored.
const AUDIO_EXTENSIONS: &[&str] = &[
    "aac", "aif", "aiff", "alac", "ape", "flac", "m4a", "mp3", "mp4", "oga", "ogg", "opus", "wav",
    "wv",
];

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("cannot read {path}: {source}")]
    Walk {
        path: PathBuf,
        #[source]
        source: walkdir::Error,
    },
    #[error("cannot read tags from {path}: {source}")]
    Tags {
        path: PathBuf,
        #[source]
        source: lofty::error::FileParseError,
    },
}

/// Walk `root`, yielding one [`TrackInput`] per audio file. A broken file is one error,
/// never the end of the scan.
pub fn scan(root: &Path) -> impl Iterator<Item = Result<TrackInput, ScanError>> + '_ {
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| match entry {
            Ok(entry) => {
                let path = entry.into_path();
                (entry_is_audio(&path)).then(|| read_track(&path))
            }
            Err(source) => {
                let path = source.path().unwrap_or(root).to_path_buf();
                Some(Err(ScanError::Walk { path, source }))
            }
        })
}

fn entry_is_audio(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| AUDIO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

fn read_track(path: &Path) -> Result<TrackInput, ScanError> {
    let tagged = lofty::read_from_path(path).map_err(|source| ScanError::Tags {
        path: path.to_path_buf(),
        source,
    })?;
    let duration_ms = i64::try_from(tagged.properties().duration().as_millis()).ok();
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    Ok(input_from(path, tag, duration_ms))
}

fn input_from(path: &Path, tag: Option<&Tag>, duration_ms: Option<i64>) -> TrackInput {
    let fallback_title = title_from_stem(path.file_stem().and_then(|s| s.to_str()).unwrap_or(""));
    let field = |value: Option<std::borrow::Cow<'_, str>>| {
        value
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let item = |key: ItemKey| {
        tag.and_then(|t| t.get_string(key))
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };

    TrackInput {
        provider: PROVIDER_ID.to_string(),
        provider_track_id: path.to_string_lossy().into_owned(),
        title: field(tag.and_then(Accessor::title)).unwrap_or(fallback_title),
        artist: field(tag.and_then(Accessor::artist)).unwrap_or_else(unknown_artist),
        album: field(tag.and_then(Accessor::album)),
        album_artist: item(ItemKey::AlbumArtist),
        year: tag
            .and_then(Accessor::date)
            .map(|date| i32::from(date.year)),
        duration_ms,
        isrc: item(ItemKey::Isrc),
        musicbrainz_id: item(ItemKey::MusicBrainzRecordingId),
        quality: path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase),
    }
}

fn unknown_artist() -> String {
    "Unknown Artist".to_string()
}

/// `01 - Night Ferry.flac` → `Night Ferry`; underscores become spaces.
fn title_from_stem(stem: &str) -> String {
    let spaced = stem.replace('_', " ");
    let stripped = spaced
        .split_once(" - ")
        .filter(|(prefix, _)| {
            !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit() || c == '.')
        })
        .map(|(_, rest)| rest.trim())
        .unwrap_or(spaced.trim());
    stripped.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lofty::tag::TagType;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("cadence-scan-{name}-{unique}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn title_falls_back_to_filename_without_track_number() {
        assert_eq!(title_from_stem("01 - Night Ferry"), "Night Ferry");
        assert_eq!(title_from_stem("Night_Ferry"), "Night Ferry");
        assert_eq!(title_from_stem("Night Ferry"), "Night Ferry");
        assert_eq!(title_from_stem("03. Night Ferry"), "03. Night Ferry");
    }

    #[test]
    fn tagged_fields_win_and_missing_ones_fall_back() {
        let mut tag = Tag::new(TagType::VorbisComments);
        tag.set_title("Night Ferry (Remastered)".into());
        tag.set_artist("The Harbour".into());
        tag.set_album("Low Tide".into());
        tag.insert_text(ItemKey::Isrc, "GBAYE0601498".into());
        tag.insert_text(
            ItemKey::MusicBrainzRecordingId,
            "2f3c4e5a-0000-0000-0000-000000000000".into(),
        );

        let input = input_from(
            Path::new("/music/A/Harbour/01 - whatever.flac"),
            Some(&tag),
            Some(240_000),
        );

        assert_eq!(input.title, "Night Ferry (Remastered)");
        assert_eq!(input.artist, "The Harbour");
        assert_eq!(input.album.as_deref(), Some("Low Tide"));
        assert_eq!(input.isrc.as_deref(), Some("GBAYE0601498"));
        assert!(input.musicbrainz_id.is_some());
        assert_eq!(input.duration_ms, Some(240_000));
        assert_eq!(
            input.provider_track_id,
            "/music/A/Harbour/01 - whatever.flac"
        );
    }

    #[test]
    fn untagged_file_uses_path_identity() {
        let input = input_from(Path::new("/music/01 - Night Ferry.flac"), None, None);
        assert_eq!(input.title, "Night Ferry");
        assert_eq!(input.artist, "Unknown Artist");
        assert_eq!(input.album, None);
        assert_eq!(input.quality.as_deref(), Some("flac"));
    }

    #[test]
    fn scan_skips_non_audio_and_survives_broken_files() {
        let root = temp_root("walk");
        fs::write(root.join("notes.txt"), b"not music").unwrap();
        fs::write(root.join("broken.flac"), b"not really a flac").unwrap();
        let nested = root.join("Album");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("track.mp3"), b"junk").unwrap();

        let results: Vec<_> = scan(&root).collect();
        assert_eq!(results.len(), 2, "two audio extensions, one file each");
        assert!(results.iter().all(Result::is_err));

        fs::remove_dir_all(&root).unwrap();
    }
}
