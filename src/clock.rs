//! clock.rs — ffplay.c-style clock.
//!
//! Rewritten following the ffplay/mpv implementation (reference:
//! FFmpeg/fftools/ffplay.c, functions `set_clock_at`, `get_clock`,
//! `compute_target_delay`, `sync_clock_to_slave`).
//!
//! Core idea: the Clock stores `pts_drift = pts - time`, not an
//! accumulated counter that has to be "advanced". Whenever something
//! (audio callback, video refresh, seek) updates the clock, it does a
//! single `set(pts, serial)` and from that moment `now()` interpolates
//! `pts_drift + time` against wall time. There's no `advance()` adding
//! µs sample by sample anymore — a pattern that was the source of
//! every race and of the post-seek desynchronization.
//!
//! Two clocks:
//!   * `audclk`: set to the PTS of the last audio frame RIGHT at the
//!     moment the cpal callback pushes it toward the hardware. It
//!     compensates for `playback_delay` (samples still queued in the
//!     driver's buffer) so "now" reflects what the user is hearing.
//!   * `vidclk`: set to the PTS of the last video frame shown. The
//!     player updates it on every rendered frame.
//!
//! The master clock is `audclk` when there's audio, `vidclk` otherwise.
//!
//! The `serial` invalidates stale samples/frames after a seek. Any
//! `set()` with serial != master serial is ignored, and `now()`
//! returns NaN (which the player reads as "no useful clock yet, show
//! the next frame right away").

