//! hls.rs — HLS support (HTTP Live Streaming): Twitch live streams,
//! TV channels, .m3u8 VODs…
//!
//! How rtv plays HLS: libavformat's `hls` demuxer is what fetches the
//! playlist, downloads each fragment (.ts/.m4s) as it becomes due and
//! chains them into a continuous stream — refreshing the playlist for
//! live content. That engine already exists and is the same one
//! ffplay/mpv use; what it does NOT handle well on its own is what
//! this module covers:
//!
//!   1. Master playlist → one variant. A master lists N qualities
//!      (1080p60, 720p, audio_only…). Hand it to libavformat as-is
//!      and it exposes ALL of them as streams and starts downloading
//!      several at once until the player discards the rest (wasted
//!      bandwidth, slow startup). `pick_variant()` downloads the
//!      master (over libavformat's own HTTP stack, zero new
//!      dependencies), picks the best quality ≤1080p and hands the
//!      demuxer only that media playlist.
//!
//!   2. Live-oriented demuxer options (`open_opts()`): start near the
//!      live edge (not at the beginning of the DVR), keep the HTTP
//!      connection alive between fragments (less per-fragment
//!      latency) and tolerate playlists that are slow to refresh
//!      (Twitch under load). Harmless for VODs.
//!
//! Twitch: `rtv https://twitch.tv/channel` goes through yt-dlp
//! (source.rs), which returns the stream's .m3u8 URL; that URL comes
//! through here. So does a direct .m3u8 (say from usher.ttvnw.net or
//! a TV channel).

use anyhow::{anyhow, bail, Result};
use ffmpeg_the_third as ffmpeg;
use ffmpeg::sys as ff;
use std::ffi::CString;
use std::path::{Path, PathBuf};

/// Does this look like an HLS URL? — the path (minus query/fragment)
/// ends in .m3u8 (or .m3u, the older flavour). Twitch/TV URLs carry a
/// mile-long query string, so trim it before checking the extension.
pub fn is_hls_url(s: &str) -> bool {
    if !crate::source::is_url(s) {
        return false;
    }
    let path = s.split(['?', '#']).next().unwrap_or(s);
    let p = path.to_ascii_lowercase();
    p.ends_with(".m3u8") || p.ends_with(".m3u")
}

/// Extra hls demuxer options for `source::open()` (on top of the
/// generic network ones). Only applied to URLs `is_hls_url` accepts.
pub fn open_opts(opts: &mut ffmpeg::Dictionary) {
    // Live: start 3 fragments before the live edge (the de-facto
    // standard: headroom to download without running dry). Ignored
    // for VODs.
    opts.set("live_start_index", "-3");
    // Reuse the HTTP connection between fragments (keep-alive):
    // without it every fragment pays a TCP+TLS handshake — very
    // noticeable on live streams with 2 s fragments.
    opts.set("http_persistent", "1");
    // Number of playlist refreshes with no new fragments before
    // declaring the stream dead. The default (3) cuts off real live
    // streams when the CDN hiccups; 15 ≈ 30 s of patience with 2 s
    // fragments.
    opts.set("m3u8_hold_counters", "15");
    // Fragments may come without an extension or with unusual ones
    // (Twitch uses odd URLs); don't let the demuxer reject them.
    opts.set("extension_picky", "0");
    // Download the next fragment in parallel with demuxing the
    // current one. Otherwise every fragment boundary puts the network
    // stall (RTT + CDN TTFB) in series with the decode: that's the
    // periodic micro-stutter on live streams with 2 s fragments
    // (Twitch).
    opts.set("http_multiple", "1");
    // Short probe: avformat_find_stream_info analyzes ~5 s of data by
    // default — on a live stream that eats the 3-fragment cushion from
    // live_start_index before anything is shown, and playback starts
    // glued to the live edge with no headroom: the laggy first stretch
    // until the window grows back. 2 s of analysis is plenty for
    // Twitch's H.264+AAC, and the cushion reaches the player intact.
    opts.set("analyzeduration", "2000000"); // µs
    opts.set("probesize", "2000000"); // bytes
    // Note on Twitch ads: they come stitched — ad fragments spliced
    // into the same playlist with #EXT-X-DISCONTINUITY (plus a
    // DATERANGE of class twitch-stitched-ad; checked against the real
    // usher.ttvnw.net playlist and streamlink's plugin). The hls
    // demuxer crosses them without cutting; the PTS jump at the splice
    // is absorbed by the player's discontinuity protection (re-anchors
    // clocks and re-aligns audio, like a seek landing).
}

