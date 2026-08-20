#!/usr/bin/env python3
"""Integration test: start_time rebase (containers with shifted PTS).

Twitch HLS VODs (and any MPEG-TS captured mid-broadcast) do NOT start
at PTS 0: VOD 2848940456 has start_time=62 s with real PTS 62..8432
and a declared duration of 8370. Without the rebase in
source::start_offset():
  * the HUD started "at minute 1" (clock = raw PTS),
  * the progress bar was misaligned,
  * seeks aimed at shifted targets and the last stretch of the video
    was unreachable (clamp [0, duration-0.5] vs real PTS).

This test reproduces the scenario WITHOUT network: it generates a
local MPEG-TS with `-output_ts_offset 900` (start_time≈901 s) and
verifies, via RTV_SYNC_LOG:
  1. the first emitted PTS is ≈0 (not ≈901),
  2. a bar click at ~50% lands near duration/2 (0-based),
  3. a relative backward seek lands where it should,
  4. the process exits cleanly with q (exit 0).

Usage: python3 tests/integration_start_offset.py [binary_path]
       (default target/release/rtv, or $RTV_BIN)
"""

import fcntl
import os
import pty
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

RTV = os.environ.get("RTV_BIN") or os.path.join(
    os.path.dirname(__file__), "..", "target", "release", "rtv"
)
if len(sys.argv) > 1:
    RTV = sys.argv[1]

COLS, ROWS = 100, 30
# 100x30 → 2-line HUD → bar on row ROWS-1, col 5, width 24
# (bar_hitbox/hud_bar_w in player.rs).
BAR_ROW, BAR_COL, BAR_W = ROWS - 1, 5, 24

FAILS = []


def check(name, ok, detail=""):
    tag = "OK" if ok else "FAIL"
    print(f"[start-offset] {tag}: {name}" + (f" — {detail}" if detail else ""))
    if not ok:
        FAILS.append(name)


def make_fixture(path):
    """20 s MPEG-TS with start_time≈901 s (PTS shifted by 900 s)."""
    subprocess.run(
        [
            "ffmpeg", "-y", "-v", "error",
            "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=30:duration=20",
            "-c:v", "libx264", "-preset", "ultrafast", "-g", "30",
            "-output_ts_offset", "900",
            "-f", "mpegts", path,
        ],
        check=True,
    )


def sgr_click(col, row):
    return f"\x1b[<0;{col};{row}M\x1b[<0;{col};{row}m".encode()


def main():
    if not shutil.which("ffmpeg"):
        print("[start-offset] SKIP: ffmpeg no disponible")
        return 0
    if not os.path.isfile(RTV):
        print(f"[start-offset] SKIP: binario no encontrado: {RTV}")
        return 0

    tmp = tempfile.mkdtemp(prefix="rtv-startoff-")
    fixture = os.path.join(tmp, "offset.ts")
    log_path = os.path.join(tmp, "sync.log")
    make_fixture(fixture)

    m, s = pty.openpty()
    # Careful: crossterm queries the terminal size with ioctl
    # (TIOCGWINSZ) — COLUMNS/LINES in the env are NOT enough. Without
    # setting the winsize, bar_hitbox() doesn't exist and clicks are
    # ignored.
    fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    env = dict(os.environ, TERM="xterm-256color", RTV_SYNC_LOG=log_path)
    p = subprocess.Popen(
        [RTV, fixture, "--no-audio", "--backend", "ascii"],
        stdin=s, stdout=s, stderr=subprocess.DEVNULL, env=env, close_fds=True,
    )
    os.close(s)

    def drain(dur):
        end = time.time() + dur
        while time.time() < end:
            r, _, _ = select.select([m], [], [], 0.2)
            if r:
                try:
                    os.read(m, 65536)
                except OSError:
                    return
            if p.poll() is not None:
                return

    try:
        drain(5)
        check("process alive after startup", p.poll() is None)

        # Click at ~50% of the bar → target ≈ duration/2 ≈ 10 s.
        mid_col = BAR_COL + round(0.5 * (BAR_W - 1))
        os.write(m, sgr_click(mid_col, BAR_ROW))
        drain(4)
        check("alive after bar click", p.poll() is None)

        # Relative backward seek (←, -5 s).
        os.write(m, b"\x1b[D")
        drain(3)
        check("alive after backward seek", p.poll() is None)

        os.write(m, b"q")
        for _ in range(40):
            if p.poll() is not None:
                break
            drain(0.25)
        if p.poll() is None:
            p.send_signal(signal.SIGTERM)
            time.sleep(1)
        check("salida limpia con q", p.returncode == 0, f"exit={p.returncode}")
    finally:
        if p.poll() is None:
            p.kill()

    # --- Sync-log analysis ---
    frames = []   # (wall, pts)
    seeks = []    # (idx_in_frames, target, now)
    with open(log_path) as f:
        for line in f:
            if line.startswith("# SEEK"):
                kv = dict(
                    tok.split("=") for tok in line.split() if "=" in tok
                )
                seeks.append((len(frames), float(kv["target"]), float(kv["now"])))
            elif not line.startswith("#"):
                parts = line.split()
                if len(parts) >= 2:
                    frames.append((float(parts[0]), float(parts[1])))

    check("sync-log has frames", len(frames) > 30, f"{len(frames)} frames")
    if frames:
        first_pts = frames[0][1]
        # WITHOUT the rebase, first_pts would be ≈901. WITH it, ≈0.
        check(
            "first PTS rebased to ~0 (not ~901)",
            -0.5 <= first_pts < 5.0,
            f"first_pts={first_pts:.3f}",
        )
    check("2 seeks happened (click + ←)", len(seeks) == 2, f"{len(seeks)} seeks")
    for i, (idx, target, _now) in enumerate(seeks):
        # Target on the 0-based timeline (within [0, 20]).
        check(
            f"seek {i}: 0-based target within the duration",
            -0.5 <= target <= 20.5,
            f"target={target:.3f}",
        )
        # Landing: first post-seek frame near the target
        # (keyframe <= target, 1 s GOP in the fixture).
        post = [pts for _w, pts in frames[idx: idx + 5]]
        if post:
            landing = post[0]
            check(
                f"seek {i}: landing near the target",
                abs(landing - target) < 2.5,
                f"landing={landing:.3f} target={target:.3f}",
            )

    if FAILS:
        print(f"[start-offset] {len(FAILS)} failures: {FAILS}")
        return 1
    print("[start-offset] ALL OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
