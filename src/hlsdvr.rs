//! hlsdvr.rs — local DVR for live HLS: a proxy that downloads
//! fragments as they're published and serves them to the demuxer from
//! memory.
//!
//! WHY: playing a live stream by reading from the CDN "at playback
//! pace" has three ills this module kills at the root:
//!
//!   1. STUTTERS — every fragment boundary pays for network on the
//!      demuxer's hot path (even if http_multiple overlaps some of
//!      it). Here a thread DOWNLOADS fragments as soon as the playlist
//!      publishes them, at network speed, and the demuxer ALWAYS reads
//!      from localhost (memory): zero network jitter during playback.
//!
//!   2. SEEKING — Twitch's playlist only retains ~30 s and the hls
//!      demuxer marks the context UNSEEKABLE (playlist without
//!      ENDLIST). The proxy serves an EVENT playlist that GROWS and
//!      retains everything received since startup: a real DVR window
//!      to move through (source::open clears the UNSEEKABLE flag).
//!
//!   3. LAZY LOADING / PRELOAD — if you fall behind (pause, seek
//!      back), downloading does NOT stop: everything the channel keeps
//!      broadcasting stays preloaded locally, and watching it "time
//!      shifted" never touches the network. The latency against the
//!      live edge shows up in --stats (via `stats()`).
//!
//!   4. ADS — during an ad the download continues unchanged (it never
//!      stopped), the playlist is polled faster (400 ms) and the
//!      #EXT-X-TWITCH-PREFETCH segments (fast_bread's advanced edge)
//!      are pulled: when the ad ends the live content is already in
//!      the DVR and the return to live has no stutters. (Note: with
//!      stitched ads Twitch replaces the viewer's fragments
//!      server-side — the content broadcast DURING the ad isn't
//!      delivered in this playlist; what we guarantee is a perfectly
//!      smooth exit.)
//!
//! Design: zero new dependencies. Download HTTP goes through
//! libavformat's avio stack (hls::http_get*), and the serving side is
//! a minimal HTTP server on std::net::TcpListener at
//! 127.0.0.1:ephemeral-port. It only answers GET /live.m3u8, /seg/N
//! and /init from our own local demuxer.
//!
//! Memory: configurable cap RTV_DVR_MB (512 MB by default ≈ 10-15 min
//! of Twitch 1080p60). Past the cap the oldest fragments are dropped
//! (the playlist advances MEDIA-SEQUENCE, just like a CDN would).

use anyhow::{anyhow, Result};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Total milliseconds RECEIVED (the live edge, since DVR startup).
/// 0 = nothing received yet.
static EDGE_MS: AtomicU64 = AtomicU64::new(0);
/// End (ms) of the last fragment SERVED to the demuxer ≈ playback
/// position (give or take the small lead of the decode queue).
static SERVED_MS: AtomicU64 = AtomicU64::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// The fragment currently being SERVED to the demuxer is an ad.
static AD_PLAYING: AtomicBool = AtomicBool::new(false);
/// Total ad milliseconds RECEIVED (for --stats / telemetry).
static AD_MS: AtomicU64 = AtomicU64::new(0);

/// For the --stats HUD: (seconds_behind_live, total_dvr_s).
/// None when the DVR isn't active.
pub fn stats() -> Option<(f64, f64)> {
    if !ACTIVE.load(Ordering::Relaxed) {
        return None;
    }
    let edge = EDGE_MS.load(Ordering::Relaxed);
    let served = SERVED_MS.load(Ordering::Relaxed);
    let behind = edge.saturating_sub(served) as f64 / 1000.0;
    Some((behind, edge as f64 / 1000.0))
}

/// Is what's playing RIGHT NOW an ad? (detection only: DATERANGE
/// twitch-stitched-ad / SCTE-35 CUE-OUT / ad EXTINF title — to know
/// one showed up and, down the road, act on it.)
pub fn ad_playing() -> bool {
    ACTIVE.load(Ordering::Relaxed) && AD_PLAYING.load(Ordering::Relaxed)
}

/// Total ad seconds received since DVR startup.
pub fn ad_total_s() -> f64 {
    AD_MS.load(Ordering::Relaxed) as f64 / 1000.0
}

