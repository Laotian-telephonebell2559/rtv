#!/usr/bin/env python3
"""QUALITY RECOVERY test when GROWING the terminal.

Reported symptom: shrinking the terminal changes quality instantly,
but growing it takes a while before good quality comes back.

Root cause: the pre-decode queue holds up to ~2.5 s of frames already
scaled to the OLD (small) dims; the player rescales them with nearest
(blurry) until the decoder catches up with the new dims.

The cure: "refine-seek" — 300 ms after the last GROWING resize, the
decoder empties its queue and re-decodes from the current playback
point at the new dims (exact hr-seek with drop_until), without
touching clocks or audio.

Measurement: RTV_SYNC_LOG now includes the displayed frame dims
(columns 6 and 7: `wall master pts avdiff dropped w h`). We measure
the wall time between the first frame logged after sending the grow
SIGWINCH and the first frame with STRICTLY larger dims. Threshold:
< 1.2 s (before the cure: ~2.5 s with a full queue).

We also check that A/V sync stays healthy after recovery (median
|avdiff| < 120 ms) — the refine must not desync anything.

Usage: python3 tests/integration_grow_quality.py <video>
"""
import os, pty, sys, time, subprocess, select, signal
import fcntl, termios, struct, threading, tempfile

VIDEO = sys.argv[1]
BIN = os.environ.get("RTV_BIN") or os.path.join(
    os.path.dirname(__file__), "..", "target", "release", "rtv")

env = dict(os.environ)
env["TERM"] = "xterm-256color"
log_path = tempfile.mktemp(prefix="rtv_grow_", suffix=".log")
env["RTV_SYNC_LOG"] = log_path

master, slave = pty.openpty()

def set_winsize(rows, cols):
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

# Start SMALL: the decoder produces small frames.
set_winsize(14, 46)

proc = subprocess.Popen(
    [BIN, VIDEO, "--backend", "ascii"],
    stdin=slave, stdout=slave, stderr=subprocess.DEVNULL, env=env,
)

# Drain the pty so rtv does not block on writes.
_stop = threading.Event()
def _reader():
    while not _stop.is_set():
        r, _, _ = select.select([master], [], [], 0.02)
        if r:
            try:
                if not os.read(master, 1 << 20):
                    return
            except OSError:
                return
threading.Thread(target=_reader, daemon=True).start()

def read_log():
    rows = []
    try:
        with open(log_path) as f:
            for line in f:
                p = line.split()
                if len(p) >= 7:
                    rows.append((float(p[0]), float(p[3]), int(p[5]), int(p[6])))
    except FileNotFoundError:
        pass
    return rows  # (wall, avdiff_ms, w, h)

fails = []

# 1) Play ~3 s at the small size so the queue fills with small frames
#    (the worst case for this bug).
time.sleep(3.0)
if proc.poll() is not None:
    print("FAIL: rtv died during initial playback")
    sys.exit(1)

pre = read_log()
if not pre:
    print("FAIL: sync-log empty after 3 s of playback")
    proc.kill(); sys.exit(1)
w_old, h_old = pre[-1][2], pre[-1][3]
n_pre = len(pre)
print(f"initial frame dims: {w_old}x{h_old} ({n_pre} frames logged)")

# 2) GROW the terminal at once.
set_winsize(52, 190)
proc.send_signal(signal.SIGWINCH)

# 3) Wait until the first frame with larger dims shows up.
recovery = None
wall_resize_ref = None
deadline = time.monotonic() + 6.0
while time.monotonic() < deadline:
    rows = read_log()
    post = rows[n_pre:]
    if post and wall_resize_ref is None:
        # First frame logged after the resize → time reference on the
        # process's wall clock.
        wall_resize_ref = post[0][0]
    for wall, _av, w, h in post:
        if w > w_old and h > h_old:
            recovery = wall - wall_resize_ref
            break
    if recovery is not None:
        break
    time.sleep(0.05)

if recovery is None:
    fails.append("frames with larger dims never showed up after growing (>6 s)")
else:
    print(f"quality recovery after growing: {recovery*1000:.0f} ms")
    if recovery > 1.2:
        fails.append(f"slow recovery: {recovery*1000:.0f} ms (> 1200 ms)")

# 4) Let it run 2 more seconds and check that sync stays healthy after
#    the refine (median |avdiff| of the frames at the new dims).
time.sleep(2.0)
rows = read_log()
new_dims = [abs(av) for _w0, av, w, h in rows[n_pre:] if w > w_old and h > h_old]
if len(new_dims) < 10:
    fails.append(f"too few frames at the new dims after 2 s ({len(new_dims)})")
else:
    new_dims.sort()
    med = new_dims[len(new_dims) // 2]
    print(f"median |avdiff| post-refine: {med:.1f} ms ({len(new_dims)} frames)")
    if med > 120.0:
        fails.append(f"A/V desynced after the refine: median {med:.1f} ms")

# 5) Clean exit.
if proc.poll() is not None:
    fails.append("rtv died during the test")
else:
    os.write(master, b"q")
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        fails.append("rtv did not quit on 'q' within 5 s")

_stop.set()
try:
    os.unlink(log_path)
except OSError:
    pass

if fails:
    for f in fails:
        print("FAIL:", f)
    sys.exit(1)
print("OK: quality recovery on grow is fast and sync holds up")
