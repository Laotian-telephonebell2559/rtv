#!/usr/bin/env python3
"""Generate the GUI demo GIF for the README.

Launches `rtv --gui` on a headless X server (Xvfb + lavapipe software
rendering, same stack as tests/integration_gui.py), plays the branded
demo fixture and drives a small tour with XTEST input while
screenshotting the window at GIF_FPS:

  1. playback with the bottom OSD (progress bar) visible;
  2. hover over the bar -> seek-time tooltip;
  3. Space -> centered pause badge;
  4. double click while paused -> resume + fullscreen
     (in the GUI the 1st click toggles pause, the 2nd fullscreen);
  5. q -> clean exit (the GIF ends when the window closes).

Usage: python3 scripts/capture_gui_gif.py <rtv-gui-binary> <outdir>

Requires: Xvfb, xdotool, ImageMagick (import), ffmpeg, a software
vulkan driver (mesa lavapipe) and fonts-dejavu-core.
"""
import os
import subprocess
import sys
import tempfile
import time

from PIL import Image

# Reuse the branded fixture (and the GIF constants) from the terminal
# capture script so both demo sets show the same clip.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from capture_demo_gifs import (  # noqa: E402
    FIXTURE, GIF_FPS, GIF_MAX_W, make_fixture,
)

DISPLAY = ":98"


def wait_for(cond, timeout, step=0.1):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if cond():
            return True
        time.sleep(step)
    return False


class Tour:
    def __init__(self, win, env, tmp):
        self.win = win
        self.env = env
        self.tmp = tmp
        self.frames = []
        self.n = 0
        g = self.xdo("getwindowgeometry", "--shell", self.win)
        geo = dict(l.split("=") for l in g.splitlines() if "=" in l)
        self.wx, self.wy = int(geo["X"]), int(geo["Y"])
        self.ww, self.wh = int(geo["WIDTH"]), int(geo["HEIGHT"])
        print(f"  window {self.ww}x{self.wh} at +{self.wx}+{self.wy}", flush=True)

    def xdo(self, *args):
        r = subprocess.run(["xdotool", *args], env=self.env,
                           capture_output=True, text=True)
        return r.stdout

    def move(self, fx, fy):
        """Move the pointer to a window-relative fraction."""
        x = self.wx + int(self.ww * fx)
        y = self.wy + int(self.wh * fy)
        self.xdo("mousemove", str(x), str(y))

    def key(self, *keys):
        self.xdo("windowfocus", "--sync", self.win)
        self.xdo("key", *keys)

    def title(self):
        return self.xdo("getwindowname", self.win).strip()

    def wait_paused(self, want, timeout=4.0, retry_key=None):
        """The window title gains a ' \u23f8' suffix while paused — poll
        it so each tour phase starts from a *known* state instead of a
        guessed sleep. Optionally re-send a key once if the state did
        not change (a keypress can get lost while the compositor-less
        Xvfb window is still mapping)."""
        def ok():
            return ("\u23f8" in self.title()) == want
        if wait_for(ok, timeout / 2):
            return True
        if retry_key:
            print(f"  state not reached, re-sending {retry_key}", flush=True)
            self.key(retry_key)
        if wait_for(ok, timeout / 2):
            return True
        print(f"  !! wait_paused({want}) timed out (title: {self.title()!r})",
              flush=True)
        return False

    def click(self, times=1):
        self.xdo("click", "--repeat", str(times), "--delay", "60", "1")

    def capture(self, n_frames, jiggle=False, label=""):
        """Grab n_frames, pacing at GIF_FPS (or as fast as `import`
        allows — ~0.4 s/frame on the sandbox's software X stack; the
        GIF's frame *timing* is fixed at save time, so a slow capture
        only stretches wall-clock, not the final animation).

        jiggle nudges the pointer 1 px every few frames so the OSD's
        2.5 s auto-hide never kicks in."""
        print(f"  [{label}] {n_frames} frames…", flush=True)
        period = 1.0 / GIF_FPS
        t_next = time.time()
        for i in range(n_frames):
            if jiggle and i % 3 == 0:
                self.xdo("mousemove_relative", "--", "1" if i % 6 else "-1", "0")
            png = os.path.join(self.tmp, "cur.png")
            try:
                r = subprocess.run(["import", "-window", self.win, png],
                                   env=self.env, capture_output=True, timeout=10)
            except subprocess.TimeoutExpired:
                print("  import timed out (window gone?)", flush=True)
                return
            if r.returncode == 0:
                try:
                    img = Image.open(png).convert("RGB")
                    w, h = img.size
                    if w > GIF_MAX_W:
                        img = img.resize((GIF_MAX_W, int(h * GIF_MAX_W / w)),
                                         Image.LANCZOS)
                    self.frames.append(img)
                except Exception as e:
                    print(f"  frame failed: {e}", flush=True)
            else:
                # Window gone (e.g. after q): stop this segment.
                return
            t_next += period
            delay = t_next - time.time()
            if delay > 0:
                time.sleep(delay)


