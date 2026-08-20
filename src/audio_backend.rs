//! audio_backend.rs — the feeder shared by the audio sinks + the
//! PulseAudio backend (Termux / Linux fallback).
//!
//! The heart of the audio clock (consuming the AudioChunk ring,
//! serial-based discard, latency EMA, rate limiter) used to live
//! inside the cpal callback closure. To support Termux (where cpal
//! doesn't work: its AAudio backend needs the NDK and an Android app
//! context) it's extracted here as `SinkFeeder`, shared by BOTH
//! backends:
//!
//!   * cpal  (audio.rs): the callback calls `feeder.fill(out, delay)`.
//!   * pulse (here): a writer thread calls `feeder.fill(buf, delay)`
//!     and blocks in `pa_simple_write`.
//!
//! The pulse backend loads libpulse-simple with dlopen (libloading) at
//! runtime: ZERO build or startup dependency — if the lib or the
//! server is missing, `PulseSink::try_open` returns Err and the caller
//! degrades (to cpal or to no_audio). On Termux real audio goes
//! through PulseAudio (`pkg install pulseaudio` + `pulseaudio
//! --start`).

use crossbeam_channel::Receiver;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use crate::clock::FfClock;

/// A block of samples with a serial + the PTS of its first sample.
/// (Moved out of audio.rs so both backends can share it.)
pub struct AudioChunk {
    pub samples: Vec<f32>,
    /// Serial it was produced under. The player bumps the serial on
    /// every seek → chunks with an old serial are leftovers.
    pub serial: i32,
    /// PTS (seconds) of the chunk's first sample.
    pub first_pts: f64,
}

/// Persistent state of the audio ring consumer. ONE instance per
/// output stream, owned by the backend's callback/thread.
///
/// `fill()` reproduces EXACTLY the semantics of the original cpal
/// callback: instant discard of chunks with an old serial, silence on
/// underrun, EMA over the reported latency, a 0.5 s clamp, a floor of
/// one buffer period, a ×1.02 rate limiter with dt=0 when callbacks
/// stalled for >250 ms, and a single `set_pts` per call carrying the
/// PTS that is BEING HEARD right now.
pub struct SinkFeeder {
    stop: Arc<AtomicBool>,
    /// STARTUP gate: while it's closed (false), `fill()` emits silence
    /// without consuming the ring or anchoring the clock. The player
    /// opens it when the FIRST video frame is shown (with a time-based
    /// valve in case video never arrives). Without this, on network
    /// inputs (yt-dlp) audio would start and anchor the master clock
    /// seconds before the video decoder (open+probe of the CDN URL)
    /// produced anything → you'd hear sound over an empty screen and
    /// the video showed up "late".
    gate: Arc<AtomicBool>,
    clock: Arc<FfClock>,
    volume: Arc<AtomicU8>,
    samples_rx: Receiver<AudioChunk>,
    out_sample_rate: u32,
    out_channels: u16,

    // --- state of the chunk in flight ---
    leftover: Vec<f32>,
    leftover_offset: usize,
    leftover_serial: i32,
    leftover_first_pts: f64,
    samples_emitted_in_chunk: usize,

    // --- latency estimation + rate limiter ---
    latency_ema: f64,
    rate_lim: Option<(f64, std::time::Instant)>,
    rate_lim_serial: i32,

    // --- optional debug log (RTV_AUDIO_DEBUG=/path) ---
    dbg_log: Option<std::io::BufWriter<std::fs::File>>,
    dbg_origin: std::time::Instant,
    dbg_count: u64,
}

