//! gui.rs — windowed application mode (`gui` feature), mpv-style.
//!
//! rtv's heart remains the terminal mode; this GUI is an OPTIONAL
//! frontend that reuses exactly the same pipeline: video decoder
//! (RGB24 frames), audio pipeline (cpal/pulse) and ffplay clocks
//! (FfClock/MasterClock). Only the presentation and input layer
//! changes: instead of terminal cells and crossterm, a window with
//! GPU rendering and winit events.
//!
//! GPU STACK (eframe = winit + wgpu + egui):
//!   * winit: cross-platform window + events.
//!   * wgpu: accelerated presentation (Vulkan/Metal/DX12/GL, with a
//!     software fallback via lavapipe/llvmpipe on GPU-less machines).
//!     Video is uploaded as a texture (RGB24 → egui::ColorImage) and
//!     the GPU scales it with linear filtering — better quality than
//!     a software nearest blit.
//!   * egui: immediate-mode HUD with real typography, antialiasing
//!     and transparency — a polished look without hand-drawn glyphs.
//!   Costs more binary size than winit+softbuffer — a deliberate
//!   trade-off for a much better visual result.
//!
//! mpv-style UX:
//!   * space = pause, ←/→ = seek ±5 s, ↑/↓ = volume ±5,
//!     PgUp/PgDn = ±60 s, f = fullscreen, m = mute, q/Esc = quit.
//!   * Bottom OSD with a clickable/draggable progress bar, times and
//!     status; auto-hides after 2.5 s without mouse activity (the
//!     cursor hides with it), reappears on movement.
//!   * Mouse wheel = volume. Double click = fullscreen.
//!   * Live streams: the bar maps the DVR window and shows "LIVE".

use crate::audio::AudioHandle;
use crate::clock::{Clock, FfClock, MasterClock};
use crate::decoder::{DecoderHandle, RgbFrame};
use crate::playback::{self, fmt_time, wall_now_f64, AudioGate, FramePlan, Pipeline};
use crate::player::Config;
use anyhow::Result;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------
// PlayerCore — the same pipeline and the same sync discipline as
// player.rs (ffplay clocks, post-seek hold, audio landing,
// serial-based discard, dropping late frames), but without a
// terminal: "showing" means leaving the frame in `self.current` and
// returning StepResult::Frame so the window blits it. The shared
// pieces (pipeline opening, audio gate, seek window and sync
// arithmetic) live in `crate::playback` — a single source of truth
// for terminal and GUI.
// ---------------------------------------------------------------

pub enum StepResult {
    /// New frame available in `current` — repaint the window.
    Frame,
    /// Nothing to show yet; call again after `wake_in`.
    Idle { wake_in: Duration },
    /// Playback finished (EOF without loop).
    Eof,
}

struct PlayerCore {
    dec: DecoderHandle,
    audio: Option<AudioHandle>,
    master: Arc<MasterClock>,
    vidclk: Arc<FfClock>,
    using_audio: bool,
    loop_video: bool,
    is_live: bool,
    live_start_pts: Option<f64>,
    max_live_pts: f64,

    volume: i32,
    muted: bool,

    // Sync state (identical in spirit to player.rs).
    frame_timer: f64,
    last_shown_pts: f64,
    pending: Option<RgbFrame>,
    pending_audio_landing: bool,
    pending_video_landing: bool,
    held_frame_serial: Option<i32>,
    hold_started: Option<Instant>,
    show_one_frame_paused: bool,
    gate: AudioGate,
    fallback_frame_dur: f64,
    max_frame_dur: f64,

    /// Last frame shown (the window re-blits it on every redraw,
    /// e.g. after a resize or when painting the OSD over it).
    current: Option<RgbFrame>,
}

impl PlayerCore {
    fn open(cfg: &Config, dst_w: u32, dst_h: u32) -> Result<Self> {
        // Opening SHARED with player.rs (playback::Pipeline): clocks
        // with staleness, audio honoring --aid/--alang, master clock,
        // decoder, is_live and the startup gate.
        let (audio_tracks, _subs) = playback::probe_tracks(cfg);
        let Pipeline {
            dec,
            audio,
            master,
            vidclk,
            using_audio,
            is_live,
            fallback_frame_dur,
            max_frame_dur,
            gate,
        } = Pipeline::open(cfg, &audio_tracks, dst_w, dst_h)?;

        Ok(Self {
            dec,
            audio,
            master,
            vidclk,
            using_audio,
            loop_video: cfg.loop_video,
            is_live,
            live_start_pts: None,
            max_live_pts: f64::NEG_INFINITY,
            volume: 100,
            muted: false,
            frame_timer: wall_now_f64(),
            last_shown_pts: 0.0,
            pending: None,
            pending_audio_landing: using_audio,
            pending_video_landing: false,
            held_frame_serial: None,
            hold_started: None,
            show_one_frame_paused: false,
            gate,
            fallback_frame_dur,
            max_frame_dur,
            current: None,
        })
    }

