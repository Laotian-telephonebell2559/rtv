#!/usr/bin/env python3
"""Hardware-decode stress benchmark, with an optional mpv face-off.

Closes the last roadmap item: measuring the real --hwdec gain. The CI
sandbox has no GPU, so the decisive numbers come from running this on
real hardware — the test adapts to whatever it finds:

  * GPU present (a /dev/dri render node or nvidia-smi on Linux; on
    macOS/Windows the OS hwaccel is always there): plays a demanding
    fixture with --hwdec auto and --hwdec none and compares CPU time,
    shown frames and A/V-sync health. `auto` must not burn meaningfully
    more CPU than plain software — on working hardware it should burn
    less, and the measured gain is printed.
  * No GPU: both modes fall back to software, so the comparison turns
    into a consistency check (numbers must come out similar) and the
    gain measurement is reported as skipped.
  * mpv on PATH: plays the same fixture with mpv's terminal video
    output (--vo=tct) in an identical pty and measures the same CPU
    counters, as an external reference point. Mostly informational
    (mpv quantizes with its own renderer, so the numbers aren't
    apples-to-apples); it only fails on gross anomalies — rtv burning
    more than RTV_MPV_MAX_RATIO times mpv's CPU (default 4x).

Usage: python3 tests/stress_hwdec_bench.py [rtv_path] [video]
  With no video argument a fixture is generated (needs ffmpeg on PATH:
  HEVC 1080p when libx265 is available, otherwise H.264 1080p60).

Env knobs: RTV_BIN, STRESS_PLAY_SECS, RTV_MPV_MAX_RATIO.
"""
import glob
import os
import pty
import select
import shutil
import statistics
import struct
import subprocess
import sys
import tempfile
import termios
import fcntl
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))

args = [a for a in sys.argv[1:]]
BIN = os.path.abspath(args[0]) if args else (
    os.environ.get("RTV_BIN")
    or os.path.join(HERE, "..", "target", "release", "rtv"))
VIDEO = os.path.abspath(args[1]) if len(args) > 1 else None

PLAY_SECS = float(os.environ.get("STRESS_PLAY_SECS", "8"))
MPV_MAX_RATIO = float(os.environ.get("RTV_MPV_MAX_RATIO", "4.0"))
ROWS, COLS = 40, 120
MIN_FRAMES = 60          # ~8 s at 30 fps with generous warmup slack
AVDIFF_MEDIAN_MS = 120.0  # same threshold as integration_sync.py

fails = []


def detect_gpu():
    """Returns a human-readable GPU/hwaccel hint, or None on a bare box."""
    if sys.platform == "darwin":
        return "VideoToolbox (always present on macOS)"
    if sys.platform == "win32":
        return "D3D11VA/DXVA2 (always present on Windows)"
    nodes = glob.glob("/dev/dri/renderD*")
    if nodes:
        return f"DRI render node ({', '.join(sorted(nodes))})"
    if shutil.which("nvidia-smi"):
        try:
            r = subprocess.run(["nvidia-smi", "-L"], capture_output=True,
                               text=True, timeout=10)
            if r.returncode == 0 and r.stdout.strip():
                return r.stdout.strip().splitlines()[0]
        except (subprocess.TimeoutExpired, OSError):
            pass
    return None


def make_fixture(tmp):
    """Demanding video: HEVC 1080p30 if libx265 exists, else H.264 1080p60."""
    path = os.path.join(tmp, "bench_hevc.mp4")
    r = subprocess.run(
        ["ffmpeg", "-y", "-loglevel", "error",
         "-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=30",
         "-t", "14", "-c:v", "libx265", "-preset", "ultrafast",
         "-tag:v", "hvc1", "-an", path])
    if r.returncode == 0:
        return path
    path = os.path.join(tmp, "bench_h264.mp4")
    subprocess.run(
        ["ffmpeg", "-y", "-loglevel", "error",
         "-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=60",
         "-t", "14", "-c:v", "libx264", "-preset", "ultrafast",
         "-pix_fmt", "yuv420p", "-an", path], check=True)
    return path


