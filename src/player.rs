//! Main player loop.
//!
//! The sync engine follows ffplay.c:
//!
//!   * Two `FfClock`s: `audclk` (updated by the cpal callback with
//!     the PTS of the sample BEING HEARD, `playback_delay`
//!     compensated) and `vidclk` (updated on every shown frame).
//!   * Master clock = `audclk` when there's audio, `vidclk` otherwise.
//!   * `compute_target_delay(last_duration, video_pts, master_now)`:
//!     same logic as ffplay, with thresholds `MIN=40ms`, `MAX=100ms`,
//!     `FRAMEDUP=100ms`, `NOSYNC=10s`.
//!   * Atomic "hr-seek": `master.set(target)` bumps both clocks'
//!     serials; the video decoder uses `AVSEEK_FLAG_BACKWARD` and
//!     drops frames until it reaches `target_pts` (drop-until-target-PTS,
//!     like mpv's `--hr-seek-framedrop=yes`). Audio does the same:
//!     drains its ring and starts from the new PTS.
//!   * No "advance per sample", no accumulating µs: every
//!     `set_pts()` is a direct `pts + wall_elapsed` assignment.

use anyhow::Result;
use crossterm::{
    cursor,
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{stdout, Stdout, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::audio;
use crate::clock::Clock;
use crate::decoder;
use crate::input::{self, Cmd};
use crate::playback::{self, fmt_time, wall_now_f64, Pipeline};
use crate::renderer::{self, Renderer};
use crate::subs;
use crate::terminfo::{self, CellPx};
use crate::tracks::{self, TrackInfo};

/// Subtitle mode (CLI semantics):
///   * `Off`      — no `--sub`: subtitles are NOT shown.
///   * `Embedded` — `--sub` with no value: embedded text track from
///     the container (FFmpeg's "best"), if any.
///   * `File(p)`  — `--sub file`: external .srt/.ass file.
#[derive(Debug, Clone)]
pub enum SubMode {
    Off,
    Embedded,
    File(PathBuf),
}

pub struct Config {
    pub path: PathBuf,
    /// SEPARATE audio-only input (dual input: split DASH streams from
    /// yt-dlp). The audio pipeline —which already runs its own
    /// demuxer— opens this URL instead of `path`. None = audio lives
    /// inside `path`.
    pub audio_path: Option<PathBuf>,
    pub forced_backend: Option<String>,
    pub scale: f32,
    pub loop_video: bool,
    pub show_stats: bool,
    pub no_audio: bool,
    pub audio_backend: audio::BackendPref,
    pub hw_pref: crate::hwdec::HwPref,
    /// Subtitle mode (see `SubMode`).
    pub sub_mode: SubMode,
    /// Initial audio track: 1-based index among the audio tracks
    /// (--aid) / language (--alang).
    pub aid: Option<usize>,
    pub alang: Option<String>,
    /// Initial embedded subtitle track (--sid / --slang).
    pub sid: Option<usize>,
    pub slang: Option<String>,
}

/// One option in the subtitle cycle (`j`/`J` key):
/// Off → [external if any] → embedded 1 → embedded 2 → … → Off.
enum SubChoice {
    Off,
    External(PathBuf),
    /// ACTUAL stream_index in the container.
    Embedded(usize),
}

/// Loads the chosen subtitle option. Returns (track, label for the
/// OSD).
fn load_sub_choice(
    media: &std::path::Path,
    choice: &SubChoice,
    sub_tracks: &[TrackInfo],
) -> (Option<subs::SubTrack>, String) {
    match choice {
        SubChoice::Off => (None, "off".to_string()),
        SubChoice::External(p) => {
            let t = subs::load_external_file(p);
            let label = match &t {
                Some(t) => format!("{} (external)", t.label),
                None => "couldn't load file".to_string(),
            };
            (t, label)
        }
        SubChoice::Embedded(sidx) => {
            let t = subs::load_embedded_track(media, *sidx);
            let label = sub_tracks
                .iter()
                .find(|ti| ti.stream_index == *sidx)
                .map(|ti| ti.label())
                .unwrap_or_else(|| "embedded".to_string());
            match &t {
                Some(_) => (t, label),
                None => (None, format!("{label} — error")),
            }
        }
    }
}

struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn enter(so: &mut Stdout) -> Result<Self> {
        terminal::enable_raw_mode()?;
        // Mouse capture: for the clickable progress bar. If the
        // terminal doesn't support it, crossterm emits the sequences
        // anyway and the terminal ignores them — harmless.
        execute!(so, EnterAlternateScreen, cursor::Hide, EnableMouseCapture)?;
        // AUTOWRAP OFF (DECAWM): writing into the last column of the
        // last row with wrap enabled scrolls the WHOLE screen → the
        // video shifts up one line, the next frame repaints it…
        // massive flicker and "garbage text" on small terminals.
        // With wrap off, any overflow is clipped at the edge.
        let _ = write!(so, "\x1b[?7l");
        let _ = so.flush();
        Ok(Self { active: true })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut so = stdout();
        // Restore autowrap and leave the alt-screen.
        let _ = write!(so, "\x1b[?7h");
        let _ = execute!(so, DisableMouseCapture, cursor::Show, LeaveAlternateScreen);
        let _ = write!(so, "\x1b[0m");
        let _ = so.flush();
        let _ = terminal::disable_raw_mode();
    }
}

/// Cache of the last HUD written: (cols, rows, hud_lines, l1, l2).
/// If the content hasn't changed, the row is NOT rewritten → the HUD
/// goes from 25-60 rewrites/s down to ~1/s — flicker gone.
type HudCache = Option<(u16, u16, u16, String, String)>;

/// HUD progress-bar width by column count. Single source of truth:
/// used by `format_hud_lines` (drawing) and `bar_hitbox` (mouse
/// clicks) — if they diverge, the click lands in the wrong place.
fn hud_bar_w(cols: u16) -> usize {
    if cols >= 120 {
        40
    } else if cols >= 80 {
        24
    } else if cols >= 60 {
        16
    } else {
        8
    }
}

/// On-screen hitbox of the progress bar: `(row, start_col, width)`
/// in 1-based coordinates, or None if the current HUD draws no bar
/// (hidden HUD, or the short <60-col line that omits it).
///
/// The HUD line with a bar always starts " ▶ [" / " ⏸ [": space (1)
/// + flag (1) + space (1) + '[' (1) → the bar spans columns
/// 5..5+bar_w-1. With a 2-line HUD the bar sits on the SECOND-TO-LAST
/// row; with 1 line, on the last.
fn bar_hitbox(cols: u16, rows: u16, hud_lines: u16) -> Option<(u16, u16, u16)> {
    let bar_w = hud_bar_w(cols) as u16;
    match hud_lines {
        2 => Some((rows.saturating_sub(1).max(1), 5, bar_w)),
        1 if cols >= 60 => Some((rows, 5, bar_w)),
        _ => None,
    }
}

fn hud_rows_for(cols: u16, rows: u16) -> u16 {
    // Tiny terminal: there's NO room for a readable HUD — painting it
    // truncated to 4-15 columns just produces noise flickering over
    // the video. Better to hide it and give every row to the video.
    if rows < 5 || cols < 16 {
        0
    } else if rows >= 24 && cols >= 100 {
        2
    } else {
        1
    }
}

pub fn run(cfg: Config) -> Result<()> {
    let backend = renderer::detect_backend(cfg.forced_backend.as_deref());
    let (mut cols, mut rows) = terminal::size().unwrap_or((80, 24));

    let mut so = stdout();
    let _guard = TerminalGuard::enter(&mut so)?;

    // IMMEDIATE loading feedback: between this point and the first
    // frame there are several blocking opens/probes (track inventory,
    // audio open, video decoder open+probe). With local files that's
    // milliseconds, but with network URLs (yt-dlp) it can be several
    // seconds — and the alternate screen used to sit BLACK and
    // silent, as if rtv had hung. A centred message is enough; the
    // first frame covers it (the renderer clears the screen on the
    // first draw, and the startup hold paints as soon as a frame
    // exists).
    {
        let msg = "⏳ loading…";
        let col = (cols / 2).saturating_sub(msg.chars().count() as u16 / 2);
        let row = rows / 2;
        let _ = write!(so, "\x1b[{};{}H{}", row.max(1), col.max(1), msg);
        let _ = so.flush();
    }

    let cell_px = terminfo::probe_cell_px(cols, rows);

    // --- Container track inventory (audio + text subs) ---
    // With dual input (--audio) the AUDIO tracks live in the audio
    // file/URL and the subtitle tracks in the video one; with a
    // single input everything comes from one probe
    // (playback::probe_tracks).
    let (audio_tracks, sub_tracks) = playback::probe_tracks(&cfg);

    // Softsub subtitles (external --sub or embedded). Embedded ones
    // load on their own thread (subs-only demux).
    //
    // Track CYCLE (`j`/`J` key): Off → [external] → embedded. State
    // is (sub_choices, sub_choice_idx); cycling reloads the chosen
    // track (events load in ms–s on their own thread, without
    // touching video/audio/clocks).
    let mut sub_choices: Vec<SubChoice> = vec![SubChoice::Off];
    if let SubMode::File(p) = &cfg.sub_mode {
        sub_choices.push(SubChoice::External(p.clone()));
    }
    for t in &sub_tracks {
        sub_choices.push(SubChoice::Embedded(t.stream_index));
    }
    let mut sub_choice_idx: usize = match &cfg.sub_mode {
        SubMode::Off => 0,
        SubMode::File(_) => 1,
        SubMode::Embedded => {
            if sub_tracks.is_empty() {
                0
            } else {
                // --sid/--slang pick a specific track; without them,
                // the container's first text track.
                let pos = tracks::select(&sub_tracks, cfg.sid, cfg.slang.as_deref()).unwrap_or(0);
                1 + pos // +1 for the leading Off (no external in this mode)
            }
        }
    };
    let mut sub_track: Option<subs::SubTrack> = if sub_choice_idx == 0 {
        None
    } else {
        load_sub_choice(&cfg.path, &sub_choices[sub_choice_idx], &sub_tracks).0
    };
    let sub_rows_for = |rows: u16, has: bool| -> u16 {
        if has && rows >= 8 {
            2
        } else {
            0
        }
    };
    let mut sub_rows = sub_rows_for(rows, sub_track.is_some());
    // Cache of the last painted subtitle text (anti-flicker, same
    // philosophy as HudCache): (cols, rows, first_row, text).
    let mut sub_cache: Option<(u16, u16, u16, String)> = None;

    // Pipeline SHARED with the GUI (playback::Pipeline): ffplay
    // clocks with staleness, audio with --aid/--alang, master clock,
    // video decoder and the audio startup gate.
    let mut hud_lines = hud_rows_for(cols, rows);
    let (dst_w0, dst_h0) = terminfo::adaptive_target_pixels(
        backend,
        cols,
        rows,
        cell_px,
        cfg.scale,
        hud_lines + sub_rows,
    );
    let Pipeline {
        dec,
        audio: mut audio_handle,
        master,
        vidclk,
        using_audio,
        is_live,
        fallback_frame_dur,
        max_frame_dur,
        mut gate,
    } = Pipeline::open(&cfg, &audio_tracks, dst_w0, dst_h0)?;

    // Position of the active audio track within `audio_tracks`
    // (for cycling with `a`/`#`).
    let mut cur_audio_pos: usize = audio_handle
        .as_ref()
        .and_then(|a| a.track_index)
        .and_then(|si| audio_tracks.iter().position(|t| t.stream_index == si))
        .unwrap_or(0);

    // Initial volume (Pipeline::open already set it to 100).
    let mut volume: i32 = 100;

    // Transient HUD OSD (feedback on track cycling and startup):
    // text + creation instant; shown ~2.5 s on HUD line 1 and expires
    // on its own (the HudCache detects the text change).
    //
    // Initial value = decode outcome (HW or software), ONLY with
    // --stats: it's diagnostic info, not everyday use (the HUD label
    // already carries the permanent "+cuda" for whoever looks).
    let mut osd: Option<(String, Instant)> = if cfg.show_stats {
        Some((
            match dec.hw_name() {
                Some(hw) => format!("decode: {hw} ⚡ (hardware)"),
                None => "decode: software (no acceleration — use --verbose to see why)"
                    .to_string(),
            },
            Instant::now(),
        ))
    } else {
        None
    };

    // HUD label: "kitty" (sw) or "kitty+vaapi" (HW decode).
    // Recomputed every frame because the mid-stream fallback can
    // switch hw→sw on the fly (atomic DecoderHandle::hw_state).
    let hud_backend_label = |dec: &decoder::DecoderHandle, base: &str| -> String {
        match dec.hw_name() {
            Some(hw) => format!("{base}+{hw}"),
            None => base.to_string(),
        }
    };

    let (mut dst_w, mut dst_h, _, _) = compute_layout(
        backend,
        dec.source_size,
        cols,
        rows,
        cell_px,
        cfg.scale,
        hud_lines + sub_rows,
    );
    dec.resize(dst_w, dst_h);

    let mut renderer_ = Renderer::new(backend);
    renderer_.set_cell_px(cell_px.w, cell_px.h);

    // Last shown frame — cached for INSTANT redraw when the terminal
    // resizes (no waiting for the decoder's next frame, which can
    // take a while if we're paused or holding). It's a move (not a
    // clone): zero cost per frame.
    let mut last_frame: Option<decoder::RgbFrame> = None;

    // Frame PENDING display (ffplay architecture): we pull it from
    // the channel as soon as it's available, but if it's not due yet
    // the wait happens in `input::wait_event` — interruptible by keys
    // and resizes — and we return to the top of the loop. A resize
    // thus gets serviced in <1 ms instead of waiting out a
    // `thread::sleep` of up to 500 ms (the "non-instant" resize).
    let mut pending: Option<decoder::RgbFrame> = None;

    // Cache of the last HUD written — if the text hasn't changed the
    // row isn't rewritten (90% of refreshes), killing HUD flicker on
    // slow/small terminals.
    let mut hud_cache: HudCache = None;

    // Stats.
    let mut frames_shown_win: u64 = 0;
    let mut frames_dec_win: u64 = 0;
    let mut frames_dropped_win: u64 = 0;
    let mut last_dec_pts_ms: i64 = -1;
    let mut stats_epoch = Instant::now();
    let mut fps_shown_now: f64 = 0.0;
    let mut fps_dec_now: f64 = 0.0;
    let mut dropped_last_win: u64 = 0;

    let mut force_full_redraw = true;

    // Refresh-loop state (ffplay-style):
    //   * last_shown_pts: PTS of the most recently rendered frame.
    //   * frame_timer: wall-clock instant the next frame is
    //     "scheduled" for. Computed frame by frame:
    //     `frame_timer += delay`.
    let mut last_shown_pts: f64 = 0.0;
    let mut frame_timer: f64 = wall_now_f64();
    // LIVE streams (Twitch, live TV): the container declares no
    // duration (AV_NOPTS_VALUE / <=0). Three things change:
    //   * the HUD shows time SINCE RECEPTION STARTED (Twitch PTS
    //     carry the accumulated broadcast hours — start_time=7470 s
    //     on a 2 h stream — and without subtracting the base the
    //     clock would start at 2:04:30),
    //   * seeking is bounded to [reception start, live edge] (the
    //     DVR window rtv has seen) instead of a duration that
    //     doesn't exist,
    //   * an abrupt PTS jump (Twitch's STITCHED ads: ad fragments
    //     spliced in with #EXT-X-DISCONTINUITY) is treated as a seek
    //     landing: re-anchor clocks and re-align audio, instead of
    //     dropping/freezing in bursts.
    //     (is_live comes from the Pipeline.)
    // PTS of the FIRST shown frame (base of the live HUD clock).
    let mut live_start_pts: Option<f64> = None;
    // Highest PTS seen (≈ live edge, cap for forward seeks).
    let mut max_live_pts: f64 = f64::NEG_INFINITY;
    // After seeking while paused, we want to decode and SHOW the
    // target frame (exactly once) without leaving pause.
    let mut show_one_frame_paused = false;
    // VIDEO SLAVED TO AUDIO during startup/post-seek too: while the
    // master clock (audio) is UNANCHORED (frozen waiting for the
    // first chunk of the new serial), we show ONE frame (the
    // target's) and STAY PUT. Without this, video free-ran against a
    // frozen clock (~0.5x–2x) and once audio anchored we had to
    // drop/duplicate in bursts to resync. We store the vidclk serial
    // for which the waiting frame was already shown.
    let mut held_frame_serial: Option<i32> = None;
    // Instant the hold started — safety valve: if audio doesn't
    // anchor within a reasonable time (e.g. seek past the end of the
    // audio stream), we force the anchor so video doesn't stay
    // frozen forever.
    let mut hold_started: Option<Instant> = None;
    // MPV-STYLE SEEK (keyframe landing): on seek we do NOT touch the
    // audio yet. The video decoder lands on the keyframe <= target
    // and emits THAT frame right away (instant jump). When the first
    // post-seek frame arrives, we re-point both clocks at its actual
    // PTS (retarget, no serial bump) and THEN ask the audio to jump
    // exactly to that PTS. Image and sound thus start pinned to the
    // same media instant, without silently decoding the whole GOP up
    // to the target (which took seconds with 4K AV1 and desynced
    // everything).
    let mut pending_audio_landing = false;
    // VIDEO landing after a seek: on live HLS the demuxer lands with
    // fragment granularity and can end up several seconds AWAY from
    // the requested target — last_shown_pts/frame_timer must be
    // renormalized to the first post-seek frame's actual PTS or the
    // sync absorbs the offset as delay (see 4.8).
    let mut pending_video_landing = false;
    // QUALITY REFINEMENT after the terminal GROWS: the pre-decode
    // queue holds up to ~2.5 s of frames scaled to the OLD (small)
    // dims. Shrinking doesn't matter (downscaling a big frame looks
    // fine), but growing upscales those frames with nearest → blurry
    // until the queue drains. The fix: with a 300 ms debounce after
    // the last grow, ask the decoder for a `refine_at(now)` — a
    // re-seek to the current point that drains the queue and
    // re-decodes FROM HERE with the new dims (exact drop-until-target,
    // no visual jump). Clocks and audio are NOT touched: the sound
    // keeps going and the sharp frames slot in where they belong.
    let mut refine_deadline: Option<Instant> = None;
    // Serial of the refinement IN PROGRESS: while the decoder
    // re-decodes the GOP to catch up with the master clock, its
    // frames arrive "late" and ffplay's standard drop would discard
    // them all — a frozen, blurry screen until the catch-up ends.
    // Instead we SHOW them (mpv-style with a slow hr-seek): the image
    // turns sharp as soon as the first refined frame decodes and
    // visibly "catches up" with the audio. Cleared by the first frame
    // that arrives on time (catch-up complete).
    let mut refine_catchup: Option<i32> = None;

    // Optional sync log (for integration tests):
    // RTV_SYNC_LOG=/path/file → one line per shown frame:
    //   wall_s master_s video_pts_s avdiff_ms dropped_win dec_w dec_h
    // (dec_w/dec_h = the decoder's NATIVE dims, not the player-side
    // rescale; used to measure quality recovery after growing the
    // terminal.)
    let mut sync_log: Option<std::io::BufWriter<std::fs::File>> =
        std::env::var("RTV_SYNC_LOG").ok().and_then(|p| {
            std::fs::File::create(p).ok().map(std::io::BufWriter::new)
        });

    // NOTE: we do NOT call `master.set(0.0)` here. Clocks are born at
    // pts=0, serial=0 and UNANCHORED (now() == 0, frozen), matching
    // the producers (audio serial 0, video decoder serial 0). The
    // first audio chunk / video frame anchors the clock and starts
    // time. Calling set(0.0) would bump the serials to 1, leaving the
    // producers (serial 0) invalidated forever.

    'main: loop {
        // 0) Audio gate valve: if no video frame has been shown
        //    within 10 s (broken video stream, decoder stuck on
        //    network…), open anyway so at least the audio plays
        //    instead of everything staying silent.
        gate.tick(&audio_handle);

        // 1) Input.
        let cmds = input::poll_command().unwrap_or_default();
        for cmd in cmds {
            match cmd {
                Cmd::Quit => break 'main,
                Cmd::TogglePause => {
                    if master.is_paused() {
                        master.resume();
                        if let Some(a) = audio_handle.as_ref() {
                            a.play_stream();
                        }
                        frame_timer = wall_now_f64();
                    } else {
                        master.pause();
                        if let Some(a) = audio_handle.as_ref() {
                            a.pause_stream();
                        }
                    }
                }
                Cmd::SeekRel(..) | Cmd::MouseClick(..) => {
                    let now = master.now();
                    // Clamp: keep 0.5 s of margin before the end so we
                    // don't land exactly on EOF (frozen screen).
                    // LIVE: the range is [first PTS received, live
                    // edge] — the window rtv has seen so far.
                    let (min_t, max_t) =
                        playback::seek_window(is_live, live_start_pts, max_live_pts, dec.duration);
                    // Live stream with no window yet (less than 1 s
                    // seen): nowhere to jump — ignore the seek instead
                    // of asking the demuxer for a nonsensical jump.
                    if is_live && max_t - min_t < 1.0 {
                        continue;
                    }
                    let target = match cmd {
                        Cmd::SeekRel(delta) => (now + delta).max(min_t).min(max_t),
                        Cmd::MouseClick(mc, mr) => {
                            // Only reacts if the click lands on the HUD
                            // progress BAR (with 1 cell of grace on each
                            // side: the '[' ']' brackets). The rest of
                            // the screen ignores the mouse.
                            let Some((brow, bcol, bw)) = bar_hitbox(cols, rows, hud_lines)
                            else {
                                continue;
                            };
                            if mr != brow || mc + 1 < bcol || mc > bcol + bw {
                                continue;
                            }
                            // Live: the bar maps the DVR window
                            // [min_t, max_t]; VOD requires a real duration.
                            if !is_live && !(dec.duration.is_finite() && dec.duration > 0.0) {
                                continue;
                            }
                            if is_live && max_t <= min_t {
                                continue;
                            }
                            // Proportional position within the bar:
                            // cell i of [0, bw) → fraction i/(bw-1)
                            // (the last cell lands on the very end).
                            let i = mc.saturating_sub(bcol).min(bw.saturating_sub(1));
                            let frac = if bw > 1 {
                                f64::from(i) / f64::from(bw - 1)
                            } else {
                                0.0
                            };
                            (min_t + frac * (max_t - min_t)).max(min_t).min(max_t)
                        }
                        _ => unreachable!(),
                    };
                    if let Some(log) = sync_log.as_mut() {
                        let _ = writeln!(
                            log,
                            "# SEEK wall={:.4} target={:.3} now={:.3} anchored={}",
                            wall_now_f64(),
                            target,
                            now,
                            master.master_anchored(),
                        );
                        let _ = log.flush();
                    }
                    // ATOMIC ORDER:
                    //   (1) master.set(target) → bumps the serial on audclk
                    //       AND vidclk; any in-flight chunk/frame carrying
                    //       the old serial gets discarded by callback/player.
                    //   (2) audio.seek(target) → the audio decoder jumps and
                    //       trims samples up to the exact target.
                    //   (3) dec.seek(target)   → the video decoder jumps via
                    //       keyframe<=target + drop-until-target-PTS.
                    master.set(target);
                    // Seek direction: FORWARD lands on the keyframe
                    // >= target (guarantees progress even when the GOP
                    // is longer than the seek step — YouTube AV1 has
                    // GOPs over 6 s and with keyframe<=target the video
                    // would get stuck in place); BACKWARD lands on the
                    // keyframe <= target, as usual. With the mouse the
                    // direction is relative to the current position.
                    dec.seek_dir(target, target > now);
                    // Discard the in-flight frame: its serial is stale now.
                    pending = None;
                    // A real seek drains the queue and re-decodes at the
                    // current dims: the refine pass is no longer needed.
                    refine_deadline = None;
                    refine_catchup = None;
                    // Audio will jump to the video's LANDING PTS
                    // (keyframe <= target) once the first post-seek
                    // frame arrives — see `pending_audio_landing`.
                    pending_audio_landing = using_audio;
                    pending_video_landing = true;
                    // Reset frame_timer so the next frame shows up
                    // immediately (no carry-over from the previous delay).
                    frame_timer = wall_now_f64();
                    last_shown_pts = target;
                    force_full_redraw = true;
                    if master.is_paused() {
                        // While paused: show the target frame once.
                        show_one_frame_paused = true;
                    }
                }
                Cmd::VolumeDelta(d) => {
                    volume = (volume + d).clamp(0, 200);
                    if let Some(a) = audio_handle.as_ref() {
                        a.set_volume(volume);
                    }
                }
                Cmd::CycleAudio(dir) => {
                    // HOT audio track switch, without interrupting
                    // playback. Same protocol as a seek to the current
                    // instant:
                    //   (1) master.set(now) — bumps the serials: any
                    //       chunks from the old track still in the ring
                    //       are silenced and don't touch the clock.
                    //   (2) audio.switch_track(stream, now) — the thread
                    //       reopens the decoder on the new stream
                    //       (its own codec/rate/layout), recreates the
                    //       resampler and lands on `now` with a
                    //       sample-accurate trim.
                    // The video is left alone: it enters the standard
                    // hold (master unanchored → shows the current frame
                    // and waits) and once the first chunk of the new
                    // track arrives the clock anchors and everything
                    // resumes in sync.
                    //
                    // Note: the OSD reports based on the CONTAINER's
                    // tracks, not on whether an output device exists.
                    // With no device (headless/CI, --no-audio implied
                    // by a cpal failure) `audio_handle` is None, but
                    // the user still deserves the "Audio [2/2]: spa" /
                    // "only track" feedback when cycling — previously
                    // any `a` press in headless said "no audio" even
                    // when the file had 2 tracks.
                    if cfg.no_audio || audio_tracks.is_empty() {
                        osd = Some(("Audio: no audio".to_string(), Instant::now()));
                    } else if audio_tracks.len() < 2 {
                        let label = audio_tracks
                            .first()
                            .map(|t| t.label())
                            .unwrap_or_else(|| "only".to_string());
                        osd = Some((format!("Audio: {label} (only track)"), Instant::now()));
                    } else {
                        let n = audio_tracks.len();
                        cur_audio_pos =
                            (cur_audio_pos as i64 + dir as i64).rem_euclid(n as i64) as usize;
                        let track = &audio_tracks[cur_audio_pos];
                        // The hot switch only applies when there is a
                        // live audio pipeline; with no device the
                        // selection is still recorded (cur_audio_pos)
                        // and the OSD confirms, without touching clocks.
                        if let Some(a) = audio_handle.as_ref() {
                            let now_t = master.now().max(0.0);
                            master.set(now_t);
                            a.switch_track(track.stream_index, now_t);
                            // This is NOT a video seek: the decoder keeps
                            // going and the audio landing anchors the
                            // clock at now_t.
                            pending_audio_landing = false;
                            frame_timer = wall_now_f64();
                            if master.is_paused() {
                                // While paused the clock is re-pointed; on
                                // resume the new track plays from here.
                                show_one_frame_paused = false;
                            }
                        }
                        osd = Some((
                            format!("Audio [{}/{}]: {}", cur_audio_pos + 1, n, track.label()),
                            Instant::now(),
                        ));
                    }
                }
                Cmd::CycleSubs(dir) => {
                    // Cycle: Off → [external --sub] → embedded → Off.
                    let n = sub_choices.len();
                    if n <= 1 {
                        osd = Some(("Subs: no tracks".to_string(), Instant::now()));
                    } else {
                        sub_choice_idx =
                            (sub_choice_idx as i64 + dir as i64).rem_euclid(n as i64) as usize;
                        let (t, label) =
                            load_sub_choice(&cfg.path, &sub_choices[sub_choice_idx], &sub_tracks);
                        sub_track = t;
                        sub_cache = None;
                        osd = Some((
                            format!("Subs [{}/{}]: {}", sub_choice_idx + 1, n, label),
                            Instant::now(),
                        ));
                        // Does the layout change? (the 2 reserved rows
                        // appear/disappear) → recompute dims and redraw
                        // the last frame right away (like a resize).
                        let new_sub_rows = sub_rows_for(rows, sub_track.is_some());
                        if new_sub_rows != sub_rows {
                            sub_rows = new_sub_rows;
                            let (nw, nh, _, _) = compute_layout(
                                backend,
                                dec.source_size,
                                cols,
                                rows,
                                cell_px,
                                cfg.scale,
                                hud_lines + sub_rows,
                            );
                            dst_w = nw;
                            dst_h = nh;
                            dec.resize(dst_w, dst_h);
                            hud_cache = None;
                            renderer_.reset_layout_cache();
                            force_full_redraw = true;
                            if let Some(f) = last_frame.as_mut() {
                                rescale_frame_nearest(f, dst_w, dst_h);
                                let vid_rows =
                                    rows.saturating_sub(hud_lines + sub_rows).max(1);
                                let (ox, oy) = offsets_for_frame(
                                    backend, cell_px, f.width, f.height, cols, vid_rows,
                                );
                                let mut sol = so.lock();
                                // Sin 2J manual: reset_layout_cache ya
                                // fuerza el clear DENTRO del batch
                                // sincronizado (?2026) del renderer;
                                // el clear manual fuera del batch era
                                // el parpadeo visible al pulsar `j`.
                                let _ = renderer_.draw(&mut sol, f, cols, vid_rows, ox, oy);
                                let _ = sol.flush();
                                force_full_redraw = false;
                            }
                        }
                    }
                }
                Cmd::Resize(c, r) => {
                    // Robust, INSTANT resize: don't touch clocks, sync,
                    // or the frame queue. Just (1) recompute the layout,
                    // (2) atomically store the new dims for the decoder
                    // and (3) RESCALE the last cached frame to the new
                    // dims (nearest, player-side) and paint it right
                    // away — also when the terminal GROWS (previously
                    // only shrinking was handled; on growth the picture
                    // stayed small until the decoder caught up with the
                    // new dims, up to ~2.5 s with a full queue).
                    cols = c.max(4);
                    rows = r.max(3);
                    hud_lines = hud_rows_for(cols, rows);
                    sub_rows = sub_rows_for(rows, sub_track.is_some());
                    let (nw, nh, _, _) = compute_layout(
                        backend,
                        dec.source_size,
                        cols,
                        rows,
                        cell_px,
                        cfg.scale,
                        hud_lines + sub_rows,
                    );
                    // Did the video area grow? → schedule a refine pass
                    // (300 ms debounce: during a drag we only refine on
                    // release). Shrinking cancels it: downscaling the
                    // large frames already queued looks perfect as-is.
                    let grew =
                        (u64::from(nw) * u64::from(nh)) > (u64::from(dst_w) * u64::from(dst_h));
                    refine_deadline = if grew {
                        Some(Instant::now() + Duration::from_millis(300))
                    } else {
                        None
                    };
                    dst_w = nw;
                    dst_h = nh;
                    dec.resize(dst_w, dst_h);
                    hud_cache = None; sub_cache = None;
                    renderer_.reset_layout_cache();
                    force_full_redraw = true;
                    if let Some(f) = last_frame.as_mut() {
                        rescale_frame_nearest(f, dst_w, dst_h);
                        let vid_rows = rows.saturating_sub(hud_lines + sub_rows).max(1);
                        let (ox, oy) =
                            offsets_for_frame(backend, cell_px, f.width, f.height, cols, vid_rows);
                        let mut sol = so.lock();
                        // The renderer emits a SINGLE 2J (layout changed);
                        // the previous manual double clear doubled the
                        // "flash" per resize event.
                        let _ = renderer_.draw(&mut sol, f, cols, vid_rows, ox, oy);
                        let _ = sol.flush();
                        force_full_redraw = false;
                    }
                }
                Cmd::None => {}
            }
        }

        // 1.2) Track-switch OSD expiry (~2.5 s): once osd goes back
        //      to None the HUD text changes and HudCache forces a
        //      repaint — no extra timers needed.
        if osd
            .as_ref()
            .map(|(_, t0)| t0.elapsed() > Duration::from_millis(2500))
            .unwrap_or(false)
        {
            osd = None;
        }

        // 1.5) Quality REFINE trigger (debounce expired). While
        //      playing: re-decode from slightly ahead of the master
        //      clock, so the first refined frame doesn't arrive
        //      already late. While paused: re-decode the on-screen
        //      frame and show it via show_one_frame_paused (without
        //      touching audio or clocks — pending_audio_landing stays
        //      false, so the landing doesn't re-point anything).
        if refine_deadline.map(|t| Instant::now() >= t).unwrap_or(false) {
            refine_deadline = None;
            let max_t = (dec.duration - 0.5).max(0.0);
            if master.is_paused() {
                dec.refine_at(last_shown_pts.min(max_t));
                show_one_frame_paused = true;
                refine_catchup = None;
            } else if using_audio && !master.master_anchored() {
                // Mid post-seek hold: retry in 200 ms (the seek in
                // progress already decodes at the new dims, so this
                // usually won't even be needed).
                refine_deadline = Some(Instant::now() + Duration::from_millis(200));
            } else {
                // SMALL lead (50 ms): the first refined frame with
                // pts >= target shows as soon as the master reaches
                // it. A large lead imposes THAT much extra freeze even
                // when decode is instant; a small one doesn't penalize
                // the slow case (late frames get dropped either way
                // and the re-sync point is the same: when decode
                // catches up to the master clock).
                let target = (master.now() + 0.05).max(0.0).min(max_t);
                refine_catchup = Some(dec.refine_at(target));
                pending = None; // stale serial
            }
        }

        // 2) Paused: sleep a bit and update the HUD. If a seek is
        //    pending display, pull ONE frame from the decoder (the
        //    target's) and paint it without leaving the pause.
        if master.is_paused() {
            if show_one_frame_paused {
                if let Ok(mut frame) = dec.rx.recv_timeout(Duration::from_millis(200)) {
                    if frame.serial == dec.current_serial() {
                        // Decoder-NATIVE dims (before the player-side
                        // rescale) — this is what the sync-log records
                        // so quality recovery can be measured.
                        let (dec_w, dec_h) = (frame.width, frame.height);
                        if frame.width != dst_w || frame.height != dst_h {
                            rescale_frame_nearest(&mut frame, dst_w, dst_h);
                        }
                        // Paused seek landing: re-point the clocks to
                        // the real PTS and align the audio so that on
                        // resume it plays EXACTLY from here.
                        if pending_audio_landing {
                            master.retarget(frame.pts);
                            if let Some(a) = audio_handle.as_ref() {
                                a.seek(frame.pts);
                            }
                            pending_audio_landing = false;
                        }
                        let vid_rows = rows.saturating_sub(hud_lines + sub_rows).max(1);
                        let (ox, oy) = offsets_for_frame(
                            backend, cell_px, frame.width, frame.height, cols, vid_rows,
                        );
                        let mut sol = so.lock();
                        if force_full_redraw {
                            // Clear inside the renderer's ?2026 batch
                            // (via reset_layout_cache) — no flash.
                            force_full_redraw = false;
                            hud_cache = None; sub_cache = None;
                            renderer_.reset_layout_cache();
                        }
                        if matches!(renderer_.draw(&mut sol, &frame, cols, vid_rows, ox, oy), Ok(true)) {
                            hud_cache = None; sub_cache = None;
                        }
                        drop(sol);
                        last_shown_pts = frame.pts;
                        show_one_frame_paused = false;
                        // Log this frame in the sync-log too: it's the
                        // "first post-seek frame" even though we're
                        // paused (the integration test measures it).
                        if let Some(log) = sync_log.as_mut() {
                            let m = master.now();
                            let _ = writeln!(
                                log,
                                "{:.4} {:.4} {:.4} {:+.1} {} {} {}",
                                wall_now_f64(),
                                m,
                                frame.pts,
                                (frame.pts - m) * 1000.0,
                                frames_dropped_win,
                                dec_w,
                                dec_h,
                            );
                            let _ = log.flush();
                        }
                        last_frame = Some(frame);
                    }
                }
            } else {
                // Event-INTERRUPTIBLE wait: a key press or a resize
                // wakes us up instantly (previously: a fixed 20 ms
                // sleep that delayed responsiveness while paused).
                input::wait_event(Duration::from_millis(50));
            }
            let vb = last_frame.as_ref().map(|f| {
                let vid_rows = rows.saturating_sub(hud_lines + sub_rows).max(1);
                video_bottom_row(backend, cell_px, f.width, f.height, cols, vid_rows)
            });
            draw_subs_dispatch(
                &mut so,
                cols,
                rows,
                hud_lines,
                sub_rows,
                vb,
                sub_track.as_ref(),
                last_shown_pts,
                &mut sub_cache,
            );
            let live_hud = if is_live {
                live_start_pts.map(|b| (b, max_live_pts.max(b)))
            } else {
                None
            };
            draw_hud_dispatch(
                &mut so,
                cols,
                rows,
                hud_lines,
                &*master,
                dec.duration,
                live_hud,
                volume,
                &hud_backend_label(&dec, backend.name()),
                cell_px,
                dst_w,
                dst_h,
                fps_shown_now,
                fps_dec_now,
                dropped_last_win,
                using_audio,
                dec.hw_name(),
                cfg.show_stats,
                true,
                osd.as_ref().map(|(s, _)| s.as_str()),
                &mut hud_cache,
            );
            continue;
        }

        // 2.5) Post-seek/startup HOLD with audio: if we already showed
        //      the target frame and the master clock is still frozen
        //      (no real audio yet), wait without consuming more frames.
        if using_audio
            && !master.master_anchored()
            && held_frame_serial == Some(dec.current_serial())
        {
            // Relief valve: if audio doesn't anchor within 1.5 s (seek
            // past the end of the audio, dead device…), start the
            // clock anyway so the video keeps going.
            if hold_started.map(|t| t.elapsed() > Duration::from_millis(1500)).unwrap_or(false) {
                master.master().force_anchor();
            } else {
                // Interruptible: a resize during the hold is handled immediately.
                input::wait_event(Duration::from_millis(4));
                continue;
            }
        }

        // 3) Get the next frame: the PENDING one from the previous
        //    iteration (not yet due for display) or a new one from
        //    the channel (short timeout so input and HUD keep being
        //    processed if the decoder is slow).
        let mut frame = match pending.take() {
            Some(f) => f,
            None => match dec.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(f) => f,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if dec.eof.load(Ordering::Relaxed) {
                        if cfg.loop_video {
                            master.set(0.0);
                            dec.seek(0.0);
                            pending_audio_landing = using_audio;
                            pending_video_landing = true;
                            frame_timer = wall_now_f64();
                            last_shown_pts = 0.0;
                            force_full_redraw = true;
                            continue;
                        }
                        break 'main;
                    }
                    continue;
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break 'main,
            },
        };

        // 4) Discard frames with a stale serial (leftovers from a
        //    recent seek that no longer apply).
        let cur_serial = dec.current_serial();
        if frame.serial != cur_serial {
            continue;
        }

        // 4.1) Frame with OLD dims (resize in flight): nearest
        //      player-side rescale to the new dims. The queue can hold
        //      up to ~2.5 s of frames pre-decoded at the previous
        //      dims; they used to show cropped (shrinking) or tiny
        //      (growing) until the decoder reached the new dims — the
        //      resize "lagged". Now ALL frames display at the new size
        //      right away. (dec_w/dec_h keep the NATIVE dims for the
        //      sync-log.)
        let (dec_w, dec_h) = (frame.width, frame.height);
        if frame.width != dst_w || frame.height != dst_h {
            rescale_frame_nearest(&mut frame, dst_w, dst_h);
        }

        let cur_pts_ms = (frame.pts * 1000.0) as i64;
        if cur_pts_ms != last_dec_pts_ms {
            frames_dec_win += 1;
            last_dec_pts_ms = cur_pts_ms;
        }

        // 4.5) Master clock UNANCHORED (audio not started yet after a
        //      seek/startup): show this frame NOW (it's the target's
        //      frame) and enter the hold until audio anchors the
        //      clock. This way the video "snaps" to the requested
        //      point and starts EXACTLY when the audio plays.
        if using_audio && !master.master_anchored() {
            // First post-seek frame: real landing PTS (keyframe
            // <= target). Re-point the clocks and jump the audio THERE.
            if pending_audio_landing {
                master.retarget(frame.pts);
                if let Some(a) = audio_handle.as_ref() {
                    a.seek(frame.pts);
                }
                pending_audio_landing = false;
            }
            {
                let vid_rows = rows.saturating_sub(hud_lines + sub_rows).max(1);
                let (ox, oy) =
                    offsets_for_frame(backend, cell_px, frame.width, frame.height, cols, vid_rows);
                let mut sol = so.lock();
                if force_full_redraw {
                    // No manual 2J outside the batch: reset_layout_cache
                    // makes the renderer emit the clear INSIDE the
                    // synchronized batch (?2026) → no black flash.
                    force_full_redraw = false;
                    hud_cache = None; sub_cache = None;
                    renderer_.reset_layout_cache();
                }
                if matches!(renderer_.draw(&mut sol, &frame, cols, vid_rows, ox, oy), Ok(true)) {
                    hud_cache = None; sub_cache = None;
                }
            }
            vidclk.set_pts(frame.pts, vidclk.current_serial());
            last_shown_pts = frame.pts;
            frame_timer = wall_now_f64();
            pending_video_landing = false;
            held_frame_serial = Some(frame.serial);
            hold_started = Some(Instant::now());
            // Live: the first frame (which arrives through this path
            // with audio) sets the HUD clock base and the DVR edge.
            if is_live {
                if live_start_pts.is_none() {
                    live_start_pts = Some(frame.pts);
                }
                max_live_pts = max_live_pts.max(frame.pts);
            }
            // FIRST frame on screen → open the audio gate: sound
            // starts now, with the picture already visible.
            gate.open(&audio_handle);
            if let Some(log) = sync_log.as_mut() {
                let m = master.now();
                let _ = writeln!(
                    log,
                    "{:.4} {:.4} {:.4} {:+.1} {} {} {}",
                    wall_now_f64(),
                    m,
                    frame.pts,
                    (frame.pts - m) * 1000.0,
                    frames_dropped_win,
                    dec_w,
                    dec_h,
                );
                let _ = log.flush();
            }
            let vb = {
                let vid_rows = rows.saturating_sub(hud_lines + sub_rows).max(1);
                video_bottom_row(backend, cell_px, frame.width, frame.height, cols, vid_rows)
            };
            draw_subs_dispatch(
                &mut so,
                cols,
                rows,
                hud_lines,
                sub_rows,
                Some(vb),
                sub_track.as_ref(),
                frame.pts,
                &mut sub_cache,
            );
            let live_hud = if is_live {
                live_start_pts.map(|b| (b, max_live_pts.max(b)))
            } else {
                None
            };
            draw_hud_dispatch(
                &mut so,
                cols,
                rows,
                hud_lines,
                &*master,
                dec.duration,
                live_hud,
                volume,
                &hud_backend_label(&dec, backend.name()),
                cell_px,
                dst_w,
                dst_h,
                fps_shown_now,
                fps_dec_now,
                dropped_last_win,
                using_audio,
                dec.hw_name(),
                cfg.show_stats,
                false,
                osd.as_ref().map(|(s, _)| s.as_str()),
                &mut hud_cache,
            );
            last_frame = Some(frame);
            continue;
        }
        // Clock anchored: if we came out of a hold, resync frame_timer
        // to the wall clock so the wait time isn't carried over as
        // "debt", and RE-ANCHOR vidclk to the shown frame: vidclk was
        // set on ENTERING the hold and kept extrapolating into the
        // void for its whole duration (it has no staleness) → without
        // this re-set, `diff = vidclk.now() - master.now()` came out
        // as +[hold duration], the "exact wait" slept 0.5 s (the cap)
        // and the video started late after every audio anchor.
        if held_frame_serial.take().is_some() {
            frame_timer = wall_now_f64();
            hold_started = None;
            vidclk.set_pts(last_shown_pts, vidclk.current_serial());
        }

        // 4.7) LIVE — received-window bookkeeping and PTS
        //      DISCONTINUITY (Twitch stitched ads, encoder cuts): if
        //      the PTS jumps more than 10 s from the last shown frame
        //      WITHOUT a seek, the clocks would fall behind and the
        //      loop would drop/freeze in bursts forever. Treat the
        //      jump as a seek landing: re-anchor the clocks to the
        //      new PTS and re-align the audio there. (10 s > any real
        //      frame jitter; ad jumps are minutes or hours.)
        if is_live {
            if live_start_pts.is_none() {
                live_start_pts = Some(frame.pts);
            }
            if frame.pts > max_live_pts {
                max_live_pts = frame.pts;
            }
            let jump = frame.pts - last_shown_pts;
            if jump.is_finite()
                && jump.abs() > 10.0
                && !pending_audio_landing
                && !pending_video_landing
            {
                // Time ALREADY elapsed on the HUD (before overwriting
                // last_shown_pts) — so the new base preserves it and
                // the displayed clock stays continuous across the jump.
                let elapsed =
                    (last_shown_pts - live_start_pts.unwrap_or(frame.pts)).max(0.0);
                master.retarget(frame.pts);
                if let Some(a) = audio_handle.as_ref() {
                    a.seek(frame.pts);
                }
                vidclk.set_pts(frame.pts, vidclk.current_serial());
                last_shown_pts = frame.pts;
                frame_timer = wall_now_f64();
                live_start_pts = Some(frame.pts - elapsed);
                max_live_pts = max_live_pts.max(frame.pts);
            }
        }

        // 4.8) VIDEO LANDING after a seek: the decoder can land far
        //      from the target (live HLS seeks with fragment
        //      granularity — measured: +6.3 s on Twitch).
        //      last_shown_pts still holds the target, so natural_delay
        //      would absorb the offset as "real delay" and frame_timer
        //      would jump seconds into the future → 1 frame every
        //      500 ms (the sleep cap) until the debt drains.
        //      Renormalize to the real landing PTS and show NOW. (With
        //      audio the 4.5 hold already covers this; this covers
        //      --no-audio / no device, where the master is the video
        //      clock itself.)
        if pending_video_landing {
            pending_video_landing = false;
            if !using_audio {
                master.retarget(frame.pts);
            }
            last_shown_pts = frame.pts;
            frame_timer = wall_now_f64();
        }

        // 5) ffplay-style SYNC — arithmetic SHARED with the GUI
        //    (playback::plan_frame: vp_duration + compute_target_delay
        //    + 100 ms resync + Show/Late/Wait classification).
        match playback::plan_frame(
            frame.pts,
            last_shown_pts,
            &mut frame_timer,
            fallback_frame_dur,
            max_frame_dur,
            &vidclk,
            &master,
        ) {
            playback::FramePlan::Late => {
                // EXCEPTION — post-resize refine catch-up: while the
                // decoder re-decodes the GOP to catch up with the
                // clock, ALL of its frames arrive "late"; dropping
                // them = frozen, blurry screen until the catch-up
                // ends. Show them without sleeping (quality recovers
                // on the first sharp frame and the video visibly
                // "catches up" to the audio, mpv-style). The catch-up
                // ends with the first on-time frame.
                if refine_catchup == Some(frame.serial) {
                    frame_timer = wall_now_f64(); // show NOW, no debt
                } else {
                    // Standard drop. plan_frame already reverted the
                    // frame_timer advance: a dropped frame does NOT
                    // consume a presentation slot (without this, the
                    // post-seek catch-up — land on a keyframe and drop
                    // until the target, e.g. 134 frames with 10 s GOPs
                    // — pushed frame_timer ~5 s into the future and
                    // the player fell to 1 frame every 500 ms).
                    frames_dropped_win += 1;
                    last_shown_pts = frame.pts;
                    continue;
                }
            }
            playback::FramePlan::Wait { sleep_s, target_delay } => {
                // Not due yet: INTERRUPTIBLE wait. If an event
                // arrives (resize, key) put the frame back in
                // `pending`, rewind frame_timer (it gets re-added on
                // reprocessing) and go back to the top of the loop to
                // handle the event NOW. If the wait expires with no
                // events, the frame is shown.
                refine_catchup = None; // frame on time
                if input::wait_event(Duration::from_secs_f64(sleep_s)) {
                    frame_timer -= target_delay;
                    pending = Some(frame);
                    continue;
                }
            }
            playback::FramePlan::Show => {
                refine_catchup = None; // frame on time: catch-up complete
            }
        }

        // 6) Draw the frame + HUD. The layout (centering offsets) is
        //    recomputed PER FRAME from the frame's REAL dims: during a
        //    resize, frames with old and new dims coexist, and each
        //    one gets centered/cropped correctly without losing the
        //    pre-decode cushion or touching the sync.
        {
            let vid_rows = rows.saturating_sub(hud_lines + sub_rows).max(1);
            let (ox, oy) =
                offsets_for_frame(backend, cell_px, frame.width, frame.height, cols, vid_rows);
            let mut sol = so.lock();
            if force_full_redraw {
                // Clear inside the renderer's ?2026 batch
                // (via reset_layout_cache) — no flash.
                force_full_redraw = false;
                hud_cache = None; sub_cache = None;
                renderer_.reset_layout_cache();
            }
            if matches!(renderer_.draw(&mut sol, &frame, cols, vid_rows, ox, oy), Ok(true)) {
                hud_cache = None; sub_cache = None;
            }
        }

        // 7) Update vidclk to the PTS of the frame we JUST showed.
        //    With no audio, this is the master clock. With audio it
        //    serves the HUD and any future sync-to-slave.
        //    We use vidclk's OWN serial as the token: stale-frame
        //    filtering already happened above against the decoder's
        //    serial (frame.serial != dec.current_serial() → skip), and
        //    the clock's and decoder's counters are independent.
        vidclk.set_pts(frame.pts, vidclk.current_serial());
        last_shown_pts = frame.pts;
        // First frame via the anchored path (if the hold never
        // kicked in): make sure the gate is open here too.
        gate.open(&audio_handle);

        if let Some(log) = sync_log.as_mut() {
            let m = master.now();
            let _ = writeln!(
                log,
                "{:.4} {:.4} {:.4} {:+.1} {} {} {}",
                wall_now_f64(),
                m,
                frame.pts,
                (frame.pts - m) * 1000.0,
                frames_dropped_win,
                dec_w,
                dec_h,
            );
            let _ = log.flush();
        }

        let vb = {
            let vid_rows = rows.saturating_sub(hud_lines + sub_rows).max(1);
            video_bottom_row(backend, cell_px, frame.width, frame.height, cols, vid_rows)
        };
        draw_subs_dispatch(
            &mut so,
            cols,
            rows,
            hud_lines,
            sub_rows,
            Some(vb),
            sub_track.as_ref(),
            frame.pts,
            &mut sub_cache,
        );
        let live_hud = if is_live {
            live_start_pts.map(|b| (b, max_live_pts.max(b)))
        } else {
            None
        };
        draw_hud_dispatch(
            &mut so,
            cols,
            rows,
            hud_lines,
            &*master,
            dec.duration,
            live_hud,
            volume,
            &hud_backend_label(&dec, backend.name()),
            cell_px,
            dst_w,
            dst_h,
            fps_shown_now,
            fps_dec_now,
            dropped_last_win,
            using_audio,
            dec.hw_name(),
            cfg.show_stats,
            false,
            osd.as_ref().map(|(s, _)| s.as_str()),
            &mut hud_cache,
        );
        last_frame = Some(frame);
        frames_shown_win += 1;

        let el = stats_epoch.elapsed();
        if el >= Duration::from_secs(1) {
            let secs = el.as_secs_f64();
            fps_shown_now = frames_shown_win as f64 / secs;
            fps_dec_now = frames_dec_win as f64 / secs;
            dropped_last_win = frames_dropped_win;
            frames_shown_win = 0;
            frames_dec_win = 0;
            frames_dropped_win = 0;
            stats_epoch = Instant::now();
        }
    }

    // Cleanup.
    // Sync-log: EXPLICIT flush + fsync before exiting. The log is
    // flushed line by line, but on some filesystems (9p/WSL, sandbox
    // overlays, NFS) data from a freshly-dead process can take a few
    // ms to become visible to an external reader: the integration
    // test read the file right after wait() and saw 0 rows even
    // though the file was complete (flaky ~1/6). sync_all() forces
    // the data to stable storage BEFORE exit() is observable.
    if let Some(mut log) = sync_log.take() {
        let _ = log.flush();
        if let Ok(f) = log.into_inner() {
            let _ = f.sync_all();
        }
    }
    if let Some(mut a) = audio_handle.take() {
        a.stop();
    }
    let _ = dec;
    Ok(())
}

