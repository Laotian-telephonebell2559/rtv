//! Terminal rendering — Kitty / iTerm2 / Sixel / HalfBlocks / ASCII
//! backends.
//!
//! Notable details:
//!   * `fit_aspect` avoids losing pixels when aligning to the cell
//!     grid (floor-rounding to a cell multiple so the letterbox
//!     never looks "broken").
//!   * `reset_layout_cache` forces a clear on resize/seek.
//!   * Pixels-per-cell come from the detected `CellPx`, not fixed values.
//!   * **Clipping to the terminal bounds in ALL backends**: during a
//!     resize, frames arrive with "stale" dims (bigger than the
//!     freshly shrunk terminal) — they used to be written
//!     off-screen, causing visual garbage, phantom scrolling and
//!     panics. `draw()` now receives the usable area (cols × rows
//!     minus the HUD) and each backend clips rows and columns that
//!     don't fit. Frames with out-of-date dims render clipped for
//!     the ~1-2 frames it takes the decoder to pick up the new dims
//!     — without losing the pre-decode cushion or sync.
//!   * `set_cell_px` lets the Kitty clipping know how many pixels
//!     each cell occupies.

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use flate2::{write::ZlibEncoder, Compression};
use std::io::{StdoutLock, Write};

use crate::decoder::RgbFrame;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Kitty,
    Iterm2,
    Sixel,
    HalfBlocks,
    Ascii,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::Kitty => "kitty",
            Backend::Iterm2 => "iterm2",
            Backend::Sixel => "sixel",
            Backend::HalfBlocks => "blocks",
            Backend::Ascii => "ascii",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "kitty" => Backend::Kitty,
            "iterm2" | "iterm" => Backend::Iterm2,
            "sixel" => Backend::Sixel,
            "blocks" | "halfblocks" | "half" => Backend::HalfBlocks,
            "ascii" => Backend::Ascii,
            _ => return None,
        })
    }
}

/// Heuristic detection based on environment variables.
pub fn detect_backend(forced: Option<&str>) -> Backend {
    if let Some(f) = forced {
        if let Some(b) = Backend::from_str(f) {
            return b;
        }
    }
    let term = std::env::var("TERM").unwrap_or_default();
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let kitty = std::env::var("KITTY_WINDOW_ID").is_ok()
        || term.contains("kitty")
        || term_program.eq_ignore_ascii_case("ghostty")
        || term_program.eq_ignore_ascii_case("wezterm");
    if kitty {
        return Backend::Kitty;
    }
    // iTerm2: TERM_PROGRAM=iTerm.app locally; LC_TERMINAL=iTerm2
    // propagates over ssh (iTerm2 exports it and sshd usually
    // accepts LC_*).
    let lc_terminal = std::env::var("LC_TERMINAL").unwrap_or_default();
    if term_program.eq_ignore_ascii_case("iTerm.app")
        || lc_terminal.eq_ignore_ascii_case("iTerm2")
    {
        return Backend::Iterm2;
    }
    // Sixel: terminals that support it out of the box. xterm only
    // enables it when built with --enable-sixel-graphics AND launched
    // with -ti vt340 — in that case TERM is usually "xterm-sixel"
    // (or the user forces --backend sixel). mlterm/foot/contour
    // always ship it.
    if term.contains("sixel")
        || term.starts_with("mlterm")
        || std::env::var("MLTERM").is_ok()
        || term == "foot" || term == "foot-extra"
        || term.starts_with("contour")
        || term_program.eq_ignore_ascii_case("contour")
    {
        return Backend::Sixel;
    }
    Backend::HalfBlocks
}

/// Fits (w, h) keeping the video aspect ratio inside the target area.
/// Returns the new dims plus pixel offsets (for centering the letterbox).
///
/// Extra: aligns to a multiple of `cell_w`/`cell_h` on the "short"
/// axis so the letterbox lands exactly on terminal cell boundaries —
/// avoids visually messy "half" black bars.
pub fn fit_aspect(
    src: (u32, u32),
    dst: (u32, u32),
    align_w: u32,
    align_h: u32,
) -> ((u32, u32), (u32, u32)) {
    let (sw, sh) = (src.0 as f32, src.1 as f32);
    let (dw, dh) = (dst.0 as f32, dst.1 as f32);
    if sw <= 0.0 || sh <= 0.0 {
        return ((dst.0.max(2), dst.1.max(2)), (0, 0));
    }
    let ar_src = sw / sh;
    let ar_dst = dw / dh;
    let (mut w, mut h) = if ar_src > ar_dst {
        (dw as u32, ((dw / ar_src).max(2.0)) as u32)
    } else {
        (((dh * ar_src).max(2.0)) as u32, dh as u32)
    };
    // Floor-align to the cell multiple so ox/oy also land on a cell
    // boundary.
    if align_w > 1 {
        w = (w / align_w).max(1) * align_w;
    }
    if align_h > 1 {
        h = (h / align_h).max(1) * align_h;
    }
    w = w.min(dst.0).max(2);
    h = h.min(dst.1).max(2);
    let ox = (dst.0.saturating_sub(w)) / 2;
    let oy = (dst.1.saturating_sub(h)) / 2;
    // Align ox/oy to the cell grid as well.
    let ox = if align_w > 1 { (ox / align_w) * align_w } else { ox };
    let oy = if align_h > 1 { (oy / align_h) * align_h } else { oy };
    ((w, h), (ox, oy))
}

