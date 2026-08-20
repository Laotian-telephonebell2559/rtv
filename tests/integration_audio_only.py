#!/usr/bin/env python3
"""Integration test: audio-only files (no video stream).

Previously, `rtv song.mp3` failed with "no video stream found". Now
decoder::spawn detects the case and sets up a synthetic visualization
pipeline (spawn_audio_only) with the same contract as the real
decoder. This test verifies, with an mp3 and a flac generated on the
fly:
  1. the player starts and emits frames (sync-log with PTS from ~0),
  2. bar click at 50% → seek to ~duration/2; ← → seek back,
  3. pause/resume don't kill the process,
  4. clean exit with q (exit 0).

Usage: python3 tests/integration_audio_only.py [binary_path]
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
# 100x30 → 2-line HUD → bar on row ROWS-1, col 5, width 24.
BAR_ROW, BAR_COL, BAR_W = ROWS - 1, 5, 24

FAILS = []


def check(name, ok, detail=""):
    tag = "OK" if ok else "FAIL"
    print(f"[audio-only] {tag}: {name}" + (f" — {detail}" if detail else ""))
    if not ok:
        FAILS.append(name)


def make_audio(path, codec, freq, secs):
    subprocess.run(
        [
            "ffmpeg", "-y", "-v", "error",
            "-f", "lavfi", "-i", f"sine=frequency={freq}:duration={secs}",
            "-c:a", codec, path,
        ],
        check=True,
    )


def sgr_click(col, row):
    return f"\x1b[<0;{col};{row}M\x1b[<0;{col};{row}m".encode()


def run_case(label, path, secs, do_interact):
    log_path = path + ".sync.log"
    m, s = pty.openpty()
    # crossterm lee el winsize por ioctl — COLUMNS/LINES no bastan.
    fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    env = dict(os.environ, TERM="xterm-256color", RTV_SYNC_LOG=log_path)
    p = subprocess.Popen(
        [RTV, path, "--no-audio", "--backend", "ascii"],
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
        drain(4)
        check(f"{label}: vivo tras arranque", p.poll() is None)
        if do_interact and p.poll() is None:
            mid_col = BAR_COL + round(0.5 * (BAR_W - 1))
            os.write(m, sgr_click(mid_col, BAR_ROW))   # click 50%
            drain(3)
            os.write(m, b"\x1b[D")                     # seek back
            drain(2)
            os.write(m, b" ")                          # pause
            drain(1.5)
            os.write(m, b" ")                          # reanudar
            drain(1.5)
            check(f"{label}: vivo tras seeks/pausa", p.poll() is None)
        if p.poll() is None:
            os.write(m, b"q")
            for _ in range(40):
                if p.poll() is not None:
                    break
                drain(0.25)
        if p.poll() is None:
            p.send_signal(signal.SIGTERM)
            time.sleep(1)
        check(f"{label}: salida limpia con q", p.returncode == 0,
              f"exit={p.returncode}")
    finally:
        if p.poll() is None:
            p.kill()

    frames, seeks = [], []
    try:
        with open(log_path) as f:
            for line in f:
                if line.startswith("# SEEK"):
                    kv = dict(t.split("=") for t in line.split() if "=" in t)
                    seeks.append(float(kv["target"]))
                elif not line.startswith("#"):
                    parts = line.split()
                    if len(parts) >= 2:
                        frames.append(float(parts[1]))
    except FileNotFoundError:
        pass

    check(f"{label}: emits visualization frames", len(frames) > 30,
          f"{len(frames)} frames")
    if frames:
        check(f"{label}: primer PTS ≈ 0", -0.5 <= frames[0] < 3.0,
              f"first_pts={frames[0]:.3f}")
    if do_interact:
        check(f"{label}: hubo 2 seeks (click + ←)", len(seeks) == 2,
              f"{len(seeks)} seeks")
        for i, target in enumerate(seeks):
            check(f"{label}: seek {i} dentro de [0, {secs}]",
                  -0.5 <= target <= secs + 0.5, f"target={target:.3f}")


def main():
    if not shutil.which("ffmpeg"):
        print("[audio-only] SKIP: ffmpeg no disponible")
        return 0
    if not os.path.isfile(RTV):
        print(f"[audio-only] SKIP: binario no encontrado: {RTV}")
        return 0

    tmp = tempfile.mkdtemp(prefix="rtv-audioonly-")
    mp3 = os.path.join(tmp, "test.mp3")
    flac = os.path.join(tmp, "test.flac")
    make_audio(mp3, "libmp3lame", 440, 15)
    make_audio(flac, "flac", 880, 10)

    run_case("mp3", mp3, 15, do_interact=True)
    run_case("flac", flac, 10, do_interact=False)

    if FAILS:
        print(f"[audio-only] {len(FAILS)} fallos: {FAILS}")
        return 1
    print("[audio-only] TODO OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
