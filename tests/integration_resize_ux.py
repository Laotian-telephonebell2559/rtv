#!/usr/bin/env python3
"""RESIZE UX test for rtv — latency, visual garbage and flicker.

Covers exactly the reported symptoms:

  A. LATENCY: the redraw after a resize must be (nearly) instant.
     We measure the time between sending SIGWINCH and seeing the
     redraw clear (`ESC[2J`). Threshold: p95 < 250 ms (before: up to
     ~500 ms due to the non-interruptible thread::sleep + a queue of
     frames at the old dims).

  B. VISUAL GARBAGE on a small terminal: after shrinking to a tiny
     size, NO cursor positioning sequence may point out of bounds
     (row > rows or column > cols) and there must be no stray
     newlines (scroll). The stream must also carry autowrap disabled
     (DECAWM off, `ESC[?7l`).

  C. HUD FLICKER: during stable playback the HUD should only be
     rewritten when its content changes (~1-2 times/s from the
     clock), not at full fps. We count writes to the HUD row per
     second. Threshold: <= 4/s. And on a tiny terminal (< 16 cols or
     < 5 rows) the HUD must be HIDDEN (0 writes).

  D. Coherent render on a small terminal (pyte): we emulate the
     terminal and check that after the small resize the screen holds
     video content within bounds, with no HUD leftovers.

Usage: python3 tests/integration_resize_ux.py <video>
"""
import os, pty, re, sys, time, subprocess, select, signal
import fcntl, termios, struct, threading

import pyte

VIDEO = sys.argv[1]
BIN = os.environ.get("RTV_BIN") or os.path.join(
    os.path.dirname(__file__), "..", "target", "release", "rtv")

env = dict(os.environ)
env["TERM"] = "xterm-256color"

master, slave = pty.openpty()

def set_winsize(rows, cols):
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

COLS0, ROWS0 = 100, 30
set_winsize(ROWS0, COLS0)

proc = subprocess.Popen(
    [BIN, VIDEO, "--backend", "ascii"],
    stdin=slave, stdout=slave, stderr=subprocess.DEVNULL, env=env,
)

# ---- Continuous reader with per-chunk timestamps ----
chunks = []           # [(t, bytes)]
chunks_lock = threading.Lock()
_stop = threading.Event()

def _reader():
    while not _stop.is_set():
        r, _, _ = select.select([master], [], [], 0.02)
        if r:
            try:
                data = os.read(master, 1 << 20)
            except OSError:
                return
            if data:
                with chunks_lock:
                    chunks.append((time.monotonic(), data))

reader_t = threading.Thread(target=_reader, daemon=True)
reader_t.start()

def resize(rows, cols):
    set_winsize(rows, cols)
    proc.send_signal(signal.SIGWINCH)

def wait_alive(secs):
    t0 = time.monotonic()
    while time.monotonic() - t0 < secs:
        if proc.poll() is not None:
            print(f"FAIL: rtv died (exit={proc.returncode})")
            sys.exit(1)
        time.sleep(0.05)

def bytes_since(t):
    with chunks_lock:
        return b"".join(d for (ts, d) in chunks if ts >= t)

