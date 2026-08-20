//! Video demuxer + decoder. Runs on a dedicated thread and pushes
//! frames (RGB24, already scaled to the terminal size) into a bounded
//! channel that the renderer consumes.
//!
//! Design notes worth keeping in mind:
//!
//!   * `Serial` instead of `generation`: every seek bumps a counter.
//!     Frames leaving the pipeline with a stale serial are dropped
//!     silently.
//!
//!   * **Keyframe seeking (mpv's default behaviour)**: after
//!     `avformat_seek_file` with max_ts=target, FFmpeg lands on the
//!     keyframe <= target. The decoder emits FROM THAT KEYFRAME: the
//!     first post-seek frame shows up after ~1 decode (an instant
//!     jump) instead of silently decoding the whole GOP up to the
//!     target (which took several seconds with 4K AV1 and 3.5 s
//!     GOPs). The player aligns AUDIO to the video's actual landing
//!     PTS (frame.pts of the first frame), so there is no desync —
//!     we simply land on the keyframe.
//!
//!   * **Multithreaded decode**: `thread_count=0` (auto) + frame
//!     threading. Without it, dav1d/4K AV1 decoded on ONE thread at
//!     ~1.2x realtime, starving the audio thread → underruns and
//!     master-clock jumps.
//!
//!   * We no longer track `last_pts_ms` — the player owns the clock
//!     via `vidclk.set_pts` on every rendered frame.
//!
//! Resize handling:
//!
//!   * **`target_dims` as an `AtomicU64`** (w<<32|h): `resize()` is a
//!     single atomic store — no channel, no draining the queue of
//!     pre-decoded frames (which used to throw away ~2.5 s of buffer
//!     on every resize event → stalls), and resize storms coalesce
//!     for free (the decoder always reads the LATEST value).
//!
//!   * **`struct Scaler`**: bundles the `SwsCtx` with the output RGB
//!     frame and rebuilds them TOGETHER whenever any dimension or
//!     format changes. ffmpeg-the-third's `SwsCtx::run()` sizes the
//!     output frame exactly ONCE (while it's empty) and afterwards
//!     requires it to match the context: the old code recreated the
//!     context but REUSED the stale frame → `Error::OutputChanged`
//!     on every later `run()` → the decoder stopped emitting forever
//!     ("everything crashes" on resize). On error the Scaler resets
//!     to `None` and rebuilds cleanly on the next call — it can never
//!     stay broken.

use crate::hwdec::{self, ActiveHw, HwPref};
use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender, TrySendError};
use ffmpeg_the_third as ffmpeg;
use ffmpeg::format::Pixel;
use ffmpeg::media::Type;
use ffmpeg::software::scaling::{context::Context as SwsCtx, flag::Flags};
use ffmpeg::util::frame::video::Video as VideoFrame;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Packs (w, h) into a u64 for the atomic resize store/load.
#[inline]
fn pack_dims(w: u32, h: u32) -> u64 {
    (u64::from(w) << 32) | u64::from(h)
}

/// Unpacks the atomic u64 back into (w, h), clamped to at least 2x2
/// so degenerate dims (0/1) never reach sws_scale.
#[inline]
fn unpack_dims(v: u64) -> (u32, u32) {
    (((v >> 32) as u32).max(2), ((v & 0xFFFF_FFFF) as u32).max(2))
}

/// Robust scaler: keeps the `SwsCtx` AND the output RGB frame as one
/// unit. When the input dims change (mid-stream) or the output dims
/// change (terminal resize), it rebuilds BOTH at once — the fresh
/// (empty) output frame gets sized on the first `run()` with the new
/// context's dims, avoiding `Error::OutputChanged`. If `SwsCtx::get`
/// or `run` fail, it drops to `None` and retries on the next call:
/// it never poisons the decode loop.
struct Scaler {
    sws: Option<SwsCtx>,
    rgb: VideoFrame,
    in_w: u32,
    in_h: u32,
    in_fmt: Pixel,
    out_w: u32,
    out_h: u32,
}

impl Scaler {
    fn new() -> Self {
        Self {
            sws: None,
            rgb: VideoFrame::empty(),
            in_w: 0,
            in_h: 0,
            in_fmt: Pixel::None,
            out_w: 0,
            out_h: 0,
        }
    }

    /// Scales `frame` to `dst_w`x`dst_h` RGB24. Returns `Some(&rgb)`
    /// or `None` when the conversion wasn't possible (a fresh context
    /// will be retried on the next call).
    fn scale(&mut self, frame: &VideoFrame, dst_w: u32, dst_h: u32) -> Option<&VideoFrame> {
        let iw = frame.width();
        let ih = frame.height();
        let ifmt = frame.format();
        if iw == 0 || ih == 0 || ifmt == Pixel::None {
            return None;
        }
        let dw = dst_w.max(2);
        let dh = dst_h.max(2);

        let needs_rebuild = self.sws.is_none()
            || iw != self.in_w
            || ih != self.in_h
            || ifmt != self.in_fmt
            || dw != self.out_w
            || dh != self.out_h;

        if needs_rebuild {
            match SwsCtx::get(ifmt, iw, ih, Pixel::RGB24, dw, dh, Flags::FAST_BILINEAR) {
                Ok(ctx) => {
                    self.sws = Some(ctx);
                    // CRITICAL: a FRESH output frame must go with the
                    // fresh context. Reusing the old one (already sized
                    // to the previous dims) triggers Error::OutputChanged
                    // on every later run().
                    self.rgb = VideoFrame::empty();
                    self.in_w = iw;
                    self.in_h = ih;
                    self.in_fmt = ifmt;
                    self.out_w = dw;
                    self.out_h = dh;
                }
                Err(_) => {
                    self.sws = None;
                    return None;
                }
            }
        }

        match self.sws.as_mut()?.run(frame, &mut self.rgb) {
            Ok(()) => Some(&self.rgb),
            Err(_) => {
                // Potentially inconsistent state → reset EVERYTHING;
                // the next call rebuilds from scratch.
                self.sws = None;
                None
            }
        }
    }
}

