#!/usr/bin/env python3
"""Hardware decode (--hwdec) integration test.

This sandbox has NO /dev/dri and no GPU: it's the perfect NEGATIVE
environment to validate hwdec's main contract — the transparent
fallback to software. The positive path (hwaccel actually active)
needs a real GPU and is documented as pending in the README.

Checks:

  1. --hwdec auto, none and vaapi play N seconds in a pty and exit
     with 0 (q). vaapi without /dev/dri MUST degrade to software
     without aborting or dirtying the TUI.

  2. Each mode's sync-log has a comparable frame count (they all end
     up decoding in software → same pipeline) and A/V sync is healthy:
     median |avdiff| < 120 ms — the same thresholds as
     integration_sync.py. The fallback must not cost sync.

  3. --hwdec badvalue → exit 2 with a usage message on stderr
     (validation BEFORE stderr is silenced).

Usage: python3 tests/integration_hwdec.py <video>
"""
import os, pty, sys, time, subprocess, select, signal
import fcntl, termios, struct, threading, tempfile, statistics

VIDEO = sys.argv[1]
BIN = os.environ.get("RTV_BIN") or os.path.join(
    os.path.dirname(__file__), "..", "target", "release", "rtv")

PLAY_SECS = 6.0
MIN_FRAMES = 40          # ~6 s a 25 fps con margen amplio de warmup
AVDIFF_MEDIAN_MS = 120.0 # mismo umbral que integration_sync.py

fails = []


def run_mode(mode):
    """Reproduce PLAY_SECS con --hwdec <mode> en un pty. Devuelve
    (exit_code, rows) donde rows = [(wall, avdiff_ms), ...]."""
    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
    log_path = tempfile.mktemp(prefix=f"rtv_hwdec_{mode}_", suffix=".log")
    env["RTV_SYNC_LOG"] = log_path

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))

    proc = subprocess.Popen(
        [BIN, VIDEO, "--backend", "ascii", "--hwdec", mode],
        stdin=slave, stdout=slave, stderr=subprocess.DEVNULL, env=env,
    )

    # Drenar el pty: sin esto rtv se bloquea escribiendo frames.
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

    time.sleep(PLAY_SECS)
    os.write(master, b"q")
    try:
        code = proc.wait(timeout=8)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
        code = -9
    _stop.set()
    os.close(master)
    os.close(slave)

    # ROBUST sync-log read: in this sandbox (and generally on
    # filesystems with delayed cache visibility — 9p/WSL, overlays) a
    # read() right after wait() can return 0 bytes even when stat()
    # already reports the final size; a few ms later the same file
    # returns all the content. It was the cause of the flaky
    # "0 frames in a random mode with exit 0" (~1/6 runs): rtv did
    # play and wrote the full log. Retry for up to 2 s if the read
    # comes back empty.
    def read_rows():
        out = []
        try:
            with open(log_path) as f:
                for line in f:
                    if line.startswith("#"):
                        continue
                    p = line.split()
                    if len(p) >= 4:
                        try:
                            out.append((float(p[0]), float(p[3])))
                        except ValueError:
                            pass  # cabecera
        except FileNotFoundError:
            pass
        return out

    rows = read_rows()
    deadline = time.time() + 2.0
    while not rows and time.time() < deadline:
        time.sleep(0.05)
        rows = read_rows()
    try:
        os.unlink(log_path)
    except OSError:
        pass
    return code, rows


# ── 1+2: auto / none / vaapi deben reproducir con sync sano ─────────
frame_counts = {}
for mode in ("auto", "none", "vaapi"):
    code, rows = run_mode(mode)
    n = len(rows)
    frame_counts[mode] = n
    print(f"--hwdec {mode}: exit={code} frames={n}")
    if code != 0:
        fails.append(f"--hwdec {mode}: exit {code} != 0")
        continue
    if n < MIN_FRAMES:
        fails.append(f"--hwdec {mode}: solo {n} frames (< {MIN_FRAMES})")
        continue
    # Sync: descartar el warmup (primer segundo) como en los otros tests.
    t0 = rows[0][0]
    settled = [abs(av) for (w, av) in rows if w - t0 > 1.0]
    if settled:
        med = statistics.median(settled)
        print(f"  median |avdiff| after warmup: {med:.1f} ms")
        if med > AVDIFF_MEDIAN_MS:
            fails.append(f"--hwdec {mode}: median avdiff {med:.1f} ms > {AVDIFF_MEDIAN_MS}")

# All three modes end up in software in this sandbox → comparable
# frame counts (±40%). Catches a fallback that "plays" at 2 fps.
if all(m in frame_counts and frame_counts[m] >= MIN_FRAMES for m in ("auto", "none", "vaapi")):
    base = frame_counts["none"]
    for mode in ("auto", "vaapi"):
        ratio = frame_counts[mode] / base
        if not (0.6 <= ratio <= 1.4):
            fails.append(
                f"--hwdec {mode}: {frame_counts[mode]} frames vs none={base} (ratio {ratio:.2f} outside 0.6-1.4)"
            )

# ── 3: invalid CLI → exit 2 with a visible message ──────────────────
p = subprocess.run([BIN, VIDEO, "--hwdec", "badvalue"],
                   capture_output=True, text=True, timeout=10)
print(f"--hwdec badvalue: exit={p.returncode}")
if p.returncode != 2:
    fails.append(f"--hwdec badvalue: exit {p.returncode} != 2")
if "not recognized" not in p.stderr:
    fails.append("--hwdec badvalue: usage message missing from stderr")

if fails:
    print("\nFAIL:")
    for f in fails:
        print("  *", f)
    sys.exit(1)
print("\nOK: transparent fallback, healthy sync and correct CLI validation")