// (wall_now_f64 and fmt_time live in crate::playback — shared with
// the GUI.)

// -------------------- layout / HUD helpers --------------------

fn compute_layout(
    backend: renderer::Backend,
    source_size: (u32, u32),
    cols: u16,
    rows: u16,
    cell_px: CellPx,
    scale: f32,
    hud_rows: u16,
) -> (u32, u32, u16, u16) {
    let (avail_w, avail_h) =
        terminfo::adaptive_target_pixels(backend, cols, rows, cell_px, scale, hud_rows);
    let (align_w, align_h) = px_per_cell(backend, cell_px);
    let ((w, h), (ox, oy)) = renderer::fit_aspect(source_size, (avail_w, avail_h), align_w, align_h);
    let (col_ox, row_oy) = px_to_cells(backend, cell_px, ox, oy);
    (w.max(2), h.max(2), col_ox, row_oy)
}

/// Cells occupied by a frame of `fw`×`fh` pixels on this backend, and
/// centering offsets within `cols`×`vid_rows` (the HUD-free area).
/// Computed PER FRAME from the frame's real dims — key for frames
/// with "old" dims during a resize to stay properly centered and
/// cropped.
fn offsets_for_frame(
    backend: renderer::Backend,
    cell: CellPx,
    fw: u32,
    fh: u32,
    cols: u16,
    vid_rows: u16,
) -> (u16, u16) {
    let (pcx, pcy) = px_per_cell(backend, cell);
    let cw = fw.div_ceil(pcx.max(1)).max(1);
    let ch = fh.div_ceil(pcy.max(1)).max(1);
    let ox = (u32::from(cols).saturating_sub(cw)) / 2;
    let oy = (u32::from(vid_rows).saturating_sub(ch)) / 2;
    (ox.min(u16::MAX as u32) as u16, oy.min(u16::MAX as u32) as u16)
}