def save_gif_full(frames, path):
    """Like capture_demo_gifs.save_gif but WITHOUT resampling: the tour
    is a choreography, every captured frame is kept."""
    if not frames:
        print(f"  !! no frames for {path}")
        return False
    # Uniform size: fullscreen frames are larger than windowed ones.
    w = min(f.size[0] for f in frames)
    h = min(f.size[1] for f in frames)
    frames = [f if f.size == (w, h) else f.resize((w, h), Image.LANCZOS)
              for f in frames]
    frames = [f.quantize(colors=128, method=Image.MEDIANCUT) for f in frames]
    frames[0].save(
        path, save_all=True, append_images=frames[1:],
        duration=int(1000 / GIF_FPS), loop=0, optimize=True,
    )
    kb = os.path.getsize(path) // 1024
    print(f"  -> {path} ({len(frames)} frames, {kb} KB)")
    return True


def main():
    if len(sys.argv) != 3:
        print(__doc__)
        sys.exit(2)
    binary, outdir = os.path.abspath(sys.argv[1]), sys.argv[2]
    os.makedirs(outdir, exist_ok=True)
    make_fixture()

    xvfb = subprocess.Popen(
        ["Xvfb", DISPLAY, "-screen", "0", "1600x900x24"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    sock = "/tmp/.X11-unix/X" + DISPLAY.lstrip(":")
    if not wait_for(lambda: os.path.exists(sock), 10):
        print("ERROR: Xvfb did not start")
        sys.exit(2)

    env = {**os.environ, "DISPLAY": DISPLAY}
    proc = None
    tmp = tempfile.mkdtemp(prefix="rtv-gui-gif-")
    try:
        proc = subprocess.Popen(
            [binary, "--gui", "--loop-video", "--no-audio", FIXTURE],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)

        r = subprocess.run(["xdotool", "search", "--sync", "--name", "rtv"],
                           env=env, capture_output=True, text=True, timeout=30)
        wins = [w for w in r.stdout.split() if w.strip()]
        if not wins:
            print("ERROR: the rtv window did not appear")
            sys.exit(1)
        t = Tour(wins[0], env, tmp)
        time.sleep(2.0)  # first frames land, startup OSD fades

        # 1 — playback + OSD: pointer into the frame wakes the OSD.
        t.move(0.5, 0.55)
        t.capture(10, jiggle=True, label="playback+OSD")

        # 2 — tooltip: hover over the progress bar. With k =
        # height/480 the bar sits at y = H - 34k - h (≈0.922·H for a
        # 720p window) and the hover hit-band spans ±12 px around it,
        # so 0.925 lands on the track itself; 0.955 fell just below.
        t.move(0.38, 0.925)
        t.capture(14, jiggle=True, label="bar tooltip")

        # 3 — Space: centered pause badge (bar keeps showing). Confirm
        # via the window title (gains ' \u23f8' while paused) before
        # capturing, so the badge segment is guaranteed.
        t.key("space")
        t.wait_paused(True, retry_key="space")
        t.capture(12, jiggle=True, label="pause badge")

        # 4 — double click while paused: 1st click resumes, 2nd goes
        #     fullscreen -> playing, fullscreen.
        t.move(0.5, 0.5)
        t.click(times=2)
        t.wait_paused(False)  # 1st click resumed playback
        time.sleep(0.8)  # let the fullscreen transition finish
        t.capture(14, jiggle=True, label="fullscreen")

        # 5 — q: clean exit. Capture stops when the window dies.
        t.key("q")
        t.capture(4, label="quit")

        print(f"  {len(t.frames)} frames captured", flush=True)
        ok = save_gif_full(t.frames, os.path.join(outdir, "demo-gui.gif"))
        sys.exit(0 if ok else 1)
    finally:
        if proc and proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(5)
            except subprocess.TimeoutExpired:
                proc.kill()
        xvfb.terminate()


if __name__ == "__main__":
    main()