    fn open_gate(&mut self) {
        self.gate.open(&self.audio);
    }

    fn effective_volume(&self) -> i32 {
        if self.muted {
            0
        } else {
            self.volume
        }
    }

    fn apply_volume(&self) {
        if let Some(a) = self.audio.as_ref() {
            a.set_volume(self.effective_volume());
        }
    }

    fn toggle_pause(&mut self) {
        if self.master.is_paused() {
            self.master.resume();
            if let Some(a) = self.audio.as_ref() {
                a.play_stream();
            }
            self.frame_timer = wall_now_f64();
        } else {
            self.master.pause();
            if let Some(a) = self.audio.as_ref() {
                a.pause_stream();
            }
        }
    }

    /// Valid seek window [min_t, max_t] (VOD, or live with DVR).
    fn seek_window(&self) -> (f64, f64) {
        playback::seek_window(
            self.is_live,
            self.live_start_pts,
            self.max_live_pts,
            self.dec.duration,
        )
    }

    /// ABSOLUTE seek with the same atomic order as player.rs:
    /// master.set → dec.seek_dir; audio lands with the first
    /// post-seek frame (pending_audio_landing).
    fn seek_to(&mut self, target: f64) {
        let (min_t, max_t) = self.seek_window();
        if self.is_live && max_t - min_t < 1.0 {
            return; // live stream with no window yet
        }
        let now = self.master.now();
        let target = target.max(min_t).min(max_t);
        self.master.set(target);
        self.dec.seek_dir(target, !now.is_finite() || target > now);
        self.pending = None;
        self.pending_audio_landing = self.using_audio;
        self.pending_video_landing = true;
        self.frame_timer = wall_now_f64();
        self.last_shown_pts = target;
        if self.master.is_paused() {
            self.show_one_frame_paused = true;
        }
    }

    fn seek_rel(&mut self, delta: f64) {
        let now = self.master.now();
        let base = if now.is_finite() { now } else { self.last_shown_pts };
        self.seek_to(base + delta);
    }

    /// Bar fraction [0,1] → proportional seek (VOD: duration;
    /// live: DVR window).
    fn seek_frac(&mut self, frac: f64) {
        let (min_t, max_t) = self.seek_window();
        if max_t <= min_t {
            return;
        }
        self.seek_to(min_t + frac.clamp(0.0, 1.0) * (max_t - min_t));
    }

    /// Current progress for the bar: (fraction, time, total).
    fn progress(&self) -> (f64, f64, f64) {
        let now = self.master.now();
        let t = if now.is_finite() { now } else { self.last_shown_pts };
        if self.is_live {
            let (lo, hi) = self.seek_window();
            let total = (hi - lo).max(0.0);
            let cur = (t - lo).clamp(0.0, total);
            let frac = if total > 0.0 { cur / total } else { 1.0 };
            (frac, cur, total)
        } else {
            let total = self.dec.duration.max(0.0);
            let cur = t.clamp(0.0, total);
            let frac = if total > 0.0 { cur / total } else { 0.0 };
            (frac, cur, total)
        }
    }

