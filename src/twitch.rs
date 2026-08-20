//! twitch.rs — native Twitch resolution (live streams and VODs), no yt-dlp.
//!
//! Everything yt-dlp does for a Twitch live stream or VOD boils down
//! to two requests:
//!
//!   1. GQL `PlaybackAccessToken` (gql.twitch.tv, POST with the web
//!      player's public Client-ID) → token + signature. Live streams
//!      use isLive/login; VODs use isVod/vodID.
//!   2. usher.ttvnw.net/api/channel/hls/<channel>.m3u8 (live) or
//!      usher.ttvnw.net/vod/<id>.m3u8 (VOD) ?token=…&sig=… →
//!      master playlist listing the available qualities.
//!
//! yt-dlp takes ~2.5 s (Python startup plus the generic extractor);
//! doing it natively over libavformat's own HTTP stack (zero new
//! dependencies) takes ~0.3 s. If anything goes wrong (Twitch rotated
//! the persisted query hash, channel offline, subscriber-only VOD…)
//! source::resolve falls back to yt-dlp as usual — this is a fast
//! path, not a replacement.
//!
//! The resulting master goes through hls::pick_variant (picks ≤1080p)
//! and then the rest of the existing HLS pipeline.

use anyhow::{anyhow, bail, Result};
use ffmpeg_the_third as ffmpeg;
use ffmpeg::sys as ff;
use std::ffi::CString;

/// Public Client-ID of the Twitch web player (the same one streamlink
/// and yt-dlp use; not a secret, it ships in twitch.tv's JS).
const CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";

/// Hash of the `PlaybackAccessToken` persisted query (stable for
/// years; should Twitch rotate it, resolve() fails and we fall back
/// to yt-dlp).
const PAT_HASH: &str = "0828119ded1c13477966434e15800ff57ddacf13ba1911c129dc2200705b0712";

/// Is this a Twitch live channel URL? → channel login.
/// Accepts twitch.tv/<channel> (with or without www/m). VODs go
/// through vod_from_url; any other path returns None → yt-dlp.
pub fn channel_from_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let rest = rest
        .strip_prefix("www.")
        .or_else(|| rest.strip_prefix("m."))
        .unwrap_or(rest);
    let rest = rest.strip_prefix("twitch.tv/")?;
    let login = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Channel root only: twitch.tv/channel (twitch.tv/videos/… and
    // twitch.tv/directory/… are not channel live pages).
    let after = &rest[login.len()..];
    if !after.is_empty() && !after.starts_with(['?', '#']) {
        return None;
    }
    // Valid Twitch logins are roughly [a-zA-Z0-9_]{3,25}. Also filter
    // out the obvious reserved paths.
    if login.len() < 3
        || login.len() > 25
        || !login.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        || matches!(login, "videos" | "directory" | "settings" | "downloads" | "jobs" | "p")
    {
        return None;
    }
    Some(login.to_ascii_lowercase())
}

/// Is this a Twitch VOD URL (twitch.tv/videos/<ID>)? → numeric ID.
/// Accepts www/m and query strings (twitch.tv/videos/123?t=1h2m3s).
pub fn vod_from_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let rest = rest
        .strip_prefix("www.")
        .or_else(|| rest.strip_prefix("m."))
        .unwrap_or(rest);
    let rest = rest.strip_prefix("twitch.tv/videos/")?;
    let id = rest.split(['/', '?', '#']).next().unwrap_or("");
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(id.to_string())
}

/// Resolve a live channel → usher master playlist URL. Errors out when
/// the channel is offline or Twitch changed the API — the caller then
/// falls back to yt-dlp.
pub fn resolve(login: &str) -> Result<String> {
    let vars = format!(
        r#"{{"isLive":true,"login":"{login}","isVod":false,"vodID":"","playerType":"embed"}}"#
    );
    let (token, sig) = playback_token(&vars, "channel offline?")?;
    Ok(format!(
        "https://usher.ttvnw.net/api/channel/hls/{login}.m3u8\
         ?client_id={CLIENT_ID}&token={}&sig={}&allow_source=true\
         &allow_audio_only=true&fast_bread=true&player_backend=mediaplayer\
         &playlist_include_framerate=true&supported_codecs=h265,h264&p={}",
        pct(&token),
        pct(&sig),
        cache_buster(),
    ))
}

/// Resolve a VOD → usher master playlist URL (/vod/). Same as
/// resolve() but with the video-flavoured token (isVod:true). The
/// resulting playlist carries #EXT-X-ENDLIST, so the HLS demuxer
/// treats it as a regular VOD: clean native seeking, no local DVR.
pub fn resolve_vod(id: &str) -> Result<String> {
    let vars = format!(
        r#"{{"isLive":false,"login":"","isVod":true,"vodID":"{id}","playerType":"embed"}}"#
    );
    let (token, sig) = playback_token(&vars, "VOD deleted or sub-only?")?;
    Ok(format!(
        "https://usher.ttvnw.net/vod/{id}.m3u8\
         ?client_id={CLIENT_ID}&token={}&sig={}&allow_source=true\
         &allow_audio_only=true&player_backend=mediaplayer\
         &playlist_include_framerate=true&supported_codecs=h265,h264&p={}",
        pct(&token),
        pct(&sig),
        cache_buster(),
    ))
}

