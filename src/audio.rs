//! audio.rs — audio pipeline with an ffplay-style audio clock.
//!
//! Key design points:
//!   * The cpal callback does NOT call `advance()` accumulating µs
//!     sample by sample. Instead, each time it consumes a block of
//!     samples from the ring it makes a SINGLE `audclk.set_pts()`
//!     with the PTS of the last emitted sample MINUS the estimated
//!     `playback_delay` (samples still sitting in the driver buffer
//!     plus the block it just emitted). That `pts` reflects what the
//!     user is actually hearing in real time.
//!   * The audio decoder tags every `AudioChunk` with
//!     `(serial, first_pts)`, where `first_pts` is the PTS of the
//!     first sample. The callback uses it to compute the running PTS.
//!   * Serial: on seek, the player bumps `audclk.serial` BEFORE
//!     queuing the seek to the decoder. Chunks with a stale serial
//!     still in the ring are muted and do NOT update the clock.
//!   * Pause: `cpal::Stream::pause()` at the OS level
//!     (WASAPI/ALSA/CoreAudio). The callback also zero-fills as a
//!     second line of defense.

use anyhow::{anyhow, Context, Result};
#[cfg(feature = "cpal-audio")]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender, TrySendError};
use ffmpeg_the_third as ffmpeg;
use ffmpeg::format::sample::Type as SampleType;
use ffmpeg::format::Sample as SampleFormat;
use ffmpeg::media::Type as MediaType;
use ffmpeg::software::resampling::context::Context as SwrCtx;
use ffmpeg::util::frame::audio::Audio as AudioFrame;
use ffmpeg::{ChannelLayout, ChannelLayoutMask};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::audio_backend::AudioChunk;
#[cfg(feature = "cpal-audio")]
use crate::audio_backend::SinkFeeder;
use crate::clock::FfClock;

/// Control messages for the audio-decoder thread.
enum AudioMsg {
    /// Seek: target position plus the new serial.
    Seek { target_secs: f64, serial: i32 },
    /// Runtime track switch: container stream plus the current
    /// playback position. The thread reopens the decoder on the new
    /// stream, rebuilds the resampler and lands at `at_secs` with a
    /// sample-accurate trim — same mechanism as a seek.
    Switch { stream_index: usize, at_secs: f64, serial: i32 },
}

pub struct AudioHandle {
    stop: Arc<AtomicBool>,
    /// Sink startup gate (see `SinkFeeder::gate`). Starts CLOSED;
    /// the player opens it with `open_gate()` when the first video
    /// frame is shown.
    gate: Arc<AtomicBool>,
    volume: Arc<AtomicU8>,
    msg_tx: Sender<AudioMsg>,
    pub clock: Arc<FfClock>,
    pub has_audio: bool,
    #[allow(dead_code)] // informational API (diagnostics / future use)
    pub sample_rate: u32,
    #[allow(dead_code)]
    pub channels: u16,
    /// Index of the audio stream the pipeline STARTED with (best, or
    /// the one requested via --aid/--alang). Informational: the
    /// player uses it to position the track-cycling order.
    pub track_index: Option<usize>,
    decoder_join: Option<thread::JoinHandle<()>>,
    sink: Option<SinkRuntime>,
    /// Active backend ("cpal" | "pulse" | "none") — verbose/diagnostics.
    pub backend_name: &'static str,
}

/// Runtime handle for the active output sink.
enum SinkRuntime {
    #[cfg(feature = "cpal-audio")]
    Cpal(cpal::Stream),
    #[cfg(feature = "pulse")]
    Pulse(crate::audio_backend::pulse::PulseRuntime),
}

impl AudioHandle {
    pub fn set_volume(&self, v: i32) {
        let clamped = v.clamp(0, 200) as u8;
        self.volume.store(clamped, Ordering::Relaxed);
    }

    /// Opens the startup gate: the sink begins emitting real audio
    /// (and anchoring the clock). Idempotent. The player calls it
    /// when the FIRST video frame is shown — this way, with network
    /// inputs (yt-dlp), no sound plays over a still-blank screen
    /// while the video decoder is opening/probing the CDN URL.
    pub fn open_gate(&self) {
        self.gate.store(true, Ordering::Release);
    }