    /// One step of the presentation loop. Blocking time while
    /// waiting for a frame is kept short — the GUI must keep
    /// servicing window events.
    fn step(&mut self) -> StepResult {
        // Audio gate relief valve (broken video — let it play).
        self.gate.tick(&self.audio);

        // Paused: only pull ONE frame if a seek is pending a repaint.
        if self.master.is_paused() {
            if self.show_one_frame_paused {
                if let Ok(frame) = self.dec.rx.recv_timeout(Duration::from_millis(100)) {
                    if frame.serial == self.dec.current_serial() {
                        if self.pending_audio_landing {
                            self.master.retarget(frame.pts);
                            if let Some(a) = self.audio.as_ref() {
                                a.seek(frame.pts);
                            }
                            self.pending_audio_landing = false;
                        }
                        self.last_shown_pts = frame.pts;
                        self.show_one_frame_paused = false;
                        self.current = Some(frame);
                        return StepResult::Frame;
                    }
                }
                return StepResult::Idle { wake_in: Duration::from_millis(10) };
            }
            return StepResult::Idle { wake_in: Duration::from_millis(50) };
        }

        // Post-seek/startup HOLD: target frame already shown, audio
        // not yet anchored — wait (1.5 s valve → force_anchor).
        if self.using_audio
            && !self.master.master_anchored()
            && self.held_frame_serial == Some(self.dec.current_serial())
        {
            if self
                .hold_started
                .map(|t| t.elapsed() > Duration::from_millis(1500))
                .unwrap_or(false)
            {
                self.master.master().force_anchor();
            } else {
                return StepResult::Idle { wake_in: Duration::from_millis(4) };
            }
        }

        // Next frame: pending one, or from the channel.
        let frame = match self.pending.take() {
            Some(f) => f,
            None => match self.dec.rx.recv_timeout(Duration::from_millis(20)) {
                Ok(f) => f,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if self.dec.eof.load(Ordering::Relaxed) {
                        if self.loop_video {
                            self.master.set(0.0);
                            self.dec.seek(0.0);
                            self.pending_audio_landing = self.using_audio;
                            self.pending_video_landing = true;
                            self.frame_timer = wall_now_f64();
                            self.last_shown_pts = 0.0;
                            return StepResult::Idle { wake_in: Duration::from_millis(4) };
                        }
                        return StepResult::Eof;
                    }
                    return StepResult::Idle { wake_in: Duration::from_millis(5) };
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return StepResult::Eof,
            },
        };

        // Stale serial (post-seek residue).
        if frame.serial != self.dec.current_serial() {
            return StepResult::Idle { wake_in: Duration::ZERO };
        }

        // Clock unanchored (startup/post-seek with audio): show this
        // frame NOW (it's the target's) and enter the hold.
        if self.using_audio && !self.master.master_anchored() {
            if self.pending_audio_landing {
                self.master.retarget(frame.pts);
                if let Some(a) = self.audio.as_ref() {
                    a.seek(frame.pts);
                }
                self.pending_audio_landing = false;
            }
            self.vidclk.set_pts(frame.pts, self.vidclk.current_serial());
            self.last_shown_pts = frame.pts;
            self.frame_timer = wall_now_f64();
            self.pending_video_landing = false;
            self.held_frame_serial = Some(frame.serial);
            self.hold_started = Some(Instant::now());
            self.live_accounting(frame.pts);
            self.open_gate();
            self.current = Some(frame);
            return StepResult::Frame;
        }
        // Leaving the hold: re-anchor vidclk and frame_timer (see
        // player.rs — without this, video starts late after anchoring).
        if self.held_frame_serial.take().is_some() {
            self.frame_timer = wall_now_f64();
            self.hold_started = None;
            self.vidclk.set_pts(self.last_shown_pts, self.vidclk.current_serial());
        }

        // LIVE: PTS discontinuity (stitched ads) — re-anchor.
        if self.is_live {
            self.live_accounting(frame.pts);
            let jump = frame.pts - self.last_shown_pts;
            if jump.is_finite()
                && jump.abs() > 10.0
                && !self.pending_audio_landing
                && !self.pending_video_landing
            {
                let elapsed =
                    (self.last_shown_pts - self.live_start_pts.unwrap_or(frame.pts)).max(0.0);
                self.master.retarget(frame.pts);
                if let Some(a) = self.audio.as_ref() {
                    a.seek(frame.pts);
                }
                self.vidclk.set_pts(frame.pts, self.vidclk.current_serial());
                self.last_shown_pts = frame.pts;
                self.frame_timer = wall_now_f64();
                self.live_start_pts = Some(frame.pts - elapsed);
                self.max_live_pts = self.max_live_pts.max(frame.pts);
            }
        }

        // Video-only landing (seek with --no-audio / distant HLS).
        if self.pending_video_landing {
            self.pending_video_landing = false;
            if !self.using_audio {
                self.master.retarget(frame.pts);
            }
            self.last_shown_pts = frame.pts;
            self.frame_timer = wall_now_f64();
        }

        // ffplay-style SYNC — arithmetic SHARED with player.rs
        // (playback::plan_frame: vp_duration + compute_target_delay +
        // 100 ms resync + late-frame drop + wait).
        match playback::plan_frame(
            frame.pts,
            self.last_shown_pts,
            &mut self.frame_timer,
            self.fallback_frame_dur,
            self.max_frame_dur,
            &self.vidclk,
            &self.master,
        ) {
            FramePlan::Late => {
                // Drop (frame_timer already reverted: consumes no slot).
                self.last_shown_pts = frame.pts;
                return StepResult::Idle { wake_in: Duration::ZERO };
            }
            FramePlan::Wait { sleep_s, target_delay } => {
                // Not due yet: put it back in pending, reverting the
                // timer advance (it gets re-added on reprocessing).
                self.frame_timer -= target_delay;
                self.pending = Some(frame);
                return StepResult::Idle { wake_in: Duration::from_secs_f64(sleep_s) };
            }
            FramePlan::Show => {}
        }

        // Show it.
        self.vidclk.set_pts(frame.pts, self.vidclk.current_serial());
        self.last_shown_pts = frame.pts;
        self.open_gate();
        if self.is_live {
            self.live_accounting(frame.pts);
        }
        self.current = Some(frame);
        StepResult::Frame
    }