/// An RGB24 frame ready to render. `width` and `height` are in
/// pixels (not columns/rows). PTS is in seconds. `serial` is the
/// serial it was produced under — the player drops frames whose
/// serial differs from the current one (leftovers after a seek).
pub struct RgbFrame {
    pub width: u32,
    pub height: u32,
    pub pts: f64,
    pub serial: i32,
    pub data: Vec<u8>,
}

pub struct DecoderHandle {
    pub rx: Receiver<RgbFrame>,
    pub duration: f64,
    pub source_size: (u32, u32),
    /// Estimated stream frame rate (avg_frame_rate).
    pub fps: f64,
    pub eof: Arc<AtomicBool>,
    /// Hwaccel state: raw AVHWDeviceType value while HW decode is
    /// active, or -1 for software (including the mid-stream fallback).
    /// The player reads it every frame for the HUD label.
    pub hw_state: Arc<AtomicI32>,
    seek_tx: Sender<SeekReq>,
    /// Target dims (w<<32|h) that the decoder reads RIGHT BEFORE
    /// scaling each frame. `resize()` is an atomic store: resize
    /// storms coalesce automatically and no event is ever lost.
    target_dims: Arc<AtomicU64>,
    /// Video decoder serial. Bumped on every seek. The player reads
    /// it to know which frames are still valid.
    pub serial: Arc<AtomicI32>,
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

pub struct SeekReq {
    pub target_secs: f64,
    pub serial: i32,
    /// REFINE seek (post-resize): the decoder drops the GOP's frames
    /// until pts >= target (exact hr-seek) instead of emitting from
    /// the keyframe. The player does NOT touch clocks or audio:
    /// playback continues and only the resolution of incoming frames
    /// changes.
    pub refine: bool,
    /// Seek direction (mpv-style):
    ///   * `forward=true` (→): land on the keyframe >= target.
    ///     Without this, when GOPs are longer than the seek step
    ///     (YouTube AV1 has >6 s GOPs) the keyframe <= target was THE
    ///     SAME on every keypress and the video got "stuck" — you
    ///     couldn't skip past a certain point.
    ///   * `forward=false` (← / loop restart): keyframe <= target,
    ///     as usual.
    pub forward: bool,
}

impl DecoderHandle {
    /// Queues a seek to the decoder thread. Returns the new serial.
    /// The caller (player) MUST also bump the clock serial BEFORE
    /// calling this (or at the same time). See `MasterClock::set`.
    pub fn seek(&self, target_secs: f64) -> i32 {
        self.seek_dir(target_secs, false)
    }

    /// Seek with an explicit direction. `forward=true` lands on the
    /// keyframe >= target (guarantees a forward seek ALWAYS makes
    /// progress, even when the GOP is longer than the seek step).
    pub fn seek_dir(&self, target_secs: f64, forward: bool) -> i32 {
        let new_serial = self.serial.fetch_add(1, Ordering::AcqRel) + 1;
        // Drain stale frames from the channel to cut latency.
        while self.rx.try_recv().is_ok() {}
        // Unbounded channel + send: a try_send could drop the last
        // seek of a burst and leave video and audio pointing at
        // different targets.
        let _ = self.seek_tx.send(SeekReq {
            target_secs,
            serial: new_serial,
            refine: false,
            forward,
        });
        new_serial
    }

    /// Re-decodes from `target_secs` using the CURRENT target dims
    /// (quality refinement after the terminal grows). Unlike `seek()`:
    ///   * the decoder DROPS the GOP's frames until pts >= target
    ///     (exact landing, not on the keyframe) → no visible jump
    ///     backwards;
    ///   * the player doesn't touch clocks or audio — sound keeps
    ///     going and the sharp frames slot in as soon as they catch
    ///     up with the master clock.
    ///
    /// The queue is drained: it held up to ~2.5 s of frames scaled to
    /// the old (small) dims, which looked blurry when upscaled — the
    /// "quality takes a while to come back" symptom after growing the
    /// window.
    pub fn refine_at(&self, target_secs: f64) -> i32 {
        let new_serial = self.serial.fetch_add(1, Ordering::AcqRel) + 1;
        while self.rx.try_recv().is_ok() {}
        let _ = self.seek_tx.send(SeekReq {
            target_secs,
            serial: new_serial,
            refine: true,
            forward: false,
        });
        new_serial
    }

    pub fn current_serial(&self) -> i32 {
        self.serial.load(Ordering::Acquire)
    }

    /// Changes the scaler's target dims. Lock-free and instant: it
    /// does NOT drain the queue of pre-decoded frames (the ~2.5 s
    /// cushion is preserved — "old" frames carry their own dims and
    /// the renderer crops them), and it does NOT use a channel (a
    /// resize storm collapses into the last value automatically).
    pub fn resize(&self, w: u32, h: u32) {
        self.target_dims
            .store(pack_dims(w.max(2), h.max(2)), Ordering::Release);
    }

    /// Name of the active hwaccel ("vaapi", "cuda"…) or None when
    /// decoding in software. Reflects mid-stream fallbacks live.
    pub fn hw_name(&self) -> Option<&'static str> {
        hwdec::name_of_raw(self.hw_state.load(Ordering::Acquire))
    }