struct Seg {
    /// GLOBAL sequence number (never reset when old ones are dropped).
    seq: u64,
    dur_ms: u64,
    /// End of the fragment in ms since DVR startup.
    end_ms: u64,
    /// #EXT-X-DISCONTINUITY before this fragment? (Twitch ads)
    disc_before: bool,
    /// Is this fragment an ad? (see detection in parse_media)
    is_ad: bool,
    data: Vec<u8>,
}

struct Store {
    segs: VecDeque<Seg>,
    /// fMP4 init fragment (#EXT-X-MAP), served at /init.
    init: Option<Vec<u8>>,
    target_dur: u32,
    /// The source ended (ENDLIST or dead) → so does our playlist.
    ended: bool,
    bytes: usize,
}

/// If `url` is a live HLS MEDIA playlist (no ENDLIST), starts the DVR
/// and returns the local URL to use instead. VOD, master or error →
/// None (the original URL plays as-is).
pub fn start_if_live(url: &str) -> Option<String> {
    if !crate::hls::is_hls_url(url) {
        return None;
    }
    let text = crate::hls::http_get(url, 1024 * 1024).ok()?;
    if !text.contains("#EXTM3U") || text.contains("#EXT-X-ENDLIST") {
        return None; // VOD (or not HLS): the demuxer already nails it
    }
    if text.contains("#EXT-X-STREAM-INF") {
        return None; // master: pick_variant comes first, then here
    }
    match start(url.to_string(), text) {
        Ok(local) => {
            eprintln!("live HLS: local DVR active ({local})");
            Some(local)
        }
        Err(e) => {
            eprintln!("local DVR unavailable ({e}); playing directly");
            None
        }
    }
}