fn px_per_cell(backend: renderer::Backend, cell: CellPx) -> (u32, u32) {
    match backend {
        renderer::Backend::HalfBlocks => (1, 2),
        renderer::Backend::Ascii => (1, 1),
        _ => (cell.w.max(1), cell.h.max(1)),
    }
}

fn px_to_cells(
    backend: renderer::Backend,
    cell: CellPx,
    px_x: u32,
    px_y: u32,
) -> (u16, u16) {
    let (pcx, pcy) = px_per_cell(backend, cell);
    ((px_x / pcx.max(1)) as u16, (px_y / pcy.max(1)) as u16)
}

/// 1-based row of the LAST cell row occupied by the video (vertical
/// centering offset + frame height in cells), within the `vid_rows`
/// video area. Used to anchor subtitles right below the picture
/// instead of at the bottom of the terminal.
fn video_bottom_row(
    backend: renderer::Backend,
    cell: CellPx,
    fw: u32,
    fh: u32,
    cols: u16,
    vid_rows: u16,
) -> u16 {
    let (_, pcy) = px_per_cell(backend, cell);
    let ch = fh.div_ceil(pcy.max(1)).max(1).min(u32::from(vid_rows)) as u16;
    let (_, oy) = offsets_for_frame(backend, cell, fw, fh, cols, vid_rows);
    (oy + ch).min(vid_rows)
}