impl SinkFeeder {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stop: Arc<AtomicBool>,
        gate: Arc<AtomicBool>,
        clock: Arc<FfClock>,
        volume: Arc<AtomicU8>,
        samples_rx: Receiver<AudioChunk>,
        out_sample_rate: u32,
        out_channels: u16,
    ) -> Self {
        let dbg_log = std::env::var("RTV_AUDIO_DEBUG")
            .ok()
            .and_then(|p| std::fs::File::create(p).ok().map(std::io::BufWriter::new));
        SinkFeeder {
            stop,
            gate,
            clock,
            volume,
            samples_rx,
            out_sample_rate,
            out_channels,
            leftover: Vec::new(),
            leftover_offset: 0,
            leftover_serial: 0,
            leftover_first_pts: 0.0,
            samples_emitted_in_chunk: 0,
            latency_ema: 0.0,
            rate_lim: None,
            rate_lim_serial: i32::MIN,
            dbg_log,
            dbg_origin: std::time::Instant::now(),
            dbg_count: 0,
        }
    }

    /// Fills `out` (interleaved f32) from the ring and updates the
    /// audio clock. `reported_delay_secs`: the output latency reported
    /// by the backend (cpal: playback−callback; pulse:
    /// pa_simple_get_latency). It may be 0 — a floor of one buffer
    /// period is applied.
    ///
    /// Returns `true` if valid audio was emitted (the clock anchored).
    pub fn fill(&mut self, out: &mut [f32], reported_delay_secs: f64) -> bool {
        // ---- Silent output on stop / pause ----
        if self.stop.load(Ordering::Relaxed) {
            out.fill(0.0);
            return false;
        }
        if self.clock.paused.load(Ordering::Acquire) != 0 {
            out.fill(0.0);
            return false;
        }
        // ---- Startup gate closed: silence WITHOUT consuming ----
        // Don't touch the ring: the chunks sit waiting and audio
        // starts from the exact beginning once the player opens the
        // gate (first video frame on screen). The clock doesn't anchor
        // either → the player's startup hold stays frozen, so picture
        // and sound are born together.
        if !self.gate.load(Ordering::Acquire) {
            out.fill(0.0);
            return false;
        }
        let vol_pct = self.volume.load(Ordering::Relaxed) as f32 / 100.0;
        // The serial that's valid NOW (the single serial shared by
        // clock and pipeline). Read once up front: if a seek lands
        // mid-call, the final `set_pts` will be rejected by the
        // clock's serial guard.
        let current_serial = self.clock.current_serial();

        let mut filled = 0usize;
        // PTS of the FIRST valid sample emitted in this call, and its
        // offset (in per-channel frames) inside `out`.
        let mut first_pts_emitted: Option<(f64, usize)> = None;

        while filled < out.len() {
            // Current chunk carries an old serial → DISCARD INSTANTLY
            // (without "playing" its duration as silence).
            if self.leftover_offset < self.leftover.len()
                && self.leftover_serial != current_serial
            {
                self.leftover_offset = self.leftover.len();
                continue;
            }
            // Chunk exhausted → fetch the next one.
            if self.leftover_offset >= self.leftover.len() {
                match self.samples_rx.try_recv() {
                    Ok(chunk) => {
                        self.leftover = chunk.samples;
                        self.leftover_offset = 0;
                        self.leftover_serial = chunk.serial;
                        self.leftover_first_pts = chunk.first_pts;
                        self.samples_emitted_in_chunk = 0;
                    }
                    Err(_) => {
                        // Underrun: silence.
                        out[filled..].fill(0.0);
                        break;
                    }
                }
                continue;
            }
            // Valid emission.
            if first_pts_emitted.is_none() {
                let pts_here = self.leftover_first_pts
                    + self.samples_emitted_in_chunk as f64 / self.out_sample_rate as f64;
                first_pts_emitted = Some((pts_here, filled / self.out_channels as usize));
            }
            let take = (out.len() - filled).min(self.leftover.len() - self.leftover_offset);
            for i in 0..take {
                out[filled + i] = self.leftover[self.leftover_offset + i] * vol_pct;
            }
            filled += take;
            self.leftover_offset += take;
            self.samples_emitted_in_chunk += take / self.out_channels as usize;
        }

        // ---- Update audclk with LATENCY COMPENSATION ----
        let Some((pts_first, frame_offset)) = first_pts_emitted else {
            return false;
        };
        // Some backends (ALSA→Pulse, null sinks, certain drivers)
        // report delay=0 even though the buffer we're filling will NOT
        // sound until the current period drains. Robust minimum
        // estimate: the duration of ONE buffer period (ffplay does the
        // same with audio_hw_buf_size).
        let buf_period_secs =
            (out.len() / self.out_channels as usize) as f64 / self.out_sample_rate as f64;
        // Upper clamp: after an underrun PulseAudio can report absurd
        // delays (>1 s) — without a cap, the audio clock would jump
        // SECONDS backwards.
        let raw_delay = reported_delay_secs.max(buf_period_secs).min(0.5);
        if self.latency_ema == 0.0 {
            self.latency_ema = raw_delay;
        } else {
            self.latency_ema = 0.9 * self.latency_ema + 0.1 * raw_delay;
        }
        let delay_secs = self.latency_ema;
        let offset_secs = frame_offset as f64 / self.out_sample_rate as f64;
        let mut pts_being_heard = (pts_first - offset_secs - delay_secs).max(0.0);
        // ---- Rate limiter: the "being heard" PTS can't advance
        // faster than wall time (×1.02). On connect, Pulse swallows
        // ~0.4 s AT ONCE for its prebuffer while reporting delay=0;
        // without this the clock jumped +0.4 s and decode-bound video
        // stayed behind forever. dt=0 when callbacks stalled >250 ms
        // (the DAC consumed nothing during the gap). ----
        let now_i = std::time::Instant::now();
        if self.rate_lim_serial != current_serial {
            self.rate_lim_serial = current_serial;
            self.rate_lim = None;
        }
        if let Some((prev_pts, prev_wall)) = self.rate_lim {
            let raw_dt = now_i.duration_since(prev_wall).as_secs_f64();
            let dt = if raw_dt > 0.25 { 0.0 } else { raw_dt };
            let cap = prev_pts + dt * 1.02;
            if pts_being_heard > cap {
                pts_being_heard = cap;
            }
        }
        self.rate_lim = Some((pts_being_heard, now_i));
        self.clock.set_pts(pts_being_heard, current_serial);
        if let Some(log) = self.dbg_log.as_mut() {
            use std::io::Write as _;
            self.dbg_count += 1;
            let _ = writeln!(
                log,
                "{:.4} cb#{} buf={} pts_first={:.4} rep_delay={:.4} set={:.4}",
                self.dbg_origin.elapsed().as_secs_f64(),
                self.dbg_count,
                out.len(),
                pts_first,
                reported_delay_secs,
                pts_being_heard,
            );
        }
        true
    }
}