    fn live_accounting(&mut self, pts: f64) {
        if self.is_live {
            if self.live_start_pts.is_none() {
                self.live_start_pts = Some(pts);
            }
            if pts > self.max_live_pts {
                self.max_live_pts = pts;
            }
        }
    }
}

// ---------------------------------------------------------------
// Window + GPU rendering + HUD: eframe (winit + wgpu + egui).
// ---------------------------------------------------------------

use eframe::egui;

/// Fits (sw, sh) inside (bw, bh) preserving aspect ratio.
fn fit_inside(sw: u32, sh: u32, bw: u32, bh: u32) -> (u32, u32) {
    if sw == 0 || sh == 0 || bw == 0 || bh == 0 {
        return (bw.max(1), bh.max(1));
    }
    let scale = (bw as f64 / sw as f64).min(bh as f64 / sh as f64);
    (
        ((sw as f64 * scale) as u32).max(1),
        ((sh as f64 * scale) as u32).max(1),
    )
}

/// How long until the OSD/cursor auto-hides without activity.
const OSD_HIDE_AFTER: Duration = Duration::from_millis(2500);
/// Lifetime of the transient OSD (volume, mute…).
const FLASH_DURATION: Duration = Duration::from_millis(1500);
/// HUD accent color (rtv blue).
const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x3d, 0x9b, 0xff);

/// Rectangle with a vertical gradient (OSD scrims) — a 4-vertex Mesh
/// with per-vertex color; the GPU interpolates.
fn gradient_rect(
    painter: &egui::Painter,
    rect: egui::Rect,
    top: egui::Color32,
    bottom: egui::Color32,
) {
    use egui::epaint::{Mesh, Vertex, WHITE_UV};
    let mut mesh = Mesh::default();
    let v = |pos: egui::Pos2, color: egui::Color32| Vertex { pos, uv: WHITE_UV, color };
    mesh.vertices.push(v(rect.left_top(), top));
    mesh.vertices.push(v(rect.right_top(), top));
    mesh.vertices.push(v(rect.right_bottom(), bottom));
    mesh.vertices.push(v(rect.left_bottom(), bottom));
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(egui::Shape::mesh(mesh));
}

/// Input actions resolved in one pass (collected inside `ctx.input`
/// and applied afterwards, to avoid fighting the borrow checker).
#[derive(Default)]
struct Actions {
    quit: bool,
    toggle_pause: bool,
    toggle_fullscreen: bool,
    toggle_mute: bool,
    seek_rel: Option<f64>,
    vol_delta: i32,
    /// Proportional seek from a click/drag on the bar.
    seek_frac: Option<f64>,
    touched: bool,
}

struct RtvApp {
    core: PlayerCore,
    title_base: String,
    /// Texture of the current video frame (updated on every Frame).
    tex: Option<egui::TextureHandle>,
    /// Dims requested from the decoder (video fit to the window, px).
    fit_w: u32,
    fit_h: u32,

    // --- OSD / mouse ---
    last_activity: Instant,
    last_cursor: Option<egui::Pos2>,
    dragging_bar: bool,
    last_click: Option<Instant>,
    flash: Option<(String, Instant)>,
    fullscreen: bool,
    title_paused: bool,
}

impl RtvApp {
    fn osd_visible(&self) -> bool {
        self.last_activity.elapsed() < OSD_HIDE_AFTER
            || self.core.master.is_paused()
            || self.dragging_bar
    }

    fn flash_msg(&mut self, s: String) {
        self.flash = Some((s, Instant::now()));
        self.last_activity = Instant::now();
    }

    /// Progress-bar geometry in points: (track rect, clickable-zone
    /// rect with a grace margin).
    fn bar_geom(&self, screen: egui::Rect, k: f32) -> (egui::Rect, egui::Rect) {
        let m = (screen.width() / 26.0).clamp(16.0, 48.0);
        let h = (3.5 * k).max(4.0);
        let y = screen.bottom() - 34.0 * k - h;
        let track = egui::Rect::from_min_size(
            egui::pos2(screen.left() + m, y),
            egui::vec2((screen.width() - 2.0 * m).max(1.0), h),
        );
        let hit = track.expand2(egui::vec2(0.0, 12.0));
        (track, hit)
    }