    /// Queues a seek to the audio-decoder thread. IMPORTANT: the
    /// order on the player side is:
    ///   (1) `master.set(t)` — bumps audclk.serial + vidclk.serial.
    ///   (2) `audio.seek(t)` — the audio-decoder thread flushes and jumps.
    ///   (3) `decoder.seek(t)` — the video-decoder thread flushes and jumps.
    /// Since the serial is already bumped in (1), any stale chunk
    /// leaving the ring during (2)/(3) is muted by the callback.
    pub fn seek(&self, target_secs: f64) {
        // The reference serial is the CLOCK's, which the player just
        // bumped with `master.set(target)` BEFORE calling us. This
        // way the audio pipeline shares exactly the same serial as
        // the clock: pre-seek chunks (stale serial) are discarded in
        // the callback and post-seek chunks (new serial) anchor it.
        let serial = self.clock.current_serial();
        // Unbounded channel: a try_send on a bounded channel could
        // DROP the last seek of a burst (→→→←←) and leave the audio
        // landed at a different target than the video — a constant
        // ±5 s A/V offset after the burst.
        let _ = self.msg_tx.send(AudioMsg::Seek {
            target_secs,
            serial,
        });
    }

    /// Switches the audio track LIVE. The player must have bumped
    /// the serials (`master.set(now)`) BEFORE calling — just like
    /// with `seek` — so that chunks from the old track still in the
    /// ring get muted and never touch the clock.
    pub fn switch_track(&self, stream_index: usize, at_secs: f64) {
        let serial = self.clock.current_serial();
        let _ = self.msg_tx.send(AudioMsg::Switch {
            stream_index,
            at_secs,
            serial,
        });
    }

    pub fn pause_stream(&self) {
        match self.sink.as_ref() {
            #[cfg(feature = "cpal-audio")]
            Some(SinkRuntime::Cpal(s)) => {
                if let Err(e) = s.pause() {
                    eprintln_verbose(&format!("cpal pause failed: {e}"));
                }
            }
            // pulse: the simple API has no native pause — the feeder
            // emits silence while clock.paused is set (same second
            // line of defense the cpal callback already had).
            _ => {}
        }
    }
    pub fn play_stream(&self) {
        match self.sink.as_ref() {
            #[cfg(feature = "cpal-audio")]
            Some(SinkRuntime::Cpal(s)) => {
                if let Err(e) = s.play() {
                    eprintln_verbose(&format!("cpal play failed: {e}"));
                }
            }
            _ => {}
        }
    }

