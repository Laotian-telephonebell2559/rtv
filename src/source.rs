//! source.rs — input resolution before anything is opened.
//!
//! rtv accepts three kinds of input:
//!   * a local path (the usual behaviour),
//!   * a direct http/https URL to a media file (libavformat ships the
//!     network protocols built in: http, https when the build has TLS,
//!     HLS .m3u8, DASH…),
//!   * a video site page (YouTube, Twitch, Vimeo…): we delegate to
//!     `yt-dlp` (when installed) to turn it into the stream's direct
//!     URL, just like mpv does with its ytdl_hook.
//!
//! yt-dlp is NOT compiled into the binary (it's a Python program; its
//! Unlicense would allow it, but it would end up frozen and YouTube
//! breaks extractors every few weeks). Instead, rtv releases bundle
//! the official standalone yt-dlp binary next to rtv (linux/windows/
//! macos; there's no bionic build for Termux → pip). The lookup order
//! always favours whatever is most updatable:
//!   1. $RTV_YTDLP (the user rules),
//!   2. yt-dlp on the PATH (pip/pkg/winget keep it fresh),
//!   3. the yt-dlp bundled next to the rtv executable (fallback;
//!      it self-updates via `yt-dlp -U` — it's the official
//!      PyInstaller build, which supports self-update).
//!
//! Dual input (on by default with yt-dlp): YouTube's higher formats
//! are DASH with video and audio in separate streams. rtv already runs
//! one demuxer for video (decoder.rs) and another for audio
//! (audio.rs), so playing them just means handing the audio URL to the
//! audio pipeline: that's `MediaSource::audio`, which player.rs wires
//! up (and `--audio-file` lets you assemble it by hand, like mpv). The
//! --ytdl-format default asks for "bv*[height<=?1080]+ba/b": best
//! video ≤1080p + best audio as separate streams, falling back to
//! muxed.

use anyhow::{anyhow, bail, Context as _, Result};
use ffmpeg_the_third as ffmpeg;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A resolved input, ready to open with `open()`.
pub struct MediaSource {
    /// What libavformat opens for the video (and the audio too, when
    /// the format is muxed): a local path or the stream's direct URL.
    pub video: PathBuf,
    /// Separate audio-only input (split DASH streams that yt-dlp
    /// returns for "bv*+ba" formats). The audio pipeline opens it with
    /// its own demuxer. None = the audio lives inside `video`.
    pub audio: Option<PathBuf>,
    /// Human-readable title (the one yt-dlp extracts). For `--info`.
    pub title: Option<String>,
}

/// Is this an http/https URL? (case-insensitive, like curl).
pub fn is_url(s: &str) -> bool {
    let l = s.get(..8).unwrap_or(s).to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://")
}

/// Host of a URL, lowercased, minus userinfo/port.
/// "https://user@WWW.YouTube.com:443/watch?v=x" → "www.youtube.com".
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let auth = rest.split(['/', '?', '#']).next()?;
    let no_user = auth.rsplit_once('@').map(|(_, h)| h).unwrap_or(auth);
    let host = no_user.split(':').next()?.to_ascii_lowercase();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// Sites resolved through yt-dlp automatically. The list is short on
/// purpose (the big ones); for any other site yt-dlp supports there's
/// `--ytdl`, which forces the resolution.
fn is_ytdl_host(host: &str) -> bool {
    const SITES: &[&str] = &[
        "youtube.com",
        "youtu.be",
        "twitch.tv",
        "vimeo.com",
        "dailymotion.com",
        "dai.ly",
    ];
    SITES
        .iter()
        .any(|s| host == *s || host.ends_with(&format!(".{s}")))
}

/// Resolve the input argument:
///   * local path → as-is,
///   * video site URL (or any URL with `--ytdl`) → yt-dlp,
///   * any other URL → straight to libavformat.
pub fn resolve(arg: &str, force_ytdl: bool, ytdl_format: &str) -> Result<MediaSource> {
    if !is_url(arg) {
        return Ok(MediaSource {
            video: PathBuf::from(arg),
            audio: None,
            title: None,
        });
    }
    let site = host_of(arg).map(|h| is_ytdl_host(&h)).unwrap_or(false);
    // Twitch fast path: live channel or VOD → native GQL+usher
    // (~0.3 s vs yt-dlp's ~2.5 s). Any failure (offline, rotated API,
    // subscriber-only VOD) drops to the usual yt-dlp route. --ytdl
    // forces it explicitly.
    if !force_ytdl {
        if let Some(login) = crate::twitch::channel_from_url(arg) {
            match crate::twitch::resolve(&login) {
                Ok(master) => {
                    eprintln!("Twitch: live stream of \"{login}\" resolved natively");
                    return Ok(MediaSource {
                        video: PathBuf::from(master),
                        audio: None,
                        title: Some(login),
                    });
                }
                Err(e) => eprintln!("native Twitch failed ({e}); trying yt-dlp…"),
            }
        } else if let Some(id) = crate::twitch::vod_from_url(arg) {
            match crate::twitch::resolve_vod(&id) {
                Ok(master) => {
                    eprintln!("Twitch: VOD {id} resolved natively");
                    return Ok(MediaSource {
                        video: PathBuf::from(master),
                        audio: None,
                        title: Some(format!("VOD {id}")),
                    });
                }
                Err(e) => eprintln!("native Twitch failed ({e}); trying yt-dlp…"),
            }
        }
    }
    if force_ytdl || site {
        return ytdl_resolve(arg, ytdl_format);
    }
    Ok(MediaSource {
        video: PathBuf::from(arg),
        audio: None,
        title: None,
    })
}

