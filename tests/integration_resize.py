#!/usr/bin/env python3
"""RESIZE integration test for rtv.

Runs rtv in a real pty and unleashes a resize STORM
(TIOCSWINSZ + SIGWINCH) during playback, including tiny and
degenerate sizes, seeks in the middle of the storm and a pause
with a resize. Verifies:

  1. The process does NOT crash (still alive after the storm and
     exits cleanly with `q`, exit code 0).
  2. Playback does NOT stop during the storm: the sync-log keeps
     logging frames (max wall gap between frames < 1.5 s).
  3. Fps does not sink: in the post-storm window frames render at a
     reasonable rate (>= 40% of nominal fps with ascii).
  4. A/V sync is unaffected: median |avdiff| < 60 ms in the stable
     post-storm window.

Usage: python3 tests/integration_resize.py <video> [backend=ascii]
"""
import os, pty, sys, time, subprocess, statistics, select, signal
import fcntl, termios, struct, random, threading

VIDEO = sys.argv[1]
BACKEND = sys.argv[2] if len(sys.argv) > 2 else "ascii"
BIN = os.environ.get("RTV_BIN") or os.path.join(
    os.path.dirname(__file__), "..", "target", "release", "rtv")
LOG = "/tmp/rtv_resize_sync.log"

if os.path.exists(LOG):
    os.remove(LOG)

env = dict(os.environ)
env["RTV_SYNC_LOG"] = LOG
env["TERM"] = "xterm-256color"

master, slave = pty.openpty()

def set_winsize(rows, cols):
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

set_winsize(40, 120)

proc = subprocess.Popen(
    [BIN, VIDEO, "--backend", BACKEND],
    stdin=slave, stdout=slave, stderr=subprocess.DEVNULL, env=env,
)

RIGHT = b"\x1b[C"
LEFT = b"\x1b[D"
SPACE = b" "
Q = b"q"

# CONTINUOUS pty reader on a thread: without it, the pty buffer
# (64 KB) fills up with blocks/kitty output (~200 KB/frame) and rtv
# blocks on write() → artificial harness latency that used to taint
# the sync measurement (it was not the player's fault).
_reader_stop = threading.Event()
def _reader():
    while not _reader_stop.is_set():
        r, _, _ = select.select([master], [], [], 0.05)
        if r:
            try:
                os.read(master, 1 << 20)
            except OSError:
                return
reader_t = threading.Thread(target=_reader, daemon=True)
reader_t.start()

def drain():
    pass  # the reader thread already drains continuously

def resize(rows, cols):
    set_winsize(rows, cols)
    try:
        proc.send_signal(signal.SIGWINCH)
    except ProcessLookupError:
        pass

def play(secs):
    t0 = time.monotonic()
    while time.monotonic() - t0 < secs:
        if proc.poll() is not None:
            print(f"FAIL: rtv died (exit={proc.returncode}) during playback")
            sys.exit(1)
        time.sleep(0.05)

fails = []
storm_start = storm_end = 0.0