    /// Cooperative stop with a BOUNDED join (500 ms), mirroring
    /// `DecoderHandle::stop`. The audio-decoder thread may be:
    ///   * sleeping in the `send_with_stop` backoff (exits within
    ///     <4 ms once it sees the flag), or
    ///   * blocked inside FFmpeg (send_packet/receive_frame) or on a
    ///     full channel whose consumer (the cpal callback) no longer
    ///     drains because the stream was paused/stopped — not
    ///     recoverable via the flag. In that case it gets detached:
    ///     the process is exiting and the OS reaps the thread. We
    ///     never hang on shutdown.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // The pulse writer blocks in pa_simple_write for at most
        // ~tlength (100 ms): it has its own bounded join inside stop().
        #[cfg(feature = "pulse")]
        if let Some(SinkRuntime::Pulse(rt)) = self.sink.as_mut() {
            rt.stop();
        }
        if let Some(j) = self.decoder_join.take() {
            let deadline = std::time::Instant::now() + Duration::from_millis(500);
            loop {
                if j.is_finished() {
                    let _ = j.join();
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    drop(j); // last-resort detach
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

impl Drop for AudioHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Output backend preference (--audio-backend).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BackendPref {
    /// Termux/Android: pulse→cpal; elsewhere: cpal→pulse.
    Auto,
    Cpal,
    Pulse,
    /// No audio (same as --no-audio).
    NoAudio,
}

impl BackendPref {
    pub fn parse(s: &str) -> Result<BackendPref> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(BackendPref::Auto),
            "cpal" => Ok(BackendPref::Cpal),
            "pulse" | "pulseaudio" => Ok(BackendPref::Pulse),
            "none" | "off" => Ok(BackendPref::NoAudio),
            other => Err(anyhow!(
                "invalid --audio-backend: {other:?} (accepted: auto|cpal|pulse|none)"
            )),
        }
    }
}

/// Are we running inside Termux? (Android app with its own prefix).
fn is_termux() -> bool {
    std::env::var_os("TERMUX_VERSION").is_some()
        || std::env::var("PREFIX")
            .map(|p| p.contains("com.termux"))
            .unwrap_or(false)
}

/// Chosen sink plan (connection opened, not yet started).
enum SinkPlan {
    #[cfg(feature = "cpal-audio")]
    Cpal(cpal::Device, cpal::StreamConfig),
    #[cfg(feature = "pulse")]
    Pulse(crate::audio_backend::pulse::PulseSink),
}

fn try_cpal_plan(out_channels: u16) -> Option<(SinkPlan, u32)> {
    #[cfg(feature = "cpal-audio")]
    {
        let host = cpal::default_host();
        let device = host.default_output_device()?;
        let supported = match device.default_output_config() {
            Ok(s) => s,
            Err(e) => {
                eprintln_verbose(&format!("cpal default_output_config failed: {e}"));
                return None;
            }
        };
        let rate = supported.sample_rate().0;
        let config = cpal::StreamConfig {
            channels: out_channels,
            sample_rate: cpal::SampleRate(rate),
            buffer_size: cpal::BufferSize::Default,
        };
        Some((SinkPlan::Cpal(device, config), rate))
    }
    #[cfg(not(feature = "cpal-audio"))]
    {
        let _ = out_channels;
        None
    }
}

fn try_pulse_plan(out_channels: u16) -> Option<(SinkPlan, u32)> {
    #[cfg(feature = "pulse")]
    {
        // 48 kHz: PulseAudio's native rate on Android/Termux and the
        // de facto standard; swresample normalizes every track to it.
        match crate::audio_backend::pulse::PulseSink::try_open(48000, out_channels) {
            Ok(s) => Some((SinkPlan::Pulse(s), 48000)),
            Err(e) => {
                eprintln_verbose(&format!("pulse unavailable: {e}"));
                None
            }
        }
    }
    #[cfg(not(feature = "pulse"))]
    {
        let _ = out_channels;
        None
    }
}

/// `start_track`: index of the container audio stream to start with
/// (from `--aid`/`--alang`); `None` = FFmpeg's "best" track. If the
/// index is not a valid audio stream we silently fall back to "best".
pub fn spawn<P: AsRef<Path>>(
    path: P,
    clock: Arc<FfClock>,
    start_track: Option<usize>,
    backend: BackendPref,
) -> Result<AudioHandle> {
    let path = path.as_ref().to_owned();

    let ictx = crate::source::open(&path).with_context(|| format!("opening {:?}", path))?;
    let requested = start_track.filter(|&i| {
        ictx.stream(i)
            .map(|s| s.parameters().medium() == MediaType::Audio)
            .unwrap_or(false)
    });
    let audio_idx = match requested.or_else(|| ictx.streams().best(MediaType::Audio).map(|s| s.index()))
    {
        Some(i) => i,
        None => return Ok(no_audio(clock)),
    };
    drop(ictx);

    // --- Output backend selection ---
    // An explicit preference does NOT fall back to another backend
    // (failure → no audio, with the reason under --verbose). `Auto`
    // tries them in order.
    let out_channels: u16 = 2;
    let plan = match backend {
        BackendPref::NoAudio => None,
        BackendPref::Cpal => try_cpal_plan(out_channels),
        BackendPref::Pulse => try_pulse_plan(out_channels),
        BackendPref::Auto => {
            if is_termux() {
                // cpal (AAudio/NDK) doesn't work from a Termux
                // console process: try pulse first.
                try_pulse_plan(out_channels).or_else(|| try_cpal_plan(out_channels))
            } else {
                try_cpal_plan(out_channels).or_else(|| try_pulse_plan(out_channels))
            }
        }
    };
    let Some((plan, out_sample_rate)) = plan else {
        return Ok(no_audio(clock));
    };

    let (samples_tx, samples_rx) = bounded::<AudioChunk>(64);
    let samples_rx_for_drain = samples_rx.clone();

    let stop = Arc::new(AtomicBool::new(false));
    // Startup gate CLOSED: the sink emits silence (without draining
    // the ring or anchoring the clock) until the player opens it
    // when the first video frame is shown.
    let gate = Arc::new(AtomicBool::new(false));
    let volume = Arc::new(AtomicU8::new(100));
    let (msg_tx, msg_rx) = unbounded::<AudioMsg>();

    let decoder_join = {
        let stop = stop.clone();
        let path2 = path.clone();
        thread::Builder::new()
            .name("rtv-audio-decoder".into())
            .spawn(move || {
                let _ = audio_decode_loop(
                    path2,
                    audio_idx,
                    out_sample_rate,
                    out_channels,
                    samples_tx,
                    samples_rx_for_drain,
                    msg_rx,
                    stop,
                );
            })?
    };

    // --- Start the chosen sink ---
    // All the audio-clock logic (serial-based discard, latency EMA,
    // rate limiter) lives in SinkFeeder (audio_backend.rs) and is
    // IDENTICAL for both backends.
    let (sink, backend_name): (Option<SinkRuntime>, &'static str) = match plan {
        #[cfg(feature = "cpal-audio")]
        SinkPlan::Cpal(device, stream_config) => {
            let mut feeder = SinkFeeder::new(
                stop.clone(),
                gate.clone(),
                clock.clone(),
                volume.clone(),
                samples_rx,
                out_sample_rate,
                out_channels,
            );
            let build = device.build_output_stream(
                &stream_config,
                move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
                    // cpal reports: playback = when the FIRST frame
                    // of `out` reaches the DAC; callback = now.
                    let ts = info.timestamp();
                    let reported_delay = ts
                        .playback
                        .duration_since(&ts.callback)
                        .map(|d| d.as_secs_f64())
                        .unwrap_or(0.0);
                    feeder.fill(out, reported_delay);
                },
                |err| eprintln_verbose(&format!("cpal stream error: {err}")),
                None,
            );
            match build {
                Ok(s) => match s.play() {
                    Ok(()) => (Some(SinkRuntime::Cpal(s)), "cpal"),
                    Err(e) => {
                        eprintln_verbose(&format!("cpal play failed: {e}"));
                        (None, "none")
                    }
                },
                Err(e) => {
                    eprintln_verbose(&format!("cpal build_output_stream failed: {e}"));
                    (None, "none")
                }
            }
        }
        #[cfg(feature = "pulse")]
        SinkPlan::Pulse(psink) => {
            let rt = psink.start(
                stop.clone(),
                gate.clone(),
                clock.clone(),
                volume.clone(),
                samples_rx,
            );
            (Some(SinkRuntime::Pulse(rt)), "pulse")
        }
    };

    let has_audio = sink.is_some();

    Ok(AudioHandle {
        stop,
        gate,
        volume,
        msg_tx,
        clock,
        has_audio,
        sample_rate: out_sample_rate,
        channels: out_channels,
        track_index: Some(audio_idx),
        decoder_join: Some(decoder_join),
        sink,
        backend_name,
    })
}

fn no_audio(clock: Arc<FfClock>) -> AudioHandle {
    let stop = Arc::new(AtomicBool::new(true));
    let (msg_tx, _msg_rx) = bounded::<AudioMsg>(1);
    AudioHandle {
        stop,
        gate: Arc::new(AtomicBool::new(true)),
        volume: Arc::new(AtomicU8::new(100)),
        msg_tx,
        clock,
        has_audio: false,
        sample_rate: 48000,
        channels: 2,
        track_index: None,
        decoder_join: None,
        sink: None,
        backend_name: "none",
    }
}

/// Decode state for ONE audio track (decoder + resampler + input
/// parameters). Rebuilt from scratch on a runtime track switch: each
/// track may have a different codec, sample_rate and layout — the
/// resampler always normalizes to the sink's FIXED output format
/// (f32 interleaved, out_sample_rate, out_channels), so the output
/// stream itself is never touched.
struct TrackState {
    decoder: ffmpeg::decoder::Audio,
    tb_num: f64,
    tb_den: f64,
    in_sample_rate: u32,
    in_ch_layout_raw: ffmpeg::sys::AVChannelLayout,
    in_format: SampleFormat,
    swr: SwrCtx,
}

fn mk_out_layout(out_channels: u16) -> ChannelLayout<'static> {
    if out_channels == 1 {
        ChannelLayout::MONO
    } else {
        ChannelLayout::STEREO
    }
}