/// Paints the subtitle rows. Content-cached: only rewrites when the
/// text (or its position) changes — events last seconds → ~zero cost
/// per refresh. The text is centered and clipped to the width; if it
/// has more lines than reserved rows, the LAST ones (the most recent
/// dialogue) are shown.
///
/// Placement: if the video is letterboxed (black bar at the bottom of
/// the video area), the subtitles stick RIGHT below the picture (one
/// row of margin) instead of sitting at the bottom of the terminal,
/// far from the video. Without letterbox they fall in their usual
/// reserved rows (above the HUD).
#[allow(clippy::too_many_arguments)]
fn draw_subs_dispatch(
    so: &mut Stdout,
    cols: u16,
    rows: u16,
    hud_lines: u16,
    sub_rows: u16,
    video_bottom: Option<u16>,
    track: Option<&subs::SubTrack>,
    t: f64,
    cache: &mut Option<(u16, u16, u16, String)>,
) {
    if sub_rows == 0 {
        return;
    }
    let Some(track) = track else { return };
    let text = track.query(t).unwrap_or_default();
    // Classic reserved row (right above the HUD) = lower bound.
    let reserved_first = rows.saturating_sub(hud_lines + sub_rows) + 1;
    let first_row = match video_bottom {
        // +2 = one blank separator row below the picture.
        Some(vb) => (vb + 2).min(reserved_first),
        None => reserved_first,
    };
    let key = (cols, rows, first_row, text);
    if cache.as_ref() == Some(&key) {
        return;
    }
    // If the position changed (e.g. a resize without an intervening
    // 2J), clear the rows at the previous position before painting.
    let prev_row = cache.as_ref().map(|(_, _, r, _)| *r);
    let lines: Vec<&str> = key.3.lines().collect();
    let start = lines.len().saturating_sub(sub_rows as usize);
    let mut sol = so.lock();
    if let Some(pr) = prev_row {
        if pr != first_row {
            for i in 0..sub_rows {
                let _ = renderer::draw_hud_at(&mut sol, cols, pr + i, "");
            }
        }
    }
    for i in 0..sub_rows {
        let content = lines.get(start + i as usize).copied().unwrap_or("");
        let centered = center_text(content, cols);
        let _ = renderer::draw_sub_line(&mut sol, cols, first_row + i, &centered);
    }
    let _ = sol.flush();
    *cache = Some(key);
}