def find_after(t_mark, needle, timeout=2.0):
    """Returns the timestamp of the first chunk >= t_mark containing
    `needle`, or None."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        with chunks_lock:
            for ts, d in chunks:
                if ts >= t_mark and needle in d:
                    return ts
        time.sleep(0.005)
    return None

fails = []

# ================= Warmup =================
wait_alive(3.0)

raw_start = bytes_since(0)
# B(partial): autowrap disabled at startup.
if b"\x1b[?7l" not in raw_start:
    fails.append("autowrap (ESC[?7l) is not disabled at startup")

# ================= A. Resize latency =================
latencies = []
sizes = [(20, 60), (35, 110), (15, 45), (40, 130), (25, 80),
         (12, 36), (45, 150), (30, 100)]
for (r, c) in sizes:
    time.sleep(0.35)  # let it settle (no pending 2J)
    t_mark = time.monotonic()
    resize(r, c)
    t_seen = find_after(t_mark, b"\x1b[2J", timeout=2.0)
    if t_seen is None:
        fails.append(f"resize to {c}x{r}: no redraw (2J) within 2 s")
    else:
        latencies.append(t_seen - t_mark)
if latencies:
    latencies.sort()
    p95 = latencies[max(0, int(len(latencies) * 0.95) - 1)]
    print(f"[latency] n={len(latencies)} min={latencies[0]*1000:.0f}ms "
          f"median={latencies[len(latencies)//2]*1000:.0f}ms p95={p95*1000:.0f}ms")
    if p95 > 0.25:
        fails.append(f"resize p95 latency {p95*1000:.0f}ms > 250ms")

# ================= C1. HUD flicker in steady state =================
resize(30, 100)
time.sleep(0.5)
t0 = time.monotonic()
wait_alive(3.0)
raw = bytes_since(t0)
# Writes to row 30 (1-line HUD at 100x30) — ESC[30;1H pattern.
hud_writes = raw.count(b"\x1b[30;1H")
rate = hud_writes / 3.0
print(f"[hud] writes/s in steady state = {rate:.1f}")
if rate > 4.0:
    fails.append(f"HUD rewritten {rate:.1f} times/s (>4) — flicker")

# ================= B + C2 + D. Tiny terminal =================
TR, TC = 4, 12   # 12 cols × 4 rows: below the HUD threshold
resize(TR, TC)
time.sleep(0.4)  # headroom to drain frames at the old dims
t0 = time.monotonic()
wait_alive(2.5)
raw = bytes_since(t0)

# B: no cursor position out of bounds.
cup = re.compile(rb"\x1b\[(\d+);(\d+)H")
out_of_bounds = []
for m in cup.finditer(raw):
    rr, cc = int(m.group(1)), int(m.group(2))
    if rr > TR or cc > TC:
        out_of_bounds.append((rr, cc))
print(f"[bounds] positions outside {TC}x{TR}: {len(out_of_bounds)}"
      + (f" e.g.={out_of_bounds[:5]}" if out_of_bounds else ""))
if out_of_bounds:
    fails.append(f"{len(out_of_bounds)} out-of-bounds writes on a "
                 f"{TC}x{TR} terminal (visual garbage)")

# B: no stray newlines that would cause scroll.
if b"\n" in raw.replace(b"\r\n", b""):
    fails.append("newlines in the render stream (ghost scroll)")

# C2: HUD hidden on a tiny terminal (no write to row TR that is HUD
# text — the video itself may still paint row TR).
# Textual verification with pyte below.
hud_row_writes = raw.count(f"\x1b[{TR};1H".encode())

# D: pyte emulation — the screen must not contain HUD text
# ("q=", "vol", "fps", "▶") and must contain video content (gradient chars).
screen = pyte.Screen(TC, TR)
stream = pyte.ByteStream(screen)
screen.resize(TR, TC)
try:
    stream.feed(raw)
except Exception as e:
    fails.append(f"pyte could not parse the stream: {e}")
disp = "\n".join(screen.display)
for token in ("q=", "vol", "fps", "▶", "⏸"):
    if token in disp:
        fails.append(f"HUD visible on a tiny terminal ({token!r} on screen)")
        break
video_chars = sum(disp.count(ch) for ch in ".:-=+*#%@")
print(f"[tiny] screen {TC}x{TR}: hud_row_writes={hud_row_writes} "
      f"video_chars={video_chars}")
if video_chars == 0:
    fails.append("no video content on a tiny terminal")

# ================= Back to large: recovery =================
resize(35, 120)
t_mark = time.monotonic()
t_seen = find_after(t_mark, b"\x1b[2J", timeout=2.0)
if t_seen is None:
    fails.append("no redraw when growing back")
else:
    print(f"[recovery] redraw on grow in {(t_seen-t_mark)*1000:.0f}ms")
wait_alive(2.0)

# ================= Clean exit =================
os.write(master, b"q")
t0 = time.monotonic()
while proc.poll() is None and time.monotonic() - t0 < 10:
    time.sleep(0.02)
if proc.poll() is None:
    proc.kill()
    fails.append("rtv did not quit on q")
elif proc.returncode != 0:
    fails.append(f"exit code {proc.returncode} != 0")

_stop.set(); reader_t.join(timeout=2)
os.close(master); os.close(slave)

if fails:
    print("\nFAIL:")
    for f in fails:
        print(" -", f)
    sys.exit(1)
print("\nOK: instant resize, no visual garbage or flicker on a small terminal")