fn open_track(
    ictx: &ffmpeg::format::context::Input,
    stream_idx: usize,
    out_channels: u16,
    out_sample_rate: u32,
) -> Result<TrackState> {
    let stream = ictx
        .stream(stream_idx)
        .ok_or_else(|| anyhow!("stream {stream_idx} does not exist"))?;
    if stream.parameters().medium() != MediaType::Audio {
        return Err(anyhow!("stream {stream_idx} is not an audio stream"));
    }
    // OWNED copy of the parameters (decoupled from the ictx borrow).
    let codec_params: ffmpeg::codec::Parameters = {
        let src_ref = stream.parameters();
        let mut owned = ffmpeg::codec::Parameters::new();
        unsafe {
            ffmpeg::sys::avcodec_parameters_copy(owned.as_mut_ptr(), src_ref.as_ptr());
        }
        owned
    };
    let tb = stream.time_base();
    let dec_ctx = ffmpeg::codec::context::Context::from_parameters(codec_params)?;
    let decoder = dec_ctx.decoder().audio()?;
    let in_sample_rate = decoder.rate();
    let in_ch_layout_raw: ffmpeg::sys::AVChannelLayout =
        decoder.ch_layout().to_owned().into_owned();
    let in_format: SampleFormat = decoder.format();
    let swr = SwrCtx::get2(
        in_format,
        ChannelLayout::from(&in_ch_layout_raw),
        in_sample_rate,
        SampleFormat::F32(SampleType::Packed),
        mk_out_layout(out_channels),
        out_sample_rate,
    )
    .map_err(|e| anyhow!("swresample init: {e}"))?;
    Ok(TrackState {
        decoder,
        tb_num: f64::from(tb.numerator()),
        tb_den: f64::from(tb.denominator()),
        in_sample_rate,
        in_ch_layout_raw,
        in_format,
        swr,
    })
}