/// If `media` is a master playlist, return the absolute URL of the
/// best variant ≤1080p (or the highest-bitrate one when none declares
/// a resolution). If it's already a media playlist, or anything fails
/// (network, parsing…), return None and play the original URL as-is —
/// this is an optimization, never a reason not to play.
pub fn pick_variant(media: &Path) -> Option<PathBuf> {
    let url = media.to_string_lossy();
    if !is_hls_url(&url) {
        return None;
    }
    let text = match http_get(&url, 512 * 1024) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[rtv] HLS: couldn't read the playlist ({e}); continuing with the original URL");
            return None;
        }
    };
    if !text.trim_start().starts_with("#EXTM3U") {
        return None; // not actually HLS; let libavformat decide
    }
    if text.contains("#EXTINF") {
        // Media playlist: fragments directly. Without #EXT-X-ENDLIST
        // it's live (the playlist grows as it's broadcast).
        if !text.contains("#EXT-X-ENDLIST") {
            eprintln!("[rtv] live HLS (fragments arrive as they are broadcast)");
        }
        return None;
    }
    let variants = parse_master(&text);
    let best = select_variant(&variants)?;
    let abs = join_url(&url, &best.uri);
    let label = match (best.width, best.height) {
        (Some(w), Some(h)) => format!("{w}x{h}"),
        _ => format!("{} kbps", best.bandwidth / 1000),
    };
    eprintln!("[rtv] HLS: picked variant {label} out of {} available", variants.len());
    Some(PathBuf::from(abs))
}

// -------------------------------------------------------- master m3u8 --

/// One variant from the master playlist (#EXT-X-STREAM-INF + URI).
#[derive(Debug, PartialEq)]
struct Variant {
    bandwidth: u64,
    width: Option<u32>,
    height: Option<u32>,
    uri: String,
}

/// Parse the variants of a master playlist. #EXT-X-MEDIA entries
/// (alternate audio/sub tracks) are left alone: they're referenced
/// from the media playlist and the hls demuxer resolves them itself.
fn parse_master(text: &str) -> Vec<Variant> {
    let mut out = Vec::new();
    let mut lines = text.lines().map(str::trim);
    while let Some(line) = lines.next() {
        let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") else {
            continue;
        };
        // The URI is the next non-comment, non-empty line.
        let uri = lines
            .by_ref()
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_string);
        let Some(uri) = uri else { continue };
        let bandwidth = attr_value(attrs, "BANDWIDTH")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let (width, height) = attr_value(attrs, "RESOLUTION")
            .and_then(|v| {
                let (w, h) = v.split_once(['x', 'X'])?;
                Some((w.parse().ok(), h.parse().ok()))
            })
            .unwrap_or((None, None));
        out.push(Variant { bandwidth, width, height, uri });
    }
    out
}

/// Value of a `KEY=value` attribute in an m3u8 attribute list.
/// Values containing commas are quoted (RFC 8216 §4.2) — splitting on
/// commas has to respect quotes.
pub(crate) fn attr_value<'a>(attrs: &'a str, key: &str) -> Option<&'a str> {
    let mut rest = attrs;
    while !rest.is_empty() {
        // Name runs up to the '='.
        let (name, after) = rest.split_once('=')?;
        let name = name.trim();
        let (value, next) = if let Some(q) = after.strip_prefix('"') {
            let end = q.find('"')?;
            let v = &q[..end];
            let n = q[end + 1..].strip_prefix(',').unwrap_or(&q[end + 1..]);
            (v, n)
        } else {
            match after.split_once(',') {
                Some((v, n)) => (v, n),
                None => (after, ""),
            }
        };
        if name.eq_ignore_ascii_case(key) {
            return Some(value.trim());
        }
        rest = next;
    }
    None
}