def read_proc_cpu(pid):
    """utime+stime (seconds) from /proc/<pid>/stat; None if it's gone
    or we're not on Linux (macOS/Windows just skip the CPU numbers)."""
    try:
        with open(f"/proc/{pid}/stat", "rb") as f:
            data = f.read().decode("ascii", "replace")
        rest = data.rsplit(")", 1)[1].split()
        return (int(rest[11]) + int(rest[12])) / os.sysconf("SC_CLK_TCK")
    except (FileNotFoundError, ProcessLookupError, IndexError,
            ValueError, OSError):
        return None


def run_in_pty(argv, env, secs):
    """Runs argv in a fresh pty for `secs`, then sends 'q'. The pty is
    drained in a thread so our own backpressure doesn't skew anything.
    Returns (exit_code, cpu_seconds_or_None)."""
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ,
                struct.pack("HHHH", ROWS, COLS, 0, 0))
    proc = subprocess.Popen(argv, stdin=slave, stdout=slave,
                            stderr=subprocess.DEVNULL, env=env)
    os.close(slave)

    stop = threading.Event()

    def drain():
        while not stop.is_set():
            r, _, _ = select.select([master], [], [], 0.02)
            if r:
                try:
                    if not os.read(master, 1 << 20):
                        return
                except OSError:
                    return
    threading.Thread(target=drain, daemon=True).start()

    # Sample the CPU counters periodically; the last good sample before
    # the process dies is the one we keep (reading after wait() is too
    # late — /proc/<pid> is gone).
    cpu = None
    deadline = time.time() + secs
    while time.time() < deadline:
        time.sleep(0.25)
        c = read_proc_cpu(proc.pid)
        if c is not None:
            cpu = c
        if proc.poll() is not None:
            break

    try:
        os.write(master, b"q")
    except OSError:
        pass
    try:
        code = proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
        code = -9
    stop.set()
    os.close(master)
    return code, cpu


def parse_sync_log(path):
    """[(wall, avdiff_ms)] with the same delayed-visibility retry that
    integration_hwdec.py needed on 9p/overlay filesystems."""
    def read_rows():
        out = []
        try:
            with open(path) as f:
                for line in f:
                    if line.startswith("#"):
                        continue
                    p = line.split()
                    if len(p) >= 4:
                        try:
                            out.append((float(p[0]), float(p[3])))
                        except ValueError:
                            pass
        except FileNotFoundError:
            pass
        return out

    rows = read_rows()
    deadline = time.time() + 2.0
    while not rows and time.time() < deadline:
        time.sleep(0.05)
        rows = read_rows()
    return rows


def bench_rtv(video, mode):
    """One rtv run with --hwdec <mode>. Returns a metrics dict."""
    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
    for var in ("KITTY_WINDOW_ID", "TERM_PROGRAM", "WT_SESSION",
                "COLORTERM", "LC_TERMINAL", "MLTERM"):
        env.pop(var, None)
    log_path = tempfile.mktemp(prefix=f"rtv_bench_{mode}_", suffix=".log")
    env["RTV_SYNC_LOG"] = log_path

    code, cpu = run_in_pty(
        [BIN, video, "--backend", "blocks", "--no-audio", "--hwdec", mode],
        env, PLAY_SECS)

    rows = parse_sync_log(log_path)
    try:
        os.unlink(log_path)
    except OSError:
        pass

    med = None
    if rows:
        t0 = rows[0][0]
        settled = [abs(av) for (w, av) in rows if w - t0 > 1.0]
        if settled:
            med = statistics.median(settled)
    return {"code": code, "cpu": cpu, "frames": len(rows), "avdiff": med}


def bench_mpv(video):
    """One mpv run with the terminal video output, same pty and clock.
    mpv ignores the `q` keystroke in a non-interactive pty, so instead
    of pressing keys it gets --end=<secs>: it plays the same window as
    the rtv runs and exits 0 on its own."""
    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
    code, cpu = run_in_pty(
        ["mpv", "--vo=tct", "--ao=null", "--really-quiet", "--no-config",
         "--hwdec=no", f"--end={PLAY_SECS}", video],
        env, PLAY_SECS + 4.0)
    return {"code": code, "cpu": cpu}


def fmt_cpu(c):
    return f"{c:.2f}s" if c is not None else "n/a"