/// GQL `PlaybackAccessToken` → (token, signature). `vars` is the
/// already-serialized variables object (live or VOD); `hint` adds
/// context to the error message.
fn playback_token(vars: &str, hint: &str) -> Result<(String, String)> {
    let body = format!(
        concat!(
            r#"{{"operationName":"PlaybackAccessToken","#,
            r#""extensions":{{"persistedQuery":{{"version":1,"sha256Hash":"{}"}}}},"#,
            r#""variables":{}}}"#
        ),
        PAT_HASH, vars
    );
    let resp = http_post(
        "https://gql.twitch.tv/gql",
        &format!("Client-ID: {CLIENT_ID}\r\nContent-Type: text/plain;charset=UTF-8\r\n"),
        body.as_bytes(),
    )?;
    // Response: {"data":{"streamPlaybackAccessToken":{"value":"…","signature":"…"}}}
    // (or videoPlaybackAccessToken for VODs). value is escaped JSON
    // nested inside the JSON. When access is denied the field is null.
    let token = json_str_field(&resp, "value")
        .ok_or_else(|| anyhow!("Twitch GQL: no token ({hint})"))?;
    let sig = json_str_field(&resp, "signature")
        .ok_or_else(|| anyhow!("Twitch GQL: no signature"))?;
    Ok((token, sig))
}

/// Random p= value (CDN cache buster, same as the web player).
fn cache_buster() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() % 10_000_000)
        .unwrap_or(1_234_567)
}

/// RFC 3986 percent-encoding (everything except unreserved chars).
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Extract the first string field `"name":"…"` from a JSON blob,
/// undoing escapes (\" \\ \n \uXXXX…). Deliberately a minimal parser:
/// the two responses we care about are flat, and this saves pulling in
/// serde for just this.
fn json_str_field(json: &str, name: &str) -> Option<String> {
    let needle = format!("\"{name}\":");
    let at = json.find(&needle)? + needle.len();
    let rest = json[at..].trim_start();
    let mut chars = rest.strip_prefix('"')?.chars();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    let cp = u32::from_str_radix(&hex, 16).ok()?;
                    out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                }
                other => out.push(other),
            },
            _ => out.push(c),
        }
    }
    None
}

/// HTTP POST via libavformat's stack (avio_open2 + post_data).
/// `post_data` forces the POST method and sends the body during the
/// handshake; the response reads like any other avio stream. The body
/// travels as a BINARY option → hex string (av_opt_set convention).
fn http_post(url: &str, headers: &str, body: &[u8]) -> Result<String> {
    let _ = ffmpeg::init();
    let curl = CString::new(url).map_err(|_| anyhow!("URL contains NUL"))?;
    let hex: String = body.iter().map(|b| format!("{b:02x}")).collect();
    unsafe {
        let mut opts: *mut ff::AVDictionary = std::ptr::null_mut();
        for (k, v) in [
            ("headers", headers),
            ("post_data", hex.as_str()),
            ("rw_timeout", "10000000"),
        ] {
            let ck = CString::new(k).unwrap();
            let cv = CString::new(v).unwrap();
            ff::av_dict_set(&mut opts, ck.as_ptr(), cv.as_ptr(), 0);
        }
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
            bail!("avio_open2 POST: error {ret}");
        }
        let mut data = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = ff::avio_read(ctx, chunk.as_mut_ptr() as *mut _, chunk.len() as i32);
            if n <= 0 {
                break;
            }
            data.extend_from_slice(&chunk[..n as usize]);
            if data.len() >= 512 * 1024 {
                break;
            }
        }
        ff::avio_closep(&mut ctx);
        Ok(String::from_utf8_lossy(&data).into_owned())
    }
}

// ---------------------------------------------------------------- test --

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_channels() {
        assert_eq!(channel_from_url("https://www.twitch.tv/smaugy"), Some("smaugy".into()));
        assert_eq!(channel_from_url("https://twitch.tv/Canal_123"), Some("canal_123".into()));
        assert_eq!(channel_from_url("https://m.twitch.tv/abc?x=1"), Some("abc".into()));
        assert_eq!(channel_from_url("https://twitch.tv/videos/123456"), None);
        assert_eq!(channel_from_url("https://twitch.tv/directory/gaming"), None);
        assert_eq!(channel_from_url("https://twitch.tv/ab"), None); // too short
        assert_eq!(channel_from_url("https://youtube.com/watch?v=x"), None);
        assert_eq!(channel_from_url("https://twitch.tv/canal/clip/xyz"), None);
    }

    #[test]
    fn detects_vods() {
        assert_eq!(vod_from_url("https://www.twitch.tv/videos/2354946000"), Some("2354946000".into()));
        assert_eq!(vod_from_url("https://twitch.tv/videos/123?t=1h2m3s"), Some("123".into()));
        assert_eq!(vod_from_url("https://m.twitch.tv/videos/99/"), Some("99".into()));
        assert_eq!(vod_from_url("https://twitch.tv/videos/"), None);
        assert_eq!(vod_from_url("https://twitch.tv/videos/abc"), None);
        assert_eq!(vod_from_url("https://twitch.tv/smaugy"), None);
        assert_eq!(vod_from_url("https://youtube.com/videos/1"), None);
    }

    #[test]
    fn json_field_with_escapes() {
        let j = r#"{"data":{"tok":{"value":"{\"a\":1,\"b\":\"x\"}","signature":"abc123"}}}"#;
        assert_eq!(json_str_field(j, "value"), Some(r#"{"a":1,"b":"x"}"#.into()));
        assert_eq!(json_str_field(j, "signature"), Some("abc123".into()));
        assert_eq!(json_str_field(j, "nada"), None);
        let u = r#"{"value":"a\u00e9b"}"#;
        assert_eq!(json_str_field(u, "value"), Some("aéb".into()));
    }

    #[test]
    fn pct_encoding() {
        assert_eq!(pct("abc-_.~123"), "abc-_.~123");
        assert_eq!(pct("{\"a\":1}"), "%7B%22a%22%3A1%7D");
        assert_eq!(pct("a b"), "a%20b");
    }
}