use parking_lot::Mutex;
use std::sync::atomic::{AtomicI32, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// Thresholds from ffplay.c — don't change them without a reason.
pub const AV_SYNC_THRESHOLD_MIN: f64 = 0.04; // 40 ms
pub const AV_SYNC_THRESHOLD_MAX: f64 = 0.10; // 100 ms
pub const AV_SYNC_FRAMEDUP_THRESHOLD: f64 = 0.10;
pub const AV_NOSYNC_THRESHOLD: f64 = 10.0; // reset once diff > 10 s

pub trait Clock: Send + Sync {
    fn now(&self) -> f64;
    fn pause(&self);
    fn resume(&self);
    fn is_paused(&self) -> bool;
    fn set(&self, t: f64);
}

/// ffplay-style internal clock: `pts_drift + time` while playing,
/// `pts` while paused. No accumulators, no per-sample advance().
pub struct FfClock {
    inner: Mutex<FfClockInner>,
    pub paused: AtomicU8, // 0=play, 1=pause
    /// Monotonic serial. Producers (audio callback / video loop) write
    /// with their current serial; if it doesn't match this one at
    /// `set_pts` time, the write is ignored (post-seek leftover).
    pub serial: AtomicI32,
}

struct FfClockInner {
    /// "Base" PTS (absolute media seconds).
    pts: f64,
    /// Wall-clock moment of the last set (for the paused clock).
    last_updated: Instant,
    /// PTS frozen while paused.
    pts_at_pause: f64,
    /// Is the clock "anchored" to real producer data?
    /// After a seek (`set`) it flips to false: `now()` returns the
    /// target FROZEN until the producer (audio callback / shown video
    /// frame) does the first `set_pts` under the new serial. Same as
    /// ffplay's NaN clock after a seek — without it, the clock kept
    /// running during the ~100-300 ms the decoder needs to rehydrate,
    /// the first frame at the target arrived "late", got dropped, and
    /// A/V started out of sync after every seek.
    anchored: bool,
    /// Maximum extrapolation allowed since the last real `set_pts`
    /// (seconds). If the producer stops feeding the clock (audio
    /// device stall, ring underrun, audio stream EOF), `now()` FREEZES
    /// at `pts + staleness` and `anchored()` flips to false — video
    /// (the slave) stops instead of racing against a clock that no
    /// longer represents what's being heard. Without this, a ~2 s
    /// PulseAudio startup stall advanced the video 2 s in silence and
    /// then the master jumped backwards (+1900 ms of avdiff).
    /// INFINITY = no limit (vidclk).
    staleness: f64,
}

impl FfClock {
    pub fn new() -> Arc<Self> {
        let now = Instant::now();
        Arc::new(Self {
            inner: Mutex::new(FfClockInner {
                pts: 0.0,
                last_updated: now,
                pts_at_pause: 0.0,
                anchored: false,
                staleness: f64::INFINITY,
            }),
            paused: AtomicU8::new(0),
            serial: AtomicI32::new(0),
        })
    }

    /// Writes `pts` as the new reference point. Only when `serial`
    /// matches the current one — that's how writes from a decoder that
    /// hasn't seen the seek yet get invalidated.
    pub fn set_pts(&self, pts: f64, serial: i32) {
        if serial != self.serial.load(Ordering::Acquire) {
            return; // post-seek leftover
        }
        let mut g = self.inner.lock();
        g.pts = pts;
        g.last_updated = Instant::now();
        g.anchored = true;
    }

    /// Serial bump. Call BEFORE touching the pts.
    pub fn bump_serial(&self) -> i32 {
        self.serial.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn current_serial(&self) -> i32 {
        self.serial.load(Ordering::Acquire)
    }

    /// Sets the maximum extrapolation without real data (see the
    /// `staleness` field). The player sets it on the AUDIO clock
    /// (≈250 ms; callbacks arrive every 25-100 ms, so 250 ms with no
    /// data = the device is NOT consuming).
    pub fn set_staleness(&self, secs: f64) {
        self.inner.lock().staleness = secs.max(0.0);
    }

    /// Is the clock anchored to real producer data? After a `set()`
    /// (seek) it returns false until the first `set_pts` under the new
    /// serial. Also returns false when the last real data is older
    /// than `staleness` (audio stall/underrun/EOF). The player uses it
    /// to decide whether video must WAIT (frozen clock) or follow a
    /// running one.
    pub fn anchored(&self) -> bool {
        let g = self.inner.lock();
        if !g.anchored {
            return false;
        }
        if self.paused.load(Ordering::Acquire) != 0 {
            return true; // no new data while paused, and that's fine
        }
        g.last_updated.elapsed().as_secs_f64() <= g.staleness
    }

    /// Re-points the frozen target WITHOUT bumping the serial. Used
    /// when video lands on the real keyframe (<= the seek target): the
    /// clock becomes frozen at the landing PTS so audio starts aligned
    /// with the picture being shown.
    pub fn retarget(&self, t: f64) {
        let mut g = self.inner.lock();
        g.pts = t.max(0.0);
        g.last_updated = Instant::now();
        g.pts_at_pause = t.max(0.0);
        g.anchored = false;
    }

    /// Safety valve: anchors the clock at its current pts even if no
    /// producer has written yet. Used when audio doesn't show up
    /// within a reasonable time after a seek (e.g. seeking past the
    /// end of the audio stream) — without it, video stayed frozen
    /// forever waiting for an anchor that never comes.
    pub fn force_anchor(&self) {
        let mut g = self.inner.lock();
        // Freeze the current effective pts and restart from there
        // (covers both "never anchored" and "anchored but stale").
        let elapsed = g.last_updated.elapsed().as_secs_f64().min(g.staleness);
        g.pts += elapsed;
        g.last_updated = Instant::now();
        g.anchored = true;
    }
}

impl Clock for FfClock {
    fn now(&self) -> f64 {
        let g = self.inner.lock();
        if self.paused.load(Ordering::Acquire) != 0 {
            return g.pts_at_pause;
        }
        // Unanchored (right after seek / startup): time stays frozen
        // at the target until the first real data arrives.
        if !g.anchored {
            return g.pts;
        }
        // now = pts + wall time elapsed since the last set_pts.
        // Equivalent to ffplay's `pts_drift + av_gettime()` formula,
        // but using `Instant`, which is monotonic with no fixed origin.
        // Capped at `staleness`: with no fresh data the clock freezes.
        let elapsed = g.last_updated.elapsed().as_secs_f64().min(g.staleness);
        g.pts + elapsed
    }

    fn pause(&self) {
        if self.paused.swap(1, Ordering::AcqRel) == 0 {
            // Freeze the effective pts at this instant (a single lock,
            // no race window between computing and writing).
            let mut g = self.inner.lock();
            let elapsed = g.last_updated.elapsed().as_secs_f64().min(g.staleness);
            g.pts_at_pause = g.pts + elapsed;
        }
    }

    fn resume(&self) {
        if self.paused.swap(0, Ordering::AcqRel) != 0 {
            // Resume WITHOUT a jump: set pts = pts_at_pause and
            // last_updated = now, so `now()` = pts_at_pause at the
            // instant right after resume.
            let now = Instant::now();
            let mut g = self.inner.lock();
            g.pts = g.pts_at_pause;
            g.last_updated = now;
        }
    }

    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire) != 0
    }

    /// Absolute seek. Bumps the serial (invalidating in-flight writes)
    /// and pins the clock to `t` under the new serial.
    fn set(&self, t: f64) {
        let new_serial = self.bump_serial();
        let now = Instant::now();
        let mut g = self.inner.lock();
        g.pts = t.max(0.0);
        g.last_updated = now;
        g.pts_at_pause = t.max(0.0);
        // Unanchor: `now()` returns `t` frozen until the first
        // `set_pts` from a producer carrying the new serial.
        g.anchored = false;
        // Serial already bumped atomically above.
        let _ = new_serial;
    }
}