    /// Cooperative shutdown with NO risk of hanging (a longstanding
    /// bug: an untimed `join()` hung the exit ~25% of the time under
    /// saturated decode — HEVC 1080p+, channel full):
    ///   1. Signals `stop` and keeps DRAINING the channel IN A LOOP
    ///      while it waits: draining once isn't enough — if the
    ///      thread was sleeping in `send_with_stop`'s backoff, it can
    ///      slip one more frame into the freshly opened slot and fill
    ///      the channel again.
    ///   2. Bounded `join` (500 ms) via `is_finished()`: if the
    ///      thread is still inside a blocking FFmpeg call that the
    ///      flag can't interrupt (avcodec_send_packet / receive_frame
    ///      with saturated frame-threading, av_read_frame over slow
    ///      I/O), it gets detached — the process is exiting anyway
    ///      and the OS reaps the thread. The user's terminal is NEVER
    ///      left hanging on FFmpeg.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let deadline = std::time::Instant::now() + Duration::from_millis(500);
            loop {
                // ALWAYS drain before checking the thread's state:
                // it opens a slot so any in-flight try_send completes
                // and the thread reaches its next `stop` check.
                while self.rx.try_recv().is_ok() {}
                if j.is_finished() {
                    let _ = j.join();
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    // Last-resort detach: we won't block the exit on
                    // a thread stuck inside FFmpeg.
                    drop(j);
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

impl Drop for DecoderHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn spawn<P: AsRef<Path>>(
    path: P,
    dst_w: u32,
    dst_h: u32,
    hw_pref: HwPref,
) -> Result<DecoderHandle> {
    let path = path.as_ref().to_owned();
    let ictx = crate::source::open(&path).with_context(|| format!("opening {:?}", path))?;

    let Some(stream) = ictx.streams().best(Type::Video) else {
        // AUDIO-ONLY files (mp3/flac/ogg/m4a/wav… with no video
        // stream): instead of failing, a synthetic generator emits
        // visualization frames with 0-based PTS at 30 fps. The rest
        // of the player never notices: audio (which opens its OWN
        // context in audio.rs) is still the master clock, and
        // HUD/seeks/pause behave exactly as with a real video.
        if ictx.streams().best(Type::Audio).is_some() {
            return spawn_audio_only(&ictx, dst_w, dst_h);
        }
        return Err(anyhow!("no video or audio stream found"));
    };
    let video_stream_index = stream.index();
    let time_base = stream.time_base();
    let duration = if stream.duration() > 0 {
        stream.duration() as f64 * f64::from(time_base.numerator())
            / f64::from(time_base.denominator())
    } else {
        ictx.duration() as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE)
    };

    // Average stream fps, used by the player's frame-duration fallback.
    let afr = stream.avg_frame_rate();
    let fps = if afr.numerator() > 0 && afr.denominator() > 0 {
        f64::from(afr.numerator()) / f64::from(afr.denominator())
    } else {
        30.0
    };

    // Auto-rotation (Display Matrix / rotate tag): phone videos shot
    // in portrait are stored landscape plus a "rotate 90° on display"
    // metadata hint. We apply it to the ALREADY scaled RGB frame
    // (cheap) and `source_size` becomes the AS-PRESENTED size — the
    // player computes aspect/layout without ever knowing.
    let transform = crate::rotation::from_stream(&stream);

    let (decoder, active_hw) = open_video_decoder(&stream, hw_pref)?;

    let (src_w, src_h) = transform.display_size(decoder.width(), decoder.height());

    // Pre-decode queue sized by a MEMORY BUDGET (~48 MB): small
    // frames (ascii/halfblocks) fit 64 → the decoder builds up a
    // ~2.5 s cushion while audio spins up (PulseAudio can take ~2 s
    // to deliver its first callback) or after a seek, absorbing the
    // 4K AV1/HEVC decode warmup. Large frames (kitty at 2K) get
    // capped at 4-8 so we don't eat all the RAM. With bounded(2) the
    // decoder sat BLOCKED through the startup hold and then never
    // recovered the deficit (a fixed -580 ms).
    let frame_bytes = (dst_w.max(2) as usize) * (dst_h.max(2) as usize) * 3;
    let cap = (48 * 1024 * 1024 / frame_bytes.max(1)).clamp(4, 64);
    let (tx, rx) = bounded::<RgbFrame>(cap);
    let (seek_tx, seek_rx) = unbounded::<SeekReq>();
    let target_dims = Arc::new(AtomicU64::new(pack_dims(dst_w.max(2), dst_h.max(2))));
    let stop = Arc::new(AtomicBool::new(false));
    let eof = Arc::new(AtomicBool::new(false));
    let serial = Arc::new(AtomicI32::new(0));
    let hw_state = Arc::new(AtomicI32::new(
        active_hw
            .as_ref()
            .map(|h| h.device_type.0 as i32)
            .unwrap_or(-1),
    ));

    let stop_th = stop.clone();
    let eof_th = eof.clone();
    let serial_th = serial.clone();
    let target_dims_th = target_dims.clone();
    let hw_state_th = hw_state.clone();

    let join = thread::Builder::new()
        .name("rtv-decoder".into())
        .spawn(move || {
            let _ = decode_loop(
                path,
                video_stream_index,
                decoder,
                active_hw,
                hw_state_th,
                tx,
                seek_rx,
                target_dims_th,
                stop_th,
                eof_th.clone(),
                serial_th,
                transform,
            );
            eof_th.store(true, Ordering::Relaxed);
        })?;

    Ok(DecoderHandle {
        rx,
        duration,
        source_size: (src_w, src_h),
        fps,
        eof,
        hw_state,
        seek_tx,
        target_dims,
        serial,
        stop,
        join: Some(join),
    })
}

/// Opens the video decoder, attempting hwaccel per `hw_pref`.
///
/// The HW attempt gets its own context: if `avcodec_open2` fails with
/// the hwaccel attached, that context is UNRECOVERABLE (FFmpeg won't
/// reopen a failed context) → the software path is ALWAYS built on a
/// fresh, clean context.
///
/// Threading per path:
///   * HW: `Type::None`, count=1 — the GPU does the heavy lifting;
///     CPU frame-threading doesn't apply and with some hwaccels it
///     adds latency or actively gets in the way.
///   * SW: `Type::Frame`, count=0 (auto, all cores) — critical for
///     software 4K AV1/HEVC (single-threaded dav1d can't keep up
///     with realtime and steals CPU from audio → underruns).
fn open_video_decoder(
    stream: &ffmpeg::Stream,
    hw_pref: HwPref,
) -> Result<(ffmpeg::decoder::Video, Option<ActiveHw>)> {
    // ── HW attempt ──────────────────────────────────────────────
    if !matches!(hw_pref, HwPref::None) {
        let codec_id = {
            let ctx =
                ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
            ctx.id()
        };
        // DECODER candidates for the HW attempt. FFmpeg defaults to
        // external decoders when compiled in (AV1 → libdav1d, which
        // is software-ONLY and advertises no hwaccel). The codec's
        // NATIVE decoder (same name as the codec: "av1", "h264"…) is
        // the one that exposes hwaccels — for AV1 the native one is
        // even hwaccel-only, it exists precisely for this. If the
        // default isn't the native one, we try the native next; the
        // software path below still uses the default (dav1d is the
        // best software AV1 decoder).
        let mut try_codecs: Vec<ffmpeg::Codec> = Vec::new();
        if let Some(c) = ffmpeg::codec::decoder::find(codec_id) {
            try_codecs.push(c);
        }
        let native_name = unsafe {
            std::ffi::CStr::from_ptr(ffmpeg::ffi::avcodec_get_name(codec_id.into()))
        }
        .to_str()
        .unwrap_or("")
        .to_string();
        if !native_name.is_empty()
            && try_codecs.first().map(|c| c.name() != native_name).unwrap_or(false)
        {
            if let Some(c) = ffmpeg::codec::decoder::find_by_name(&native_name) {
                hwdec::diag(format!(
                    "hwdec: trying native decoder '{native_name}' \
                     (the default one has no hwaccel support)"
                ));
                try_codecs.push(c);
            }
        }
        for codec in try_codecs {
            let ctx =
                ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
            let mut dec = ctx.decoder();
            let hw = unsafe { hwdec::try_enable(dec.as_mut_ptr(), codec.as_ptr(), hw_pref) };
            if let Some(active) = hw {
                let mut tc = ffmpeg::codec::threading::Config::kind(
                    ffmpeg::codec::threading::Type::None,
                );
                tc.count = 1;
                dec.set_threading(tc);
                match dec.open_as(codec).and_then(|o| o.video()) {
                    Ok(v) => return Ok((v, Some(active))),
                    Err(e) => {
                        // A failed context can't be reused → clear the
                        // get_format static and try the next decoder
                        // (or fall through to software).
                        hwdec::diag(format!(
                            "hwdec: {} attached but avcodec_open2 failed ({e}) → software",
                            active.name()
                        ));
                        hwdec::disable_expected_fmt();
                    }
                }
            }
        }
    }

    // ── Software path (fresh, clean context) ────────────────────
    let mut ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
    {
        let mut tc = ffmpeg::codec::threading::Config::kind(
            ffmpeg::codec::threading::Type::Frame,
        );
        tc.count = 0; // 0 = auto (all cores)
        ctx.set_threading(tc);
    }
    Ok((ctx.decoder().video()?, None))
}

/// Rebuilds a 100% software decoder for `video_idx` (mid-stream
/// fallback when the HW path dies). `None` when even the software
/// path couldn't open (corrupt/vanished stream).
fn reopen_software(
    ictx: &ffmpeg::format::context::Input,
    video_idx: usize,
) -> Option<ffmpeg::decoder::Video> {
    let stream = ictx.stream(video_idx)?;
    let mut ctx =
        ffmpeg::codec::context::Context::from_parameters(stream.parameters()).ok()?;
    {
        let mut tc = ffmpeg::codec::threading::Config::kind(
            ffmpeg::codec::threading::Type::Frame,
        );
        tc.count = 0;
        ctx.set_threading(tc);
    }
    ctx.decoder().video().ok()
}

#[allow(clippy::too_many_arguments)]
fn decode_loop(
    path: std::path::PathBuf,
    video_idx: usize,
    mut decoder: ffmpeg::decoder::Video,
    mut hw: Option<ActiveHw>,
    hw_state: Arc<AtomicI32>,
    tx: Sender<RgbFrame>,
    seek_rx: Receiver<SeekReq>,
    target_dims: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    eof: Arc<AtomicBool>,
    serial_atomic: Arc<AtomicI32>,
    transform: crate::rotation::Transform,
) -> Result<()> {
    let mut ictx = crate::source::open(&path)?;
    let time_base = ictx
        .stream(video_idx)
        .ok_or_else(|| anyhow!("stream disappeared"))?
        .time_base();
    let tb_num = f64::from(time_base.numerator());
    let tb_den = f64::from(time_base.denominator());
    // start_time rebase (Twitch HLS VODs: PTS keep the broadcast
    // timestamps — starting at e.g. 62 s). Subtracted on emit and
    // added to seek targets: the player lives in 0..duration.
    let start_offset = crate::source::start_offset(&ictx);

    // Optional decoder-thread debug log (RTV_DEC_DEBUG=/path).
    let mut dbg: Option<std::io::BufWriter<std::fs::File>> =
        std::env::var("RTV_DEC_DEBUG").ok().and_then(|p| {
            std::fs::File::create(p).ok().map(std::io::BufWriter::new)
        });
    let dbg_origin = std::time::Instant::now();
    macro_rules! dbglog {
        ($($arg:tt)*) => {
            if let Some(l) = dbg.as_mut() {
                use std::io::Write as _;
                let _ = writeln!(l, "{:.4} {}", dbg_origin.elapsed().as_secs_f64(), format!($($arg)*));
                let _ = l.flush();
            }
        };
    }

    let mut scaler = Scaler::new();
    let mut frame = VideoFrame::empty();
    // Staging frame for the GPU→RAM copy-back (HW decode). Reused
    // across frames (av_frame_unref + transfer recycle it).
    let mut sw_frame = VideoFrame::empty();
    // Last emitted PTS: resume point for the mid-stream hw→sw
    // fallback (seek + drop_until, the same exact-landing mechanism
    // the refine-seek uses) — without touching the player's serials
    // or clocks.
    let mut last_emitted_pts: f64 = 0.0;
    // CONSECUTIVE send_packet errors while hw is active: past the
    // threshold the hwaccel is broken (driver gone, unsupported
    // profile mid-stream) → fall back to software.
    let mut hw_pkt_errors: u32 = 0;
    // Serial this thread believes it's processing right now. Updated
    // whenever a SeekReq arrives.
    let mut current_serial: i32 = 0;
    // Have we hit EOF? Instead of killing the thread we "park" it
    // waiting for a seek (needed for post-end seeks and --loop).
    let mut at_eof = false;
    // Drop threshold for REFINE seeks: frames with pts < drop_until
    // are not emitted (exact landing at the current playback point,
    // no jump back to the keyframe). sws is skipped for dropped
    // frames — only the decode is paid for.
    let mut drop_until: Option<f64> = None;
    let mut first_decoded_logged = true;
    let mut first_emitted_logged = true;

    'outer: loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // Process pending seeks: we keep ONLY the last one, since
        // each is absolute.
        let mut latest_seek: Option<SeekReq> = None;
        while let Ok(req) = seek_rx.try_recv() {
            latest_seek = Some(req);
        }
        if let Some(req) = latest_seek {
            current_serial = req.serial;
            // IMPORTANT (units): `Input::seek` calls
            // `avformat_seek_file(ctx, -1, min, ts, max, 0)` with
            // stream_index = -1, in which case FFmpeg interprets the
            // timestamps in AV_TIME_BASE (microseconds), NOT in the
            // stream's time_base. We used to pass stream ticks (e.g.
            // 1/15360) → the demuxer landed tens of seconds BEFORE
            // the target and drop-until-target had to decode minutes
            // of video → glacial seeks and desynced A/V. With the
            // `..ts` range the demuxer picks the optimal keyframe
            // <= target (equivalent to AVSEEK_FLAG_BACKWARD for
            // hr-seek).
            // INCLUSIVE range `..=ts`: with exclusive `..ts` the
            // max_ts ended up at ts-1 < ts and avformat_seek_file
            // returned EINVAL without moving the demuxer — the
            // "seek" only worked forward thanks to drop-until-target
            // (decoding seconds of extra video) and backwards it did
            // NOT work at all.
            let ts_target = ((req.target_secs + start_offset)
                * f64::from(ffmpeg::ffi::AV_TIME_BASE)) as i64;
            // Landing direction (mpv-style):
            //   * FORWARD: range `ts..` → avformat_seek_file with
            //     min_ts = ts → the demuxer picks the first keyframe
            //     >= target. Guarantees progress EVERY time: with the
            //     old `..=ts` range, if the GOP was longer than the
            //     seek step (YouTube AV1: >6 s GOPs), the keyframe
            //     <= target was the same on every → press and the
            //     video got stuck at that point. If it fails (target
            //     beyond the last keyframe), fall back to the
            //     keyframe <= target.
            //   * BACKWARD / refine: range `..=ts` → keyframe
            //     <= target, as usual (exact hr-seek for refine).
            let seek_res = if req.forward && !req.refine {
                ictx.seek(ts_target, ts_target..)
                    .or_else(|_| ictx.seek(ts_target, ..=ts_target))
            } else {
                ictx.seek(ts_target, ..=ts_target)
            };
            dbglog!("SEEK target={:.3} serial={} refine={} fwd={} res={:?}", req.target_secs, req.serial, req.refine, req.forward, seek_res.as_ref().err());
            decoder.flush();
            // Refine: drop up to the exact point (1 ms tolerance for
            // PTS rounding). Normal BACKWARD seek: emit from the
            // keyframe (instant mpv-style jump). FORWARD seek: also
            // drop-until — in the normal case the demuxer already
            // lands on a keyframe >= target (the first frame clears
            // the threshold at zero cost), but near the END the MP4
            // demuxer SILENTLY clamps to the last keyframe (no
            // error): without the drop-until, every → landed on that
            // same keyframe BEFORE the current position and the video
            // jumped backwards — the final stretch was impossible to
            // skip through.
            drop_until = if req.refine || req.forward {
                Some(req.target_secs)
            } else {
                None
            };
            // FAST CATCH-UP: during drop-until-target every
            // pre-target frame is dropped unseen — no need to decode
            // the NON-reference ones (non-ref B-frames):
            // skip_frame=NONREF speeds up the GOP re-decode (the dead
            // time that sank post-resize throughput to ~0.76x in the
            // rtv-vs-mpv benchmark's resize storm). Restored to
            // Default as soon as the target is reached. Decoders
            // without skip_frame support ignore it (a safe no-op).
            decoder.skip_frame(if drop_until.is_some() {
                ffmpeg::Discard::NonReference
            } else {
                ffmpeg::Discard::Default
            });
            // Keyframe seek (mpv-style): we do NOT drop frames up to
            // the target. The first decoded frame (the keyframe
            // <= target) is emitted as-is → an instant jump. The
            // player will align audio to its actual PTS.
            at_eof = false;
            eof.store(false, Ordering::Relaxed);
            first_decoded_logged = false;
            first_emitted_logged = false;
            // Emit nothing else from the old iteration.
            continue;
        }

        // Parked at EOF: sleep and re-check seeks/stop.
        if at_eof {
            thread::sleep(Duration::from_millis(25));
            continue;
        }

        let pkt = match ictx.packets().next() {
            Some(Ok((s, p))) => {
                if s.index() != video_idx {
                    continue;
                }
                p
            }
            Some(Err(e)) => {
                dbglog!("PKT_ERR {:?}", e);
                continue;
            }
            None => {
                dbglog!("EOF_DEMUX serial={}", current_serial);
                let _ = decoder.send_eof();
                drain(
                    &mut decoder,
                    &mut scaler,
                    &mut frame,
                    &hw,
                    &mut sw_frame,
                    &target_dims,
                    &tx,
                    &stop,
                    tb_num,
                    tb_den,
                    start_offset,
                    current_serial,
                    &serial_atomic,
                    &mut drop_until,
                    transform,
                );
                eof.store(true, Ordering::Relaxed);
                // We do NOT exit the thread: we park waiting for a
                // possible backward seek or the --loop restart.
                at_eof = true;
                continue 'outer;
            }
        };

        match decoder.send_packet(&pkt) {
            Ok(()) => hw_pkt_errors = 0,
            Err(_) => {
                if hw.is_some() {
                    hw_pkt_errors += 1;
                }
            }
        }

        let mut hw_transfer_failed = false;
        while decoder.receive_frame(&mut frame).is_ok() {
            if stop.load(Ordering::Relaxed) {
                break 'outer;
            }
            // If ANOTHER seek with a newer serial arrived while we
            // were decoding, drop these: they belong to the segment
            // in between.
            if serial_atomic.load(Ordering::Acquire) != current_serial {
                continue;
            }

            let pts_ticks = frame.pts().unwrap_or(0);
            let pts_secs = pts_ticks as f64 * tb_num / tb_den - start_offset;
            if !first_decoded_logged {
                first_decoded_logged = true;
                dbglog!("FIRST_DECODED pts={:.3} serial={}", pts_secs, current_serial);
            }

            // Refine in progress: drop the GOP's frames that precede
            // the current playback point (don't re-emit the past).
            if let Some(t) = drop_until {
                if pts_secs < t - 0.001 {
                    continue;
                }
                drop_until = None;
                // Target reached: go back to decoding EVERYTHING (the
                // skipped non-refs were catch-up only).
                decoder.skip_frame(ffmpeg::Discard::Default);
            }

            // GPU→RAM copy-back when the frame is a HW surface
            // (VAAPI/CUDA/…). The result (typically NV12) follows the
            // normal pipeline: sws NV12→RGB24.
            let src: &VideoFrame = match hw.as_ref() {
                Some(h) if hwdec::is_hw_frame(&frame, h) => {
                    if hwdec::transfer_to_ram(&frame, &mut sw_frame) {
                        &sw_frame
                    } else {
                        hw_transfer_failed = true;
                        break;
                    }
                }
                _ => &frame,
            };

            // Read the FRESHEST target dims right before scaling: the
            // resize applies to the very next frame (atomic
            // coalescing; the Scaler rebuilds when any input or
            // output dimension changes). With 90/270 rotation we
            // scale to the TRANSPOSED dims: after rotating, the frame
            // lands exactly on (dst_w, dst_h).
            let (dst_w, dst_h) = unpack_dims(target_dims.load(Ordering::Acquire));
            let (sc_w, sc_h) = transform.pre_rotate_dims(dst_w, dst_h);
            let mut out = match scaler.scale(src, sc_w, sc_h) {
                Some(rgb) => build_rgb_frame(rgb, pts_secs, current_serial),
                None => continue,
            };
            crate::rotation::rotate_frame(&mut out, transform);
            last_emitted_pts = pts_secs;
            if !first_emitted_logged {
                first_emitted_logged = true;
                dbglog!("FIRST_EMIT pts={:.3} serial={}", pts_secs, current_serial);
            }
            if send_with_stop(&tx, out, &stop, &serial_atomic, current_serial).is_err() {
                break 'outer;
            }
        }

        // Mid-stream HW→SW fallback: (a) the GPU→RAM transfer broke,
        // or (b) a burst of send_packet errors with hwaccel active.
        // A clean software decoder is rebuilt and decoding resumes
        // from the last emitted frame (seek + drop_until — the same
        // exact-landing mechanism as the refine-seek) WITHOUT
        // touching serials or clocks: to the player it's just a
        // decoder that took a few extra frames.
        if hw.is_some() && (hw_transfer_failed || hw_pkt_errors > 30) {
            // The reason matters for diagnostics: "transfer" = the
            // GPU→RAM copy (av_hwframe_transfer_data) failed (driver,
            // unmappable format); "packets" = the HW decoder rejects
            // the packets (codec profile/level unsupported by the
            // GPU's decode engine — e.g. AV1 on GPUs without AV1).
            hwdec::diag(format!(
                "hwdec: {} abandoned mid-stream ({}) → software",
                hw.as_ref().map(|h| h.name()).unwrap_or("?"),
                if hw_transfer_failed {
                    "GPU→RAM transfer failed".to_string()
                } else {
                    format!("{hw_pkt_errors} consecutive send_packet errors")
                }
            ));
            hw = None;
            hw_pkt_errors = 0;
            hwdec::disable_expected_fmt();
            hw_state.store(-1, Ordering::Release);
            match reopen_software(&ictx, video_idx) {
                Some(d) => {
                    decoder = d;
                    scaler = Scaler::new();
                    let ts = ((last_emitted_pts + start_offset)
                        * f64::from(ffmpeg::ffi::AV_TIME_BASE)) as i64;
                    let _ = ictx.seek(ts, ..=ts);
                    drop_until = Some(last_emitted_pts);
                    // Same fast catch-up as the refine: pre-target
                    // frames get dropped → skip decoding the
                    // non-reference ones.
                    decoder.skip_frame(ffmpeg::Discard::NonReference);
                }
                None => break 'outer,
            }
            continue 'outer;
        }
    }

    Ok(())
}

fn build_rgb_frame(rgb: &VideoFrame, pts: f64, serial: i32) -> RgbFrame {
    let stride = rgb.stride(0);
    let w = rgb.width() as usize;
    let h = rgb.height() as usize;
    let expected = w * h * 3;
    let mut buf = vec![0u8; expected];
    let src = rgb.data(0);
    for y in 0..h {
        let s = y * stride;
        let d = y * w * 3;
        let end_s = s + w * 3;
        if end_s > src.len() || d + w * 3 > buf.len() {
            break;
        }
        buf[d..d + w * 3].copy_from_slice(&src[s..end_s]);
    }
    RgbFrame {
        width: rgb.width(),
        height: rgb.height(),
        pts,
        serial,
        data: buf,
    }
}

/// Sends a frame while honouring `stop`, and bails if the serial
/// changes while we wait for channel space (that frame would already
/// be a leftover).
fn send_with_stop(
    tx: &Sender<RgbFrame>,
    mut frame: RgbFrame,
    stop: &Arc<AtomicBool>,
    serial_atomic: &Arc<AtomicI32>,
    my_serial: i32,
) -> std::result::Result<(), ()> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(());
        }
        if serial_atomic.load(Ordering::Acquire) != my_serial {
            // Our serial is already stale. Drop it and return to the
            // main loop (Ok — we don't want to abort the whole thread).
            return Ok(());
        }
        match tx.try_send(frame) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(f)) => {
                frame = f;
                thread::sleep(Duration::from_millis(2));
            }
            Err(TrySendError::Disconnected(_)) => return Err(()),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn drain(
    decoder: &mut ffmpeg::decoder::Video,
    scaler: &mut Scaler,
    frame: &mut VideoFrame,
    hw: &Option<ActiveHw>,
    sw_frame: &mut VideoFrame,
    target_dims: &Arc<AtomicU64>,
    tx: &Sender<RgbFrame>,
    stop: &Arc<AtomicBool>,
    tb_num: f64,
    tb_den: f64,
    start_offset: f64,
    current_serial: i32,
    serial_atomic: &Arc<AtomicI32>,
    drop_until: &mut Option<f64>,
    transform: crate::rotation::Transform,
) {
    while decoder.receive_frame(frame).is_ok() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if serial_atomic.load(Ordering::Acquire) != current_serial {
            continue;
        }
        let pts_ticks = frame.pts().unwrap_or(0);
        let pts_secs = pts_ticks as f64 * tb_num / tb_den - start_offset;
        if let Some(t) = *drop_until {
            if pts_secs < t - 0.001 {
                continue;
            }
            *drop_until = None;
            decoder.skip_frame(ffmpeg::Discard::Default);
        }
        // GPU→RAM copy-back applies to the final flush too. If the
        // transfer fails here there's no possible recovery (this is
        // the EOF drain): drop the frame and keep going.
        let src: &VideoFrame = match hw.as_ref() {
            Some(h) if hwdec::is_hw_frame(frame, h) => {
                if hwdec::transfer_to_ram(frame, sw_frame) {
                    &*sw_frame
                } else {
                    continue;
                }
            }
            _ => &*frame,
        };
        let (dst_w, dst_h) = unpack_dims(target_dims.load(Ordering::Acquire));
        let (sc_w, sc_h) = transform.pre_rotate_dims(dst_w, dst_h);
        let mut out = match scaler.scale(src, sc_w, sc_h) {
            Some(rgb) => build_rgb_frame(rgb, pts_secs, current_serial),
            None => continue,
        };
        crate::rotation::rotate_frame(&mut out, transform);
        if send_with_stop(tx, out, stop, serial_atomic, current_serial).is_err() {
            break;
        }
    }
}

