#!/usr/bin/env python3
"""Benchmark of rtv's 6 backends: bytes emitted per frame and CPU.

Plays a synthetic video (testsrc2 720p 30fps) in a 100x30 pty with
each backend for N seconds, measuring:
  - Total bytes emitted to the terminal (read from the pty) -> KB/frame
  - CPU consumed by the rtv process (utime+stime from /proc/<pid>/stat)

Usage: python3 tests/bench_backends.py <path-to-rtv> [seconds]
"""
import os
import pty
import select
import signal
import subprocess
import sys
import time

BACKENDS = ["kitty", "iterm2", "sixel", "blocks", "ascii"]
COLS, ROWS = 100, 30
FPS = 30  # fps del fixture


def make_fixture(path):
    """Generates the test video: testsrc2 1280x720 30fps 6s h264."""
    if os.path.exists(path):
        return
    subprocess.run(
        [
            "ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
            "-f", "lavfi", "-i", f"testsrc2=size=1280x720:rate={FPS}:duration=6",
            "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
            path,
        ],
        check=True,
    )


def read_proc_cpu(pid):
    """Reads utime+stime (seconds) from /proc/<pid>/stat. None if the process died."""
    try:
        with open(f"/proc/{pid}/stat", "rb") as f:
            data = f.read().decode("ascii", "replace")
        # The command name is parenthesized and may contain spaces:
        # the numeric fields start after the last ')'.
        rest = data.rsplit(")", 1)[1].split()
        # rest[0] = state (field 3); utime = field 14 -> rest[11], stime -> rest[12]
        utime = int(rest[11])
        stime = int(rest[12])
        return (utime + stime) / os.sysconf("SC_CLK_TCK")
    except (FileNotFoundError, ProcessLookupError, IndexError, ValueError):
        return None


def bench(rtv, video, backend, seconds):
    """Runs rtv with a backend in a pty and returns (total_bytes, cpu_s)."""
    master, slave = pty.openpty()
    # Set the pty size
    import fcntl, termios, struct
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

    env = dict(os.environ)
    for var in ("KITTY_WINDOW_ID", "TERM_PROGRAM", "WT_SESSION", "COLORTERM",
                "LC_TERMINAL", "MLTERM"):
        env.pop(var, None)
    env["TERM"] = "xterm-256color"

    p = subprocess.Popen(
        [rtv, "--backend", backend, "--no-audio", video],
        stdin=slave, stdout=slave, stderr=subprocess.DEVNULL,
        env=env, close_fds=True,
    )
    os.close(slave)

    total = 0
    cpu = 0.0
    deadline = time.time() + seconds
    try:
        while time.time() < deadline:
            r, _, _ = select.select([master], [], [], 0.1)
            if r:
                try:
                    chunk = os.read(master, 1 << 20)
                except OSError:
                    break
                if not chunk:
                    break
                total += len(chunk)
            if p.poll() is not None:
                break
        # Sample the CPU BEFORE killing/reaping the child (avoids the
        # race with Popen's reap that broke os.wait4 with ECHILD).
        sample = read_proc_cpu(p.pid)
        if sample is not None:
            cpu = sample
        # Clean exit: 'q', falling back to a signal
        try:
            os.write(master, b"q")
        except OSError:
            pass
        try:
            p.wait(timeout=2)
        except subprocess.TimeoutExpired:
            p.send_signal(signal.SIGKILL)
            p.wait()
        # Drain whatever is left in the pty (doesn't count toward the total: the clock already stopped)
    finally:
        os.close(master)
        if p.poll() is None:
            p.kill()
            p.wait()
    return total, cpu


def main():
    if len(sys.argv) < 2:
        print(f"uso: {sys.argv[0]} <rtv> [segundos]", file=sys.stderr)
        sys.exit(2)
    rtv = sys.argv[1]
    seconds = float(sys.argv[2]) if len(sys.argv) > 2 else 5.0

    video = "/tmp/bench_fixture.mp4"
    make_fixture(video)

    frames = seconds * FPS
    print(f"fixture: testsrc2 1280x720@{FPS} h264 | pty {COLS}x{ROWS} | {seconds:.0f}s por backend\n")
    print(f"{'backend':<9} {'MB emitidos':>12} {'KB/frame':>10} {'CPU s':>7} {'CPU %':>6}")
    for backend in BACKENDS:
        total, cpu = bench(rtv, video, backend, seconds)
        mb = total / (1024 * 1024)
        kbf = total / 1024 / frames
        cpu_pct = 100.0 * cpu / seconds
        print(f"{backend:<9} {mb:>12.2f} {kbf:>10.1f} {cpu:>7.2f} {cpu_pct:>5.0f}%")


if __name__ == "__main__":
    main()