/// Best variant for a terminal: the highest resolution ≤1080p tall
/// (same philosophy as the --ytdl-format default; beyond 1080 is pure
/// decode cost for tiny cells), ties broken by bitrate. When none
/// declares a resolution, take the highest bitrate. Audio-only
/// variants are skipped when there are alternatives with video.
fn select_variant<'a>(vs: &'a [Variant]) -> Option<&'a Variant> {
    let with_video: Vec<&Variant> = vs.iter().filter(|v| v.height.is_some()).collect();
    if with_video.is_empty() {
        return vs.iter().max_by_key(|v| v.bandwidth);
    }
    let capped: Vec<&&Variant> = with_video
        .iter()
        .filter(|v| v.height.unwrap_or(0) <= 1080)
        .collect();
    if capped.is_empty() {
        // All >1080p: take the lightest one (closest to 1080).
        return with_video.into_iter().min_by_key(|v| (v.height.unwrap_or(0), v.bandwidth));
    }
    capped
        .into_iter()
        .max_by_key(|v| (v.height.unwrap_or(0), v.bandwidth))
        .copied()
}

/// Resolve `rel` against the `base` URL (good enough for playlists:
/// no ".." normalization). Absolute → as-is; "/x" → scheme+host +
/// path; relative → base's directory + rel.
pub(crate) fn join_url(base: &str, rel: &str) -> String {
    if crate::source::is_url(rel) {
        return rel.to_string();
    }
    let base_clean = base.split(['?', '#']).next().unwrap_or(base);
    if let Some(path) = rel.strip_prefix('/') {
        // scheme://host[:port]
        if let Some((scheme, rest)) = base_clean.split_once("://") {
            let host = rest.split('/').next().unwrap_or(rest);
            return format!("{scheme}://{host}/{path}");
        }
        return rel.to_string();
    }
    match base_clean.rfind('/') {
        // Don't cut into the scheme's "//".
        Some(i) if i > base_clean.find("://").map(|j| j + 2).unwrap_or(0) => {
            format!("{}/{rel}", &base_clean[..i])
        }
        _ => format!("{base_clean}/{rel}"),
    }
}

// ------------------------------------------------------ HTTP via avio --

/// Download `url` (up to `max` bytes) over libavformat's HTTP/TLS
/// stack — the same one that later downloads the fragments, so if this
/// works, playback works too (and vice versa: same proxy, TLS and
/// redirect support). Zero new HTTP dependencies.
pub(crate) fn http_get(url: &str, max: usize) -> Result<String> {
    Ok(String::from_utf8_lossy(&http_get_bytes(url, max)?).into_owned())
}

/// Binary variant of http_get: .ts/.m4s segments are raw bytes
/// (from_utf8_lossy would corrupt them). Used by the DVR proxy.
pub(crate) fn http_get_bytes(url: &str, max: usize) -> Result<Vec<u8>> {
    // Idempotent; needed if it hasn't run yet (network/winsock init).
    let _ = ffmpeg::init();
    let curl = CString::new(url).map_err(|_| anyhow!("URL contains NUL"))?;
    unsafe {
        let mut opts: *mut ff::AVDictionary = std::ptr::null_mut();
        let k = CString::new("rw_timeout").unwrap();
        let v = CString::new("15000000").unwrap(); // 15 s, in µs
        ff::av_dict_set(&mut opts, k.as_ptr(), v.as_ptr(), 0);
        let mut ctx: *mut ff::AVIOContext = std::ptr::null_mut();
        let ret = ff::avio_open2(
            &mut ctx,
            curl.as_ptr(),
            ff::AVIO_FLAG_READ as i32,
            std::ptr::null(),
            &mut opts,
        );
        ff::av_dict_free(&mut opts);
        if ret < 0 || ctx.is_null() {
            bail!("avio_open2: error {ret}");
        }
        let mut data = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            // `as *mut _`: avio_read's buffer is unsigned char* — let
            // the compiler type the pointer (ARM portability, same
            // lesson as av_err_str in hwdec.rs).
            let n = ff::avio_read(ctx, chunk.as_mut_ptr() as *mut _, chunk.len() as i32);
            if n <= 0 {
                break;
            }
            data.extend_from_slice(&chunk[..n as usize]);
            if data.len() >= max {
                break;
            }
        }
        ff::avio_closep(&mut ctx);
        Ok(data)
    }
}