fn start(url: String, first_playlist: String) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let store = Arc::new(Mutex::new(Store {
        segs: VecDeque::new(),
        init: None,
        target_dur: 6,
        ended: false,
        bytes: 0,
    }));
    let cap_bytes = std::env::var("RTV_DVR_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(512)
        .saturating_mul(1024 * 1024);

    // ── downloader thread ──────────────────────────────────────────
    {
        let store = Arc::clone(&store);
        std::thread::Builder::new()
            .name("hlsdvr-dl".into())
            .spawn(move || downloader(url, first_playlist, store, cap_bytes))
            .map_err(|e| anyhow!("spawn dl: {e}"))?;
    }
    // ── server thread ──────────────────────────────────────────────
    {
        let store = Arc::clone(&store);
        std::thread::Builder::new()
            .name("hlsdvr-srv".into())
            .spawn(move || {
                for conn in listener.incoming().flatten() {
                    let store = Arc::clone(&store);
                    let _ = std::thread::Builder::new()
                        .name("hlsdvr-conn".into())
                        .spawn(move || handle_conn(conn, store));
                }
            })
            .map_err(|e| anyhow!("spawn srv: {e}"))?;
    }
    // Wait for the first fragment: if the demuxer opens an empty
    // playlist it finds no streams and aborts. Generous timeout; if
    // nothing arrives, falling back to direct playback beats failing.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(12);
    loop {
        {
            let st = store.lock().unwrap();
            if !st.segs.is_empty() || st.ended {
                if st.segs.is_empty() {
                    return Err(anyhow!("source ended with no fragments"));
                }
                break;
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(anyhow!("no fragments after 12s"));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    ACTIVE.store(true, Ordering::Relaxed);
    Ok(format!("http://127.0.0.1:{port}/live.m3u8"))
}

/// SOURCE playlist entry awaiting download.
struct RemoteSeg {
    seq: u64,
    dur_ms: u64,
    disc_before: bool,
    is_ad: bool,
    /// #EXT-X-TWITCH-PREFETCH segment: the advanced edge Twitch
    /// publishes before the official EXTINF. If its download fails
    /// (still being uploaded to the CDN) it is NOT skipped: it'll
    /// arrive as official later.
    prefetch: bool,
    url: String,
}

/// Parses a MEDIA playlist: fragments with global sequence, MAP and
/// target duration. (Master playlists are hls.rs's business.)
fn parse_media(text: &str, base: &str) -> (Vec<RemoteSeg>, Option<String>, u32, bool) {
    let mut segs = Vec::new();
    let mut map_url = None;
    let mut target = 6u32;
    let mut seq = 0u64;
    let mut dur_ms = 0u64;
    let mut disc = false;
    let mut ended = false;
    // AD detection (of any kind). Two mechanisms:
    //  * in_ad_range: a RANGE tag opened an ad break
    //    (#EXT-X-DATERANGE with the twitch-stitched-ad class, or
    //    SCTE-35 #EXT-X-CUE-OUT) — every fragment until the close
    //    (CUE-IN / DATERANGE whose END-DATE already passed) is an ad.
    //  * next_is_ad: PER-FRAGMENT signal — the EXTINF title Twitch
    //    puts on ad fragments ("Amazon|123..."; normal content says
    //    "live"). Only marks the very next fragment.
    let mut in_ad_range = false;
    let mut next_is_ad = false;
    let mut next_is_live = false;
    // Advanced edge: #EXT-X-TWITCH-PREFETCH URLs (fast_bread). They
    // continue the sequence after the last official fragment; their
    // duration isn't declared → estimated from the last EXTINF.
    let mut prefetch_urls: Vec<String> = Vec::new();
    let mut last_dur_ms = 2000u64;
    for line in text.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            seq = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            target = v.trim().parse().unwrap_or(6);
        } else if let Some(v) = line.strip_prefix("#EXTINF:") {
            let mut it = v.splitn(2, ',');
            let d: f64 = it.next().unwrap_or("0").trim().parse().unwrap_or(0.0);
            dur_ms = (d * 1000.0).round() as u64;
            if dur_ms > 0 {
                last_dur_ms = dur_ms;
            }
            // EXTINF title: Twitch tags ad fragments with the provider
            // ("Amazon|...") and real content as "live". Any title
            // starting with a known provider or containing "-ad-"
            // counts as an ad.
            let title = it.next().unwrap_or("").trim();
            if title == "live" {
                next_is_live = true;
            } else if !title.is_empty() {
                let t = title.to_ascii_lowercase();
                if t.starts_with("amazon") || t.contains("stitched-ad") || t.contains("commercial") {
                    next_is_ad = true;
                }
            }
        } else if line == "#EXT-X-DISCONTINUITY" {
            disc = true;
        } else if let Some(v) = line.strip_prefix("#EXT-X-DATERANGE:") {
            // Twitch stitched ads: CLASS="twitch-stitched-ad". The
            // range spans from this point for its duration; as a
            // robust approximation, open ad mode until "live" content
            // or a CUE-IN shows up.
            if v.contains("twitch-stitched-ad") || v.to_ascii_lowercase().contains("class=\"ad") {
                in_ad_range = true;
            }
        } else if line.starts_with("#EXT-X-CUE-OUT") {
            // SCTE-35: start (or continuation) of an ad break.
            in_ad_range = true;
        } else if line.starts_with("#EXT-X-CUE-IN") {
            in_ad_range = false;
        } else if let Some(v) = line.strip_prefix("#EXT-X-MAP:") {
            if let Some(u) = crate::hls::attr_value(v, "URI") {
                map_url = Some(crate::hls::join_url(base, u));
            }
        } else if line == "#EXT-X-ENDLIST" {
            ended = true;
        } else if let Some(u) = line.strip_prefix("#EXT-X-TWITCH-PREFETCH:") {
            let u = u.trim();
            if !u.is_empty() {
                prefetch_urls.push(crate::hls::join_url(base, u));
            }
        } else if !line.is_empty() && !line.starts_with('#') {
            // A fragment titled "live" CLOSES range-based ad mode:
            // Twitch's DATERANGEs don't always emit an explicit close
            // visible within the playlist window, but real content
            // comes back tagged "live".
            if next_is_live {
                in_ad_range = false;
            }
            let is_ad = (in_ad_range || next_is_ad) && !next_is_live;
            segs.push(RemoteSeg {
                seq,
                dur_ms: dur_ms.max(1),
                disc_before: disc,
                is_ad,
                prefetch: false,
                url: crate::hls::join_url(base, line),
            });
            seq += 1;
            dur_ms = 0;
            disc = false;
            next_is_ad = false;
            next_is_live = false;
        }
    }
    // Materialize the prefetches as fragments that CONTINUE the
    // sequence. Key during ADS: the DVR keeps pulling the stream's
    // advanced edge so that when the ad ends there's already content
    // downloaded and the return to live has no stutters.
    for u in prefetch_urls {
        segs.push(RemoteSeg {
            seq,
            dur_ms: last_dur_ms,
            disc_before: false,
            is_ad: in_ad_range,
            prefetch: true,
            url: u,
        });
        seq += 1;
    }
    (segs, map_url, target, ended)
}

fn downloader(url: String, first: String, store: Arc<Mutex<Store>>, cap: usize) {
    let mut next_seq: Option<u64> = None; // first fragment not pinned yet
    let mut fails = 0u32;
    let mut playlist = Some(first);
    loop {
        let text = match playlist.take() {
            Some(t) => t,
            None => match crate::hls::http_get(&url, 1024 * 1024) {
                Ok(t) => {
                    fails = 0;
                    t
                }
                Err(_) => {
                    fails += 1;
                    if fails > 20 {
                        store.lock().unwrap().ended = true;
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1000));
                    continue;
                }
            },
        };
        let (remote, map_url, target, src_ended) = parse_media(&text, &url);
        {
            let mut st = store.lock().unwrap();
            st.target_dur = target;
            if st.init.is_none() {
                if let Some(mu) = &map_url {
                    st.init = crate::hls::http_get_bytes(mu, 16 * 1024 * 1024).ok();
                }
            }
        }
        // First pass: start 3 fragments behind the edge (same as
        // live_start_index=-3) — the DVR retains FROM here. Prefetches
        // don't count as the edge: they may not be on the CDN yet.
        if next_seq.is_none() {
            let official: Vec<u64> =
                remote.iter().filter(|r| !r.prefetch).map(|r| r.seq).collect();
            let n = official.len();
            next_seq = Some(*official.get(n.saturating_sub(3)).unwrap_or(&0));
        }
        let from = next_seq.unwrap_or(0);
        for rs in remote.iter().filter(|r| r.seq >= from) {
            match crate::hls::http_get_bytes(&rs.url, 64 * 1024 * 1024) {
                Ok(data) => {
                    if rs.is_ad {
                        AD_MS.fetch_add(rs.dur_ms, Ordering::Relaxed);
                    }
                    let mut st = store.lock().unwrap();
                    let end_ms = st.segs.back().map(|s| s.end_ms).unwrap_or(0) + rs.dur_ms;
                    st.bytes += data.len();
                    st.segs.push_back(Seg {
                        seq: rs.seq,
                        dur_ms: rs.dur_ms,
                        end_ms,
                        disc_before: rs.disc_before,
                        is_ad: rs.is_ad,
                        data,
                    });
                    EDGE_MS.store(end_ms, Ordering::Relaxed);
                    next_seq = Some(rs.seq + 1);
                    // Memory cap: drop the oldest ones.
                    while st.bytes > cap && st.segs.len() > 4 {
                        if let Some(old) = st.segs.pop_front() {
                            st.bytes -= old.data.len();
                        }
                    }
                }
                Err(_) => {
                    if rs.prefetch {
                        // Prefetch not on the CDN yet: do NOT skip it
                        // — it'll show up as official on the next
                        // reload and get downloaded then.
                        break;
                    }
                    // Fragment expired on the CDN (the download truly
                    // fell behind): skip it and move on.
                    next_seq = Some(rs.seq + 1);
                }
            }
        }
        if src_ended {
            store.lock().unwrap().ended = true;
            return;
        }
        // Twitch publishes every ~2 s; polling at 1 s leaves plenty of
        // margin. During an AD we poll faster (400 ms): that way we
        // latch onto the first live fragment as soon as Twitch
        // publishes it and the exit from the ad is seamless.
        let in_ad = remote.last().map(|r| r.is_ad).unwrap_or(false);
        let poll_ms = if in_ad { 400 } else { 1000 };
        std::thread::sleep(std::time::Duration::from_millis(poll_ms));
    }
}