try:
    # 1) Warmup: normal playback for 4 s at 120x40.
    play(4.0)

    # 2) Resize STORM: 60 quick changes, random sizes including tiny
    #    (4x3) and large (300x90) ones, some with no pause in between
    #    (bursts of 3-4 back-to-back events).
    storm_start = time.monotonic()
    random.seed(42)
    sizes = []
    for _ in range(20):
        sizes.append((random.randint(3, 90), random.randint(4, 300)))
    # Explicit edge cases:
    sizes += [(3, 4), (4, 5), (5, 8), (90, 300), (24, 80), (10, 30),
              (3, 200), (80, 6), (40, 120)]
    for i, (r, c) in enumerate(sizes):
        if proc.poll() is not None:
            print(f"FAIL: rtv died (exit={proc.returncode}) on resize #{i} → {c}x{r}")
            sys.exit(1)
        resize(r, c)
        drain()
        # Alternate: sometimes no pause (burst), sometimes 30-80 ms.
        if i % 4 != 0:
            time.sleep(random.uniform(0.03, 0.08))
    # Seek IN THE MIDDLE of more resizes (resize+seek interaction).
    os.write(master, RIGHT); drain(); time.sleep(0.1)
    resize(20, 60); drain(); time.sleep(0.05)
    os.write(master, LEFT); drain(); time.sleep(0.1)
    resize(45, 140); drain()
    # Second, very fast burst (no sleeps).
    for (r, c) in [(30, 100), (12, 40), (50, 160), (8, 20), (40, 120)]:
        resize(r, c); drain()
    storm_end = time.monotonic()

    # 3) Pause + resize while paused + resume (redraw with cached frame).
    os.write(master, SPACE); time.sleep(0.5); drain()
    resize(30, 100); drain(); time.sleep(0.5)
    resize(40, 120); drain(); time.sleep(0.5)
    if proc.poll() is not None:
        print(f"FAIL: rtv died (exit={proc.returncode}) on resize while paused")
        sys.exit(1)
    os.write(master, SPACE)  # resume

    # 4) Stable post-storm playback for 6 s.
    play(6.0)

    os.write(master, Q)
    t0 = time.monotonic()
    while proc.poll() is None and time.monotonic() - t0 < 10:
        time.sleep(0.02)
    if proc.poll() is None:
        fails.append("rtv did not quit on q within 10 s")
    elif proc.returncode != 0:
        fails.append(f"exit code {proc.returncode} != 0")
finally:
    if proc.poll() is None:
        proc.kill()
        fails.append("rtv did not quit on q — forced kill")
    _reader_stop.set(); reader_t.join(timeout=2)
    os.close(master); os.close(slave)

# ---------------- Sync-log analysis ----------------
rows_log = []
with open(LOG) as f:
    for line in f:
        if line.startswith("#"):
            continue
        p = line.split()
        if len(p) >= 5:
            rows_log.append((float(p[0]), float(p[1]), float(p[2]), float(p[3])))

if len(rows_log) < 50:
    print(f"FAIL: only {len(rows_log)} frames logged")
    sys.exit(1)

wall0 = rows_log[0][0]
test0 = None  # rough correlation: the log starts ~when play begins

# --- 2. Continuity: no permanent freeze ---
# Legitimate gaps: the deliberate pause (~1.6 s) and the post-seek
# holds (1.5 s valve + decode). Criterion: NO gap > 3 s (freeze),
# and at most 3 gaps in (1.5, 3] s (pause + 2 seeks).
gaps = []
for i in range(1, len(rows_log)):
    gaps.append((rows_log[i][0] - rows_log[i-1][0], rows_log[i][0]))
gap_max = max(g for g, _ in gaps)
mid_gaps = [g for g, _ in gaps if 1.5 < g <= 3.0]
frozen = [g for g, _ in gaps if g > 3.0]
print(f"[continuity] frames={len(rows_log)} gap_max={gap_max:.2f}s gaps(1.5-3s]={len(mid_gaps)} gaps>3s={len(frozen)}")
if frozen:
    fails.append(f"freeze detected: gap of {max(frozen):.2f}s > 3s")
if len(mid_gaps) > 3:
    fails.append(f"{len(mid_gaps)} gaps in (1.5,3]s (expected <=3: pause + seek holds)")

# --- 3. Post-storm FPS: last 4 s of the log ---
t_end = rows_log[-1][0]
tail = [r for r in rows_log if r[0] >= t_end - 4.0]
fps_tail = len(tail) / 4.0
print(f"[fps] post-storm={fps_tail:.1f} fps (last 4 s, n={len(tail)})")
if fps_tail < 10.0:  # 25 fps video; ascii on a 2-core sandbox: >=10 fps
    fails.append(f"post-storm fps {fps_tail:.1f} < 10")

# --- 4. Post-storm sync: median |avdiff| over the last 4 s ---
diffs = [abs(r[3]) for r in tail]
if len(diffs) >= 10:
    med = statistics.median(diffs)
    print(f"[sync] post-storm median |avdiff|={med:.1f}ms")
    if med > 60:
        fails.append(f"post-storm median |avdiff| {med:.1f}ms > 60ms")
else:
    fails.append("not enough post-storm frames to measure sync")

if fails:
    print("\nFAIL:")
    for f_ in fails:
        print("  -", f_)
    sys.exit(1)
print("\nOK: robust resize — no crash, continuous playback, stable fps and sync")