// -------------------- Master clock chooser --------------------

/// Wraps two `FfClock`s (audio + video). The master is chosen by the
/// presence of audio, with video as fallback. It exposes `Clock` so
/// the player keeps using it as before.
pub struct MasterClock {
    audclk: Option<Arc<FfClock>>,
    vidclk: Arc<FfClock>,
    /// Local pause state, propagated to both clocks.
    paused: AtomicU8,
}

impl MasterClock {
    pub fn with_audio(audclk: Arc<FfClock>, vidclk: Arc<FfClock>) -> Arc<Self> {
        Arc::new(Self {
            audclk: Some(audclk),
            vidclk,
            paused: AtomicU8::new(0),
        })
    }
    pub fn video_only(vidclk: Arc<FfClock>) -> Arc<Self> {
        Arc::new(Self {
            audclk: None,
            vidclk,
            paused: AtomicU8::new(0),
        })
    }
    /// The "master" clock — audio when present, video otherwise.
    pub fn master(&self) -> &Arc<FfClock> {
        self.audclk.as_ref().unwrap_or(&self.vidclk)
    }
    /// Is the master clock anchored (producing real time)?
    pub fn master_anchored(&self) -> bool {
        self.master().anchored()
    }
    /// Re-points BOTH clocks to a seek's real landing PTS WITHOUT
    /// bumping serials (in-flight producers stay valid).
    pub fn retarget(&self, t: f64) {
        self.vidclk.retarget(t);
        if let Some(a) = &self.audclk {
            a.retarget(t);
        }
    }
}

impl Clock for MasterClock {
    fn now(&self) -> f64 {
        self.master().now()
    }
    fn pause(&self) {
        self.paused.store(1, Ordering::Release);
        self.vidclk.pause();
        if let Some(a) = &self.audclk {
            a.pause();
        }
    }
    fn resume(&self) {
        self.paused.store(0, Ordering::Release);
        self.vidclk.resume();
        if let Some(a) = &self.audclk {
            a.resume();
        }
    }
    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire) != 0
    }
    fn set(&self, t: f64) {
        // A global `set` (from the player) bumps BOTH serials and pins
        // BOTH clocks to the target. Producers (audio callback / video
        // loop) will see the new serial and drop their in-flight
        // writes carrying the old one.
        self.vidclk.set(t);
        if let Some(a) = &self.audclk {
            a.set(t);
        }
    }
}

// -------------------- compute_target_delay --------------------

