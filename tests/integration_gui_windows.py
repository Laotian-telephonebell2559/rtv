#!/usr/bin/env python3
"""Native Windows GUI integration test (Win32, no deps).

Pure Python + ctypes against user32/gdi32 — nothing to install on the
Actions runner:

  * window: EnumWindows + GetWindowThreadProcessId → the process HWND.
  * capture: PrintWindow(PW_RENDERFULLCONTENT) → GDI bitmap → BGRA
    bytes (PW_RENDERFULLCONTENT also captures wgpu's DirectX/D3D12
    content on Win10+; if it returns a black frame we retry with a
    screen BitBlt).
  * keys: PostMessage(WM_KEYDOWN/WM_KEYUP) — reaches the window
    thread's queue even without focus (winit reads the message pump).
  * fixture: rawvideo AVI generated in pure Python (reuses make_avi
    from integration_gui_macos.py) — no ffmpeg CLI in the job.

Verifies: the window gets created, the frame has real color variety,
the video ADVANCES between two captures 1 s apart, and `q` (VK 0x51)
produces a clean exit with exit 0.

Usage:
  python tests/integration_gui_windows.py <rtv_gui.exe_path> [video]
"""

import ctypes
import ctypes.wintypes as wt
import os
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from integration_gui_macos import make_avi  # shared AVI fixture

user32 = ctypes.windll.user32
gdi32 = ctypes.windll.gdi32

FAILURES = []


def logf(msg):
    print(f"[gui-win] {msg}", flush=True)


def fail(msg):
    FAILURES.append(msg)
    logf(f"FAILURE: {msg}")


# -------------------------------------------------------------- window ----
def find_hwnd(pid, timeout=30):
    """HWND of the process's MAIN window.

    winit creates several helper windows (thread event target, IME…)
    that can pass the IsWindowVisible filter with a 0x0 client rect —
    on CI EnumWindows would return one of those and the capture failed
    with "degenerate client rect". A real client rect (>=10x10) is
    required, which also covers the moment when the window exists but
    winit has not sized it yet.
    """
    EnumWindowsProc = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)
    found = []

    def cb(hwnd, _):
        wpid = wt.DWORD()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(wpid))
        if wpid.value == pid and user32.IsWindowVisible(hwnd):
            rect = wt.RECT()
            user32.GetClientRect(hwnd, ctypes.byref(rect))
            if rect.right - rect.left >= 10 and rect.bottom - rect.top >= 10:
                found.append(hwnd)
        return True

    t0 = time.time()
    while time.time() - t0 < timeout:
        found.clear()
        user32.EnumWindows(EnumWindowsProc(cb), 0)
        if found:
            return found[0]
        time.sleep(0.5)
    return None


# ------------------------------------------------------------- capture ----
def capture_window(hwnd):
    """BGRA pixels of the window via PrintWindow (or BitBlt fallback)."""
    rect = wt.RECT()
    user32.GetClientRect(hwnd, ctypes.byref(rect))
    w, h = rect.right - rect.left, rect.bottom - rect.top
    if w < 10 or h < 10:
        raise RuntimeError(f"degenerate client rect {w}x{h}")

    hdc_win = user32.GetDC(hwnd)
    hdc_mem = gdi32.CreateCompatibleDC(hdc_win)
    hbmp = gdi32.CreateCompatibleBitmap(hdc_win, w, h)
    gdi32.SelectObject(hdc_mem, hbmp)

    PW_RENDERFULLCONTENT = 0x00000002
    ok = user32.PrintWindow(hwnd, hdc_mem, PW_RENDERFULLCONTENT)
    if not ok:
        # Fallback: copy from the window DC (requires it not to be covered).
        gdi32.BitBlt(hdc_mem, 0, 0, w, h, hdc_win, 0, 0, 0x00CC0020)

    class BITMAPINFOHEADER(ctypes.Structure):
        _fields_ = [("biSize", wt.DWORD), ("biWidth", wt.LONG),
                    ("biHeight", wt.LONG), ("biPlanes", wt.WORD),
                    ("biBitCount", wt.WORD), ("biCompression", wt.DWORD),
                    ("biSizeImage", wt.DWORD),
                    ("biXPelsPerMeter", wt.LONG),
                    ("biYPelsPerMeter", wt.LONG),
                    ("biClrUsed", wt.DWORD), ("biClrImportant", wt.DWORD)]

    bmi = BITMAPINFOHEADER()
    bmi.biSize = ctypes.sizeof(BITMAPINFOHEADER)
    bmi.biWidth, bmi.biHeight = w, -h  # top-down
    bmi.biPlanes, bmi.biBitCount, bmi.biCompression = 1, 32, 0
    buf = ctypes.create_string_buffer(w * h * 4)
    got = gdi32.GetDIBits(hdc_mem, hbmp, 0, h, buf, ctypes.byref(bmi), 0)

    gdi32.DeleteObject(hbmp)
    gdi32.DeleteDC(hdc_mem)
    user32.ReleaseDC(hwnd, hdc_win)
    if got != h:
        raise RuntimeError(f"GetDIBits returned {got}/{h} rows")
    return bytes(buf), w, h


