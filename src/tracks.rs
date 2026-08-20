//! tracks.rs — inventory of the container's audio and subtitle
//! tracks, used for runtime track switching (`a`/`#` and `j`/`J`
//! keys) and CLI selection (`--aid/--alang/--sid/--slang`).
//!
//! We probe once when the file is opened (header demux only, nothing
//! gets decoded). For subtitles only text tracks are listed (SRT/ASS/
//! mov_text/WebVTT…); bitmap ones (PGS/dvdsub) can't be rendered as
//! text in a terminal.

use ffmpeg_the_third as ffmpeg;
use ffmpeg::media::Type;
use std::path::Path;

/// Metadata for a single track (audio or subtitles).
#[derive(Debug, Clone)]
pub struct TrackInfo {
    /// Actual stream index within the container (what the demuxer uses).
    pub stream_index: usize,
    /// Language ("eng", "spa"…) or empty when the container omits it.
    pub lang: String,
    /// Track title, possibly empty.
    pub title: String,
    /// Short codec name ("aac", "opus", "subrip"…).
    pub codec: String,
}

impl TrackInfo {
    /// Compact label for the HUD/OSD: "eng (aac)", "Commentary (ac3)",
    /// or just the codec when there's no metadata to show.
    pub fn label(&self) -> String {
        let name = if !self.lang.is_empty() {
            self.lang.clone()
        } else if !self.title.is_empty() {
            self.title.clone()
        } else {
            return self.codec.clone();
        };
        if self.codec.is_empty() {
            name
        } else {
            format!("{name} ({})", self.codec)
        }
    }
}

/// Is this a text subtitle codec we can render?
pub fn is_text_sub_codec(id: ffmpeg::codec::Id) -> bool {
    use ffmpeg::codec::Id;
    matches!(
        id,
        Id::SUBRIP | Id::SRT | Id::ASS | Id::SSA | Id::TEXT | Id::MOV_TEXT | Id::WEBVTT
    )
}

/// List (audio tracks, text subtitle tracks) from the container, in
/// order of appearance.
pub fn probe(path: &Path) -> (Vec<TrackInfo>, Vec<TrackInfo>) {
    let mut audio = Vec::new();
    let mut subs = Vec::new();
    let Ok(ictx) = crate::source::open(path) else {
        return (audio, subs);
    };
    for stream in ictx.streams() {
        let params = stream.parameters();
        let medium = params.medium();
        if medium != Type::Audio && medium != Type::Subtitle {
            continue;
        }
        if medium == Type::Subtitle && !is_text_sub_codec(params.id()) {
            continue;
        }
        let md = stream.metadata();
        let info = TrackInfo {
            stream_index: stream.index(),
            lang: md.get("language").unwrap_or("").to_string(),
            title: md.get("title").unwrap_or("").to_string(),
            codec: codec_short_name(params.id()),
        };
        if medium == Type::Audio {
            audio.push(info);
        } else {
            subs.push(info);
        }
    }
    (audio, subs)
}

fn codec_short_name(id: ffmpeg::codec::Id) -> String {
    format!("{id:?}").to_ascii_lowercase()
}

/// Resolve the initial track requested on the CLI within `tracks`:
///   * `id`   — 1-based index among the tracks of that type
///     (`--aid 2` = second audio track), mpv style.
///   * `lang` — language code; matching is case-insensitive and
///     prefix-based in both directions ("en" matches "eng" and
///     "eng" matches "en").
///
/// Returns the position within `tracks` (not the stream_index), or
/// `None` when nothing matches (the caller picks the fallback).
pub fn select(tracks: &[TrackInfo], id: Option<usize>, lang: Option<&str>) -> Option<usize> {
    if let Some(n) = id {
        if n >= 1 && n <= tracks.len() {
            return Some(n - 1);
        }
        return None;
    }
    if let Some(l) = lang {
        let l = l.trim().to_ascii_lowercase();
        if l.is_empty() {
            return None;
        }
        return tracks.iter().position(|t| {
            let tl = t.lang.to_ascii_lowercase();
            !tl.is_empty() && (tl == l || tl.starts_with(&l) || l.starts_with(&tl))
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(idx: usize, lang: &str) -> TrackInfo {
        TrackInfo {
            stream_index: idx,
            lang: lang.into(),
            title: String::new(),
            codec: "aac".into(),
        }
    }

    #[test]
    fn select_by_id_is_one_based() {
        let ts = vec![t(1, "eng"), t(2, "spa")];
        assert_eq!(select(&ts, Some(1), None), Some(0));
        assert_eq!(select(&ts, Some(2), None), Some(1));
        assert_eq!(select(&ts, Some(3), None), None);
        assert_eq!(select(&ts, Some(0), None), None);
    }

    #[test]
    fn select_by_lang_prefix() {
        let ts = vec![t(1, "eng"), t(2, "spa")];
        assert_eq!(select(&ts, None, Some("spa")), Some(1));
        assert_eq!(select(&ts, None, Some("en")), Some(0));
        assert_eq!(select(&ts, None, Some("SPA")), Some(1));
        assert_eq!(select(&ts, None, Some("fra")), None);
        assert_eq!(select(&ts, None, Some("")), None);
    }

    #[test]
    fn id_takes_precedence_over_lang() {
        let ts = vec![t(1, "eng"), t(2, "spa")];
        assert_eq!(select(&ts, Some(1), Some("spa")), Some(0));
    }

    #[test]
    fn label_formats() {
        let mut x = t(1, "eng");
        assert_eq!(x.label(), "eng (aac)");
        x.lang.clear();
        x.title = "Director".into();
        assert_eq!(x.label(), "Director (aac)");
        x.title.clear();
        assert_eq!(x.label(), "aac");
    }
}