// ---------------------------------------------------------------- test --

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_hls_urls() {
        assert!(is_hls_url("https://cdn.tv/live/canal.m3u8"));
        assert!(is_hls_url("https://usher.ttvnw.net/api/channel/hls/x.m3u8?token=a,b&sig=c"));
        assert!(is_hls_url("HTTP://a.b/pl.M3U8#frag"));
        assert!(is_hls_url("https://a.b/lista.m3u"));
        assert!(!is_hls_url("https://a.b/video.mp4"));
        assert!(!is_hls_url("https://a.b/m3u8/video.mp4?x=.m3u8")); // query doesn't count
        assert!(!is_hls_url("/local/path/list.m3u8")); // not a URL
    }

    #[test]
    fn parses_master_and_picks_variant() {
        let master = r#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="es",URI="audio/es.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=6000000,RESOLUTION=1920x1080,CODECS="avc1.64002a,mp4a.40.2"
1080p60/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=15000000,RESOLUTION=3840x2160
4k/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1280x720
720p/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=128000
audio_only/index.m3u8
"#;
        let vs = parse_master(master);
        assert_eq!(vs.len(), 4);
        assert_eq!(vs[0].height, Some(1080));
        assert_eq!(vs[0].bandwidth, 6_000_000);
        // Picks 1080p (highest resolution ≤1080), not the 4K or audio.
        let best = select_variant(&vs).unwrap();
        assert_eq!(best.uri, "1080p60/index.m3u8");
    }

    #[test]
    fn variants_without_resolution_use_bitrate() {
        let vs = vec![
            Variant { bandwidth: 100, width: None, height: None, uri: "a".into() },
            Variant { bandwidth: 900, width: None, height: None, uri: "b".into() },
        ];
        assert_eq!(select_variant(&vs).unwrap().uri, "b");
    }

    #[test]
    fn all_above_1080_picks_the_smallest() {
        let vs = vec![
            Variant { bandwidth: 30_000_000, width: Some(3840), height: Some(2160), uri: "4k".into() },
            Variant { bandwidth: 12_000_000, width: Some(2560), height: Some(1440), uri: "2k".into() },
        ];
        assert_eq!(select_variant(&vs).unwrap().uri, "2k");
    }

    #[test]
    fn attributes_with_quotes_and_commas() {
        let attrs = r#"BANDWIDTH=1000,CODECS="avc1.4d,mp4a.40",RESOLUTION=640x360"#;
        assert_eq!(attr_value(attrs, "BANDWIDTH"), Some("1000"));
        assert_eq!(attr_value(attrs, "CODECS"), Some("avc1.4d,mp4a.40"));
        assert_eq!(attr_value(attrs, "RESOLUTION"), Some("640x360"));
        assert_eq!(attr_value(attrs, "MISSING"), None);
    }

    #[test]
    fn url_joining() {
        assert_eq!(
            join_url("https://a.b/live/master.m3u8?tok=1", "720p/index.m3u8"),
            "https://a.b/live/720p/index.m3u8"
        );
        assert_eq!(
            join_url("https://a.b/live/master.m3u8", "/abs/index.m3u8"),
            "https://a.b/abs/index.m3u8"
        );
        assert_eq!(
            join_url("https://a.b/master.m3u8", "https://cdn.c/x.m3u8"),
            "https://cdn.c/x.m3u8"
        );
    }
}