pub struct Renderer {
    pub backend: Backend,
    scratch: Vec<u8>,
    b64: String,
    last_layout: Option<(u16, u16, u32, u32, u32, u32)>,
    /// Terminal pixels per cell (only relevant for Kitty: halfblocks
    /// is implicitly 1×2 and ascii 1×1). Used for clipping to the
    /// terminal bounds.
    cell_px_w: u32,
    cell_px_h: u32,
    /// Clipping buffer for Kitty/iTerm2 (RGB crop before base64).
    crop_buf: Vec<u8>,
    /// Sixel: palette-index buffer (1 byte/px) of the already
    /// quantized + dithered frame.
    sixel_idx: Vec<u8>,
    /// Sixel: per-color bit masks for the current band
    /// ([color][column] layout) used by the per-color pass.
    sixel_band: Vec<u8>,
    /// Sixel: fixed palette definition (built once).
    sixel_palette: String,
    /// iTerm2: buffer for the BMP file built in memory.
    file_buf: Vec<u8>,
    /// Kitty: id of the image currently on screen (double-buffered
    /// ids). 0 = none. See `draw_kitty` for the rationale.
    kitty_live_id: u32,
    /// Kitty: reusable buffer for the zlib-compressed payload
    /// (`o=z`). Compressing the raw RGB before base64 shrinks the
    /// escape stream ~3-6× on real video and is what makes sustained
    /// video fps possible (the bottleneck was the terminal decoding
    /// ~1.3 MB of base64 PER FRAME, not the decode).
    kitty_z: Vec<u8>,
    /// LOCAL Kitty: shared-memory transport (`t=s`).
    /// `Some(counter)` = active. The frame is written to a POSIX shm
    /// object (/dev/shm) and the escape carries only its NAME in
    /// base64 (~60 bytes/frame): no zlib, no bitmap base64 —
    /// pixel-exact quality with minimal overhead. Only enabled with
    /// real kitty running locally (see `kitty_shm_available`).
    kitty_shm: Option<u64>,
}

/// Can we use the kitty protocol's shm transport (`t=s`)?
///
/// Requirements (all of them):
///   * Linux (POSIX shm objects in /dev/shm, written with fs::write)
///     or macOS (no /dev/shm: shm_open/ftruncate/mmap via libc —
///     kitty on Mac opens them with shm_open just the same, the
///     protocol is identical).
///   * REAL kitty (KITTY_WINDOW_ID — ghostty/wezterm don't always
///     export it and their t=s support isn't guaranteed).
///   * LOCAL session: over ssh the terminal lives on another machine
///     and cannot read our shared memory.
///   * Manual opt-out with RTV_KITTY_NO_SHM=1 (escape hatch).
fn kitty_shm_available() -> bool {
    let os_ok = if cfg!(target_os = "linux") {
        std::path::Path::new("/dev/shm").is_dir()
    } else {
        cfg!(target_os = "macos")
    };
    os_ok
        && std::env::var_os("KITTY_WINDOW_ID").is_some()
        && std::env::var_os("SSH_CONNECTION").is_none()
        && std::env::var_os("SSH_TTY").is_none()
        && std::env::var_os("SSH_CLIENT").is_none()
        && std::env::var_os("RTV_KITTY_NO_SHM").is_none()
}

/// Number of shm names kept "in flight" before self-cleanup: the
/// object from N frames ago is removed when emitting the current
/// one. kitty removes them itself after reading (spec: the terminal
/// does shm_unlink after the read); this is the safety net so
/// /dev/shm doesn't grow if something stalls (or the terminal
/// detection guessed wrong).
const KITTY_SHM_INFLIGHT: u64 = 8;

fn kitty_shm_name(counter: u64) -> String {
    format!("rtv-{}-{}", std::process::id(), counter)
}

/// Writes `payload` into a POSIX shm object named `/name`.
///
/// Linux: /dev/shm is a mounted tmpfs — direct fs::write (fast, no
/// unsafe). macOS: there's no /dev/shm; the object is created with
/// shm_open + ftruncate + mmap (libc). kitty opens it with shm_open
/// on both platforms, so the name traveling in the escape is
/// identical.
#[cfg(target_os = "linux")]
fn kitty_shm_write(name: &str, payload: &[u8]) -> std::io::Result<()> {
    std::fs::write(format!("/dev/shm/{name}"), payload)
}