/// Reimplements ffplay.c's `compute_target_delay` with its EXACT
/// semantics: `diff = get_clock(vidclk) - get_master_clock()` — i.e.
/// the drift between the video clock (PTS of the frame ON SCREEN,
/// extrapolated) and the master. It is NOT "next frame's PTS -
/// master": that variant baked in a +1 frame offset which, combined
/// with the smooth correction, left a systematic ~-40 ms bias.
///
/// * If video is LATE against the master (diff <= -threshold):
///   `delay = max(0, delay + diff)` — show now, or even drop.
/// * If it's FAR AHEAD (diff >= threshold && delay > FRAMEDUP):
///   `delay = delay + diff` — wait exactly as long as needed.
/// * If it's slightly AHEAD (diff >= threshold):
///   `delay = 2 * delay` — double the delay to let the master catch up.
pub fn compute_target_delay(natural_delay: f64, diff: f64) -> f64 {
    let sync_threshold = natural_delay.clamp(AV_SYNC_THRESHOLD_MIN, AV_SYNC_THRESHOLD_MAX);

    if diff.is_finite() && diff.abs() < AV_NOSYNC_THRESHOLD {
        if diff <= -sync_threshold {
            // Video is late → show NOW.
            return (natural_delay + diff).max(0.0);
        } else if diff >= sync_threshold
            && (natural_delay > AV_SYNC_FRAMEDUP_THRESHOLD || diff > AV_SYNC_THRESHOLD_MAX)
        {
            // Far ahead (or a big backwards jump of the master, e.g.
            // audio re-anchoring after a stall): wait EXACTLY. With
            // ffplay's doubling, a +300 ms jump took ~8 frames to
            // converge, all of them shown out of sync.
            return natural_delay + diff;
        } else if diff >= sync_threshold {
            return 2.0 * natural_delay;
        }
        // SMOOTH correction inside the threshold: ffplay tolerates up
        // to ±sync_threshold without correcting, which leaves
        // systematic ~±40 ms offsets pinned forever (e.g. the one
        // established when audio anchors after a seek).
        //
        // Two regimes:
        //   * |diff| <= 10 ms → FULL correction in one frame (capped
        //     at ±30% of the natural delay). Shifting presentation by
        //     <=10 ms is invisible, and it wipes out in one go the
        //     residue the proportional correction left dying
        //     geometrically (tenths of a ms of median in the
        //     post-seek sync log for seconds).
        //   * |diff| > 10 ms → proportional correction (50% of the
        //     diff per frame, same cap) that converges with no visible
        //     jitter.
        let correction = if diff.abs() <= 0.010 {
            diff.clamp(-natural_delay * 0.3, natural_delay * 0.3)
        } else {
            (diff * 0.5).clamp(-natural_delay * 0.3, natural_delay * 0.3)
        };
        return (natural_delay + correction).max(0.0);
    }
    natural_delay
}

/// Natural duration between frames by PTS. When invalid (NaN, ≤0,
/// >max_frame_duration), it falls back to `fallback` (e.g. 1/fps).
pub fn vp_duration(cur_pts: f64, next_pts: f64, fallback: f64, max: f64) -> f64 {
    let d = next_pts - cur_pts;
    if !d.is_finite() || d <= 0.0 || d > max {
        fallback
    } else {
        d
    }
}

#[allow(dead_code)]
pub fn sleep_until(clock: &dyn Clock, target_secs: f64) {
    let now = clock.now();
    if target_secs > now {
        let delta = (target_secs - now).min(0.5);
        std::thread::sleep(Duration::from_secs_f64(delta));
    }
}

