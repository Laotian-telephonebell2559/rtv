#!/usr/bin/env python3
"""Integration test: mouse-clickable progress bar.

Runs rtv in a pty (120x40 → 2-line HUD, 40-cell bar on the
second-to-last row) and injects SGR mouse events (\\x1b[<0;COL;ROWM)
just like a real terminal with mouse capture enabled emits them.

Checks:
  1. rtv ENABLES mouse capture on startup (emits ?1000/?1006 h) and
     DISABLES it on exit (l) — without this the user's terminal is
     left "eaten" after exiting.
  2. Click at 75% of the bar → forward seek: the sync-log PTS jumps
     to ~75% of the duration (±10%).
  3. Click at 25% of the bar → backward seek: the PTS goes back to
     ~25% (±10%).
  4. Click OUTSIDE the bar (center of the video) → NO seek (no
     additional PTS jump >2 s).
  5. Clean exit with `q` (rc=0).
"""
import os
import pty
import re
import select
import struct
import subprocess
import sys
import termios
import time
import fcntl

VIDEO = sys.argv[1]
RTV = os.environ.get("RTV_BIN") or os.path.join(
    os.path.dirname(__file__), "..", "target", "release", "rtv")
LOG = "/tmp/rtv_mouse_sync.log"
COLS, ROWS = 120, 40
FAIL = 0


def check(name, ok, detail=""):
    global FAIL
    tag = "PASS" if ok else "FAIL"
    print(f"[{tag}] {name}" + (f" — {detail}" if detail else ""))
    if not ok:
        FAIL = 1


def sgr_click(col, row):
    """Click izquierdo (press+release) en coordenadas 1-based."""
    return f"\x1b[<0;{col};{row}M\x1b[<0;{col};{row}m".encode()


def drain(m, buf):
    while select.select([m], [], [], 0)[0]:
        try:
            data = os.read(m, 65536)
        except OSError:
            return False
        if not data:
            return False
        buf.extend(data)
    return True


def wait(m, buf, dur):
    end = time.time() + dur
    while time.time() < end:
        r, _, _ = select.select([m], [], [], 0.05)
        if r:
            try:
                buf.extend(os.read(m, 65536))
            except OSError:
                break


def parse_log():
    rows = []
    if not os.path.exists(LOG):
        return rows
    with open(LOG) as f:
        for ln in f:
            if ln.startswith("#"):
                continue
            p = ln.split()
            if len(p) >= 4:
                try:
                    rows.append((float(p[0]), float(p[2])))
                except ValueError:
                    pass
    return rows


def main():
    if os.path.exists(LOG):
        os.remove(LOG)
    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
    env["RTV_SYNC_LOG"] = LOG

    duration = float(
        subprocess.run(
            ["ffprobe", "-v", "error", "-show_entries", "format=duration",
             "-of", "csv=p=0", VIDEO],
            capture_output=True, text=True, check=True,
        ).stdout.strip()
    )

    m, s = pty.openpty()
    fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    p = subprocess.Popen(
        [RTV, VIDEO, "--backend", "ascii"],
        stdin=s, stdout=s, stderr=subprocess.DEVNULL, env=env, close_fds=True,
    )
    os.close(s)

    raw = bytearray()
    wait(m, raw, 3.0)  # startup + normal playback

    # Bar geometry (must match the player's bar_hitbox):
    # 2-line HUD → bar on row ROWS-1, cols 5..5+40-1, bar_w=40.
    bar_row, bar_col, bar_w = ROWS - 1, 5, 40

    # --- click at 75% ---
    col75 = bar_col + round(0.75 * (bar_w - 1))
    os.write(m, sgr_click(col75, bar_row))
    wait(m, raw, 3.0)

    # --- click at 25% ---
    col25 = bar_col + round(0.25 * (bar_w - 1))
    os.write(m, sgr_click(col25, bar_row))
    wait(m, raw, 3.0)

    # --- click outside the bar (center of the video) ---
    os.write(m, sgr_click(COLS // 2, ROWS // 2))
    wait(m, raw, 2.5)

    os.write(m, b"q")
    t0 = time.time()
    while p.poll() is None and time.time() - t0 < 5.0:
        if not drain(m, raw):
            break
        time.sleep(0.05)
    try:
        rc = p.wait(timeout=3)
    except subprocess.TimeoutExpired:
        p.kill()
        p.wait()
        rc = -9
    # FINAL drain: the teardown sequences (DisableMouseCapture,
    # LeaveAlternateScreen…) are written right before exit and can
    # linger in the pty buffer after p.wait().
    end = time.time() + 1.0
    while time.time() < end:
        r, _, _ = select.select([m], [], [], 0.1)
        if not r:
            break
        try:
            data = os.read(m, 65536)
        except OSError:
            break
        if not data:
            break
        raw.extend(data)
    try:
        os.close(m)
    except OSError:
        pass

    out = bytes(raw).decode("utf-8", "replace")
    check("clean exit with q", rc == 0, f"rc={rc}")

    # 1. Mouse capture enabled and disabled.
    on = re.search(r"\x1b\[\?1000h", out) or re.search(r"\x1b\[\?100[26]h", out)
    off = re.search(r"\x1b\[\?1000l", out) or re.search(r"\x1b\[\?100[26]l", out)
    check("mouse capture ON at startup", bool(on), "?1000/?1006 h was not emitted")
    check("mouse capture OFF on exit", bool(off), "?1000/?1006 l was not emitted")

    rows = parse_log()
    check("sync-log has frames", len(rows) > 30, f"{len(rows)} frames")
    if not rows:
        return finish()

    # Detect PTS jumps > 2 s.
    jumps = []
    for i in range(1, len(rows)):
        if abs(rows[i][1] - rows[i - 1][1]) > 2.0:
            jumps.append(i)

    check("exactly 2 click seeks happened", len(jumps) == 2,
          f"{len(jumps)} jumps detected")

    if len(jumps) >= 1:
        pts75 = rows[jumps[0]][1]
        tgt = 0.75 * duration
        check("75% click lands at ~75%", abs(pts75 - tgt) < duration * 0.10,
              f"pts={pts75:.1f}s, expected ~{tgt:.1f}s")
    if len(jumps) >= 2:
        pts25 = rows[jumps[1]][1]
        tgt = 0.25 * duration
        check("25% click lands at ~25%", abs(pts25 - tgt) < duration * 0.10,
              f"pts={pts25:.1f}s, expected ~{tgt:.1f}s")

    return finish()


def finish():
    print("\n" + ("ALL OK" if FAIL == 0 else "THERE ARE FAILURES"))
    sys.exit(FAIL)


if __name__ == "__main__":
    main()