#[cfg(target_os = "macos")]
fn kitty_shm_write(name: &str, payload: &[u8]) -> std::io::Result<()> {
    use std::ffi::CString;
    let cname = CString::new(format!("/{name}"))
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "shm name"))?;
    unsafe {
        // O_EXCL: the name is unique per (pid, counter). On macOS,
        // ftruncate on an shm object can only be done ONCE (the size
        // becomes fixed), so the object must be fresh; if leftovers
        // from a recycled pid exist, unlink and retry.
        let flags = libc::O_CREAT | libc::O_RDWR | libc::O_EXCL;
        let mut fd = libc::shm_open(cname.as_ptr(), flags, 0o600 as libc::c_uint);
        if fd < 0 {
            libc::shm_unlink(cname.as_ptr());
            fd = libc::shm_open(cname.as_ptr(), flags, 0o600 as libc::c_uint);
        }
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::ftruncate(fd, payload.len() as libc::off_t) < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            libc::shm_unlink(cname.as_ptr());
            return Err(e);
        }
        let p = libc::mmap(
            std::ptr::null_mut(),
            payload.len(),
            libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        );
        if p == libc::MAP_FAILED {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            libc::shm_unlink(cname.as_ptr());
            return Err(e);
        }
        std::ptr::copy_nonoverlapping(payload.as_ptr(), p.cast::<u8>(), payload.len());
        libc::munmap(p, payload.len());
        libc::close(fd);
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn kitty_shm_write(_name: &str, _payload: &[u8]) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "shm not supported on this platform",
    ))
}