    /// Pumps the player and returns when the next repaint is due.
    fn pump(&mut self, ctx: &egui::Context) -> Option<Duration> {
        for _ in 0..8 {
            match self.core.step() {
                StepResult::Frame => {
                    if let Some(f) = self.core.current.as_ref() {
                        let img = egui::ColorImage::from_rgb(
                            [f.width as usize, f.height as usize],
                            &f.data,
                        );
                        match self.tex.as_mut() {
                            Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                            None => {
                                self.tex = Some(ctx.load_texture(
                                    "video",
                                    img,
                                    egui::TextureOptions::LINEAR,
                                ));
                            }
                        }
                    }
                    return Some(Duration::ZERO);
                }
                StepResult::Idle { wake_in } => {
                    if wake_in == Duration::ZERO {
                        continue; // retry now (frame was dropped)
                    }
                    return Some(wake_in);
                }
                StepResult::Eof => return None,
            }
        }
        Some(Duration::from_millis(1))
    }

    /// Collects this frame's input and translates it into actions.
    fn collect_input(&mut self, ctx: &egui::Context, bar_hit: egui::Rect, bar_track: egui::Rect) -> Actions {
        let mut a = Actions::default();
        ctx.input(|i| {
            use egui::Key;
            if i.viewport().close_requested() {
                a.quit = true;
            }
            if i.key_pressed(Key::Q) || i.key_pressed(Key::Escape) {
                a.quit = true;
            }
            if i.key_pressed(Key::Space) {
                a.toggle_pause = true;
            }
            if i.key_pressed(Key::ArrowRight) {
                a.seek_rel = Some(5.0);
            }
            if i.key_pressed(Key::ArrowLeft) {
                a.seek_rel = Some(-5.0);
            }
            if i.key_pressed(Key::PageUp) {
                a.seek_rel = Some(60.0);
            }
            if i.key_pressed(Key::PageDown) {
                a.seek_rel = Some(-60.0);
            }
            if i.key_pressed(Key::ArrowUp) {
                a.vol_delta += 5;
            }
            if i.key_pressed(Key::ArrowDown) {
                a.vol_delta -= 5;
            }
            if i.key_pressed(Key::M) {
                a.toggle_mute = true;
            }
            if i.key_pressed(Key::F) {
                a.toggle_fullscreen = true;
            }
            if i.keys_down.len() > 0 || !i.events.is_empty() {
                // Any key/event brings the OSD back.
                if i.events.iter().any(|e| matches!(e, egui::Event::Key { .. })) {
                    a.touched = true;
                }
            }

            // Mouse wheel = volume (like mpv). Read from the raw
            // MouseWheel event: smooth_scroll_delta is smoothed and
            // fires several times per notch.
            for ev in &i.raw.events {
                if let egui::Event::MouseWheel { delta, .. } = ev {
                    if delta.y > 0.0 {
                        a.vol_delta += 5;
                    } else if delta.y < 0.0 {
                        a.vol_delta -= 5;
                    }
                }
            }

            // Mouse movement — make the OSD visible.
            let pos = i.pointer.latest_pos();
            if pos != self.last_cursor && pos.is_some() {
                self.last_cursor = pos;
                a.touched = true;
            }

            // Bar: click/drag — proportional seek.
            if i.pointer.primary_pressed() {
                a.touched = true;
                if let Some(p) = pos {
                    if bar_hit.contains(p) {
                        self.dragging_bar = true;
                        a.seek_frac = Some(
                            ((p.x - bar_track.left()) / bar_track.width().max(1.0))
                                .clamp(0.0, 1.0) as f64,
                        );
                    } else {
                        // Double click = fullscreen; single click = pause.
                        let now = Instant::now();
                        let dbl = self
                            .last_click
                            .map(|t| now.duration_since(t) < Duration::from_millis(350))
                            .unwrap_or(false);
                        self.last_click = Some(now);
                        if dbl {
                            a.toggle_fullscreen = true;
                        } else {
                            a.toggle_pause = true;
                        }
                    }
                }
            }
            if self.dragging_bar {
                if i.pointer.primary_down() {
                    if let Some(p) = pos {
                        a.seek_frac = Some(
                            ((p.x - bar_track.left()) / bar_track.width().max(1.0))
                                .clamp(0.0, 1.0) as f64,
                        );
                    }
                } else {
                    self.dragging_bar = false;
                }
            }
        });
        a
    }