// ============================================================
// PulseAudio backend (`pulse` feature) — libpulse-simple via dlopen.
// ============================================================
//
// PulseAudio's simple API: one blocking connection per stream.
// `pa_simple_write` blocks until the bytes land in the server's
// buffer → the write itself sets the pace, just like the cpal
// callback. Real latency comes from `pa_simple_get_latency` (µs) and
// is handed to the feeder.
//
// dlopen at runtime (libloading): the binary does NOT link libpulse —
// on systems without PulseAudio (or without a running server)
// `try_open` returns Err and the caller tries the next backend. On
// Termux: `pkg install pulseaudio && pulseaudio --start` and it works.
#[cfg(feature = "pulse")]
pub mod pulse {
    use super::{AudioChunk, SinkFeeder};
    use crate::clock::FfClock;
    use anyhow::{anyhow, Result};
    use crossbeam_channel::Receiver;
    use std::ffi::{c_char, c_int, c_void, CString};
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    // --- libpulse-simple ABI (stable for ~15 years) ---
    const PA_STREAM_PLAYBACK: c_int = 1;
    const PA_SAMPLE_FLOAT32LE: c_int = 5;

    #[repr(C)]
    struct PaSampleSpec {
        format: c_int,
        rate: u32,
        channels: u8,
    }

    #[repr(C)]
    struct PaBufferAttr {
        maxlength: u32,
        tlength: u32,
        prebuf: u32,
        minreq: u32,
        fragsize: u32,
    }

    type PaSimpleNew = unsafe extern "C" fn(
        server: *const c_char,
        name: *const c_char,
        dir: c_int,
        dev: *const c_char,
        stream_name: *const c_char,
        ss: *const PaSampleSpec,
        map: *const c_void,
        attr: *const PaBufferAttr,
        error: *mut c_int,
    ) -> *mut c_void;
    type PaSimpleWrite = unsafe extern "C" fn(
        s: *mut c_void,
        data: *const c_void,
        bytes: usize,
        error: *mut c_int,
    ) -> c_int;
    type PaSimpleGetLatency = unsafe extern "C" fn(s: *mut c_void, error: *mut c_int) -> u64;
    type PaSimpleFree = unsafe extern "C" fn(s: *mut c_void);

    struct PulseFns {
        write: PaSimpleWrite,
        get_latency: PaSimpleGetLatency,
        free: PaSimpleFree,
    }

    /// An open, ready PulseAudio connection (no writer thread yet).
    pub struct PulseSink {
        pa: *mut c_void,
        fns: PulseFns,
        /// The Library must outlive any use of the fn pointers.
        _lib: libloading::Library,
        pub sample_rate: u32,
        pub channels: u16,
    }
    // The pa_simple pointer is only used by the writer thread (the
    // simple API isn't thread-safe, but there's a SINGLE user here).
    unsafe impl Send for PulseSink {}