#[allow(clippy::too_many_arguments)]
fn audio_decode_loop(
    path: PathBuf,
    audio_idx: usize,
    out_sample_rate: u32,
    out_channels: u16,
    samples_tx: Sender<AudioChunk>,
    samples_rx_for_drain: Receiver<AudioChunk>,
    msg_rx: Receiver<AudioMsg>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let mut ictx = crate::source::open(&path)?;
    // start_time rebase (Twitch HLS VODs — same base as the video
    // decoder, see source::start_offset): subtracted from frame PTS
    // and added to seek targets.
    let start_offset = crate::source::start_offset(&ictx);
    // Index of the ACTIVE track (changes with AudioMsg::Switch).
    let mut active_idx = audio_idx;
    let mut ts = open_track(&ictx, active_idx, out_channels, out_sample_rate)?;

    // Decoder-thread debug log (RTV_AUDIO_DEC_DEBUG=/path).
    let mut dec_log: Option<std::io::BufWriter<std::fs::File>> =
        std::env::var("RTV_AUDIO_DEC_DEBUG").ok().and_then(|p| {
            std::fs::File::create(p).ok().map(std::io::BufWriter::new)
        });
    let dec_origin = std::time::Instant::now();

    let mut in_frame = AudioFrame::empty();

    // Running PTS: updated whenever the decoder produces a frame
    // with a valid PTS; subsequent frames add n_samples/rate.
    let mut running_pts: f64 = 0.0;
    // Serial this thread is currently processing. Chunks are tagged
    // with THIS one (not the clock's, which the player may have
    // bumped before we get to process the seek).
    let mut current_serial: i32 = 0;
    // After a seek: trim samples until landing EXACTLY on the target.
    // FFmpeg positions the demuxer on the packet before the target,
    // so without this trim the audio would start BEFORE the requested
    // point (up to ~1 s with AAC) — desync after every seek.
    let mut trim_until_pts: Option<f64> = None;
    // At EOF? Park the thread waiting for seek/stop (for backward
    // seeks after the end and for --loop).
    let mut at_eof = false;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // Process pending messages BEFORE reading the next packet.
        // Coalescing: of a burst only the LAST destination matters
        // (target, track) — but an intermediate Switch DOES change
        // the track even if a Seek arrives afterwards.
        let mut land_at: Option<(f64, i32)> = None; // (target, serial)
        let mut switch_to: Option<usize> = None;
        while let Ok(msg) = msg_rx.try_recv() {
            match msg {
                AudioMsg::Seek { target_secs, serial } => {
                    land_at = Some((target_secs, serial));
                }
                AudioMsg::Switch { stream_index, at_secs, serial } => {
                    switch_to = Some(stream_index);
                    land_at = Some((at_secs, serial));
                }
            }
        }
        // Track switch: reopen decoder+resampler on the new stream.
        // On failure (invalid index, codec with no decoder…) the
        // current track is kept — the land_at landing is still valid
        // and the audio keeps playing uninterrupted.
        if let Some(idx) = switch_to {
            if idx != active_idx {
                match open_track(&ictx, idx, out_channels, out_sample_rate) {
                    Ok(new_ts) => {
                        ts = new_ts;
                        active_idx = idx;
                        if let Some(log) = dec_log.as_mut() {
                            use std::io::Write as _;
                            let _ = writeln!(
                                log,
                                "{:.4} SWITCH stream={}",
                                dec_origin.elapsed().as_secs_f64(),
                                idx
                            );
                            let _ = log.flush();
                        }
                    }
                    Err(e) => {
                        eprintln_verbose(&format!("switch_track({idx}) failed: {e}"));
                    }
                }
            }
        }
        if let Some((target, serial)) = land_at {
            current_serial = serial;
            // Units: `Input::seek` → avformat_seek_file with
            // stream_index=-1 → timestamps in AV_TIME_BASE (µs).
            // Careful with the range: `..ts` (exclusive) yields
            // max_ts = ts-1 < ts and avformat_seek_file returns
            // EINVAL WITHOUT moving the demuxer — backward seeks
            // left the audio in place (forward seeks were masked by
            // the trim). With `..=ts` it becomes (INT64_MIN, ts, ts)
            // = keyframe <= ts, exactly like ffplay.
            let seek_ts =
                ((target + start_offset) * f64::from(ffmpeg::ffi::AV_TIME_BASE)) as i64;
            let _ = ictx.seek(seek_ts, ..=seek_ts);
            ts.decoder.flush();
            // Rebuild the resampler: its internal FIFO may hold
            // pre-seek samples that would come out tagged with the
            // new PTS (old audio playing after the jump). Rebuilding
            // is cheap and guarantees a clean state.
            if let Ok(new_swr) = SwrCtx::get2(
                ts.in_format,
                ChannelLayout::from(&ts.in_ch_layout_raw),
                ts.in_sample_rate,
                SampleFormat::F32(SampleType::Packed),
                mk_out_layout(out_channels),
                out_sample_rate,
            ) {
                ts.swr = new_swr;
            }
            // Drain the ring: the callback would discard by serial
            // anyway, but we prefer fresh audio to arrive ASAP.
            while samples_rx_for_drain.try_recv().is_ok() {}
            running_pts = target;
            trim_until_pts = Some(target);
            at_eof = false;
            if let Some(log) = dec_log.as_mut() {
                use std::io::Write as _;
                let _ = writeln!(
                    log,
                    "{:.4} SEEK target={:.3} serial={}",
                    dec_origin.elapsed().as_secs_f64(),
                    target,
                    current_serial
                );
                let _ = log.flush();
            }
        }

        if at_eof {
            thread::sleep(Duration::from_millis(25));
            continue;
        }

        let pkt = match ictx.packets().next() {
            Some(Ok((s, p))) => {
                if s.index() != active_idx {
                    continue;
                }
                p
            }
            Some(Err(_)) => continue,
            None => {
                let _ = ts.decoder.send_eof();
                drain_audio(
                    &mut ts.decoder,
                    &mut ts.swr,
                    &mut in_frame,
                    &samples_tx,
                    &msg_rx,
                    &stop,
                    current_serial,
                    out_channels,
                    out_sample_rate,
                    ts.in_sample_rate,
                    &mut running_pts,
                    &mut trim_until_pts,
                );
                // Reset the decoder so it can be reused after a
                // backward seek (send_eof leaves it in draining state).
                ts.decoder.flush();
                at_eof = true;
                continue;
            }
        };

        let _ = ts.decoder.send_packet(&pkt);

        while ts.decoder.receive_frame(&mut in_frame).is_ok() {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            // PTS of the decoded frame (when valid).
            let pkt_pts = in_frame.pts().unwrap_or(ffmpeg::sys::AV_NOPTS_VALUE);
            if pkt_pts != ffmpeg::sys::AV_NOPTS_VALUE {
                running_pts = pkt_pts as f64 * ts.tb_num / ts.tb_den - start_offset;
            }

            // PTS of the first OUTPUT sample of this conversion: the
            // resampler may hold internal buffer from the previous
            // conversion, whose audio predates the current frame.
            let delay_in = ts
                .swr
                .delay()
                .map(|d| d.input as f64 / ts.in_sample_rate as f64)
                .unwrap_or(0.0);
            let out_first_pts = running_pts - delay_in;

            let samples = match resample_frame(
                &mut ts.swr,
                &in_frame,
                out_channels,
                out_sample_rate,
                ts.in_sample_rate,
            ) {
                Some(s) => s,
                None => continue,
            };
            let n_per_ch = samples.len() / out_channels as usize;

            if let Some(chunk) = make_trimmed_chunk(
                samples,
                out_first_pts,
                n_per_ch,
                current_serial,
                out_channels,
                out_sample_rate,
                &mut trim_until_pts,
            ) {
                if let Some(log) = dec_log.as_mut() {
                    use std::io::Write as _;
                    let _ = writeln!(
                        log,
                        "{:.4} CHUNK pts={:.3} n={} serial={}",
                        dec_origin.elapsed().as_secs_f64(),
                        chunk.first_pts,
                        chunk.samples.len() / out_channels as usize,
                        chunk.serial
                    );
                }
                if send_with_stop(&samples_tx, chunk, &stop, &msg_rx).is_err() {
                    return Ok(());
                }
            }
            // Advance running_pts for the next frame in case it
            // arrives without a PTS (some codecs do that). Advance by
            // the INPUT frame's duration (media timeline).
            running_pts += in_frame.samples() as f64 / ts.in_sample_rate as f64;
        }
    }
    Ok(())
}