    /// Applies the collected actions to the core/window.
    fn apply_actions(&mut self, ctx: &egui::Context, a: Actions) {
        if a.touched {
            self.last_activity = Instant::now();
        }
        if a.quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if a.toggle_pause {
            self.core.toggle_pause();
            self.last_activity = Instant::now();
        }
        if let Some(d) = a.seek_rel {
            self.core.seek_rel(d);
            let sign = if d >= 0.0 { "+" } else { "-" };
            self.flash_msg(format!("{sign}{} s", d.abs() as i64));
        }
        if let Some(f) = a.seek_frac {
            self.core.seek_frac(f);
            self.last_activity = Instant::now();
        }
        if a.vol_delta != 0 {
            self.core.volume = (self.core.volume + a.vol_delta).clamp(0, 200);
            self.core.muted = false;
            self.core.apply_volume();
            self.flash_msg(format!("Volume {}%", self.core.volume));
        }
        if a.toggle_mute {
            self.core.muted = !self.core.muted;
            self.core.apply_volume();
            let m = if self.core.muted {
                "Muted".to_string()
            } else {
                format!("Volume {}%", self.core.volume)
            };
            self.flash_msg(m);
        }
        if a.toggle_fullscreen {
            self.fullscreen = !self.fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
            self.last_activity = Instant::now();
        }
        // Title with a pause indicator (only when it changes).
        let paused = self.core.master.is_paused();
        if paused != self.title_paused {
            self.title_paused = paused;
            let state = if paused { " ⏸" } else { "" };
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
                "{} — rtv{state}",
                self.title_base
            )));
        }
    }

    /// Tells the decoder the current fit dims (physical px).
    fn relayout(&mut self, screen: egui::Rect, ppp: f32) {
        let (sw, sh) = self.core.dec.source_size;
        let (fw, fh) = fit_inside(
            sw,
            sh,
            (screen.width() * ppp) as u32,
            (screen.height() * ppp) as u32,
        );
        if (fw, fh) != (self.fit_w, self.fit_h) {
            self.fit_w = fw;
            self.fit_h = fh;
            self.core.dec.resize(fw, fh);
        }
    }
}

impl eframe::App for RtvApp {
    // eframe 0.36: the trait wants `ui(&mut Ui, &mut Frame)` — the
    // CentralPanel is shown on the root Ui, not on the Context.
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        let ctx = &ctx;
        // 1) Pump the player (decodes/syncs/updates the texture).
        let wake = self.pump(ctx);
        let Some(wake) = wake else {
            // EOF without loop — close the window cleanly.
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        };

        let screen = ctx.content_rect();
        let ppp = ctx.pixels_per_point();
        self.relayout(screen, ppp);

        // HUD scale: grows smoothly with the window.
        let k = (screen.height() / 480.0).clamp(0.85, 2.2);
        let (bar_track, bar_hit) = self.bar_geom(screen, k);

        // 2) Input → actions → apply.
        let actions = self.collect_input(ctx, bar_hit, bar_track);
        self.apply_actions(ctx, actions);

        // Flash expiry.
        if let Some((_, t0)) = &self.flash {
            if t0.elapsed() >= FLASH_DURATION {
                self.flash = None;
            }
        }

        let osd_vis = self.osd_visible();
        ctx.set_cursor_icon(if osd_vis {
            egui::CursorIcon::Default
        } else {
            egui::CursorIcon::None
        });

