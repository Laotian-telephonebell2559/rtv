//! playback.rs — shared pieces of the playback pipeline.
//!
//! player.rs (terminal) and gui.rs (window) play with the same
//! discipline: ffplay-style clocks (FfClock/MasterClock), audio
//! driving the video, post-seek hold, audio landing on the first
//! frame, serial-based discard and late-frame drops. This module holds
//! the common part so there is a single source of truth:
//!
//!   * `wall_now_f64` / `fmt_time` — wall clock and time formatting.
//!   * `probe_tracks` — track inventory (with dual-input support).
//!   * `Pipeline` — opens the full pipeline: clocks, audio, master
//!     clock, video decoder and the startup gate.
//!   * `seek_window` — valid seek range (VOD or DVR window).
//!   * `plan_frame` — the ffplay-style sync arithmetic deciding
//!     whether the candidate frame shows now, gets dropped as late, or
//!     needs a wait. It's the heart of the sync; it used to live
//!     duplicated in player.rs and gui.rs, and every change had to be
//!     applied twice.
//!
//! What deliberately does NOT live here: the terminal presentation
//! loop (interleaved with the cell renderer, subtitles, HUD,
//! post-resize refinement and sync-log) and the GUI's eframe layer.
//! Each frontend keeps its own loop; both call into these pieces.

use crate::audio::{self, AudioHandle};
use crate::clock::{
    compute_target_delay, vp_duration, Clock, FfClock, MasterClock, AV_SYNC_THRESHOLD_MAX,
};
use crate::decoder::{self, DecoderHandle};
use crate::player::Config;
use crate::tracks::{self, TrackInfo};
use anyhow::Result;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------
// Time
// ---------------------------------------------------------------

/// Monotonic wall time in seconds (based on `Instant::now` against a
/// per-process fixed origin). Equivalent to `av_gettime_relative()`.
pub fn wall_now_f64() -> f64 {
    use once_cell::sync::Lazy;
    static ORIGIN: Lazy<Instant> = Lazy::new(Instant::now);
    ORIGIN.elapsed().as_secs_f64()
}

