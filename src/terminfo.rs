//! Adaptive detection of the terminal's cell size.
//!
//! Ground rules, learned the hard way:
//!
//!   * On classic Windows consoles we never probe with CSI 16t/14t.
//!     cmd, conhost and ConEmu don't answer those queries; the probe
//!     burns its timeout every time and can even swallow bytes from
//!     real keystrokes → degenerate dims and "the video is sideways at
//!     2 fps". They get the direct heuristic instead, which happens to
//!     be right for Consolas / Cascadia Mono (8×16 - 10×20).
//!
//!   * On Unix we only probe when TERM/TERM_PROGRAM indicates a
//!     terminal we know answers (Kitty, WezTerm, Ghostty, foot,
//!     Konsole, iTerm2). Anything else falls back to the heuristic.
//!
//!   * Very short timeout (20 ms) with non-blocking `read()` from the
//!     start: if the terminal hasn't even begun answering within
//!     20 ms, it doesn't support the query, and waiting longer just
//!     wastes time.
//!
//!   * Called once at startup and cached. Resizes reuse the same
//!     `CellPx`; they don't re-probe.

// `Read` is only used by the unix probe (byte-by-byte stdin poll); on
// Windows we read with ReadConsoleInputW — importing it there is a
// warning.
#[cfg(unix)]
use std::io::Read;
use std::io::Write;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct CellPx {
    pub w: u32,
    pub h: u32,
    pub source: CellPxSource,
}

#[derive(Debug, Clone, Copy)]
pub enum CellPxSource {
    Csi16t,
    Csi14t,
    Heuristic,
}

impl CellPxSource {
    pub fn short(&self) -> &'static str {
        match self {
            CellPxSource::Csi16t => "csi16",
            CellPxSource::Csi14t => "csi14",
            CellPxSource::Heuristic => "heur",
        }
    }
}

/// Probe timeout. On Unix the roundtrip is local (PTY) and 20 ms is
/// plenty. On Windows the answer travels through conpty (WT ⇄ conhost
/// ⇄ process) and can take quite a bit longer; since the probe runs
/// once at startup, 150 ms is unnoticeable and avoids false negatives.
const PROBE_TIMEOUT_MS: u64 = if cfg!(windows) { 150 } else { 20 };

/// Return the cell size, following the rules described in the module
/// docs. `cols`/`rows` are the terminal's logical size (in case CSI
/// 14t hands us the total area and we have to divide).
pub fn probe_cell_px(cols: u16, rows: u16) -> CellPx {
    if !terminal_supports_pixel_query() {
        return heuristic();
    }
    // Now we try CSI 16t (ultra-short timeout).
    if let Some((h, w)) = query_and_parse(b"\x1b[16t", b't', 6, PROBE_TIMEOUT_MS) {
        if w > 0 && h > 0 && w < 200 && h < 200 {
            return CellPx {
                w,
                h,
                source: CellPxSource::Csi16t,
            };
        }
    }
    // Second attempt: CSI 14t (total size of the text area).
    if let Some((total_h, total_w)) = query_and_parse(b"\x1b[14t", b't', 4, PROBE_TIMEOUT_MS) {
        if cols > 0 && rows > 0 && total_w > 0 && total_h > 0 {
            let cw = (total_w / cols as u32).max(1);
            let ch = (total_h / rows as u32).max(1);
            if cw < 200 && ch < 200 {
                return CellPx {
                    w: cw,
                    h: ch,
                    source: CellPxSource::Csi14t,
                };
            }
        }
    }
    heuristic()
}

/// Default heuristic for terminals that don't answer queries. 8×16 is
/// the typical cell size for a 10pt monospaced font (Consolas,
/// Cascadia Mono, Menlo, DejaVu Sans Mono, ...).
fn heuristic() -> CellPx {
    CellPx {
        w: 8,
        h: 16,
        source: CellPxSource::Heuristic,
    }
}