// ---------------------------------------------------------------
// Audio-only — visualization generator
// ---------------------------------------------------------------

/// Synthetic pipeline for files WITHOUT a video stream (mp3, flac,
/// ogg, m4a, wav…). A thread generates RGB frames at 30 fps with
/// 0-based PTS: a procedural visualization (spectrum-style bars +
/// waveform) that's a pure function of time — no FFT and no access
/// to the samples (audio lives on its own thread with its own
/// context; sharing them would require a side channel that adds
/// nothing to the goal: giving the player frames to show and a clock
/// to obey).
///
/// The contract with the player is EXACTLY the real decoder's:
///   * frames with increasing `pts` and the current `serial`,
///   * `seek()`/`seek_dir()` respond by jumping the base PTS,
///   * `resize()` changes the frame dims,
///   * `eof` flips on past the duration (and a seek flips it off),
///   * `stop()` stops the thread.
/// Pacing comes from the bounded(8) channel: when full, the thread
/// sleeps in send_with_stop. Audio remains the master clock: the
/// player shows whichever synthetic frame's PTS is due — pause,
/// seeks and the HUD work without touching ANYTHING outside this
/// module.
fn spawn_audio_only(
    ictx: &ffmpeg::format::context::Input,
    dst_w: u32,
    dst_h: u32,
) -> Result<DecoderHandle> {
    let duration = if ictx.duration() > 0 {
        ictx.duration() as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE)
    } else {
        f64::NAN // no duration (radio stream) → is_live in the player
    };
    const FPS: f64 = 30.0;

    let (tx, rx) = bounded::<RgbFrame>(8);
    let (seek_tx, seek_rx) = unbounded::<SeekReq>();
    let target_dims = Arc::new(AtomicU64::new(pack_dims(dst_w.max(2), dst_h.max(2))));
    let stop = Arc::new(AtomicBool::new(false));
    let eof = Arc::new(AtomicBool::new(false));
    let serial = Arc::new(AtomicI32::new(0));
    let hw_state = Arc::new(AtomicI32::new(-1));

    let stop_th = stop.clone();
    let eof_th = eof.clone();
    let serial_th = serial.clone();
    let dims_th = target_dims.clone();
    let dur_th = duration;

    let join = thread::Builder::new()
        .name("rtv-audio-viz".into())
        .spawn(move || {
            let mut current_serial: i32 = 0;
            let mut t: f64 = 0.0; // PTS of the next frame
            let frame_dt = 1.0 / FPS;
            loop {
                if stop_th.load(Ordering::Relaxed) {
                    break;
                }
                // Seeks: keep only the last one (they're absolute).
                let mut latest: Option<SeekReq> = None;
                while let Ok(req) = seek_rx.try_recv() {
                    latest = Some(req);
                }
                if let Some(req) = latest {
                    current_serial = req.serial;
                    t = req.target_secs.max(0.0);
                    if dur_th.is_finite() {
                        t = t.min(dur_th);
                    }
                    eof_th.store(false, Ordering::Relaxed);
                }
                if serial_th.load(Ordering::Acquire) != current_serial {
                    // Serial bumped but the SeekReq hasn't arrived yet.
                    thread::sleep(Duration::from_millis(2));
                    continue;
                }
                if dur_th.is_finite() && t > dur_th + 0.5 {
                    eof_th.store(true, Ordering::Relaxed);
                    thread::sleep(Duration::from_millis(25));
                    continue;
                }
                let (w, h) = unpack_dims(dims_th.load(Ordering::Acquire));
                let frame = viz_frame(w, h, t, current_serial);
                if send_with_stop(&tx, frame, &stop_th, &serial_th, current_serial).is_err() {
                    break;
                }
                t += frame_dt;
            }
            eof_th.store(true, Ordering::Relaxed);
        })?;

    Ok(DecoderHandle {
        rx,
        duration,
        // Nominal 16:9: the player/GUI aspect fit starts from here.
        source_size: (1280, 720),
        fps: FPS,
        eof,
        hw_state,
        seek_tx,
        target_dims,
        serial,
        stop,
        join: Some(join),
    })
}