def main():
    global VIDEO
    if not os.path.exists(BIN):
        print(f"error: binary {BIN} does not exist", file=sys.stderr)
        return 2

    tmp = None
    if VIDEO is None:
        if not shutil.which("ffmpeg"):
            print("error: no video given and ffmpeg is not on PATH",
                  file=sys.stderr)
            return 2
        tmp = tempfile.mkdtemp(prefix="rtv-hwbench-")
        VIDEO = make_fixture(tmp)

    gpu = detect_gpu()
    print(f"GPU: {gpu or 'none detected (software-only box)'}")
    print(f"fixture: {VIDEO}")
    print(f"play window: {PLAY_SECS:.0f}s per run\n")

    # ── rtv: --hwdec auto vs --hwdec none ────────────────────────────
    res = {}
    for mode in ("auto", "none"):
        res[mode] = bench_rtv(VIDEO, mode)
        r = res[mode]
        av = f"{r['avdiff']:.1f} ms" if r["avdiff"] is not None else "n/a"
        print(f"rtv --hwdec {mode:<4}: exit={r['code']} "
              f"frames={r['frames']} cpu={fmt_cpu(r['cpu'])} "
              f"median|avdiff|={av}")
        if r["code"] != 0:
            fails.append(f"--hwdec {mode}: exit {r['code']} != 0")
        elif r["frames"] < MIN_FRAMES:
            fails.append(f"--hwdec {mode}: only {r['frames']} frames "
                         f"(< {MIN_FRAMES})")
        if r["avdiff"] is not None and r["avdiff"] > AVDIFF_MEDIAN_MS:
            fails.append(f"--hwdec {mode}: median avdiff {r['avdiff']:.1f} "
                         f"ms > {AVDIFF_MEDIAN_MS}")

    a, n = res["auto"], res["none"]
    if a["code"] == 0 and n["code"] == 0:
        # Frame throughput must be in the same ballpark either way: a
        # broken fallback that "plays" at 2 fps shows up right here.
        if n["frames"] and abs(a["frames"] - n["frames"]) > 0.4 * n["frames"]:
            fails.append(f"frame count diverges: auto={a['frames']} "
                         f"none={n['frames']} (>40%)")

        if gpu and a["cpu"] is not None and n["cpu"] is not None:
            if n["cpu"] > 0:
                gain = (1.0 - a["cpu"] / n["cpu"]) * 100.0
                print(f"\nhwdec CPU gain (auto vs none): {gain:+.1f}%  "
                      f"[{fmt_cpu(a['cpu'])} vs {fmt_cpu(n['cpu'])}]")
            # With real hardware `auto` may fall back (unsupported codec)
            # but must never cost meaningfully MORE CPU than software.
            if a["cpu"] > n["cpu"] * 1.25 + 0.5:
                fails.append(f"--hwdec auto burns more CPU than none: "
                             f"{a['cpu']:.2f}s vs {n['cpu']:.2f}s")
        elif gpu:
            print("\nhwdec CPU gain: not measurable on this OS "
                  "(no /proc) — frame/sync checks still apply")
        else:
            print("\nhwdec CPU gain: SKIPPED (no GPU — both runs are "
                  "software; consistency checked instead)")

    # ── mpv face-off (terminal mode) ─────────────────────────────────
    if shutil.which("mpv"):
        m = bench_mpv(VIDEO)
        print(f"\nmpv --vo=tct     : exit={m['code']} cpu={fmt_cpu(m['cpu'])}")
        if m["code"] != 0:
            print("  (mpv did not exit cleanly — reference skipped)")
        elif m["cpu"] and n["cpu"]:
            ratio = n["cpu"] / m["cpu"]
            print(f"  rtv(software)/mpv CPU ratio: {ratio:.2f}x "
                  f"(limit {MPV_MAX_RATIO}x)")
            if ratio > MPV_MAX_RATIO:
                fails.append(f"rtv burns {ratio:.2f}x mpv's CPU "
                             f"(> {MPV_MAX_RATIO}x)")
    else:
        print("\nmpv face-off: SKIPPED (mpv not on PATH)")

    if tmp:
        shutil.rmtree(tmp, ignore_errors=True)

    print()
    if fails:
        for f in fails:
            print(f"FAIL: {f}")
        return 1
    print("ALL OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