/// Is this terminal one of the known ones that answers `CSI 16 t` /
/// `14 t`?
///
/// Conservative allowlist: we only enable the probe when we're very
/// sure it will answer. When in doubt, heuristic.
fn terminal_supports_pixel_query() -> bool {
    // Windows: only Windows Terminal. Modern WT (the only one with
    // sixel on Windows) does answer CSI 16t/14t; the legacy consoles
    // (conhost, cmd) don't, and probing them just burns the timeout.
    // WT exports WT_SESSION in every profile.
    //
    // This is what cures the "off-center, small video" in WT with
    // sixel: the 8×16 heuristic underestimates the real cell (e.g.
    // Cascadia Mono at typical sizes/DPI is 9×19-12×24) → rtv thought
    // it was filling the width (offset 0) but the image sat in the
    // top-left corner taking up ~80%.
    #[cfg(windows)]
    {
        return std::env::var("WT_SESSION").is_ok()
            || std::env::var("WT_PROFILE_ID").is_ok();
    }

    #[cfg(not(windows))]
    {
        let term = std::env::var("TERM").unwrap_or_default();
        let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();

        // Kitty: its own env var, plus TERM=xterm-kitty.
        if std::env::var("KITTY_WINDOW_ID").is_ok() {
            return true;
        }
        // Windows Terminal via WSL: same situation as native Windows.
        if std::env::var("WT_SESSION").is_ok() {
            return true;
        }
        if term.contains("kitty") {
            return true;
        }
        // WezTerm: WEZTERM_EXECUTABLE env var, or TERM_PROGRAM=WezTerm.
        if std::env::var("WEZTERM_EXECUTABLE").is_ok()
            || term_program.eq_ignore_ascii_case("WezTerm")
        {
            return true;
        }
        // Ghostty
        if term_program.eq_ignore_ascii_case("ghostty") {
            return true;
        }
        // iTerm2
        if term_program.eq_ignore_ascii_case("iTerm.app") {
            return true;
        }
        // foot
        if term == "foot" || term == "foot-extra" {
            return true;
        }
        // Konsole
        if std::env::var("KONSOLE_VERSION").is_ok() {
            return true;
        }
        // Modern xterm: answers CSI 14t, not always 16t. Allowed
        // because the timeout is so low (20 ms) that the cost is
        // minimal.
        if term.starts_with("xterm") {
            return true;
        }
        false
    }
}

/// Send `query` to stdout and read the answer with a `timeout_ms`
/// timeout. Real code only compiles on Unix; the Windows build has its
/// own variant below.
#[cfg(unix)]
fn query_and_parse(
    query: &[u8],
    terminator: u8,
    expected_prefix: u32,
    timeout_ms: u64,
) -> Option<(u32, u32)> {
    let mut out = std::io::stdout();
    out.write_all(query).ok()?;
    out.flush().ok()?;

    // Make stdin non-blocking — before reading. Restored on exit.
    set_stdin_nonblock(true);
    let _guard = NonblockGuard;

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut buf = Vec::with_capacity(32);
    let mut handle = std::io::stdin().lock();

    while Instant::now() < deadline {
        let mut byte = [0u8; 1];
        match handle.read(&mut byte) {
            Ok(1) => {
                buf.push(byte[0]);
                if byte[0] == terminator {
                    break;
                }
                // Sanity: if the buffer grows too much without a
                // terminator, the terminal is sending real input and
                // not the probe answer → bail.
                if buf.len() > 40 {
                    return None;
                }
            }
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => break,
        }
    }

    parse_response(&buf, terminator, expected_prefix)
}

/// Windows: the terminal's answer arrives through the console input
/// buffer as KEY_EVENTs (with ENABLE_VIRTUAL_TERMINAL_INPUT, which
/// crossterm already switched on with raw mode — the probe runs after
/// `TerminalGuard::enter`). We read it with `ReadConsoleInputW` using
/// `WaitForSingleObject` as a real timeout, never blocking: if the
/// terminal doesn't answer within `timeout_ms`, we move on with the
/// heuristic. Hand-rolled FFI (3 kernel32 functions) to avoid
/// dragging in `winapi`.
#[cfg(windows)]
fn query_and_parse(
    query: &[u8],
    terminator: u8,
    expected_prefix: u32,
    timeout_ms: u64,
) -> Option<(u32, u32)> {
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const WAIT_OBJECT_0: u32 = 0;
    const KEY_EVENT: u16 = 0x0001;

    // Exact layout of KEY_EVENT_RECORD / INPUT_RECORD (wincon.h):
    // INPUT_RECORD = { WORD EventType; <pad 2>; union Event (16 bytes) }.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct KeyEventRecord {
        key_down: i32,
        repeat_count: u16,
        virtual_key_code: u16,
        virtual_scan_code: u16,
        unicode_char: u16,
        control_key_state: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct InputRecord {
        event_type: u16,
        _pad: u16,
        event: KeyEventRecord,
    }
    extern "system" {
        // Same signature as the declaration in main.rs (HANDLE = *mut
        // c_void): two declarations of the same symbol with different
        // types trip the clashing_extern_declarations lint.
        fn GetStdHandle(n: u32) -> *mut core::ffi::c_void;
        fn WaitForSingleObject(h: isize, ms: u32) -> u32;
        fn ReadConsoleInputW(
            h: isize,
            buf: *mut InputRecord,
            len: u32,
            read: *mut u32,
        ) -> i32;
    }

    let hin = unsafe { GetStdHandle(STD_INPUT_HANDLE) } as isize;
    if hin == 0 || hin == -1 {
        return None;
    }

    let mut out = std::io::stdout();
    out.write_all(query).ok()?;
    out.flush().ok()?;

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut buf: Vec<u8> = Vec::with_capacity(32);
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = (deadline - now).as_millis().max(1) as u32;
        if unsafe { WaitForSingleObject(hin, remaining) } != WAIT_OBJECT_0 {
            break; // timeout or error → no answer
        }
        // Events available: read them (this also dequeues non-key
        // events — focus, mouse — that would keep the handle
        // signaled).
        let zero = InputRecord {
            event_type: 0,
            _pad: 0,
            event: KeyEventRecord {
                key_down: 0,
                repeat_count: 0,
                virtual_key_code: 0,
                virtual_scan_code: 0,
                unicode_char: 0,
                control_key_state: 0,
            },
        };
        let mut recs = [zero; 16];
        let mut nread: u32 = 0;
        if unsafe { ReadConsoleInputW(hin, recs.as_mut_ptr(), 16, &mut nread) } == 0 {
            break;
        }
        for r in recs.iter().take(nread as usize) {
            if r.event_type != KEY_EVENT || r.event.key_down == 0 {
                continue;
            }
            let ch = r.event.unicode_char;
            if ch == 0 || ch > 255 {
                continue;
            }
            for _ in 0..r.event.repeat_count.max(1) {
                buf.push(ch as u8);
            }
            if ch as u8 == terminator {
                return parse_response(&buf, terminator, expected_prefix);
            }
            // Sanity: too many bytes without a terminator → real user
            // input, not the probe answer.
            if buf.len() > 40 {
                return None;
            }
        }
    }
    parse_response(&buf, terminator, expected_prefix)
}