/// Procedural visualization frame: vertical spectrum-style bars
/// (heights modulated by mutually incoherent sines — they look
/// "alive" without analysing the audio) over a gradient background,
/// plus a waveform up top. Deterministic in (w, h, t): the same
/// instant produces the same frame (seeks "land" on a stable image,
/// not on noise).
fn viz_frame(w: u32, h: u32, t: f64, serial: i32) -> RgbFrame {
    let w = w.max(2) as usize;
    let h = h.max(2) as usize;
    let mut data = vec![0u8; w * h * 3];

    // Background: vertical midnight-blue gradient.
    for y in 0..h {
        let g = 12 + (18.0 * y as f64 / h as f64) as u8;
        for x in 0..w {
            let d = (y * w + x) * 3;
            data[d] = 8;
            data[d + 1] = g / 2;
            data[d + 2] = g;
        }
    }

    // Bars: count scales with width; height = per-bar sine blend.
    let nbars = (w / 6).clamp(8, 64);
    let bar_w = (w / nbars).max(1);
    for b in 0..nbars {
        let fb = b as f64;
        // Distinct phase and frequency per bar → organic motion.
        let a = (t * (1.3 + 0.37 * fb).sin().abs() * 6.0 + fb * 0.7).sin();
        let c = (t * 2.1 + fb * 1.9).sin();
        let lvl = (0.25 + 0.75 * (0.5 * a + 0.5 * c).abs()).min(1.0);
        let bh = ((h as f64 * 0.55) * lvl) as usize;
        let x0 = b * bar_w;
        let x1 = (x0 + bar_w.saturating_sub(1)).min(w);
        for y in (h - bh.min(h))..h {
            // Colour: green (bottom) to amber (top) gradient.
            let fy = (h - y) as f64 / h as f64;
            let r = (60.0 + 195.0 * fy) as u8;
            let g = (200.0 - 60.0 * fy) as u8;
            for x in x0..x1 {
                let d = (y * w + x) * 3;
                data[d] = r;
                data[d + 1] = g;
                data[d + 2] = 40;
            }
        }
    }

    // Top waveform: compound sine drifting with t.
    let mid = h / 4;
    let amp = (h as f64) * 0.08;
    for x in 0..w {
        let fx = x as f64 / w as f64;
        let yv = (fx * 12.0 + t * 3.0).sin() * 0.6 + (fx * 5.0 - t * 1.7).sin() * 0.4;
        let y = (mid as f64 + yv * amp) as usize;
        if y + 1 < h {
            for yy in y..=y + 1 {
                let d = (yy * w + x) * 3;
                data[d] = 120;
                data[d + 1] = 200;
                data[d + 2] = 255;
            }
        }
    }

    RgbFrame {
        width: w as u32,
        height: h as u32,
        pts: t,
        serial,
        data,
    }
}