// -------------------- Tests --------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn ffclock_now_advances_with_wall() {
        let c = FfClock::new();
        c.set_pts(10.0, 0);
        sleep(Duration::from_millis(100));
        let t = c.now();
        assert!((10.09..10.20).contains(&t), "expected ~10.10, got {t}");
    }

    #[test]
    fn ffclock_frozen_after_seek_until_anchored() {
        let c = FfClock::new();
        c.set_pts(5.0, 0);
        c.set(42.0); // seek → unanchored
        sleep(Duration::from_millis(80));
        // Frozen at the target until real data arrives.
        assert!((c.now() - 42.0).abs() < 0.001, "clock ran while unanchored: {}", c.now());
        // First real data under the new serial → re-anchors and runs.
        c.set_pts(42.0, c.current_serial());
        sleep(Duration::from_millis(60));
        assert!(c.now() > 42.05, "clock not running after re-anchor: {}", c.now());
    }

    #[test]
    fn ffclock_set_bumps_serial_and_ignores_old_writes() {
        let c = FfClock::new();
        let old_serial = c.current_serial();
        c.set(42.0);
        assert_eq!(c.current_serial(), old_serial + 1);
        // A writer holding the old serial must NOT modify the pts.
        c.set_pts(999.0, old_serial);
        let g = c.inner.lock();
        assert!(g.pts >= 41.9 && g.pts <= 42.1, "leftover set_pts mutated pts: {}", g.pts);
    }

    #[test]
    fn compute_target_delay_matches_ffplay_ranges() {
        // Video exactly in sync (diff=0) → natural delay untouched.
        assert!((compute_target_delay(0.040, 0.0) - 0.040).abs() < 1e-9);
        // Video 200ms LATE → delay = max(0, 0.04-0.2) = 0.
        assert_eq!(compute_target_delay(0.040, -0.2), 0.0);
        // Video 60ms AHEAD (delay < FRAMEDUP, diff < MAX) → doubled delay.
        let d = compute_target_delay(0.040, 0.06);
        assert!((d - 0.080).abs() < 1e-9, "expected 0.080, got {d}");
        // Video FAR ahead (200ms > THRESHOLD_MAX) → exact wait.
        let d = compute_target_delay(0.040, 0.2);
        assert!((d - 0.240).abs() < 1e-9, "expected 0.240, got {d}");
        // Diff > NOSYNC (10s) → returns the natural delay unadjusted.
        assert_eq!(compute_target_delay(0.040, 95.0), 0.040);
    }

    #[test]
    fn compute_target_delay_small_diff_full_correction() {
        // |diff| <= 10 ms → FULL correction in one frame (invisible to
        // the eye, wipes out the post-seek residue in one pass).
        let d = compute_target_delay(0.040, 0.008);
        assert!((d - 0.048).abs() < 1e-9, "expected 0.048, got {d}");
        let d = compute_target_delay(0.040, -0.008);
        assert!((d - 0.032).abs() < 1e-9, "expected 0.032, got {d}");
        // But capped at ±30% of the natural delay (very short natural).
        let d = compute_target_delay(0.010, 0.009);
        assert!((d - 0.013).abs() < 1e-9, "expected 0.013 (30% cap), got {d}");
        // |diff| > 10 ms → proportional regime (50%), also capped.
        let d = compute_target_delay(0.040, 0.020);
        assert!((d - 0.050).abs() < 1e-9, "expected 0.050, got {d}");
    }

    #[test]
    fn retarget_keeps_serial_and_unanchors() {
        let c = FfClock::new();
        c.set(30.0); // seek → serial+1, frozen at 30
        let s = c.current_serial();
        c.retarget(27.5); // landing on the real keyframe
        assert_eq!(c.current_serial(), s, "retarget must not bump the serial");
        assert!((c.now() - 27.5).abs() < 0.001, "frozen at the landing pts");
        c.set_pts(27.5, s);
        sleep(Duration::from_millis(50));
        assert!(c.now() > 27.52, "clock runs after anchoring at the landing");
    }

    #[test]
    fn staleness_freezes_and_unanchors() {
        let c = FfClock::new();
        c.set_staleness(0.08);
        c.set_pts(5.0, 0);
        assert!(c.anchored());
        sleep(Duration::from_millis(150));
        // Frozen at pts + staleness, and unanchored.
        assert!((c.now() - 5.08).abs() < 0.02, "now={}", c.now());
        assert!(!c.anchored(), "should be stale");
        // Fresh data re-anchors and the clock runs again.
        c.set_pts(5.05, 0);
        assert!(c.anchored());
        sleep(Duration::from_millis(40));
        assert!(c.now() > 5.08 && c.now() < 5.15, "now={}", c.now());
    }

    #[test]
    fn force_anchor_starts_clock() {
        let c = FfClock::new();
        c.set(10.0);
        assert!(!c.anchored());
        c.force_anchor();
        assert!(c.anchored());
        sleep(Duration::from_millis(50));
        assert!(c.now() > 10.04, "clock runs after force_anchor: {}", c.now());
    }

    #[test]
    fn pause_resume_no_jump() {
        let c = FfClock::new();
        c.set_pts(20.0, 0);
        sleep(Duration::from_millis(50));
        c.pause();
        let t_paused = c.now();
        sleep(Duration::from_millis(200));
        // The clock does NOT advance while paused.
        assert!((c.now() - t_paused).abs() < 0.001, "clock advanced while paused");
        c.resume();
        // Right after resume, `now()` == the frozen value (no jump).
        assert!((c.now() - t_paused).abs() < 0.005);
    }
}
