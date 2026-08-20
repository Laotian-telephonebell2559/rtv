//! Non-blocking keyboard and mouse input via crossterm.
//!
//! The main loop polls with `event::poll(Duration::ZERO)` so we don't
//! need a dedicated input thread.

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub enum Cmd {
    Quit,
    TogglePause,
    SeekRel(f64),
    VolumeDelta(i32),
    Resize(u16, u16),
    /// Cycle the audio track (+1 = next, -1 = previous).
    CycleAudio(i32),
    /// Cycle the subtitle track (+1 = next, -1 = previous; the cycle
    /// includes "off" and the external --sub track when present).
    CycleSubs(i32),
    /// Left mouse click/drag at (col, row), 1-based (the top-left cell
    /// is (1,1), matching CSI sequences). The player interprets it:
    /// on the HUD progress bar it becomes a proportional seek,
    /// anywhere else it is ignored.
    MouseClick(u16, u16),
    None,
}

/// Drain every pending event without blocking.
///
/// `Resize` events are coalesced: during a resize storm (dragging the
/// window edge) the terminal queues dozens of events, and handling each
/// one meant redrawing the cached frame plus a full screen clear per
/// event, which piled up latency and caused flicker. Each resize
/// carries absolute dimensions, so only the last one matters.
pub fn poll_command() -> std::io::Result<Vec<Cmd>> {
    let mut out = Vec::new();
    while event::poll(Duration::ZERO)? {
        let ev = event::read()?;
        match ev {
            Event::Key(KeyEvent { code, modifiers, kind, .. }) => {
                if kind != KeyEventKind::Press && kind != KeyEventKind::Repeat {
                    continue;
                }
                let cmd = match (code, modifiers) {
                    (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => Cmd::Quit,
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => Cmd::Quit,
                    (KeyCode::Char(' '), _) => Cmd::TogglePause,
                    (KeyCode::Left, _) => Cmd::SeekRel(-5.0),
                    (KeyCode::Right, _) => Cmd::SeekRel(5.0),
                    (KeyCode::Up, _) => Cmd::VolumeDelta(5),
                    (KeyCode::Down, _) => Cmd::VolumeDelta(-5),
                    // Runtime track switching, mpv style: `#` cycles
                    // audio and `j`/`J` cycles subtitles. `a`/`A` is a
                    // friendlier alias for audio.
                    (KeyCode::Char('a'), _) | (KeyCode::Char('#'), _) => Cmd::CycleAudio(1),
                    (KeyCode::Char('A'), _) => Cmd::CycleAudio(-1),
                    (KeyCode::Char('j'), _) => Cmd::CycleSubs(1),
                    (KeyCode::Char('J'), _) => Cmd::CycleSubs(-1),
                    _ => Cmd::None,
                };
                if !matches!(cmd, Cmd::None) {
                    out.push(cmd);
                }
            }
            Event::Resize(c, r) => {
                // Coalesce: replace any earlier Resize still queued.
                out.retain(|c| !matches!(c, Cmd::Resize(..)));
                out.push(Cmd::Resize(c, r));
            }
            Event::Mouse(m) => {
                // Down = click; Drag = scrubbing (dragging along the
                // bar keeps repositioning). Coalesced just like
                // resizes: one drag produces dozens of events per
                // frame and only the last matters — otherwise every
                // event triggered a full seek (drain the queue plus
                // re-decode) and the burst overwhelmed the decoder.
                if matches!(
                    m.kind,
                    MouseEventKind::Down(MouseButton::Left)
                        | MouseEventKind::Drag(MouseButton::Left)
                ) {
                    out.retain(|c| !matches!(c, Cmd::MouseClick(..)));
                    // crossterm reports 0-based coordinates; the
                    // player works in 1-based screen coordinates.
                    out.push(Cmd::MouseClick(m.column + 1, m.row + 1));
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Block until a terminal event arrives or `timeout` expires. Returns
/// `true` if an event is pending. This is what makes resize feel
/// instant: the player parks its inter-frame waits here instead of in
/// `thread::sleep`, so a resize or keypress interrupts the wait and
/// gets handled in under a millisecond rather than after the sleep
/// runs out (up to 500 ms).
pub fn wait_event(timeout: Duration) -> bool {
    event::poll(timeout).unwrap_or(false)
}
