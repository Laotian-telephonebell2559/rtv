#!/usr/bin/env python3
"""GUI (--gui) integration test on headless X11 (Xvfb).

Automates the visual battery that used to be done by hand in the sandbox:

  1. The window appears and the video IS VISIBLE (frame with thousands
     of colors, not a black/empty window).
  2. The video ADVANCES (two captures 1 s apart differ in many px).
  3. Space actually PAUSES (with the HUD already auto-hidden, two
     captures 1 s apart are nearly identical; on resume it advances
     again).
  4. The seek (→) and volume (↓) keys do not kill the process.
  5. `q` closes the window and the process exits cleanly (exit 0,
     empty stderr) within a few seconds.

Usage:
    python3 tests/integration_gui.py <rtv-binary-with-gui-feature> [video]

If no video is given, a fixture is generated with the ffmpeg CLI.

Environment requirements (CI installs them): Xvfb, xdotool, ImageMagick
(import/identify/compare), ffmpeg (fixture only) and a software render
driver for wgpu (mesa: lavapipe/llvmpipe via mesa-vulkan-drivers +
libegl1/libgl1-mesa-dri).

Variables:
    RTV_GUI_DISPLAY  use an existing DISPLAY instead of launching Xvfb.
"""

import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time

DISPLAY = os.environ.get("RTV_GUI_DISPLAY", ":97")
OWN_XVFB = "RTV_GUI_DISPLAY" not in os.environ

FAILURES = []


def log(msg):
    print(f"[gui-test] {msg}", flush=True)


def fail(msg):
    log(f"FAILURE: {msg}")
    FAILURES.append(msg)


def run(cmd, **kw):
    kw.setdefault("env", {**os.environ, "DISPLAY": DISPLAY})
    return subprocess.run(cmd, capture_output=True, text=True, **kw)


def need(binname):
    if shutil.which(binname) is None:
        log(f"ERROR: `{binname}` missing from PATH")
        sys.exit(2)


def screenshot(path):
    r = run(["import", "-window", "root", path])
    if r.returncode != 0:
        fail(f"import failed: {r.stderr.strip()}")
    return path


def unique_colors(png):
    r = run(["identify", "-format", "%k", png])
    return int(r.stdout.strip() or 0)


def diff_pixels(a, b):
    # compare exits with 1 when they differ; the AE count goes to stderr.
    r = run(["compare", "-metric", "AE", a, b, "null:"])
    out = (r.stderr or r.stdout).strip().split()[0]
    try:
        return int(float(out))
    except ValueError:
        fail(f"compare returned something odd: {out!r}")
        return -1


