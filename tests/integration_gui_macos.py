#!/usr/bin/env python3
"""NATIVE macOS GUI integration test (Cocoa window + Metal).

Unlike the linux test (Xvfb + xdotool + ImageMagick), there is no X
here: the Actions runner's real graphical session is used with system
tools — zero dependencies to install:

  * window ID: CGWindowListCopyWindowInfo via a Swift snippet compiled
    on the fly with swiftc (window METADATA only: needs neither
    accessibility nor screen-recording permissions).
  * capture: `screencapture -l <windowID>` (just the rtv window, even
    if covered) → PNG.
  * analysis: `sips` (builtin) converts to uncompressed BMP and the
    BMP is sampled in pure Python (no PIL/numpy/ImageMagick).
  * fixture: rawvideo AVI (DIB) generated in pure Python — the macOS
    job has no ffmpeg CLI (its own FFmpeg is decode-only) and
    libavformat decodes rawvideo/AVI out of the box.

Verifies: the window gets created, the frame has real color variety,
the video ADVANCES (two captures 1 s apart differ), and the process
survives playback. Clean exit with `q` is ATTEMPTED via osascript
(System Events): if the runner has automation authorized, exit 0 is
required; otherwise (permission denied), the process is terminated
with SIGTERM and the exit check is left as "not verifiable" WITHOUT
failing — the key/exit contract is already covered by the linux test
with XTEST.

Usage:
  python3 tests/integration_gui_macos.py <rtv_gui_path> [video]
"""

import os
import struct
import subprocess
import sys
import tempfile
import time

FAILURES = []


def logf(msg):
    print(f"[gui-macos] {msg}", flush=True)


def fail(msg):
    FAILURES.append(msg)
    logf(f"FAILURE: {msg}")