/// Converts a frame with swresample using a FRESH output frame with
/// enough capacity for (internal buffer + current frame).
///
/// IMPORTANT: ffmpeg-the-third's `SwrCtx::run()` wrapper only
/// allocates the output frame when it's empty, sizing it from
/// `input.samples()` — a capacity that NEVER grows afterwards
/// because `nb_samples` ends up as "samples converted". With
/// out_rate < in_rate (or after AAC's short first frame) the output
/// gets truncated and the remainder piles up unbounded in the
/// resampler's internal FIFO: emitted chunks represent LESS time
/// than their PTS advances — the audio clock ran ~3-4× faster than
/// the actual sound and A/V drifted apart within seconds. We create
/// a new frame per conversion with generous capacity so EVERYTHING
/// available always gets drained.
fn resample_frame(
    swr: &mut SwrCtx,
    in_frame: &AudioFrame,
    out_channels: u16,
    out_sample_rate: u32,
    in_sample_rate: u32,
) -> Option<Vec<f32>> {
    let in_n = in_frame.samples();
    if in_n == 0 {
        return None;
    }
    // Pending internal buffer (in input samples) + current frame,
    // converted to the output rate, with headroom.
    let pending_in = swr.delay().map(|d| d.input as usize).unwrap_or(0);
    let cap = ((in_n + pending_in) as u64 * out_sample_rate as u64
        / in_sample_rate.max(1) as u64) as usize
        + 256;
    let mask = if out_channels == 1 {
        ChannelLayoutMask::MONO
    } else {
        ChannelLayoutMask::STEREO
    };
    let mut out_frame = AudioFrame::new(
        ffmpeg::format::Sample::F32(SampleType::Packed),
        cap,
        mask,
    );
    if swr.run(in_frame, &mut out_frame).is_err() {
        return None;
    }
    let samples = extract_f32_interleaved(&out_frame, out_channels);
    if samples.is_empty() {
        None
    } else {
        Some(samples)
    }
}