/// Open an input with libavformat. For URLs, adds sensible network
/// options (reconnect on drops, connection timeout). Drop-in
/// replacement for `ffmpeg::format::input` across all of rtv's
/// demuxers.
pub fn open(media: &Path) -> Result<ffmpeg::format::context::Input, ffmpeg::Error> {
    let s = media.to_string_lossy();
    if is_url(&s) {
        let mut opts = ffmpeg::Dictionary::new();
        // Automatic reconnect when the server drops mid-stream (CDNs).
        opts.set("reconnect", "1");
        opts.set("reconnect_streamed", "1");
        opts.set("reconnect_delay_max", "5");
        // Without a timeout, a silent server would freeze startup. µs.
        opts.set("rw_timeout", "15000000");
        // HLS (live Twitch, TV, .m3u8): hls demuxer options — live
        // edge, persistent connection between fragments, patience with
        // slow playlists. See hls.rs.
        if crate::hls::is_hls_url(&s) {
            crate::hls::open_opts(&mut opts);
        }
        let input = ffmpeg::format::input_with_dictionary(media, opts)?;
        // Live HLS: the hls demuxer flags the context UNSEEKABLE
        // (playlist without ENDLIST) → avformat_seek_file returns
        // ENOSYS and seeking "does nothing" (you always land back on
        // the live edge). The demuxer's seek algorithm DOES work over
        // the listed fragments; with the local DVR (hlsdvr.rs) the
        // EVENT playlist retains everything received, so clearing the
        // flag gives real seeking across that window. Without the DVR,
        // seeks land wherever the CDN's playlist still reaches (better
        // than nothing).
        if crate::hls::is_hls_url(&s) {
            unsafe {
                let ptr = input.as_ptr() as *mut ffmpeg::sys::AVFormatContext;
                (*ptr).ctx_flags &= !(ffmpeg::sys::AVFMTCTX_UNSEEKABLE as i32);
            }
        }
        Ok(input)
    } else {
        ffmpeg::format::input(media)
    }
}

/// The container's time offset (`start_time`, in seconds).
///
/// Twitch HLS VODs do not start at PTS 0: the first fragment keeps its
/// broadcast timestamps (e.g. start_time=62 s → real PTS run from 62
/// to 8432 against a declared duration of 8370). Left uncompensated,
/// the HUD starts "at minute 1", the progress bar is skewed and seeks
/// aim at shifted targets (the [0, duration-0.5] clamp doesn't cover
/// the real PTS range).
///
/// rtv's convention (same as ffplay/mpv): every consumer (video
/// decoder, audio decoder, subtitles) subtracts this same base from
/// the PTS it emits and adds it back to seek targets before calling
/// avformat_seek_file. The rest of the player then lives on a
/// 0..duration timeline and never needs to know about the offset.
/// Using the context's base (minimum across streams) rather than each
/// stream's own keeps A/V alignment intact.
///
/// Only applied to VODs (container with a declared duration). For live
/// it returns 0: each thread (video/audio/subs) opens its own context,
/// and on a live stream the playlist advances between opens → each
/// context would see a different start_time, and subtracting different
/// bases would desync A/V. Live already has its own rebase in the
/// player (live_start_pts, DVR window).
pub fn start_offset(input: &ffmpeg::format::context::Input) -> f64 {
    let (st, dur) = unsafe {
        let p = input.as_ptr();
        ((*p).start_time, (*p).duration)
    };
    if st == ffmpeg::ffi::AV_NOPTS_VALUE || dur <= 0 {
        return 0.0;
    }
    let secs = st as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE);
    // 1 ms threshold: the audio priming in many MP4s introduces
    // microscopic offsets that aren't worth touching the timeline for.
    if secs.is_finite() && secs.abs() > 0.001 {
        secs
    } else {
        0.0
    }
}

// ------------------------------------------------------------- yt-dlp --

/// Does `name` exist as an executable in any PATH directory?
fn in_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    std::env::split_paths(&paths).any(|d| !d.as_os_str().is_empty() && d.join(&exe).is_file())
}