/// Removes an shm object by name (safety net and Drop cleanup;
/// kitty normally unlinks them itself after reading).
fn kitty_shm_remove(name: &str) {
    #[cfg(target_os = "linux")]
    {
        let _ = std::fs::remove_file(format!("/dev/shm/{name}"));
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(c) = std::ffi::CString::new(format!("/{name}")) {
            unsafe {
                libc::shm_unlink(c.as_ptr());
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = name;
    }
}

/// Pair of alternating ids for Kitty's image double buffer.
const KITTY_ID_A: u32 = 4242;
const KITTY_ID_B: u32 = 4243;

impl Drop for Renderer {
    fn drop(&mut self) {
        // Clean up in-flight shm objects kitty hasn't unlinked yet
        // (clean exit: no rtv leftovers on the system).
        if let Some(counter) = self.kitty_shm {
            let from = counter.saturating_sub(KITTY_SHM_INFLIGHT);
            for c in from..counter {
                kitty_shm_remove(&kitty_shm_name(c));
            }
        }
    }
}

impl Renderer {
    pub fn new(backend: Backend) -> Self {
        Self {
            backend,
            scratch: Vec::with_capacity(1 << 20),
            b64: String::with_capacity(1 << 20),
            last_layout: None,
            cell_px_w: 8,
            cell_px_h: 16,
            crop_buf: Vec::new(),
            sixel_idx: Vec::new(),
            sixel_band: Vec::new(),
            sixel_palette: build_sixel_palette(),
            file_buf: Vec::new(),
            kitty_live_id: 0,
            kitty_z: Vec::new(),
            kitty_shm: if backend == Backend::Kitty && kitty_shm_available() {
                Some(0)
            } else {
                None
            },
        }
    }

    /// Tells the renderer the cell size in pixels (for the Kitty
    /// backend's clipping). Call at startup and after a resize if
    /// the cell size changes.
    pub fn set_cell_px(&mut self, w: u32, h: u32) {
        self.cell_px_w = w.max(1);
        self.cell_px_h = h.max(1);
    }

    pub fn reset_layout_cache(&mut self) {
        self.last_layout = None;
    }

    /// Draws `frame` with its top-left corner at cell (col_ox,
    /// row_oy), CLIPPING to a usable area of `max_cols` × `max_rows`
    /// cells (the terminal area minus the HUD). Tolerates frames
    /// whose dims don't match the current layout (resize in flight):
    /// they render clipped instead of overflowing the screen.
    ///
    /// Returns `true` if a full-screen clear (2J) was emitted — the
    /// caller must invalidate its HUD cache because the HUD row was
    /// wiped too.
    pub fn draw<W: Write>(
        &mut self,
        out: &mut W,
        frame: &RgbFrame,
        max_cols: u16,
        max_rows: u16,
        col_ox: u16,
        row_oy: u16,
    ) -> Result<bool> {
        if max_cols == 0 || max_rows == 0 || frame.width == 0 || frame.height == 0 {
            return Ok(false);
        }
        // Offsets clamped to the usable area.
        let col_ox = col_ox.min(max_cols.saturating_sub(1));
        let row_oy = row_oy.min(max_rows.saturating_sub(1));

        let layout = (
            max_cols,
            max_rows,
            frame.width,
            frame.height,
            col_ox as u32,
            row_oy as u32,
        );
        // Synchronized output (DEC 2026): the terminal accumulates
        // everything between ?2026h and ?2026l and presents it in ONE
        // refresh. Kills the visible black flash between the clear
        // (2J) and the frame repaint (e.g. pressing `j` with a layout
        // change, or on resizes). Windows Terminal, kitty, WezTerm,
        // foot, iTerm2… support it; others ignore it harmlessly.
        out.write_all(b"\x1b[?2026h")?;
        let mut cleared = false;
        if self.last_layout != Some(layout) {
            out.write_all(b"\x1b[2J\x1b[H")?;
            // Layout change (resize/seek with new dims): here we DO
            // delete ALL kitty images — it's a rare event and the
            // whole screen was just cleared anyway.
            if self.backend == Backend::Kitty {
                out.write_all(b"\x1b_Ga=d,d=A,q=2;\x1b\\")?;
                self.kitty_live_id = 0;
            }
            self.last_layout = Some(layout);
            cleared = true;
        }

        let res = match self.backend {
            Backend::Kitty => self.draw_kitty(out, frame, max_cols, max_rows, col_ox, row_oy),
            Backend::Iterm2 => self.draw_iterm2(out, frame, max_cols, max_rows, col_ox, row_oy),
            Backend::Sixel => self.draw_sixel(out, frame, max_cols, max_rows, col_ox, row_oy),
            Backend::HalfBlocks => {
                self.draw_halfblocks(out, frame, max_cols, max_rows, col_ox, row_oy)
            }
            Backend::Ascii => self.draw_ascii(out, frame, max_cols, max_rows, col_ox, row_oy),
        };
        // ALWAYS close the batch, even if the backend failed — a
        // dangling ?2026h would freeze the terminal's screen.
        out.write_all(b"\x1b[?2026l")?;
        res?;
        Ok(cleared)
    }

    fn draw_halfblocks<W: Write>(
        &mut self,
        out: &mut W,
        frame: &RgbFrame,
        max_cols: u16,
        max_rows: u16,
        col_ox: u16,
        row_oy: u16,
    ) -> Result<()> {
        let w = frame.width as usize;
        let h = frame.height as usize;
        let stride = w * 3;
        let data = &frame.data;
        if data.len() < h * stride {
            return Ok(()); // corrupt/incomplete frame: skip painting
        }

        self.scratch.clear();
        let mut last_fg: (u8, u8, u8) = (255, 255, 255);
        let mut last_bg: (u8, u8, u8) = (0, 0, 0);

        // Clipping: visible cell rows and columns within the usable
        // area. 1 cell = 1×2 px in halfblocks.
        let rows = (h / 2).min((max_rows - row_oy) as usize);
        let vis_w = w.min((max_cols - col_ox) as usize);
        for cy in 0..rows {
            let term_row = row_oy as usize + cy + 1;
            let term_col = col_ox as usize + 1;
            write!(&mut self.scratch, "\x1b[{};{}H", term_row, term_col)?;
            let mut first_cell = true;

            let y_top = cy * 2;
            let y_bot = y_top + 1;
            let row_top = &data[y_top * stride..y_top * stride + stride];
            let row_bot = &data[y_bot * stride..y_bot * stride + stride];

            for x in 0..vis_w {
                let i = x * 3;
                let fg = (row_top[i], row_top[i + 1], row_top[i + 2]);
                let bg = (row_bot[i], row_bot[i + 1], row_bot[i + 2]);

                if first_cell || fg != last_fg {
                    write!(&mut self.scratch, "\x1b[38;2;{};{};{}m", fg.0, fg.1, fg.2)?;
                    last_fg = fg;
                }
                if first_cell || bg != last_bg {
                    write!(&mut self.scratch, "\x1b[48;2;{};{};{}m", bg.0, bg.1, bg.2)?;
                    last_bg = bg;
                }
                first_cell = false;
                self.scratch.extend_from_slice(&[0xE2, 0x96, 0x80]);
            }
        }
        self.scratch.extend_from_slice(b"\x1b[0m");
        out.write_all(&self.scratch)?;
        Ok(())
    }

    fn draw_kitty<W: Write>(
        &mut self,
        out: &mut W,
        frame: &RgbFrame,
        max_cols: u16,
        max_rows: u16,
        col_ox: u16,
        row_oy: u16,
    ) -> Result<()> {
        let w = frame.width as usize;
        let h = frame.height as usize;
        let stride = w * 3;
        if frame.data.len() < h * stride {
            return Ok(());
        }

        // PIXEL clipping to the usable area: if the frame (stale dims
        // during a resize) doesn't fit, only the visible
        // sub-rectangle is sent. Without this the image overflowed
        // the video area, stomping the HUD / causing scroll.
        let avail_px_w = (max_cols - col_ox) as usize * self.cell_px_w as usize;
        let avail_px_h = (max_rows - row_oy) as usize * self.cell_px_h as usize;
        let vis_w = w.min(avail_px_w).max(1);
        let vis_h = h.min(avail_px_h).max(1);

        let payload: &[u8] = if vis_w == w && vis_h == h {
            &frame.data
        } else {
            self.crop_buf.clear();
            self.crop_buf.reserve(vis_w * vis_h * 3);
            for y in 0..vis_h {
                let s = y * stride;
                self.crop_buf.extend_from_slice(&frame.data[s..s + vis_w * 3]);
            }
            &self.crop_buf
        };

        // ANTI-FLICKER — double-buffered image ids.
        //
        // Previously: `a=d,d=A` (delete EVERYTHING), then transmit
        // the new frame. Between the delete and the terminal
        // finishing the base64 decode (~1 MB per frame) the video
        // area sat EMPTY — the background peeked through for an
        // instant on every frame: continuous flicker in kitty
        // (DEC 2026 doesn't always bridge the gap: kitty may refresh
        // mid-decode).
        //
        // Now: frames alternate two fixed ids (A/B). The new frame is
        // transmitted and placed under the "free" id and ONLY THEN is
        // the previous id deleted (`a=d,d=I,i=…`): the old frame
        // stays visible until the new one is already on top — never
        // a gap. UPPERCASE `I` deletes the placement AND frees the
        // image data in the terminal (lowercase only the placement),
        // so memory doesn't grow: at most 2 frames are alive.
        let new_id = if self.kitty_live_id == KITTY_ID_A {
            KITTY_ID_B
        } else {
            KITTY_ID_A
        };
        let old_id = self.kitty_live_id;

        self.scratch.clear();
        write!(
            &mut self.scratch,
            "\x1b[{};{}H",
            row_oy as usize + 1,
            col_ox as usize + 1
        )?;

        // SHM TRANSPORT (`t=s`) — only real kitty running LOCALLY.
        //
        // The RGB frame is written to a POSIX shm object (Linux:
        // /dev/shm; macOS: shm_open+mmap) and the escape carries only
        // its NAME (~60 bytes per frame): no zlib, no bitmap base64,
        // pixel-EXACT quality with minimal work on both sides. kitty
        // maps the object, reads the pixels and does shm_unlink
        // itself (spec). If the shm write fails (full, permissions)
        // it's disabled for the rest of the session and we fall back
        // to the zlib path.
        if let Some(counter) = self.kitty_shm {
            let name = kitty_shm_name(counter);
            match kitty_shm_write(&name, payload) {
                Ok(()) => {
                    // Safety net: remove the object from N frames ago
                    // if the terminal didn't (spec says kitty unlinks
                    // it after reading; this keeps shm memory from
                    // growing if something stalls).
                    if counter >= KITTY_SHM_INFLIGHT {
                        kitty_shm_remove(&kitty_shm_name(counter - KITTY_SHM_INFLIGHT));
                    }
                    self.kitty_shm = Some(counter + 1);

                    // Escape payload = shm name ("/rtv-…") in b64.
                    self.b64.clear();
                    B64.encode_string(format!("/{name}").as_bytes(), &mut self.b64);
                    write!(
                        &mut self.scratch,
                        "\x1b_Ga=T,f=24,t=s,i={},s={},v={},q=2;{}\x1b\\",
                        new_id, vis_w, vis_h, self.b64
                    )?;
                    if old_id != 0 {
                        write!(&mut self.scratch, "\x1b_Ga=d,d=I,i={},q=2;\x1b\\", old_id)?;
                    }
                    self.kitty_live_id = new_id;
                    out.write_all(&self.scratch)?;
                    return Ok(());
                }
                Err(_) => {
                    // shm unusable: disable it and continue with the
                    // normal transport (zlib + base64).
                    self.kitty_shm = None;
                }
            }
        }

        // zlib COMPRESSION (`o=z` in the kitty graphics protocol).
        //
        // Raw RGB for a 720p frame is ~2.7 MB — ~3.6 MB in base64.
        // That PER-FRAME stream is what drowns the terminal (kitty
        // was stuck at 17 fps on 24-25 fps video, and --hwdec made no
        // difference because decode wasn't the bottleneck). zlib at
        // level 1 (fast) compresses real video 3-6× in <2 ms and
        // kitty decompresses natively, so the terminal receives FAR
        // less and has plenty of time to present each frame.
        //
        // If compression doesn't help (pure-noise frame: rare in
        // video), the raw data is sent — never worse than baseline.
        self.kitty_z.clear();
        let mut enc = ZlibEncoder::new(std::mem::take(&mut self.kitty_z), Compression::fast());
        enc.write_all(payload)?;
        self.kitty_z = enc.finish()?;
        let compressed = self.kitty_z.len() < payload.len();
        let wire: &[u8] = if compressed { &self.kitty_z } else { payload };

        self.b64.clear();
        B64.encode_string(wire, &mut self.b64);

        let bytes = self.b64.as_bytes();
        const CHUNK: usize = 4096;
        let mut i = 0;
        let mut first = true;
        while i < bytes.len() {
            let end = (i + CHUNK).min(bytes.len());
            let more = end < bytes.len();
            let m = if more { 1 } else { 0 };
            if first {
                let o = if compressed { ",o=z" } else { "" };
                write!(
                    &mut self.scratch,
                    "\x1b_Ga=T,f=24,i={},s={},v={}{},q=2,m={};",
                    new_id, vis_w, vis_h, o, m
                )?;
                first = false;
            } else {
                write!(&mut self.scratch, "\x1b_Gm={},q=2;", m)?;
            }
            self.scratch.extend_from_slice(&bytes[i..end]);
            self.scratch.extend_from_slice(b"\x1b\\");
            i = end;
        }
        // Delete the previous frame AFTER placing the new one.
        if old_id != 0 {
            write!(&mut self.scratch, "\x1b_Ga=d,d=I,i={},q=2;\x1b\\", old_id)?;
        }
        self.kitty_live_id = new_id;
        out.write_all(&self.scratch)?;
        Ok(())
    }

    /// iTerm2 — inline images protocol (OSC 1337 File=).
    ///
    /// We build an UNCOMPRESSED 24-bit BMP in memory (zero
    /// dependencies, ~memcpy cost) and send it in base64. iTerm2
    /// decodes it with NSImage (BMP natively supported). `width`/
    /// `height` are given in CELLS so the mapping to the terminal
    /// grid is exact and independent of the Retina factor (the
    /// protocol's px values are "points", not pixels: on 2x displays
    /// the image would come out at double size).
    fn draw_iterm2<W: Write>(
        &mut self,
        out: &mut W,
        frame: &RgbFrame,
        max_cols: u16,
        max_rows: u16,
        col_ox: u16,
        row_oy: u16,
    ) -> Result<()> {
        let w = frame.width as usize;
        let h = frame.height as usize;
        let stride = w * 3;
        if frame.data.len() < h * stride {
            return Ok(());
        }

        // Pixel clipping to the usable area (same criteria as Kitty).
        let avail_px_w = (max_cols - col_ox) as usize * self.cell_px_w as usize;
        let avail_px_h = (max_rows - row_oy) as usize * self.cell_px_h as usize;
        let vis_w = w.min(avail_px_w).max(1);
        let vis_h = h.min(avail_px_h).max(1);

        // 24bpp BMP: 14-byte header + 40-byte DIB, bottom-up BGR rows
        // padded to a multiple of 4 bytes.
        let row_bytes = (vis_w * 3 + 3) & !3;
        let img_size = row_bytes * vis_h;
        let file_size = 54 + img_size;
        self.file_buf.clear();
        self.file_buf.reserve(file_size);
        let fb = &mut self.file_buf;
        fb.extend_from_slice(b"BM");
        fb.extend_from_slice(&(file_size as u32).to_le_bytes());
        fb.extend_from_slice(&0u32.to_le_bytes()); // reserved
        fb.extend_from_slice(&54u32.to_le_bytes()); // pixel data offset
        fb.extend_from_slice(&40u32.to_le_bytes()); // DIB size
        fb.extend_from_slice(&(vis_w as i32).to_le_bytes());
        fb.extend_from_slice(&(vis_h as i32).to_le_bytes());
        fb.extend_from_slice(&1u16.to_le_bytes()); // planes
        fb.extend_from_slice(&24u16.to_le_bytes()); // bpp
        fb.extend_from_slice(&0u32.to_le_bytes()); // no compression
        fb.extend_from_slice(&(img_size as u32).to_le_bytes());
        fb.extend_from_slice(&2835i32.to_le_bytes()); // 72 dpi
        fb.extend_from_slice(&2835i32.to_le_bytes());
        fb.extend_from_slice(&0u32.to_le_bytes());
        fb.extend_from_slice(&0u32.to_le_bytes());
        for y in (0..vis_h).rev() {
            let row = &frame.data[y * stride..y * stride + vis_w * 3];
            for px in row.chunks_exact(3) {
                fb.extend_from_slice(&[px[2], px[1], px[0]]); // RGB→BGR
            }
            for _ in vis_w * 3..row_bytes {
                fb.push(0);
            }
        }

        self.b64.clear();
        B64.encode_string(&self.file_buf, &mut self.b64);

        let cells_w = vis_w.div_ceil(self.cell_px_w.max(1) as usize);
        let cells_h = vis_h.div_ceil(self.cell_px_h.max(1) as usize);

        self.scratch.clear();
        write!(
            &mut self.scratch,
            "\x1b[{};{}H\x1b]1337;File=inline=1;size={};width={};height={};preserveAspectRatio=0:",
            row_oy as usize + 1,
            col_ox as usize + 1,
            file_size,
            cells_w,
            cells_h,
        )?;
        self.scratch.extend_from_slice(self.b64.as_bytes());
        self.scratch.push(0x07); // BEL — OSC terminator
        out.write_all(&self.scratch)?;
        Ok(())
    }

    /// Sixel — a real encoder (DCS `ESC P q … ESC \`).
    ///
    /// Strategy:
    ///   * FIXED palette of 252 registers (6×7×6 RGB cube, extra
    ///     levels on green: the eye is more sensitive to it).
    ///     Re-emitted on EVERY frame: xterm uses PRIVATE color
    ///     registers per image (privateColorRegisters, default on)
    ///     and without the palette every frame would render black.
    ///   * ORDERED dithering (4×4 Bayer): no serial dependencies
    ///     between pixels (unlike Floyd-Steinberg) — cheap and
    ///     stable across frames (the noise doesn't "boil").
    ///   * Encoding in 6-row bands: one pass fills per-color masks
    ///     (`sixel_band`, [color][column]) and each present color is
    ///     emitted with RLE (`!n`), `$` (CR) between colors and `-`
    ///     (LF) between bands.
    fn draw_sixel<W: Write>(
        &mut self,
        out: &mut W,
        frame: &RgbFrame,
        max_cols: u16,
        max_rows: u16,
        col_ox: u16,
        row_oy: u16,
    ) -> Result<()> {
        let w = frame.width as usize;
        let h = frame.height as usize;
        let stride = w * 3;
        if frame.data.len() < h * stride {
            return Ok(());
        }

        let avail_px_w = (max_cols - col_ox) as usize * self.cell_px_w as usize;
        let avail_px_h = (max_rows - row_oy) as usize * self.cell_px_h as usize;
        let vis_w = w.min(avail_px_w).max(1);
        let vis_h = h.min(avail_px_h).max(1);

        // --- 1) Quantization + dithering to palette indices ---
        const BAYER4: [u8; 16] = [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5];
        #[inline(always)]
        fn quant(v: u8, levels: u32, t: u32) -> u32 {
            ((v as u32 * (levels - 1) * 16 + t * 255) / (255 * 16)).min(levels - 1)
        }
        self.sixel_idx.resize(vis_w * vis_h, 0);
        for y in 0..vis_h {
            let row = &frame.data[y * stride..y * stride + vis_w * 3];
            let dst = &mut self.sixel_idx[y * vis_w..(y + 1) * vis_w];
            let by = (y & 3) * 4;
            for x in 0..vis_w {
                let t = BAYER4[by + (x & 3)] as u32;
                let i = x * 3;
                let r = quant(row[i], 6, t);
                let g = quant(row[i + 1], 7, t);
                let b = quant(row[i + 2], 6, t);
                dst[x] = (r * 42 + g * 6 + b) as u8;
            }
        }

        // --- 2) Emission ---
        self.scratch.clear();
        write!(
            &mut self.scratch,
            "\x1b[{};{}H",
            row_oy as usize + 1,
            col_ox as usize + 1
        )?;
        // DCS: P1=0 (1:1 aspect), P2=1 (zero bits are transparent,
        // don't paint background outside the letterbox), P3=0.
        // Raster attributes "Pan;Pad;Ph;Pv" help the terminal reserve
        // the area.
        write!(&mut self.scratch, "\x1bP0;1;0q\"1;1;{};{}", vis_w, vis_h)?;
        self.scratch.extend_from_slice(self.sixel_palette.as_bytes());

        // Per-color masks for the band: [color][column], bits 0-5.
        self.sixel_band.resize(256 * vis_w, 0);
        let mut used: Vec<u16> = Vec::with_capacity(64);
        let mut present = [false; 256];

        let bands = vis_h.div_ceil(6);
        for band in 0..bands {
            let y0 = band * 6;
            let rows_in = (vis_h - y0).min(6);

            used.clear();
            present.fill(false);
            for j in 0..rows_in {
                let src = &self.sixel_idx[(y0 + j) * vis_w..(y0 + j + 1) * vis_w];
                let bit = 1u8 << j;
                for (x, &c) in src.iter().enumerate() {
                    let c = c as usize;
                    if !present[c] {
                        present[c] = true;
                        used.push(c as u16);
                    }
                    self.sixel_band[c * vis_w + x] |= bit;
                }
            }

            for (k, &c) in used.iter().enumerate() {
                write!(&mut self.scratch, "#{}", c)?;
                let rowm = &self.sixel_band[c as usize * vis_w..c as usize * vis_w + vis_w];
                // Trim trailing zeros: '?' (empty) at the end adds nothing.
                let mut end = vis_w;
                while end > 0 && rowm[end - 1] == 0 {
                    end -= 1;
                }
                let mut x = 0;
                while x < end {
                    let v = rowm[x];
                    let mut run = 1;
                    while x + run < end && rowm[x + run] == v {
                        run += 1;
                    }
                    let ch = 63 + v;
                    if run >= 4 {
                        write!(&mut self.scratch, "!{}", run)?;
                        self.scratch.push(ch);
                    } else {
                        for _ in 0..run {
                            self.scratch.push(ch);
                        }
                    }
                    x += run;
                }
                if k + 1 < used.len() {
                    self.scratch.push(b'$'); // CR: next color, same band
                } else if band + 1 < bands {
                    self.scratch.push(b'-'); // LF: next band
                }
            }

            // Clear only the rows of colors actually used (not all 256).
            for &c in &used {
                self.sixel_band[c as usize * vis_w..c as usize * vis_w + vis_w].fill(0);
            }
        }
        self.scratch.extend_from_slice(b"\x1b\\"); // ST — end of DCS
        out.write_all(&self.scratch)?;
        Ok(())
    }

    fn draw_ascii<W: Write>(
        &mut self,
        out: &mut W,
        frame: &RgbFrame,
        max_cols: u16,
        max_rows: u16,
        col_ox: u16,
        row_oy: u16,
    ) -> Result<()> {
        const GRAD: &[u8] = b" .:-=+*#%@";
        let w = frame.width as usize;
        let h = frame.height as usize;
        let stride = w * 3;
        if frame.data.len() < h * stride {
            return Ok(());
        }
        self.scratch.clear();
        // Clipping: 1 cell = 1×1 px in ascii.
        let vis_h = h.min((max_rows - row_oy) as usize);
        let vis_w = w.min((max_cols - col_ox) as usize);
        for cy in 0..vis_h {
            write!(
                &mut self.scratch,
                "\x1b[{};{}H",
                row_oy as usize + cy + 1,
                col_ox as usize + 1
            )?;
            let row = &frame.data[cy * stride..cy * stride + stride];
            for x in 0..vis_w {
                let i = x * 3;
                let l = (row[i] as u32 * 299 + row[i + 1] as u32 * 587 + row[i + 2] as u32 * 114)
                    / 1000;
                let idx = (l as usize * (GRAD.len() - 1)) / 255;
                self.scratch.push(GRAD[idx]);
            }
        }
        out.write_all(&self.scratch)?;
        Ok(())
    }
}

/// Fixed sixel palette: 6×7×6 RGB cube (252 registers). The green
/// axis gets 7 levels (eye sensitivity). Values use the 0..100 scale
/// the protocol requires (`#n;2;r;g;b`). Built once and re-emitted
/// on every frame (xterm's color registers are private per image
/// with the default config).
fn build_sixel_palette() -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(252 * 16);
    for r in 0..6u32 {
        for g in 0..7u32 {
            for b in 0..6u32 {
                let idx = r * 42 + g * 6 + b;
                let rr = r * 100 / 5;
                let gg = g * 100 / 6;
                let bb = b * 100 / 5;
                let _ = write!(s, "#{};2;{};{};{}", idx, rr, gg, bb);
            }
        }
    }
    s
}

pub fn draw_hud_at(out: &mut StdoutLock, cols: u16, row: u16, line: &str) -> Result<()> {
    let (content, content_width) = truncate_to_width(line, cols as usize);
    let pad_needed = (cols as usize).saturating_sub(content_width);
    // Sequence: SGR reset → move cursor → text → padding → reset.
    // The final reset keeps a "dangling" color from bleeding into
    // the next frame.
    write!(
        out,
        "\x1b[0m\x1b[{};1H{}{}\x1b[0m",
        row,
        content,
        " ".repeat(pad_needed),
    )?;
    Ok(())
}

/// Subtitle line: like `draw_hud_at` but with its own styling —
/// bold + bright white over the terminal background, which is how
/// video players paint them and what keeps the text readable over
/// the letterbox (they used to render with the terminal's default
/// style: thin and gray, hard to read). The full-width padding is
/// UNSTYLED so it doesn't paint a background stripe.
pub fn draw_sub_line(out: &mut StdoutLock, cols: u16, row: u16, line: &str) -> Result<()> {
    let (content, content_width) = truncate_to_width(line, cols as usize);
    let pad_needed = (cols as usize).saturating_sub(content_width);
    write!(
        out,
        "\x1b[0m\x1b[{};1H\x1b[1;97m{}\x1b[0m{}",
        row,
        content,
        " ".repeat(pad_needed),
    )?;
    Ok(())
}

/// Single-line HUD, on the last row.
pub fn draw_hud(out: &mut StdoutLock, cols: u16, rows: u16, line: &str) -> Result<()> {
    draw_hud_at(out, cols, rows, line)
}

/// Two-line HUD, on the last two rows.
pub fn draw_hud_two_lines(
    out: &mut StdoutLock,
    cols: u16,
    rows: u16,
    line1: &str,
    line2: &str,
) -> Result<()> {
    draw_hud_at(out, cols, rows.saturating_sub(1).max(1), line1)?;
    draw_hud_at(out, cols, rows, line2)?;
    Ok(())
}

/// Truncates by REAL cell width (unicode-width, not chars or bytes)
/// and also returns the resulting width. Critical for the HUD: 🔊/🔇
/// are wide (2 cells) while █/░/▶/⏸/· are 1; counting "1 cell per
/// char" made the actual line overflow `cols` — autowrap+scroll on
/// the last row.
fn truncate_to_width(s: &str, max_width: usize) -> (String, usize) {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::with_capacity(s.len());
    let mut width = 0usize;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if width + cw > max_width {
            break;
        }
        out.push(c);
        width += cw;
    }
    (out, width)
}
