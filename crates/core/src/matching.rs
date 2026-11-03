//! Pure matching helpers: title parsing, normalization, scoring.
//!
//! No I/O here, so every rule in AGENTS.md §7 is testable without a database.

use crate::model::version_flags::*;

/// A title with its version segments lifted into flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTitle {
    /// Title minus feature credits and version segments.
    pub clean: String,
    pub flags: u32,
}

/// Lowercase, keep alphanumerics, collapse everything else to single spaces.
pub fn normalize(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Split parenthesised/bracketed groups and trailing `- ...` segments out of a title.
///
/// Groups that only name a version become flags; feature credits are dropped; anything
/// else is kept as part of the title.
pub fn parse_title(title: &str) -> ParsedTitle {
    let mut flags = 0;
    let mut text = String::new();
    let mut group = String::new();
    let mut depth = 0usize;

    for ch in title.chars() {
        match ch {
            '(' | '[' => {
                if depth == 0 {
                    group.clear();
                } else {
                    group.push(ch);
                }
                depth += 1;
            }
            ')' | ']' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    absorb_segment(&group, &mut flags, &mut text);
                } else {
                    group.push(ch);
                }
            }
            _ if depth > 0 => group.push(ch),
            _ => text.push(ch),
        }
    }
    if depth > 0 {
        // Unbalanced opener: treat it as plain text rather than dropping it.
        text.push('(');
        text.push_str(&group);
    }

    let mut parts = text.split(" - ").map(str::trim).filter(|p| !p.is_empty());
    let mut clean = parts.next().unwrap_or_default().to_string();
    for part in parts {
        if is_feature_credit(part) {
            continue;
        }
        let bits = flag_bits(part);
        if bits == 0 {
            clean.push_str(" - ");
            clean.push_str(part);
        } else {
            flags |= bits;
        }
    }

    ParsedTitle {
        clean: clean.split_whitespace().collect::<Vec<_>>().join(" "),
        flags,
    }
}

fn absorb_segment(segment: &str, flags: &mut u32, text: &mut String) {
    let segment = segment.trim();
    let bits = flag_bits(segment);
    *flags |= bits;
    if bits == 0 && !is_feature_credit(segment) {
        text.push_str(" (");
        text.push_str(segment);
        text.push(')');
    }
}

fn is_feature_credit(segment: &str) -> bool {
    flag_words(segment)
        .iter()
        .any(|w| matches!(w.as_str(), "feat" | "ft" | "featuring"))
}

fn flag_bits(segment: &str) -> u32 {
    let words = flag_words(segment);
    let mut flags = 0;
    for word in &words {
        flags |= match word.as_str() {
            "live" => LIVE,
            "remaster" | "remastered" => REMASTER,
            "remix" | "rmx" => REMIX,
            "acoustic" | "unplugged" => ACOUSTIC,
            "instrumental" | "karaoke" => INSTRUMENTAL,
            _ => 0,
        };
    }
    if words.windows(2).any(|pair| pair == ["radio", "edit"]) {
        flags |= RADIO_EDIT;
    }
    flags
}

fn flag_words(segment: &str) -> Vec<String> {
    segment
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The track being matched, after provider metadata has been normalized.
pub struct Query<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub album: Option<&'a str>,
    pub duration_ms: Option<i64>,
    pub flags: u32,
}

pub struct Candidate<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub album: Option<&'a str>,
    pub duration_ms: Option<i64>,
    pub flags: u32,
}

/// 0.0..=1.0. Hard rules from AGENTS.md §7 are caps, not nudges:
/// differing version flags or a duration over 2 s can never reach the auto-merge band.
pub fn score(query: &Query<'_>, candidate: &Candidate<'_>) -> f64 {
    if query.flags != candidate.flags {
        return 0.0;
    }

    // Titles may arrive raw (unit fixtures, other callers) or already parsed; parsing a
    // clean title is a no-op.
    let title = token_similarity(
        &parse_title(query.title).clean,
        &parse_title(candidate.title).clean,
    );
    let artist = token_similarity(query.artist, candidate.artist);

    let (mut score, duration_penalty) = match (query.duration_ms, candidate.duration_ms) {
        (Some(a), Some(b)) => {
            let diff = (a - b).abs();
            let boost = if diff <= 2_000 { 1.0 } else { 0.5 };
            (title * 0.55 + artist * 0.30 + boost * 0.15, diff > 2_000)
        }
        // Unknown duration on either side: renormalize title/artist weights.
        _ => (title * 0.64 + artist * 0.36, false),
    };

    if let (Some(a), Some(b)) = (query.album, candidate.album)
        && normalize(a) == normalize(b)
    {
        score = (score + 0.05).min(1.0);
    }

    if duration_penalty {
        score = score.min(0.69);
    }
    score
}