fn handle_conn(mut conn: TcpStream, store: Arc<Mutex<Store>>) {
    let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let mut buf = [0u8; 2048];
    let mut req = Vec::new();
    // Read until end of headers (the demuxer sends small GETs).
    loop {
        match conn.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                req.extend_from_slice(&buf[..n]);
                if req.windows(4).any(|w| w == b"\r\n\r\n") || req.len() > 16 * 1024 {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    let line = String::from_utf8_lossy(&req);
    let path = line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .split(['?', '#'])
        .next()
        .unwrap_or("/")
        .to_string();

    if path == "/live.m3u8" {
        let body = build_playlist(&store);
        let _ = write_resp(&mut conn, "application/vnd.apple.mpegurl", body.as_bytes());
    } else if path == "/init" {
        let data = store.lock().unwrap().init.clone().unwrap_or_default();
        let _ = write_resp(&mut conn, "video/mp4", &data);
    } else if let Some(nstr) = path.strip_prefix("/seg/") {
        let n: u64 = nstr.trim_end_matches(".ts").parse().unwrap_or(u64::MAX);
        let found = {
            let st = store.lock().unwrap();
            st.segs
                .iter()
                .find(|s| s.seq == n)
                .map(|s| (s.data.clone(), s.end_ms, s.is_ad))
        };
        match found {
            Some((data, end_ms, is_ad)) => {
                // Playback position ≈ the last fragment served.
                SERVED_MS.store(end_ms, Ordering::Relaxed);
                // Ad detection: what the demuxer is consuming NOW is
                // (or isn't) an ad — the player shows it in the HUD
                // and could act on it in the future (mute, visual
                // skip…).
                AD_PLAYING.store(is_ad, Ordering::Relaxed);
                let _ = write_resp(&mut conn, "video/mp2t", &data);
            }
            None => {
                let _ = conn.write_all(
                    b"HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                );
            }
        }
    } else {
        let _ = conn.write_all(
            b"HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        );
    }
}

fn build_playlist(store: &Arc<Mutex<Store>>) -> String {
    let st = store.lock().unwrap();
    let mut out = String::with_capacity(64 + st.segs.len() * 40);
    out.push_str("#EXTM3U\n#EXT-X-VERSION:3\n");
    out.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", st.target_dur));
    // PLAYLIST-TYPE:EVENT: "append-only at the end" — the demuxer
    // knows it can seek within what's already listed. MEDIA-SEQUENCE
    // only advances when the memory cap dropped old fragments.
    out.push_str("#EXT-X-PLAYLIST-TYPE:EVENT\n");
    let first = st.segs.front().map(|s| s.seq).unwrap_or(0);
    out.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{first}\n"));
    if st.init.is_some() {
        out.push_str("#EXT-X-MAP:URI=\"/init\"\n");
    }
    for s in &st.segs {
        if s.disc_before {
            out.push_str("#EXT-X-DISCONTINUITY\n");
        }
        out.push_str(&format!(
            "#EXTINF:{:.3},\n/seg/{}\n",
            s.dur_ms as f64 / 1000.0,
            s.seq
        ));
    }
    if st.ended {
        out.push_str("#EXT-X-ENDLIST\n");
    }
    out
}

fn write_resp(conn: &mut TcpStream, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    conn.write_all(head.as_bytes())?;
    conn.write_all(body)?;
    conn.flush()
}

// ---------------------------------------------------------------- test --

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n\
#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:2.000,live\nseg100.ts\n#EXTINF:2.000,live\n\
seg101.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:2.002,Amazon\nad0.ts\n";

    #[test]
    fn parses_media_playlist() {
        let (segs, map, target, ended) = parse_media(SAMPLE, "https://cdn.x/v1/pl.m3u8");
        assert_eq!(target, 6);
        assert!(!ended);
        assert!(map.is_none());
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].seq, 100);
        assert_eq!(segs[0].dur_ms, 2000);
        assert_eq!(segs[0].url, "https://cdn.x/v1/seg100.ts");
        assert!(!segs[1].disc_before);
        assert!(segs[2].disc_before); // the ad comes after DISCONTINUITY
        assert_eq!(segs[2].seq, 102);
        // Ad detection: EXTINF title "Amazon" -> is_ad; "live" doesn't.
        assert!(!segs[0].is_ad);
        assert!(!segs[1].is_ad);
        assert!(segs[2].is_ad);
    }

    #[test]
    fn detects_scte35_and_daterange_ads() {
        // SCTE-35: CUE-OUT opens the break, CUE-IN closes it.
        let pl = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:2.0,\na.ts\n\
#EXT-X-CUE-OUT:30\n#EXTINF:2.0,\nb.ts\n#EXTINF:2.0,\nc.ts\n\
#EXT-X-CUE-IN\n#EXTINF:2.0,\nd.ts\n";
        let (segs, _, _, _) = parse_media(pl, "https://x/p.m3u8");
        assert_eq!(segs.iter().map(|s| s.is_ad).collect::<Vec<_>>(), vec![false, true, true, false]);
        // DATERANGE twitch-stitched-ad: opens ad mode.
        let pl2 = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:2.0,live\na.ts\n\
#EXT-X-DATERANGE:ID=\"stitched-ad-123\",CLASS=\"twitch-stitched-ad\",START-DATE=\"2026\"\n\
#EXT-X-DISCONTINUITY\n#EXTINF:2.0,Amazon|x\nad1.ts\n#EXTINF:2.0,Amazon|x\nad2.ts\n";
        let (segs2, _, _, _) = parse_media(pl2, "https://x/p.m3u8");
        assert_eq!(segs2.iter().map(|s| s.is_ad).collect::<Vec<_>>(), vec![false, true, true]);
        // "live" content coming back CLOSES the DATERANGE ad mode.
        let pl3 = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n\
#EXT-X-DATERANGE:CLASS=\"twitch-stitched-ad\"\n\
#EXTINF:2.0,Amazon|x\nad1.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:2.0,live\nc.ts\n#EXTINF:2.0,live\nd.ts\n";
        let (segs3, _, _, _) = parse_media(pl3, "https://x/p.m3u8");
        assert_eq!(segs3.iter().map(|s| s.is_ad).collect::<Vec<_>>(), vec![true, false, false]);
    }

    #[test]
    fn prefetch_continues_the_sequence() {
        // fast_bread: TWITCH-PREFETCH entries continue the sequence
        // after the last official one, duration estimated from the
        // last EXTINF.
        let pl = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:2.000,live\na.ts\n\
#EXTINF:2.500,live\nb.ts\n\
#EXT-X-TWITCH-PREFETCH:https://cdn.x/pre1.ts\n\
#EXT-X-TWITCH-PREFETCH:https://cdn.x/pre2.ts\n";
        let (segs, _, _, ended) = parse_media(pl, "https://cdn.x/pl.m3u8");
        assert!(!ended);
        assert_eq!(segs.len(), 4);
        assert!(!segs[1].prefetch);
        assert!(segs[2].prefetch && segs[3].prefetch);
        assert_eq!(segs[2].seq, 12);
        assert_eq!(segs[3].seq, 13);
        assert_eq!(segs[2].dur_ms, 2500); // estimated = last EXTINF
        assert_eq!(segs[2].url, "https://cdn.x/pre1.ts");
        // During an open ad (CUE-OUT with no close) the prefetch
        // inherits the range state.
        let pl2 = "#EXTM3U\n#EXT-X-CUE-OUT:30\n#EXTINF:2.0,\nad.ts\n\
#EXT-X-TWITCH-PREFETCH:https://cdn.x/p.ts\n";
        let (s2, _, _, _) = parse_media(pl2, "https://cdn.x/pl.m3u8");
        assert!(s2[0].is_ad && s2[1].is_ad && s2[1].prefetch);
    }

    #[test]
    fn detects_endlist_and_map() {
        let vod = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\na.m4s\n#EXT-X-ENDLIST\n";
        let (segs, map, _, ended) = parse_media(vod, "https://h.x/d/pl.m3u8");
        assert!(ended);
        assert_eq!(map.as_deref(), Some("https://h.x/d/init.mp4"));
        assert_eq!(segs.len(), 1);
    }

    #[test]
    fn local_playlist_is_correct() {
        let store = Arc::new(Mutex::new(Store {
            segs: VecDeque::from([
                Seg { seq: 5, dur_ms: 2000, end_ms: 2000, disc_before: false, is_ad: false, data: vec![1] },
                Seg { seq: 6, dur_ms: 2002, end_ms: 4002, disc_before: true, is_ad: true, data: vec![2] },
            ]),
            init: None,
            target_dur: 6,
            ended: false,
            bytes: 2,
        }));
        let pl = build_playlist(&store);
        assert!(pl.contains("#EXT-X-MEDIA-SEQUENCE:5"));
        assert!(pl.contains("#EXT-X-PLAYLIST-TYPE:EVENT"));
        assert!(pl.contains("#EXTINF:2.000,\n/seg/5"));
        assert!(pl.contains("#EXT-X-DISCONTINUITY\n#EXTINF:2.002,\n/seg/6"));
        assert!(!pl.contains("#EXT-X-ENDLIST"));
        store.lock().unwrap().ended = true;
        assert!(build_playlist(&store).contains("#EXT-X-ENDLIST"));
    }
}