    impl PulseSink {
        /// Tries dlopen(libpulse-simple) + connecting to the server.
        /// Fails fast and clean when the lib or the server is missing.
        pub fn try_open(sample_rate: u32, channels: u16) -> Result<PulseSink> {
            let lib = ["libpulse-simple.so.0", "libpulse-simple.so"]
                .iter()
                .find_map(|n| unsafe { libloading::Library::new(n).ok() })
                .ok_or_else(|| anyhow!("libpulse-simple not found"))?;

            let (new_fn, fns) = unsafe {
                let new_fn: PaSimpleNew = *lib
                    .get::<PaSimpleNew>(b"pa_simple_new\0")
                    .map_err(|e| anyhow!("pa_simple_new: {e}"))?;
                let write: PaSimpleWrite = *lib
                    .get::<PaSimpleWrite>(b"pa_simple_write\0")
                    .map_err(|e| anyhow!("pa_simple_write: {e}"))?;
                let get_latency: PaSimpleGetLatency = *lib
                    .get::<PaSimpleGetLatency>(b"pa_simple_get_latency\0")
                    .map_err(|e| anyhow!("pa_simple_get_latency: {e}"))?;
                let free: PaSimpleFree = *lib
                    .get::<PaSimpleFree>(b"pa_simple_free\0")
                    .map_err(|e| anyhow!("pa_simple_free: {e}"))?;
                (new_fn, PulseFns { write, get_latency, free })
            };

            let ss = PaSampleSpec {
                format: PA_SAMPLE_FLOAT32LE,
                rate: sample_rate,
                channels: channels as u8,
            };
            // tlength ~100 ms: contained latency without underrun risk
            // on mobile devices; everything else at defaults.
            let bytes_per_sec = sample_rate * channels as u32 * 4;
            let attr = PaBufferAttr {
                maxlength: u32::MAX,
                tlength: bytes_per_sec / 10,
                prebuf: u32::MAX,
                minreq: u32::MAX,
                fragsize: u32::MAX,
            };
            let app = CString::new("rtv").unwrap();
            let stream = CString::new("playback").unwrap();
            let mut err: c_int = 0;
            let pa = unsafe {
                new_fn(
                    std::ptr::null(),
                    app.as_ptr(),
                    PA_STREAM_PLAYBACK,
                    std::ptr::null(),
                    stream.as_ptr(),
                    &ss,
                    std::ptr::null(),
                    &attr,
                    &mut err,
                )
            };
            if pa.is_null() {
                return Err(anyhow!(
                    "pa_simple_new failed (err={err}) — is the PulseAudio server running?"
                ));
            }
            Ok(PulseSink {
                pa,
                fns,
                _lib: lib,
                sample_rate,
                channels,
            })
        }

        /// Starts the writer thread: consumes the ring through the
        /// feeder and blocks in pa_simple_write (natural pacing).
        pub fn start(
            self,
            stop: Arc<AtomicBool>,
            gate: Arc<AtomicBool>,
            clock: Arc<FfClock>,
            volume: Arc<AtomicU8>,
            samples_rx: Receiver<AudioChunk>,
        ) -> PulseRuntime {
            let rate = self.sample_rate;
            let ch = self.channels;
            let stop_thread = stop.clone();
            let join = thread::Builder::new()
                .name("rtv-pulse-writer".into())
                .spawn(move || {
                    let sink = self; // move the connection into the thread
                    let mut feeder = SinkFeeder::new(
                        stop_thread.clone(),
                        gate,
                        clock,
                        volume,
                        samples_rx,
                        rate,
                        ch,
                    );
                    // 20 ms blocks (same order of magnitude as a cpal
                    // callback).
                    let frames = (rate / 50).max(64) as usize;
                    let mut buf = vec![0f32; frames * ch as usize];
                    loop {
                        if stop_thread.load(Ordering::Relaxed) {
                            break;
                        }
                        let mut err: c_int = 0;
                        let lat_us = unsafe { (sink.fns.get_latency)(sink.pa, &mut err) };
                        let lat = if lat_us == u64::MAX {
                            0.0
                        } else {
                            lat_us as f64 / 1e6
                        };
                        feeder.fill(&mut buf, lat);
                        let r = unsafe {
                            (sink.fns.write)(
                                sink.pa,
                                buf.as_ptr() as *const c_void,
                                buf.len() * 4,
                                &mut err,
                            )
                        };
                        if r < 0 {
                            // Server went down: bail out (the clock
                            // stops advancing → staleness freezes it
                            // and video waits instead of running free).
                            break;
                        }
                    }
                    unsafe { (sink.fns.free)(sink.pa) };
                })
                .ok();
            PulseRuntime { join }
        }
    }

    /// A running writer thread. `stop()` does a bounded join (the
    /// write blocks for at most ~tlength=100 ms) + detach as a last
    /// resort — mirroring the DecoderHandle::stop pattern.
    pub struct PulseRuntime {
        join: Option<thread::JoinHandle<()>>,
    }

    impl PulseRuntime {
        pub fn stop(&mut self) {
            if let Some(j) = self.join.take() {
                let deadline = std::time::Instant::now() + Duration::from_millis(500);
                loop {
                    if j.is_finished() {
                        let _ = j.join();
                        return;
                    }
                    if std::time::Instant::now() >= deadline {
                        drop(j);
                        return;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
}