/// Token Jaccard, with a full-membership boost so "The Harbour" vs "Harbour" scores
/// higher than unrelated names sharing one word.
fn token_similarity(a: &str, b: &str) -> f64 {
    let a = tokens(a);
    let b = tokens(b);
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    let intersection = a.intersection(&b).count();
    let union = a.union(&b).count();
    let jaccard = intersection as f64 / union as f64;
    if a.is_subset(&b) || b.is_subset(&a) {
        jaccard.max(0.85)
    } else {
        jaccard
    }
}

fn tokens(text: &str) -> std::collections::BTreeSet<String> {
    normalize(text)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q<'a>(title: &'a str, artist: &'a str, duration_ms: Option<i64>) -> Query<'a> {
        Query {
            title,
            artist,
            album: None,
            duration_ms,
            flags: parse_title(title).flags,
        }
    }

    fn c<'a>(title: &'a str, artist: &'a str, duration_ms: Option<i64>) -> Candidate<'a> {
        Candidate {
            title,
            artist,
            album: None,
            duration_ms,
            flags: parse_title(title).flags,
        }
    }

    #[test]
    fn strips_remaster_and_keeps_the_flag() {
        let parsed = parse_title("Night Ferry (Remastered 2011)");
        assert_eq!(parsed.clean, "Night Ferry");
        assert_eq!(parsed.flags, REMASTER);
    }

    #[test]
    fn strips_feature_credits_entirely() {
        let parsed = parse_title("Night Ferry (feat. Someone)");
        assert_eq!(parsed.clean, "Night Ferry");
        assert_eq!(parsed.flags, 0);
    }

    #[test]
    fn keeps_non_version_parentheticals() {
        let parsed = parse_title("Night Ferry (Theme)");
        assert_eq!(parsed.clean, "Night Ferry (Theme)");
        assert_eq!(parsed.flags, 0);
    }

    #[test]
    fn handles_dash_suffixes_and_brackets() {
        let parsed = parse_title("Night Ferry [Live] - Radio Edit");
        assert_eq!(parsed.clean, "Night Ferry");
        assert_eq!(parsed.flags, LIVE | RADIO_EDIT);
    }

    #[test]
    fn live_and_studio_never_match() {
        let score = score(
            &q("Night Ferry (Live)", "The Harbour", Some(240_000)),
            &c("Night Ferry", "The Harbour", Some(240_000)),
        );
        assert!(score < 0.70, "live vs studio scored {score}");
    }

    #[test]
    fn same_recording_merges() {
        let score = score(
            &q(
                "Night Ferry (Remastered 2011)",
                "The Harbour",
                Some(240_000),
            ),
            &c("Night Ferry (Remaster)", "The Harbour", Some(241_000)),
        );
        assert!(score >= 0.90, "expected merge, scored {score}");
    }

    #[test]
    fn duration_mismatch_cannot_auto_merge() {
        let score = score(
            &q("Night Ferry", "The Harbour", Some(240_000)),
            &c("Night Ferry", "The Harbour", Some(250_000)),
        );
        assert!(score < 0.90, "3 s apart scored {score}");
    }

    #[test]
    fn same_title_different_artist_scores_between_bands() {
        let score = score(
            &q("Night Ferry", "The Harbour", Some(240_000)),
            &c("Night Ferry", "Harbour Lights", Some(240_000)),
        );
        assert!((0.70..0.90).contains(&score), "scored {score}");
    }

    #[test]
    fn non_latin_titles_compare() {
        let same = score(
            &q("夜に駆ける", "YOASOBI", Some(261_000)),
            &c("夜に駆ける", "YOASOBI", Some(261_000)),
        );
        assert!(same >= 0.90, "scored {same}");

        let different = score(
            &q("夜に駆ける", "YOASOBI", Some(261_000)),
            &c("夜に走る", "YOASOBI", Some(261_000)),
        );
        assert!(different < 0.90, "scored {different}");
    }
}