/// h:mm:ss or m:ss; "--:--" when the time isn't finite (live stream
/// with no declared duration). Shared format for the terminal HUD and
/// the GUI.
pub fn fmt_time(t: f64) -> String {
    if !t.is_finite() || t < 0.0 {
        return "--:--".to_string();
    }
    let s = t as u64;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

// ---------------------------------------------------------------
// Container tracks
// ---------------------------------------------------------------

/// Track inventory (audio + text subtitles), with dual-input support
/// (`--audio`): audio tracks live in the audio file/URL and subtitle
/// tracks in the video one; with a single input everything comes from
/// the same probe.
pub fn probe_tracks(cfg: &Config) -> (Vec<TrackInfo>, Vec<TrackInfo>) {
    match &cfg.audio_path {
        None => tracks::probe(&cfg.path),
        Some(ap) => {
            let (at, _) = tracks::probe(ap);
            let (_, st) = tracks::probe(&cfg.path);
            (at, st)
        }
    }
}

// ---------------------------------------------------------------
// Audio startup gate
// ---------------------------------------------------------------

/// Audio startup gate: starts closed (the sink emits silence without
/// consuming the ring or anchoring the clock) and opens when the first
/// video frame is shown, so picture and sound start together even when
/// the video open+probe takes a while (network URLs). Safety valve: if
/// no frame has arrived by `deadline` (broken video stream?), it opens
/// anyway so the player doesn't stay mute.
pub struct AudioGate {
    opened: bool,
    deadline: Instant,
}

impl AudioGate {
    pub fn new(valve_after: Duration) -> Self {
        Self { opened: false, deadline: Instant::now() + valve_after }
    }

    /// Open the gate (idempotent).
    pub fn open(&mut self, audio: &Option<AudioHandle>) {
        if !self.opened {
            self.opened = true;
            if let Some(a) = audio.as_ref() {
                a.open_gate();
            }
        }
    }

    /// Safety valve: open if the deadline passed and nobody opened it.
    pub fn tick(&mut self, audio: &Option<AudioHandle>) {
        if !self.opened && Instant::now() >= self.deadline {
            self.open(audio);
        }
    }
}

// ---------------------------------------------------------------
// Pipeline — shared opening (clocks + audio + master + decoder)
// ---------------------------------------------------------------

/// An opened playback pipeline: exactly the same steps and parameters
/// in terminal and GUI. Fields are public because each frontend weaves
/// its own loop around them.
pub struct Pipeline {
    pub dec: DecoderHandle,
    pub audio: Option<AudioHandle>,
    pub master: Arc<MasterClock>,
    pub vidclk: Arc<FfClock>,
    pub using_audio: bool,
    /// The container declares no duration (AV_NOPTS_VALUE / <=0):
    /// live stream (Twitch, live TV) → relative HUD + DVR window.
    pub is_live: bool,
    /// The stream's real 1/fps (falling back to 1/30 when the
    /// container doesn't declare it) — the "natural" duration between
    /// frames when PTS values are invalid.
    pub fallback_frame_dur: f64,
    /// Cap on the natural duration (ffplay uses 10 s for formats
    /// without AVFMT_TS_DISCONT): a larger PTS gap is treated as
    /// invalid.
    pub max_frame_dur: f64,
    pub gate: AudioGate,
}

impl Pipeline {
    /// Open the full pipeline. `audio_tracks` is the already-probed
    /// inventory (see `probe_tracks`; probing happens outside because
    /// the terminal needs the subtitle tracks before it can compute
    /// the decoder dimensions). `dst_w`/`dst_h` are the decoder's
    /// initial dimensions (a hint — you can `resize` later).
    pub fn open(cfg: &Config, audio_tracks: &[TrackInfo], dst_w: u32, dst_h: u32) -> Result<Self> {
        // Independent audio/video clocks. Audio clock staleness: cpal
        // callbacks arrive every 25-100 ms; more than 250 ms without a
        // `set_pts` means the device is not consuming (PulseAudio
        // stall, underrun, audio stream EOF). The clock freezes and
        // `anchored()` flips to false → the video (the slave) waits
        // instead of running on in silence.
        let audclk = FfClock::new();
        audclk.set_staleness(0.25);
        let vidclk = FfClock::new();

        // Audio (optional). Initial track: --aid (1-based) / --alang,
        // falling back to FFmpeg's "best" pick when nothing matches.
        let audio_media = cfg.audio_path.as_ref().unwrap_or(&cfg.path);
        let start_audio_stream = tracks::select(audio_tracks, cfg.aid, cfg.alang.as_deref())
            .map(|pos| audio_tracks[pos].stream_index);
        let audio: Option<AudioHandle> = if cfg.no_audio
            || cfg.audio_backend == audio::BackendPref::NoAudio
        {
            None
        } else {
            match audio::spawn(audio_media, audclk.clone(), start_audio_stream, cfg.audio_backend)
            {
                Ok(h) if h.has_audio => {
                    // Only visible with --verbose (stderr goes to /dev/null otherwise).
                    eprintln!("[rtv-audio] output backend: {}", h.backend_name);
                    Some(h)
                }
                _ => None,
            }
        };
        let using_audio = audio.as_ref().map(|a| a.has_audio).unwrap_or(false);

        // MasterClock: picks audclk or vidclk as the master.
        let master: Arc<MasterClock> = if using_audio {
            MasterClock::with_audio(audclk, vidclk.clone())
        } else {
            MasterClock::video_only(vidclk.clone())
        };

        // Initial volume.
        if let Some(a) = audio.as_ref() {
            a.set_volume(100);
        }

        // Video decoder.
        let dec = decoder::spawn(&cfg.path, dst_w.max(2), dst_h.max(2), cfg.hw_pref)?;
        let is_live = !(dec.duration.is_finite() && dec.duration > 0.0);
        let fallback_frame_dur = if dec.fps > 1.0 { 1.0 / dec.fps } else { 1.0 / 30.0 };

        Ok(Self {
            dec,
            audio,
            master,
            vidclk,
            using_audio,
            is_live,
            fallback_frame_dur,
            max_frame_dur: 10.0,
            gate: AudioGate::new(Duration::from_secs(10)),
        })
    }
}

// ---------------------------------------------------------------
// Seek window
// ---------------------------------------------------------------

/// Valid seek range `[min_t, max_t]`.
///   * VOD: `[0, duration - 0.5]` (margin so we never land exactly on
///     EOF → frozen picture).
///   * Live: `[first received PTS, live edge]` — the DVR window rtv
///     has seen so far. Careful: before the first frame `max_live_pts`
///     is -inf; the window collapses to `[lo, lo]` rather than
///     producing min_t > max_t (which used to send the seek to 0.0
///     while real Twitch PTS values sit in the thousands of seconds).
pub fn seek_window(
    is_live: bool,
    live_start_pts: Option<f64>,
    max_live_pts: f64,
    duration: f64,
) -> (f64, f64) {
    if is_live {
        let lo = live_start_pts.unwrap_or(0.0);
        let hi = if max_live_pts.is_finite() { max_live_pts.max(lo) } else { lo };
        (lo, hi)
    } else {
        (0.0, (duration - 0.5).max(0.0))
    }
}

// ---------------------------------------------------------------
// plan_frame — the ffplay-style sync arithmetic
// ---------------------------------------------------------------

/// Timing decision for the candidate frame.
pub enum FramePlan {
    /// Show right now. `frame_timer` stays advanced (the frame
    /// consumed its slot).
    Show,
    /// Clearly late relative to the master. `frame_timer` is rolled
    /// back (a drop doesn't consume a presentation slot — without this
    /// the post-seek catch-up pushed the timer seconds into the future
    /// and the player got stuck at one frame every 500 ms). The caller
    /// decides: a regular drop, or show it anyway (the terminal's
    /// post-resize refinement catch-up) re-anchoring `frame_timer` to
    /// wall time.
    Late,
    /// Not due yet: sleep `sleep_s` (capped at 0.5 s) until the
    /// presentation instant. `frame_timer` stays advanced: if the
    /// caller postpones the frame (wait interrupted by an event, or
    /// looping back in the GUI) it must roll back by subtracting
    /// `target_delay`; if the wait completes, show it and touch
    /// nothing.
    Wait { sleep_s: f64, target_delay: f64 },
}

/// ffplay-style sync — identical in terminal and GUI:
///   1. natural delay between the previous frame and the candidate
///      (`vp_duration`, with fallback and cap),
///   2. drift adjustment `vidclk - master` (`compute_target_delay`;
///      vidclk extrapolates the PTS of the frame currently on screen —
///      using the pending one added a systematic +1 frame offset),
///   3. `frame_timer += target_delay`, resyncing to wall time when it
///      fell more than `AV_SYNC_THRESHOLD_MAX` (100 ms) behind so we
///      don't drag a time debt around,
///   4. drop if the frame is late relative to the master,
///   5. wait if it isn't due yet.
pub fn plan_frame(
    frame_pts: f64,
    last_shown_pts: f64,
    frame_timer: &mut f64,
    fallback_frame_dur: f64,
    max_frame_dur: f64,
    vidclk: &FfClock,
    master: &MasterClock,
) -> FramePlan {
    let natural_delay = vp_duration(last_shown_pts, frame_pts, fallback_frame_dur, max_frame_dur);
    let vid_now = vidclk.now();
    let master_now = master.now();
    let diff = if vid_now.is_finite() && master_now.is_finite() {
        vid_now - master_now
    } else {
        0.0
    };
    let target_delay = compute_target_delay(natural_delay, diff);

    // Wall-clock instant at which we "want" to show this frame.
    *frame_timer += target_delay;
    let now_wall = wall_now_f64();
    if now_wall - *frame_timer > AV_SYNC_THRESHOLD_MAX {
        *frame_timer = now_wall;
    }

    // Late relative to the master? → Late (timer already rolled back).
    let master_diff = frame_pts - master.now();
    if master_diff.is_finite() && master_diff < -AV_SYNC_THRESHOLD_MAX {
        *frame_timer -= target_delay;
        return FramePlan::Late;
    }

    // Not due yet? → Wait (timer advanced; the caller rolls it back if
    // it postpones the frame).
    if *frame_timer > now_wall {
        let sleep_s = (*frame_timer - now_wall).min(0.5);
        if sleep_s > 0.0005 {
            return FramePlan::Wait { sleep_s, target_delay };
        }
    }

    FramePlan::Show
}