        // 3) Painting.
        let paused = self.core.master.is_paused();
        let (frac, cur, total) = self.core.progress();
        let hover_pos = self.last_cursor;
        let dragging = self.dragging_bar;

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(egui::Color32::BLACK))
            .show(root, |ui| {
                let painter = ui.painter();

                // --- Video centered, fitted, linear GPU scaling ---
                if let Some(tex) = &self.tex {
                    let (fw, fh) = fit_inside(
                        self.core.dec.source_size.0,
                        self.core.dec.source_size.1,
                        screen.width() as u32,
                        screen.height() as u32,
                    );
                    let size = egui::vec2(fw as f32, fh as f32);
                    let rect = egui::Rect::from_center_size(screen.center(), size);
                    painter.image(
                        tex.id(),
                        rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                }

                let font = |sz: f32| egui::FontId::proportional(sz * k);

                if osd_vis {
                    // --- Gradient scrims (subtle top, stronger bottom) ---
                    let top_h = (48.0_f32 * k).min(screen.height() / 3.0);
                    gradient_rect(
                        painter,
                        egui::Rect::from_min_size(screen.left_top(), egui::vec2(screen.width(), top_h)),
                        egui::Color32::from_black_alpha(110),
                        egui::Color32::TRANSPARENT,
                    );
                    let block_h = (screen.height() / 4.0).clamp(96.0, 190.0);
                    gradient_rect(
                        painter,
                        egui::Rect::from_min_size(
                            egui::pos2(screen.left(), screen.bottom() - block_h),
                            egui::vec2(screen.width(), block_h),
                        ),
                        egui::Color32::TRANSPARENT,
                        egui::Color32::from_black_alpha(190),
                    );

                    // --- Title, top-left ---
                    painter.text(
                        egui::pos2(bar_track.left(), screen.top() + 12.0 * k),
                        egui::Align2::LEFT_TOP,
                        &self.title_base,
                        font(15.0),
                        egui::Color32::from_gray(240),
                    );

                    // --- Progress bar ---
                    let r = bar_track.height() / 2.0;
                    painter.rect_filled(bar_track, r, egui::Color32::from_white_alpha(50));
                    let mut filled = bar_track;
                    filled.set_right(bar_track.left() + bar_track.width() * frac as f32);
                    painter.rect_filled(filled, r, ACCENT);

                    // Hover: marker + time tooltip under the cursor.
                    let hover_frac = hover_pos.filter(|p| bar_hit.contains(*p) && !dragging).map(|p| {
                        ((p.x - bar_track.left()) / bar_track.width().max(1.0)).clamp(0.0, 1.0)
                    });
                    if let Some(hf) = hover_frac {
                        let hx = bar_track.left() + bar_track.width() * hf;
                        painter.rect_filled(
                            egui::Rect::from_center_size(
                                egui::pos2(hx, bar_track.center().y),
                                egui::vec2(2.0, bar_track.height() + 8.0),
                            ),
                            1.0,
                            egui::Color32::WHITE,
                        );
                        if total > 0.0 {
                            let txt = fmt_time(total * hf as f64);
                            let galley = painter.layout_no_wrap(txt, font(12.0), egui::Color32::WHITE);
                            let pad = egui::vec2(7.0 * k, 4.0 * k);
                            let tip_size = galley.size() + 2.0 * pad;
                            let tip_x = (hx - tip_size.x / 2.0)
                                .clamp(bar_track.left(), (bar_track.right() - tip_size.x).max(bar_track.left()));
                            let tip = egui::Rect::from_min_size(
                                egui::pos2(tip_x, bar_track.top() - tip_size.y - 8.0 * k),
                                tip_size,
                            );
                            painter.rect_filled(tip, 4.0 * k, egui::Color32::from_black_alpha(200));
                            painter.galley(tip.min + pad, galley, egui::Color32::WHITE);
                        }
                    }

                    // Circular knob (grows while dragging).
                    let knob_r = if dragging { 8.0 * k } else { 6.0 * k };
                    painter.circle_filled(
                        egui::pos2(filled.right(), bar_track.center().y),
                        knob_r,
                        egui::Color32::WHITE,
                    );

                    // --- Controls row under the bar ---
                    let row_y = bar_track.bottom() + 8.0 * k;
                    let icon = if paused { "⏸" } else { "▶" };
                    painter.text(
                        egui::pos2(bar_track.left(), row_y),
                        egui::Align2::LEFT_TOP,
                        icon,
                        font(15.0),
                        egui::Color32::WHITE,
                    );
                    // Times: current in white, total dimmed.
                    let cur_txt = fmt_time(cur);
                    let g = painter.layout_no_wrap(cur_txt, font(13.0), egui::Color32::WHITE);
                    let tx = bar_track.left() + 26.0 * k;
                    let cur_w = g.size().x;
                    painter.galley(egui::pos2(tx, row_y + 2.0 * k), g, egui::Color32::WHITE);
                    painter.text(
                        egui::pos2(tx + cur_w, row_y + 2.0 * k),
                        egui::Align2::LEFT_TOP,
                        format!(" / {}", fmt_time(total)),
                        font(13.0),
                        egui::Color32::from_gray(150),
                    );

                    // Right side: LIVE (red dot) or volume with a meter.
                    if self.core.is_live {
                        let g = painter.layout_no_wrap(
                            "LIVE".into(),
                            font(13.0),
                            egui::Color32::WHITE,
                        );
                        let rx = bar_track.right() - g.size().x;
                        painter.circle_filled(
                            egui::pos2(rx - 10.0 * k, row_y + 2.0 * k + g.size().y / 2.0),
                            4.0 * k,
                            egui::Color32::from_rgb(0xff, 0x3b, 0x30),
                        );
                        painter.galley(egui::pos2(rx, row_y + 2.0 * k), g, egui::Color32::WHITE);
                    } else {
                        let vol_txt = if self.core.muted {
                            "MUTE".to_string()
                        } else {
                            format!("{}%", self.core.volume)
                        };
                        let vcol = if self.core.muted {
                            egui::Color32::from_rgb(0xff, 0x9f, 0x0a)
                        } else {
                            egui::Color32::from_gray(216)
                        };
                        let g = painter.layout_no_wrap(vol_txt, font(13.0), vcol);
                        let rx = bar_track.right() - g.size().x;
                        let gh = g.size().y;
                        painter.galley(egui::pos2(rx, row_y + 2.0 * k), g, vcol);
                        // Small volume meter to the left of the text.
                        let meter_w = 52.0 * k;
                        let meter = egui::Rect::from_min_size(
                            egui::pos2(rx - meter_w - 8.0 * k, row_y + 2.0 * k + gh / 2.0 - 1.5 * k),
                            egui::vec2(meter_w, 3.0 * k),
                        );
                        painter.rect_filled(meter, 1.5 * k, egui::Color32::from_white_alpha(50));
                        if !self.core.muted {
                            let mut mf = meter;
                            mf.set_right(
                                meter.left()
                                    + meter.width() * (self.core.volume.clamp(0, 100) as f32 / 100.0),
                            );
                            painter.rect_filled(mf, 1.5 * k, egui::Color32::WHITE);
                        }
                    }

                    // --- Centered pause badge ---
                    if paused {
                        let r = (44.0_f32 * k).min(screen.width() / 6.0).min(screen.height() / 6.0);
                        painter.circle_filled(
                            screen.center(),
                            r,
                            egui::Color32::from_black_alpha(150),
                        );
                        painter.text(
                            screen.center(),
                            egui::Align2::CENTER_CENTER,
                            "⏸",
                            egui::FontId::proportional(r * 1.1),
                            egui::Color32::from_gray(245),
                        );
                    }
                }

                // --- Transient OSD (top-right pill) ---
                if let Some((msg, t0)) = &self.flash {
                    // Fade out over the last third.
                    let life = t0.elapsed().as_secs_f32() / FLASH_DURATION.as_secs_f32();
                    let alpha = if life > 0.66 { (1.0 - life) / 0.34 } else { 1.0 };
                    let a = (alpha.clamp(0.0, 1.0) * 255.0) as u8;
                    let g = painter.layout_no_wrap(
                        msg.clone(),
                        font(13.0),
                        egui::Color32::from_white_alpha(a),
                    );
                    let pad = egui::vec2(10.0 * k, 6.0 * k);
                    let size = g.size() + 2.0 * pad;
                    let pos = egui::pos2(
                        screen.right() - size.x - 16.0 * k,
                        screen.top() + 16.0 * k,
                    );
                    let rect = egui::Rect::from_min_size(pos, size);
                    painter.rect_filled(
                        rect,
                        size.y / 2.0,
                        egui::Color32::from_black_alpha((180.0 * alpha) as u8),
                    );
                    painter.galley(rect.min + pad, g, egui::Color32::WHITE);
                }
            });

        // 4) Schedule the next frame: immediately if new video is
        // ready, or at the core's requested wake-up; with the OSD
        // visible, repaint periodically so the bar clock advances.
        let mut next = wake;
        if osd_vis || self.flash.is_some() {
            next = next.min(Duration::from_millis(100));
        }
        ctx.request_repaint_after(next);
    }
}