/// Applies the post-seek trim: if the decoded frame starts before
/// the target, drop the leading samples so the emitted chunk starts
/// EXACTLY (sample-accurate) on the target. Returns None when the
/// whole frame falls before the target.
fn make_trimmed_chunk(
    mut samples: Vec<f32>,
    first_pts: f64,
    n_per_ch: usize,
    serial: i32,
    out_channels: u16,
    out_sample_rate: u32,
    trim_until_pts: &mut Option<f64>,
) -> Option<AudioChunk> {
    let mut chunk_first_pts = first_pts;
    if let Some(target) = *trim_until_pts {
        let end_pts = first_pts + n_per_ch as f64 / out_sample_rate as f64;
        if end_pts <= target {
            // The whole frame predates the target — drop it.
            return None;
        }
        if first_pts < target {
            let skip_per_ch = (((target - first_pts) * out_sample_rate as f64) as usize)
                .min(n_per_ch.saturating_sub(1));
            samples.drain(..skip_per_ch * out_channels as usize);
            chunk_first_pts = first_pts + skip_per_ch as f64 / out_sample_rate as f64;
        }
        *trim_until_pts = None;
    }
    if samples.is_empty() {
        return None;
    }
    Some(AudioChunk {
        samples,
        serial,
        first_pts: chunk_first_pts,
    })
}