/// Centers `s` within `cols` cells (by real unicode width).
fn center_text(s: &str, cols: u16) -> String {
    use unicode_width::UnicodeWidthStr;
    let w = s.width();
    let pad = (cols as usize).saturating_sub(w) / 2;
    let mut out = String::with_capacity(pad + s.len());
    for _ in 0..pad {
        out.push(' ');
    }
    out.push_str(s);
    out
}

#[allow(clippy::too_many_arguments)]
fn draw_hud_dispatch(
    so: &mut Stdout,
    cols: u16,
    rows: u16,
    hud_lines: u16,
    clock: &dyn Clock,
    duration: f64,
    live: Option<(f64, f64)>,
    volume: i32,
    backend_name: &str,
    cell: CellPx,
    frame_w: u32,
    frame_h: u32,
    fps_shown: f64,
    fps_decoded: f64,
    dropped: u64,
    using_audio: bool,
    hw_name: Option<&'static str>,
    show_stats: bool,
    paused: bool,
    osd: Option<&str>,
    cache: &mut HudCache,
) {
    // Tiny terminal: HUD hidden — there's nothing legible to paint
    // and rewriting it truncated every frame is the main source of
    // flicker with small windows.
    if hud_lines == 0 {
        // Pending flush for the just-drawn frame (this same dispatch
        // used to do it when writing the HUD).
        let mut sol = so.lock();
        let _ = sol.flush();
        return;
    }
    let (mut l1, l2) = format_hud_lines(
        clock,
        duration,
        live,
        volume,
        backend_name,
        cell,
        frame_w,
        frame_h,
        fps_shown,
        fps_decoded,
        dropped,
        using_audio,
        hw_name,
        show_stats,
        paused,
        cols,
        hud_lines,
    );
    // Transient OSD (track switch): replaces the HUD's main line
    // while active — it's part of the cache key, so it appears and
    // disappears with a single repaint.
    if let Some(o) = osd {
        l1 = format!(" ▸ {o}");
    }
    // Anti-flicker cache: if the HUD didn't change (same terminal
    // size and same text), the row is NOT rewritten. The HUD only
    // changes ~once/s (the clock), but it was being dispatched at
    // full fps (25-60/s): every rewrite is an erase+repaint visible
    // on slow terminals → the "HUD flicker".
    let key = (cols, rows, hud_lines, l1, l2);
    let dirty = cache.as_ref() != Some(&key);
    let mut sol = so.lock();
    if dirty {
        if hud_lines == 2 {
            let _ = renderer::draw_hud_two_lines(&mut sol, cols, rows, &key.3, &key.4);
        } else {
            let _ = renderer::draw_hud(&mut sol, cols, rows, &key.3);
        }
        *cache = Some(key);
    }
    let _ = sol.flush();
}