#[cfg(not(any(unix, windows)))]
fn query_and_parse(
    _query: &[u8],
    _terminator: u8,
    _expected_prefix: u32,
    _timeout_ms: u64,
) -> Option<(u32, u32)> {
    None
}

fn parse_response(buf: &[u8], terminator: u8, expected_prefix: u32) -> Option<(u32, u32)> {
    if buf.len() < 6 {
        return None;
    }
    let s = std::str::from_utf8(buf).ok()?;
    let s = s.strip_prefix('\x1b')?;
    let s = s.strip_prefix('[')?;
    let s = s.strip_suffix(terminator as char)?;
    let mut parts = s.split(';');
    let prefix: u32 = parts.next()?.parse().ok()?;
    if prefix != expected_prefix {
        return None;
    }
    let a: u32 = parts.next()?.parse().ok()?;
    let b: u32 = parts.next()?.parse().ok()?;
    Some((a, b))
}

#[cfg(unix)]
struct NonblockGuard;
#[cfg(unix)]
impl Drop for NonblockGuard {
    fn drop(&mut self) {
        set_stdin_nonblock(false);
    }
}

#[cfg(unix)]
fn set_stdin_nonblock(enable: bool) {
    use std::os::unix::io::AsRawFd;
    extern "C" {
        fn fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
    }
    const F_GETFL: i32 = 3;
    const F_SETFL: i32 = 4;
    const O_NONBLOCK: i32 = 0o4000;
    unsafe {
        let fd = std::io::stdin().as_raw_fd();
        let cur = fcntl(fd, F_GETFL, 0);
        if cur < 0 {
            return;
        }
        let new = if enable {
            cur | O_NONBLOCK
        } else {
            cur & !O_NONBLOCK
        };
        fcntl(fd, F_SETFL, new);
    }
}

/// Adaptive scaling policy. `reserve_bottom_rows` counts the HUD rows
/// (1 or 2). They're subtracted from the usable area before computing
/// pixels.
pub fn adaptive_target_pixels(
    backend: crate::renderer::Backend,
    cols: u16,
    rows: u16,
    cell: CellPx,
    scale: f32,
    reserve_bottom_rows: u16,
) -> (u32, u32) {
    use crate::renderer::Backend;
    // Always keep at least `reserve_bottom_rows` rows, no extra margin.
    let usable_rows = rows.saturating_sub(reserve_bottom_rows).max(1);

    let (px_per_col, px_per_row) = match backend {
        Backend::HalfBlocks => (1u32, 2u32),
        Backend::Ascii => (1, 1),
        Backend::Kitty | Backend::Iterm2 | Backend::Sixel => (cell.w.max(1), cell.h.max(1)),
    };

    let w = (cols as u32 * px_per_col) as f32 * scale;
    let h = (usable_rows as u32 * px_per_row) as f32 * scale;
    ((w as u32).max(2), (h as u32).max(2))
}
