#!/usr/bin/env python3
"""Integration test: Display Matrix auto-rotation.

Fixture: a LANDSCAPE 320x180 video whose TOP half is red and BOTTOM
half blue, with the Display Matrix of a portrait-mode iPhone written
straight into the MP4 tkhd box (ffmpeg's -display_rotation option only
exists since FFmpeg 6, and the CI runners ship 4.4).
Presented correctly (90° clockwise) it becomes PORTRAIT 180x320 with
the LEFT half blue and the RIGHT half red.

It plays with --backend blocks in a pty and parses the 24-bit color
SGRs (38;2;r;g;b / 48;2;r;g;b) from the output: the test demands
dominant red on the right half and blue on the left — a real pixel
check of the Display Matrix → transposed sws → rotate_frame → render
chain, not just "does not crash".

Usage: integration_rotation.py <rtv>
"""

import os
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import tempfile
import time

COLS, ROWS = 80, 40


def run(cmd, **kw):
    subprocess.run(cmd, check=True, **kw)


def write_rot90_matrix(path):
    """Writes into the tkhd box the Display Matrix of an iPhone recording
    in portrait (90° clockwise presentation): [0, 1, 0; -1, 0, 0; 0, 0, 1]
    in 16.16 fixed point (w in 2.30). It is the SAME matrix that the FFI
    unit test in src/rotation.rs validates, and it does not depend on the
    ffmpeg version (-display_rotation only exists since FFmpeg 6)."""
    with open(path, "rb") as f:
        data = bytearray(f.read())

    def find_box(kind, start, end):
        """Returns (payload_start, payload_end) of the first `kind` box
        among sibling boxes — a real ISO-BMFF walk, not a blind find()
        (the bytes could show up inside the mdat)."""
        pos = start
        while pos + 8 <= end:
            size = int.from_bytes(data[pos:pos + 4], "big")
            name = bytes(data[pos + 4:pos + 8])
            hdr = 8
            if size == 1:
                size = int.from_bytes(data[pos + 8:pos + 16], "big")
                hdr = 16
            elif size == 0:
                size = end - pos
            if name == kind:
                return pos + hdr, pos + size
            pos += size
        raise AssertionError(f"no {kind!r} box in the MP4")

    s, e = find_box(b"moov", 0, len(data))
    s, e = find_box(b"trak", s, e)
    idx, _ = find_box(b"tkhd", s, e)
    idx -= 4  # the math below starts from the 'tkhd' name
    version = data[idx + 4]
    # Matrix offset within the payload: version/flags (4) +
    # times/track_id/duration (20 in v0, 32 in v1) + reserved (8) +
    # layer/alternate_group/volume/reserved (8).
    off = idx + 4 + 4 + (32 if version == 1 else 20) + 8 + 8
    # Byte for byte the SAME thing `ffmpeg -display_rotation -90` writes
    # (verified by extracting the tkhd from a reference fixture): the mov
    # demuxer hands it over verbatim as AV_PKT_DATA_DISPLAYMATRIX and
    # av_display_rotation_get returns -90 → 90° clockwise presentation.
    m = [0, 65536, 0, -65536, 0, 0, 0, 0, 1 << 30]
    data[off:off + 36] = struct.pack(">9i", *m)
    with open(path, "wb") as f:
        f.write(data)


def make_fixture(tmp):
    video = os.path.join(tmp, "redblue_rot90.mp4")
    run([
        "ffmpeg", "-y", "-loglevel", "error",
        "-f", "lavfi", "-i", "color=red:size=320x90:rate=30",
        "-f", "lavfi", "-i", "color=blue:size=320x90:rate=30",
        "-filter_complex", "[0][1]vstack", "-t", "2",
        "-c:v", "libx264", "-pix_fmt", "yuv420p", video,
    ])
    write_rot90_matrix(video)
    return video


