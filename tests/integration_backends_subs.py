#!/usr/bin/env python3
"""Integration test: Sixel/iTerm2 backends and softsub subtitles.

Validates over a real PTY:
  1. sixel  — DCS `ESC P 0;1;0 q` per frame, full 6×7×6 palette,
              only protocol-valid characters, closing ST.
  2. iterm2 — OSC 1337 File= per frame, decodable base64 BMP with a
              coherent header (size == real len), dims in cells.
  3. external SRT subs (--sub): the text appears on screen within its
     time window, HTML tags stripped.
  4. embedded MKV subs: the container's subrip track is shown with no
     flags.
  5. --no-subs: no text appears.
  6. kitty and blocks regression: they still emit their protocol.

Usage:  python3 tests/integration_backends_subs.py [rtv_path]
"""

import base64
import fcntl
import os
import pty
import re
import select
import struct
import subprocess
import sys
import tempfile
import termios
import time

RTV = sys.argv[1] if len(sys.argv) > 1 else "./target/release/rtv"


def run(args, secs=6, cols=100, rows=30):
    """Runs rtv in a PTY, captures `secs` of output and exits with 'q'
    (retried: with massive output a single keypress can get lost in
    the PTY buffer)."""
    m, s = pty.openpty()
    fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 800, 480))
    p = subprocess.Popen([RTV] + args, stdin=s, stdout=s, stderr=subprocess.DEVNULL)
    os.close(s)
    buf = bytearray()
    t0 = time.time()
    while time.time() - t0 < secs:
        r, _, _ = select.select([m], [], [], 0.05)
        if r:
            try:
                buf += os.read(m, 1 << 20)
            except OSError:
                break
    t0 = time.time()
    last_q = 0.0
    while p.poll() is None and time.time() - t0 < 15:
        if time.time() - last_q > 0.5:
            try:
                os.write(m, b"q")
            except OSError:
                break
            last_q = time.time()
        r, _, _ = select.select([m], [], [], 0.02)
        if r:
            try:
                buf += os.read(m, 1 << 20)
            except OSError:
                break
    # The loop exits with OSError/EIO when the child closes the PTY: it
    # may not be reaped yet — truly wait before reading rc.
    try:
        rc = p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        p.kill()
        rc = None
    os.close(m)
    return bytes(buf), rc


def make_assets(tmp):
    video = os.path.join(tmp, "t.mp4")
    srt = os.path.join(tmp, "t.srt")
    mkv = os.path.join(tmp, "t.mkv")
    subprocess.run(
        ["ffmpeg", "-y", "-f", "lavfi", "-i",
         "testsrc2=size=640x360:rate=25:duration=15",
         "-f", "lavfi", "-i", "sine=frequency=440:duration=15",
         "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", video],
        check=True, capture_output=True)
    with open(srt, "w") as f:
        f.write("1\n00:00:01,000 --> 00:00:04,000\nHola mundo subtitulado\n\n"
                "2\n00:00:05,000 --> 00:00:08,000\nSegunda <i>línea</i> de prueba\n")
    subprocess.run(
        ["ffmpeg", "-y", "-i", video, "-i", srt, "-c:v", "copy", "-c:a",
         "copy", "-c:s", "srt", "-metadata:s:s:0", "language=spa", mkv],
        check=True, capture_output=True)
    return video, srt, mkv


def main():
    fails = []

    def check(name, cond, detail=""):
        status = "OK " if cond else "FAIL"
        print(f"  [{status}] {name}" + (f" — {detail}" if detail else ""))
        if not cond:
            fails.append(name)

    with tempfile.TemporaryDirectory() as tmp:
        video, srt, mkv = make_assets(tmp)

        print("== 1) backend sixel ==")
        out, rc = run([video, "--backend", "sixel", "--no-audio"])
        n = out.count(b"\x1bP0;1;0q")
        check("clean exit (rc=0)", rc == 0, f"rc={rc}")
        check("frames DCS sixel", n >= 5, f"{n} frames")
        check("ST per frame", out.count(b"\x1b\\") >= n)
        i = out.find(b"\x1bP0;1;0q")
        j = out.find(b"\x1b\\", i)
        body = out[i + 8:j] if i >= 0 and j > i else b""
        check("only protocol-valid chars",
              bool(re.fullmatch(rb'["#;0-9!$\-\?-~]*', body)))
        check("full palette (reg 0 and 251)",
              b"#0;2;0;0;0" in body and b"#251;2;100;100;100" in body)

        print("== 2) backend iterm2 ==")
        out, rc = run([video, "--backend", "iterm2", "--no-audio"])
        n = out.count(b"\x1b]1337;File=inline=1;")
        check("clean exit (rc=0)", rc == 0, f"rc={rc}")
        check("frames OSC 1337", n >= 5, f"{n} frames")
        i = out.find(b"\x1b]1337;File=")
        colon = out.find(b":", i)
        bel = out.find(b"\x07", colon)
        ok_bmp = False
        if 0 <= i < colon < bel:
            try:
                bmp = base64.b64decode(out[colon + 1:bel], validate=True)
                fsize = struct.unpack("<I", bmp[2:6])[0]
                ok_bmp = bmp[:2] == b"BM" and fsize == len(bmp)
            except Exception:
                pass
        check("valid, coherent base64 BMP", ok_bmp)
        hdr = out[i:colon].decode("ascii", "replace") if i >= 0 else ""
        check("cell dims in the header", "width=" in hdr and "height=" in hdr)

        print("== 3) external SRT subs ==")
        out, rc = run([video, "--sub", srt, "--no-audio", "--backend", "blocks"], secs=7)
        txt = out.decode("utf-8", "replace")
        check("event 1 visible", "Hola mundo subtitulado" in txt)
        check("event 2 visible", "Segunda línea de prueba" in txt)
        check("HTML tags stripped", "<i>" not in txt)

        print("== 4) embedded MKV subs (--sub with no value) ==")
        out, rc = run([mkv, "--sub", "--no-audio", "--backend", "blocks"], secs=7)
        txt = out.decode("utf-8", "replace")
        check("embedded track visible", "Hola mundo subtitulado" in txt)

        print("== 5) no --sub → no subtitles (default) ==")
        out, rc = run([mkv, "--no-audio", "--backend", "blocks"], secs=6)
        txt = out.decode("utf-8", "replace")
        check("no sub text by default", "Hola mundo subtitulado" not in txt)

        print("== 5b) --no-subs (compat) ==")
        out, rc = run([mkv, "--sub", "--no-subs", "--no-audio", "--backend", "blocks"], secs=6)
        txt = out.decode("utf-8", "replace")
        check("--no-subs wins over --sub", "Hola mundo subtitulado" not in txt)

        print("== 6) kitty / blocks regression ==")
        out, rc = run([video, "--backend", "kitty", "--no-audio"], secs=4)
        check("kitty emits APC _G", rc == 0 and out.count(b"\x1b_G") >= 5)
        out, rc = run([video, "--backend", "blocks", "--no-audio"], secs=4)
        check("blocks emits SGR truecolor", rc == 0 and b"\x1b[38;2;" in out)

    print()
    if fails:
        print(f"RESULTADO: {len(fails)} fallos: {fails}")
        sys.exit(1)
    print("RESULTADO: todos los checks OK")


if __name__ == "__main__":
    main()