def sample(px, w, h, step=97):
    """(b,g,r) samples every `step` pixels."""
    out = []
    total = w * h
    for i in range(0, total, step):
        off = i * 4
        out.append(px[off:off + 3])
    return out


# ----------------------------------------------------------------- keys ----
def send_key(hwnd, vk):
    WM_KEYDOWN, WM_KEYUP = 0x0100, 0x0101
    scan = user32.MapVirtualKeyW(vk, 0)
    lparam_down = 1 | (scan << 16)
    lparam_up = 1 | (scan << 16) | (1 << 30) | (1 << 31)
    user32.PostMessageW(hwnd, WM_KEYDOWN, vk, lparam_down)
    time.sleep(0.05)
    user32.PostMessageW(hwnd, WM_KEYUP, vk, lparam_up)


# ------------------------------------------------------------------ main ----
def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    rtv = os.path.abspath(sys.argv[1])
    tmp = tempfile.mkdtemp(prefix="rtv-gui-win-")
    if len(sys.argv) > 2:
        video = os.path.abspath(sys.argv[2])
    else:
        video = os.path.join(tmp, "fixture.avi")
        make_avi(video)

    log = open(os.path.join(tmp, "rtv.log"), "w+")
    proc = subprocess.Popen(
        [rtv, "--gui", "--loop-video", "--no-audio", video],
        stdout=log, stderr=log, env=dict(os.environ, RUST_BACKTRACE="1"))
    try:
        hwnd = find_hwnd(proc.pid)
        if hwnd is None:
            fail("the window did not appear within 30 s")
            raise SystemExit
        logf(f"window HWND={hwnd:#x} created")
        time.sleep(2.0)

        # 1. Real render: color variety.
        px1, w, h = capture_window(hwnd)
        s1 = sample(px1, w, h)
        colors = len(set(s1))
        logf(f"capture {w}x{h}, unique colors (sampled): {colors}")
        if colors < 100:
            fail(f"only {colors} unique colors: black/empty window?")

        # 2. Video advance.
        time.sleep(1.0)
        px2, w2, h2 = capture_window(hwnd)
        s2 = sample(px2, w2, h2)
        n = min(len(s1), len(s2))
        diff = sum(1 for i in range(n) if s1[i] != s2[i])
        logf(f"differing samples while playing: {diff}/{n}")
        if diff < n * 0.02:
            fail(f"the video does not advance (only {diff}/{n} samples change)")

        # 3. Still alive.
        if proc.poll() is not None:
            fail(f"the process died during playback "
                 f"(exit {proc.returncode})")

        # 4. Clean exit with q.
        send_key(hwnd, 0x51)  # VK 'Q'
        for _ in range(80):
            if proc.poll() is not None:
                break
            time.sleep(0.1)
        if proc.poll() is None:
            fail("q did not close the process within 8 s")
        elif proc.returncode != 0:
            fail(f"q → exit {proc.returncode} (expected 0)")
        else:
            logf("clean exit with q verified (exit 0)")
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
        logf(f"{len(FAILURES)} failure(s)")
        return 1
    logf("OK: native window, real render, advance and clean exit with q")
    return 0


if __name__ == "__main__":
    sys.exit(main())