#[allow(clippy::too_many_arguments)]
fn drain_audio(
    decoder: &mut ffmpeg::decoder::Audio,
    swr: &mut SwrCtx,
    in_frame: &mut AudioFrame,
    samples_tx: &Sender<AudioChunk>,
    msg_rx: &Receiver<AudioMsg>,
    stop: &Arc<AtomicBool>,
    current_serial: i32,
    out_channels: u16,
    out_sample_rate: u32,
    in_sample_rate: u32,
    running_pts: &mut f64,
    trim_until_pts: &mut Option<f64>,
) {
    while decoder.receive_frame(in_frame).is_ok() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let pkt_pts = in_frame.pts().unwrap_or(ffmpeg::sys::AV_NOPTS_VALUE);
        if pkt_pts != ffmpeg::sys::AV_NOPTS_VALUE {
            // Note: no time base available here; the caller keeps
            // running_pts up to date.
        }
        let delay_in = swr
            .delay()
            .map(|d| d.input as f64 / in_sample_rate as f64)
            .unwrap_or(0.0);
        let out_first_pts = *running_pts - delay_in;
        let in_n = in_frame.samples();
        let samples =
            match resample_frame(swr, in_frame, out_channels, out_sample_rate, in_sample_rate) {
                Some(s) => s,
                None => continue,
            };
        let n_per_ch = samples.len() / out_channels as usize;
        *running_pts += in_n as f64 / in_sample_rate as f64;
        if let Some(chunk) = make_trimmed_chunk(
            samples,
            out_first_pts,
            n_per_ch,
            current_serial,
            out_channels,
            out_sample_rate,
            trim_until_pts,
        ) {
            if send_with_stop(samples_tx, chunk, stop, msg_rx).is_err() {
                break;
            }
        }
    }
}

fn extract_f32_interleaved(frame: &AudioFrame, channels: u16) -> Vec<f32> {
    let n_per_ch = frame.samples();
    if n_per_ch == 0 {
        return Vec::new();
    }
    let n = n_per_ch * channels as usize;
    let bytes = frame.data(0);
    let expected_bytes = n * std::mem::size_of::<f32>();
    if bytes.len() < expected_bytes {
        return Vec::new();
    }
    let mut out = vec![0f32; n];
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.as_mut_ptr() as *mut u8, expected_bytes);
    }
    out
}

/// Sends a chunk while honoring `stop`. If a message arrives while
/// we wait for ring space (seek/switch, msg_rx non-empty), the chunk
/// is dropped and we return Ok so the main loop processes it RIGHT
/// AWAY — without this, with the stream paused (ring full, callback
/// not consuming) the thread stayed blocked and seeks issued while
/// paused never applied.
fn send_with_stop(
    tx: &Sender<AudioChunk>,
    mut chunk: AudioChunk,
    stop: &Arc<AtomicBool>,
    msg_rx: &Receiver<AudioMsg>,
) -> std::result::Result<(), ()> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(());
        }
        match tx.try_send(chunk) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(c)) => {
                if !msg_rx.is_empty() {
                    // Pending seek/switch: this chunk is already stale.
                    return Ok(());
                }
                chunk = c;
                thread::sleep(Duration::from_millis(4));
            }
            Err(TrySendError::Disconnected(_)) => return Err(()),
        }
    }
}

fn eprintln_verbose(msg: &str) {
    eprintln!("[rtv-audio] {msg}");
}