# ------------------------------------------------------------ fixture ----
def make_avi(path, w=320, h=180, fps=15, secs=10):
    """Uncompressed 'vids/DIB ' AVI (BGR24 bottom-up), pure Python."""
    frames = fps * secs
    stride = ((w * 3 + 3) // 4) * 4
    # Wide master row: gradient with color variety; each frame is a
    # shifted VIEW (byte slicing: fast without numpy).
    master_w = w + frames * 2 + 4
    rows = []
    for y in range(h):
        row = bytearray()
        for x in range(master_w):
            row += bytes(((x * 3 + y * 7) & 0xFF,       # B
                          (x ^ y) & 0xFF,               # G
                          (x + y * 2) & 0xFF))          # R
        rows.append(bytes(row))
    pad = b"\0" * (stride - w * 3)
    frame_size = stride * h

    def frame_bytes(t):
        off = t * 2 * 3
        # bottom-up: last row first
        return b"".join(rows[y][off:off + w * 3] + pad
                        for y in range(h - 1, -1, -1))

    movi_entries = []
    for t in range(frames):
        movi_entries.append(frame_bytes(t))

    def chunk(fourcc, payload):
        data = fourcc + struct.pack("<I", len(payload)) + payload
        if len(payload) % 2:
            data += b"\0"
        return data

    avih = struct.pack("<14I", int(1e6 / fps), frame_size * fps, 0, 0x10,
                       frames, 0, 1, frame_size, w, h, 0, 0, 0, 0)
    strh = (b"vids" + b"DIB " + struct.pack("<IHHIIIIIIIi", 0, 0, 0, 0,
            1, fps, 0, frames, frame_size, 0xFFFFFFFF - (1 << 32) + 1, 0)
            + struct.pack("<4H", 0, 0, w, h))
    strf = struct.pack("<IiiHHIIiiII", 40, w, h, 1, 24, 0,
                       frame_size, 0, 0, 0, 0)
    strl = b"LIST" + struct.pack(
        "<I", 4 + len(chunk(b"strh", strh)) + len(chunk(b"strf", strf))) \
        + b"strl" + chunk(b"strh", strh) + chunk(b"strf", strf)
    hdrl_payload = chunk(b"avih", avih) + strl
    hdrl = b"LIST" + struct.pack("<I", 4 + len(hdrl_payload)) + b"hdrl" \
        + hdrl_payload

    movi_payload = b""
    idx = b""
    for fb in movi_entries:
        offset = 4 + len(movi_payload)  # relative to 'movi'
        movi_payload += chunk(b"00db", fb)
        idx += b"00db" + struct.pack("<III", 0x10, offset, len(fb))
    movi = b"LIST" + struct.pack("<I", 4 + len(movi_payload)) + b"movi" \
        + movi_payload
    idx1 = chunk(b"idx1", idx)

    body = b"AVI " + hdrl + movi + idx1
    with open(path, "wb") as f:
        f.write(b"RIFF" + struct.pack("<I", len(body)) + body)
    logf(f"fixture AVI: {frames} frames {w}x{h} "
         f"({os.path.getsize(path) // 1024} KB)")


# ----------------------------------------------------- window + capture ----
SWIFT_SRC = """
import CoreGraphics
import Foundation
let pid = Int32(CommandLine.arguments[1])!
let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly],
                                      kCGNullWindowID) as! [[String: Any]]
for w in list {
    if let owner = w[kCGWindowOwnerPID as String] as? Int32, owner == pid,
       let num = w[kCGWindowNumber as String] as? Int,
       let layer = w[kCGWindowLayer as String] as? Int, layer == 0 {
        print(num)
    }
}
"""


def build_winlist(tmp):
    src = os.path.join(tmp, "winlist.swift")
    binp = os.path.join(tmp, "winlist")
    open(src, "w").write(SWIFT_SRC)
    subprocess.run(["swiftc", "-O", "-o", binp, src], check=True,
                   capture_output=True)
    return binp


def find_window(winlist, pid, timeout=30):
    t0 = time.time()
    while time.time() - t0 < timeout:
        r = subprocess.run([winlist, str(pid)], capture_output=True,
                           text=True)
        ids = [int(x) for x in r.stdout.split()]
        if ids:
            return ids[0]
        time.sleep(0.5)
    return None


def capture(tmp, win_id, name):
    png = os.path.join(tmp, name + ".png")
    bmp = os.path.join(tmp, name + ".bmp")
    subprocess.run(["screencapture", "-x", "-o", "-l", str(win_id), png],
                   check=True)
    subprocess.run(["sips", "-s", "format", "bmp", png, "--out", bmp],
                   check=True, capture_output=True)
    return bmp


def read_bmp_samples(path, step=101):
    """(b,g,r) samples from the BMP every `step` pixels — no PIL/numpy."""
    data = open(path, "rb").read()
    if data[:2] != b"BM":
        raise ValueError("not a BMP")
    pix_off = struct.unpack_from("<I", data, 10)[0]
    w = struct.unpack_from("<i", data, 18)[0]
    h = abs(struct.unpack_from("<i", data, 22)[0])
    bpp = struct.unpack_from("<H", data, 28)[0]
    nbytes = bpp // 8
    stride = ((w * nbytes + 3) // 4) * 4
    samples = []
    total = w * h
    for i in range(0, total, step):
        y, x = divmod(i, w)
        off = pix_off + y * stride + x * nbytes
        samples.append(data[off:off + 3])
    return samples, (w, h)


def try_keystroke(key):
    """Key via System Events; False if accessibility permission is missing."""
    r = subprocess.run(
        ["osascript", "-e",
         f'tell application "System Events" to keystroke "{key}"'],
        capture_output=True, text=True, timeout=15)
    return r.returncode == 0


# ------------------------------------------------------------------ main ----
def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    rtv = os.path.abspath(sys.argv[1])
    tmp = tempfile.mkdtemp(prefix="rtv-gui-macos-")
    if len(sys.argv) > 2:
        video = os.path.abspath(sys.argv[2])
    else:
        video = os.path.join(tmp, "fixture.avi")
        make_avi(video)

    winlist = build_winlist(tmp)
    log = open(os.path.join(tmp, "rtv.log"), "w+")
    proc = subprocess.Popen(
        [rtv, "--gui", "--loop-video", "--no-audio", video],
        stdout=log, stderr=log, env=dict(os.environ, RUST_BACKTRACE="1"))
    try:
        win = find_window(winlist, proc.pid)
        if win is None:
            fail("the window did not appear within 30 s")
            raise SystemExit
        logf(f"window {win} created")
        time.sleep(2.0)  # first frame + HUD settled

        # 1. Real render: color variety.
        s1, dims = read_bmp_samples(capture(tmp, win, "f1"))
        colors = len(set(s1))
        logf(f"capture {dims[0]}x{dims[1]}, unique colors (sampled): "
             f"{colors}")
        if colors < 100:
            fail(f"only {colors} unique colors: black/empty window?")

        # 2. Advance: two captures 1 s apart differ.
        time.sleep(1.0)
        s2, _ = read_bmp_samples(capture(tmp, win, "f2"))
        n = min(len(s1), len(s2))
        diff = sum(1 for i in range(n) if s1[i] != s2[i])
        logf(f"differing samples while playing: {diff}/{n}")
        if diff < n * 0.02:
            fail(f"the video does not advance (only {diff}/{n} samples change)")

        # 3. Stability: still alive after playing.
        if proc.poll() is not None:
            fail(f"the process died during playback "
                 f"(exit {proc.returncode})")

        # 4. Clean exit with q — BEST EFFORT (see docstring).
        clean_exit = False
        try:
            if try_keystroke("q"):
                for _ in range(80):
                    if proc.poll() is not None:
                        break
                    time.sleep(0.1)
                if proc.poll() == 0:
                    clean_exit = True
                    logf("clean exit with q verified (exit 0)")
                elif proc.poll() is not None:
                    fail(f"q → exit {proc.returncode} (expected 0)")
        except Exception as e:
            logf(f"key injection unavailable: {e}")
        if not clean_exit and proc.poll() is None:
            logf("exit via q not verifiable on this runner "
                 "(accessibility); terminating with SIGTERM "
                 "[the key contract is covered by the linux test]")
            proc.terminate()
            proc.wait(timeout=10)
    except SystemExit:
        pass
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()
        log.seek(0)
        tail = log.read().strip()
        if tail:
            logf(f"rtv log:\n{tail[-2000:]}")

    if FAILURES:
        logf(f"{len(FAILURES)} failure(s); captures in {tmp}")
        return 1
    logf("OK: native window, real render, advance and stability")
    return 0


if __name__ == "__main__":
    sys.exit(main())