/// Rescales an RgbFrame to `dst_w`×`dst_h` with nearest-neighbor.
/// Used ONLY in resize transients (frames pre-decoded at old dims and
/// redraws of the cached frame): the next frame out of the decoder
/// already comes rescaled with sws FAST_BILINEAR.
/// Cost: O(w·h) with an index LUT — ~1 ms for 300×90 cells, nothing
/// against the 40 ms frame budget at 25 fps.
fn rescale_frame_nearest(f: &mut decoder::RgbFrame, dst_w: u32, dst_h: u32) {
    let (sw, sh) = (f.width as usize, f.height as usize);
    let (dw, dh) = (dst_w.max(2) as usize, dst_h.max(2) as usize);
    if sw == 0 || sh == 0 || (sw == dw && sh == dh) || f.data.len() < sw * sh * 3 {
        return;
    }
    // Destination-column → source-column mapping LUT (avoids the
    // per-pixel div/mul in the hot loop).
    let mut xmap = Vec::with_capacity(dw);
    for x in 0..dw {
        xmap.push((x * sw / dw).min(sw - 1) * 3);
    }
    let mut out = vec![0u8; dw * dh * 3];
    for y in 0..dh {
        let sy = (y * sh / dh).min(sh - 1);
        let srow = &f.data[sy * sw * 3..sy * sw * 3 + sw * 3];
        let drow = &mut out[y * dw * 3..y * dw * 3 + dw * 3];
        for (x, &sx) in xmap.iter().enumerate() {
            let d = x * 3;
            drow[d] = srow[sx];
            drow[d + 1] = srow[sx + 1];
            drow[d + 2] = srow[sx + 2];
        }
    }
    f.width = dst_w.max(2);
    f.height = dst_h.max(2);
    f.data = out;
}