/// Locate the yt-dlp executable. Order (freshest first, fallback
/// last): $RTV_YTDLP → PATH → bundled next to the rtv executable.
fn ytdlp_command() -> Command {
    if let Some(p) = std::env::var_os("RTV_YTDLP") {
        if !p.is_empty() {
            return Command::new(p);
        }
    }
    if in_path("yt-dlp") {
        return Command::new("yt-dlp");
    }
    // Bundled in the release, next to rtv?
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join(if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" });
            if cand.is_file() {
                return Command::new(cand);
            }
        }
    }
    Command::new("yt-dlp") // will fail with the clear message below
}

/// Turn a page URL into direct stream URL(s) with yt-dlp.
fn ytdl_resolve(url: &str, format: &str) -> Result<MediaSource> {
    eprintln!("[rtv] resolving with yt-dlp…");
    let out = ytdlp_command()
        .args([
            "--no-warnings",
            "--no-playlist",
            "--socket-timeout",
            "20",
            "-f",
            format,
            "--print",
            "title",
            "--print",
            "urls",
            "--",
            url,
        ])
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow!(
                    "yt-dlp is not installed (needed for video site \
                     URLs). Install it with pip/pkg/winget or point \
                     $RTV_YTDLP at the executable."
                )
            } else {
                anyhow!("couldn't run yt-dlp: {e}")
            }
        })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err.lines().rev().take(4).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        bail!("yt-dlp failed ({}):\n{}", out.status, tail.join("\n"));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (title, urls) =
        parse_ytdl_output(&stdout).context("unrecognized yt-dlp output")?;
    let mut it = urls.into_iter();
    let video = PathBuf::from(it.next().ok_or_else(|| anyhow!("yt-dlp returned no URLs"))?);
    let audio = it.next().map(PathBuf::from);
    if audio.is_some() {
        eprintln!(
            "[rtv] format has video and audio in separate streams \
             (dual input)"
        );
    }
    Ok(MediaSource {
        video,
        audio,
        title: Some(title),
    })
}

/// Parse the output of `--print title --print urls`: line 1 = title,
/// the rest = URLs (1 = muxed; 2 = separate video + audio).
fn parse_ytdl_output(out: &str) -> Result<(String, Vec<String>)> {
    let mut lines = out.lines().map(str::trim).filter(|l| !l.is_empty());
    let title = lines
        .next()
        .ok_or_else(|| anyhow!("empty output"))?
        .to_string();
    let urls: Vec<String> = lines
        .filter(|l| is_url(l))
        .map(str::to_string)
        .collect();
    if urls.is_empty() {
        bail!("no URLs in the output");
    }
    if urls.len() > 2 {
        bail!("yt-dlp returned {} URLs (a playlist?); expected 1 or 2", urls.len());
    }
    Ok((title, urls))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_detection() {
        assert!(is_url("http://a.com/v.mp4"));
        assert!(is_url("HTTPS://a.com/v.mp4"));
        assert!(!is_url("video.mp4"));
        assert!(!is_url("/path/with/http://inside"));
        assert!(!is_url("ftp://a.com/v.mp4")); // libavformat could open it,
                                               // but we don't treat it as network
    }

    #[test]
    fn hosts() {
        assert_eq!(
            host_of("https://user@WWW.YouTube.com:443/w?v=x").as_deref(),
            Some("www.youtube.com")
        );
        assert_eq!(host_of("http://a.com").as_deref(), Some("a.com"));
        assert_eq!(host_of("not-a-url"), None);
    }

    #[test]
    fn ytdl_hosts() {
        assert!(is_ytdl_host("youtube.com"));
        assert!(is_ytdl_host("www.youtube.com"));
        assert!(is_ytdl_host("music.youtube.com"));
        assert!(is_ytdl_host("youtu.be"));
        assert!(!is_ytdl_host("notyoutube.com")); // dotted suffix, not substring
        assert!(!is_ytdl_host("example.com"));
    }

    #[test]
    fn ytdl_output_muxed() {
        let (t, u) = parse_ytdl_output("My video\nhttps://cdn/x.mp4\n").unwrap();
        assert_eq!(t, "My video");
        assert_eq!(u, vec!["https://cdn/x.mp4"]);
    }

    #[test]
    fn ytdl_output_split() {
        let (t, u) =
            parse_ytdl_output("Title\nhttps://cdn/video\nhttps://cdn/audio\n").unwrap();
        assert_eq!(t, "Title");
        assert_eq!(u.len(), 2);
    }

    #[test]
    fn ytdl_output_bad() {
        assert!(parse_ytdl_output("").is_err());
        assert!(parse_ytdl_output("just a title, no urls\n").is_err());
    }

    #[test]
    fn resolve_local_and_direct() {
        let s = resolve("video.mp4", false, "b").unwrap();
        assert_eq!(s.video, PathBuf::from("video.mp4"));
        assert!(s.audio.is_none() && s.title.is_none());
        let s = resolve("https://cdn.example.com/v.mp4", false, "b").unwrap();
        assert_eq!(s.video, PathBuf::from("https://cdn.example.com/v.mp4"));
    }
}