def capture_pty(rtv, video, seconds=4.0):
    """Plays `video` in a pty and returns the raw output."""
    import fcntl
    import struct
    import termios
    mfd, sfd = pty.openpty()
    fcntl.ioctl(sfd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    env = dict(os.environ, TERM="xterm-256color")
    p = subprocess.Popen(
        [rtv, video, "--backend", "blocks", "--audio-backend", "none"],
        stdin=sfd, stdout=sfd, stderr=sfd, env=env, close_fds=True,
    )
    os.close(sfd)
    out = bytearray()
    deadline = time.time() + seconds
    while time.time() < deadline and p.poll() is None:
        r, _, _ = select.select([mfd], [], [], 0.2)
        if r:
            try:
                out += os.read(mfd, 65536)
            except OSError:
                break
    if p.poll() is None:
        try:
            os.write(mfd, b"q")
        except OSError:
            pass
        try:
            p.wait(timeout=5)
        except subprocess.TimeoutExpired:
            p.send_signal(signal.SIGKILL)
            p.wait()
    # Final drain.
    while True:
        r, _, _ = select.select([mfd], [], [], 0.1)
        if not r:
            break
        try:
            chunk = os.read(mfd, 65536)
        except OSError:
            break
        if not chunk:
            break
        out += chunk
    os.close(mfd)
    return bytes(out)


# The blocks backend emits FG and BG as SEPARATE SGR sequences
# (\x1b[38;2;r;g;bm and \x1b[48;2;r;g;bm) and draws '▀' per cell.
# Strategy: keep the current color and, on each '▀', attribute it to
# the left/right half based on the column (CUPs set the column; each
# glyph advances one). ANY other CSI sequence is skipped WHOLE
# (skipping just the ESC would corrupt the column count).
FG = re.compile(rb"\x1b\[38;2;(\d+);(\d+);(\d+)m")
BG = re.compile(rb"\x1b\[48;2;(\d+);(\d+);(\d+)m")
CUP = re.compile(rb"\x1b\[(\d+);(\d+)H")
CSI = re.compile(rb"\x1b\[[0-9;?<=>]*[a-zA-Z@`~]|\x1b.")
HALFBLOCK = b"\xe2\x96\x80"  # '▀' UTF-8


def classify(r, g, b):
    if r > 128 and b < 100:
        return "red"
    if b > 128 and r < 100:
        return "blue"
    return None


def analyze(raw):
    """Walks the output following the CUPs: counts red/blue per half."""
    left = {"red": 0, "blue": 0}
    right = {"red": 0, "blue": 0}
    col = 1
    cur = []  # current fg/bg classification
    i = 0
    while i < len(raw):
        if raw[i] == 0x1B:
            m = CUP.match(raw, i)
            if m:
                col = int(m.group(2))
                i = m.end()
                continue
            m = FG.match(raw, i) or BG.match(raw, i)
            if m:
                k = classify(*(int(x) for x in m.groups()))
                is_fg = raw[i + 2:i + 4] == b"38"
                # blocks emits fg and bg as a pair: reset on seeing the fg.
                if is_fg:
                    cur = [k] if k else []
                elif k:
                    cur.append(k)
                i = m.end()
                continue
            m = CSI.match(raw, i)
            i = m.end() if m else i + 1
            continue
        if raw[i:i + 3] == HALFBLOCK:
            for k in cur:
                (left if col <= COLS // 2 else right)[k] += 1
            col += 1
            i += 3
            continue
        ch = raw[i]
        if ch >= 0x20 and (ch & 0xC0) != 0x80:
            col += 1
        i += 1
    return left, right


def main():
    rtv = sys.argv[1]
    with tempfile.TemporaryDirectory() as tmp:
        video = make_fixture(tmp)
        # --info: transposed presented dims + rotation label.
        info = subprocess.run(
            [rtv, "--info", video], capture_output=True, text=True, timeout=60
        ).stdout
        assert "180x320" in info, f"--info lacks transposed dims:\n{info}"
        assert "rotated 90" in info, f"--info lacks the rotation label:\n{info}"
        print("[ok] --info: 180x320 + 'rotated 90°'")

        raw = capture_pty(rtv, video)
        assert len(raw) > 1000, "empty pty output — did rtv crash?"
        left, right = analyze(raw)
        print(f"[dbg] left={left} right={right}")
        # Left half: dominant blue; right half: dominant red.
        assert left["blue"] > left["red"] * 3 and left["blue"] > 50, \
            f"left half is not blue: {left}"
        assert right["red"] > right["blue"] * 3 and right["red"] > 50, \
            f"right half is not red: {right}"
        print("[ok] pixels: left blue / right red — rotation is correct")
    print("PASS integration_rotation")
    return 0


if __name__ == "__main__":
    sys.exit(main())