#[allow(clippy::too_many_arguments)]
fn format_hud_lines(
    clock: &dyn Clock,
    duration: f64,
    live: Option<(f64, f64)>,
    volume: i32,
    backend_name: &str,
    cell: CellPx,
    frame_w: u32,
    frame_h: u32,
    fps_shown: f64,
    fps_decoded: f64,
    dropped: u64,
    using_audio: bool,
    hw_name: Option<&'static str>,
    show_stats: bool,
    paused: bool,
    cols: u16,
    hud_lines: u16,
) -> (String, String) {
    // LIVE (live = Some((base, edge))): time is measured from the
    // start of reception (clock.now() - base) and the bar represents
    // the position within the received DVR window [base, edge] — so
    // ←/→ and bar clicks have a real visual reference.
    // VOD: absolute time over the duration, as usual.
    let (t, frac) = if let Some((base, edge)) = live {
        let t = (clock.now() - base).max(0.0);
        let win = (edge - base).max(0.0);
        let frac = if win > 0.5 { (t / win).min(1.0) } else { 1.0 };
        (t, frac)
    } else {
        let t = clock.now().max(0.0).min(duration.max(0.0));
        let frac = if duration > 0.0 {
            (t / duration).min(1.0)
        } else {
            0.0
        };
        (t, frac)
    };
    // Time label: "mm:ss/mm:ss" for VOD; live streams have no total
    // duration, so show the elapsed time + "LIVE".
    let time_label = if live.is_some() {
        // Ad detection (hlsdvr): if what's being served to the
        // demuxer RIGHT NOW is an ad, flag it in the HUD — ALWAYS
        // visible (not just with --stats), so you know it appeared.
        if crate::hlsdvr::ad_playing() {
            format!("{} 🔴LIVE 📢AD", fmt_time(t))
        } else {
            format!("{} 🔴LIVE", fmt_time(t))
        }
    } else {
        format!("{}/{}", fmt_time(t), fmt_time(duration))
    };
    let flag = if paused { "⏸" } else { "▶" };

    let bar_w = hud_bar_w(cols);
    let filled = ((frac * bar_w as f64).round() as usize).min(bar_w);
    let bar = "█".repeat(filled) + &"░".repeat(bar_w - filled);
    let audio_tag = if using_audio { "🔊" } else { "🔇" };
    // HW decode indicator ALWAYS visible (not just with --stats):
    // "⚡cuda" on the main line. Reflects the mid-stream fallback
    // live (atomic hw_state → disappears if it drops to software).
    let hw_tag = hw_name.map(|h| format!(" ⚡{h}")).unwrap_or_default();

    // Metrics block: ONLY with --stats. The 2-line HUD used to show
    // them always (backend, resolution, cell, fps, drops) and the
    // flag "did nothing"; now the default HUD is clean (transport +
    // volume) and --stats adds the telemetry.
    let stats_block = || {
        let mut b = format!(
            " · {} {}×{} (cell {}×{} {}) · {:5.1} fps ({:.0} dec, {} drop)",
            backend_name,
            frame_w,
            frame_h,
            cell.w,
            cell.h,
            cell.source.short(),
            fps_shown,
            fps_decoded,
            dropped,
        );
        // Live LATENCY (--stats): how many seconds you are behind
        // the live edge according to the local DVR (fragments
        // received vs served to the demuxer) and how much DVR has
        // accumulated.
        if let Some((behind, total)) = crate::hlsdvr::stats() {
            b.push_str(&format!(" · lat {behind:.1}s (dvr {})", fmt_time(total)));
            // Accumulated advertising (if any): total ad seconds the
            // DVR has received since startup.
            let ad_s = crate::hlsdvr::ad_total_s();
            if ad_s > 0.0 {
                b.push_str(&format!(" · ads {}", fmt_time(ad_s)));
            }
        }
        b
    };

    if hud_lines == 2 {
        let mut line1 = format!(
            " {} [{}] {} · vol {} {}{}",
            flag,
            bar,
            time_label,
            volume,
            audio_tag,
            hw_tag,
        );
        if show_stats {
            line1.push_str(&stats_block());
        }
        let line2 =
            " q=quit · ␣=pause · ←/→=seek ±5s · click bar=go to · ↑/↓=vol ±5 · a=audio · j=subs"
                .to_string();
        (line1, line2)
    } else if cols >= 60 {
        let mut line = format!(
            " {} [{}] {} · vol {} {}{}",
            flag,
            bar,
            time_label,
            volume,
            audio_tag,
            hw_tag,
        );
        if show_stats {
            line.push_str(&stats_block());
            line.push_str(" · q=quit");
        } else {
            line.push_str(" · q=quit · ␣=pause · ←/→=seek");
        }
        (line, String::new())
    } else {
        let line = if show_stats {
            format!(
                " {} {} · {:.0} fps ({} drop) · q",
                flag,
                time_label,
                fps_shown,
                dropped,
            )
        } else {
            format!(" {} {} · q", flag, time_label)
        };
        (line, String::new())
    }
}