def key(win, *keys):
    # XTEST (without --window): winit ignores the synthetic XSendEvent
    # events that `xdotool key --window` produces. With focus set,
    # XTEST injects "real" keys that the GUI does process.
    run(["xdotool", "windowfocus", "--sync", win])
    r = run(["xdotool", "key", *keys])
    if r.returncode != 0:
        fail(f"xdotool key {keys} failed: {r.stderr.strip()}")


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(2)
    rtv = os.path.abspath(sys.argv[1])
    for b in ("xdotool", "import", "identify", "compare"):
        need(b)
    if OWN_XVFB:
        need("Xvfb")

    tmp = tempfile.mkdtemp(prefix="rtv-gui-test-")
    shot = lambda name: os.path.join(tmp, name)

    # --- fixture --------------------------------------------------------
    if len(sys.argv) >= 3:
        video = sys.argv[2]
    else:
        need("ffmpeg")
        video = os.path.join(tmp, "fixture.mp4")
        r = subprocess.run(
            ["ffmpeg", "-y", "-loglevel", "error",
             "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30",
             "-t", "6", "-c:v", "libx264", "-preset", "ultrafast",
             "-pix_fmt", "yuv420p", "-an", video],
            capture_output=True, text=True)
        if r.returncode != 0:
            log(f"ERROR generating fixture: {r.stderr}")
            sys.exit(2)

    # --- Xvfb -----------------------------------------------------------
    xvfb = None
    if OWN_XVFB:
        xvfb = subprocess.Popen(
            ["Xvfb", DISPLAY, "-screen", "0", "1280x800x24"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        sock = "/tmp/.X11-unix/X" + DISPLAY.lstrip(":")
        for _ in range(50):
            if os.path.exists(sock):
                break
            time.sleep(0.1)
        else:
            log("ERROR: Xvfb did not start")
            sys.exit(2)

    proc = None
    logf = None
    try:
        # --- launch the GUI --------------------------------------------
        # --no-audio: the runners have no audio hardware and the focus
        # of this test is the graphical frontend (the audio path is
        # validated by the terminal tests).
        logf = open(os.path.join(tmp, "rtv.log"), "w")
        proc = subprocess.Popen(
            [rtv, "--gui", "--loop-video", "--no-audio", video],
            env={**os.environ, "DISPLAY": DISPLAY},
            stdout=logf, stderr=subprocess.STDOUT)

        try:
            r = run(["xdotool", "search", "--sync", "--name", "rtv"],
                    timeout=30)
            wins = [w for w in r.stdout.split() if w.strip()]
        except subprocess.TimeoutExpired:
            wins = []
        if not wins:
            fail("the 'rtv' window did not appear within 30 s")
            raise SystemExit
        win = wins[0]
        log(f"window {win} created")
        time.sleep(2.5)

        if proc.poll() is not None:
            fail(f"the process died right after starting (exit {proc.returncode})")
            raise SystemExit

        # --- 1: video is visible (no black window) ----------------------
        s1 = screenshot(shot("play1.png"))
        colors = unique_colors(s1)
        log(f"unique colors in the frame: {colors}")
        if colors < 500:
            fail(f"frame too poor ({colors} colors): black window?")

        # --- 2: the video advances --------------------------------------
        time.sleep(1.0)
        s2 = screenshot(shot("play2.png"))
        playing_diff = diff_pixels(s1, s2)
        log(f"differing pixels while playing: {playing_diff}")
        if playing_diff < 5000:
            fail(f"the video does not advance (diff={playing_diff} px)")

        # --- 3: real pause ----------------------------------------------
        key(win, "space")
        # Move the mouse away and wait for the HUD auto-hide (~2.5 s) so
        # the captures compare ONLY the frozen video frame.
        run(["xdotool", "mousemove", "5", "5"])
        time.sleep(3.5)
        p1 = screenshot(shot("pause1.png"))
        time.sleep(1.0)
        p2 = screenshot(shot("pause2.png"))
        paused_diff = diff_pixels(p1, p2)
        log(f"differing pixels while paused: {paused_diff}")
        if playing_diff > 0 and paused_diff > max(500, playing_diff // 10):
            fail(f"pause does not freeze the frame (diff={paused_diff} px "
                 f"vs {playing_diff} while playing)")

        # --- resume and check it advances again --------------------------
        key(win, "space")
        time.sleep(0.8)
        r1 = screenshot(shot("resume1.png"))
        time.sleep(1.0)
        r2 = screenshot(shot("resume2.png"))
        resume_diff = diff_pixels(r1, r2)
        log(f"differing pixels after resuming: {resume_diff}")
        if resume_diff < 5000:
            fail(f"after resuming the video does not advance (diff={resume_diff} px)")

        # --- 4: seek and volume do not kill the process -----------------
        key(win, "Right")
        key(win, "Down")
        time.sleep(1.0)
        if proc.poll() is not None:
            fail(f"the process died after seek/volume (exit {proc.returncode})")

        # --- 5: clean exit with q ----------------------------------------
        key(win, "q")
        try:
            code = proc.wait(timeout=8)
        except subprocess.TimeoutExpired:
            fail("q did not close the process within 8 s")
            proc.kill()
            code = proc.wait()
        if code != 0:
            fail(f"exit code {code} != 0")
        logf.close()
        logf = None
        with open(os.path.join(tmp, "rtv.log")) as f:
            leftovers = f.read().strip()
        if leftovers:
            fail(f"stderr/stdout not empty on exit:\n{leftovers[:2000]}")

    except SystemExit:
        pass
    finally:
        if logf is not None:
            logf.close()
        if proc is not None and proc.poll() is None:
            proc.send_signal(signal.SIGKILL)
            proc.wait()
        if xvfb is not None:
            xvfb.terminate()
            xvfb.wait()

    if FAILURES:
        log(f"{len(FAILURES)} failure(s); captures in {tmp}")
        sys.exit(1)
    log("OK: render, advance, pause, resume, seek/volume and clean exit")
    shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