/// GUI mode entry point (`rtv --gui …`). Blocks until the user
/// closes the window or the video ends.
pub fn run(cfg: Config) -> Result<()> {
    // Open the pipeline BEFORE creating the window: the initial size
    // is the video's (capped at 1280x720).
    let title_base = cfg
        .path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| cfg.path.to_string_lossy().into_owned());
    let core = PlayerCore::open(&cfg, 640, 360)?;
    let (sw, sh) = core.dec.source_size;
    let (iw, ih) = fit_inside(sw.max(2), sh.max(2), 1280, 720);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(format!("{title_base} — rtv"))
            .with_inner_size([iw as f32, ih as f32])
            .with_min_inner_size([160.0, 90.0])
            .with_app_id("rtv"),
        ..Default::default()
    };

    let app = RtvApp {
        core,
        title_base,
        tex: None,
        fit_w: 0,
        fit_h: 0,
        last_activity: Instant::now(),
        last_cursor: None,
        dragging_bar: false,
        last_click: None,
        flash: None,
        fullscreen: false,
        title_paused: false,
    };

    eframe::run_native("rtv", options, Box::new(move |_cc| Ok(Box::new(app)))).map_err(|e| {
        anyhow::anyhow!(
            "couldn't start the graphical environment ({e}); is an X11/Wayland server available? \
             (terminal mode always works: run rtv without --gui)"
        )
    })
}
