#!/usr/bin/env python3
"""Generate one demo GIF per rendering backend for the README.

Plays a short synthetic clip in a real pty with each backend and
decodes what rtv actually emits to the terminal:

  - kitty:  parses the kitty graphics protocol chunks (a=T, m=0/1),
            base64 + optional zlib (o=z) -> raw RGB frames.
  - iterm2: parses OSC 1337 File= payloads -> BMP -> frames.
  - sixel:  extracts each DCS sixel image and decodes it with
            sixel2png (libsixel-bin).
  - blocks / ascii: feeds the byte stream to a pyte terminal emulator
            and rasterizes the screen (half-block cells as two solid
            rectangles, everything else as monospace glyphs).

Usage: python3 scripts/capture_demo_gifs.py <rtv-binary> <outdir>
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
import termios
import time
import zlib

from PIL import Image, ImageDraw, ImageFont

COLS, ROWS = 100, 30
CELL_W, CELL_H = 8, 16  # rtv's defaults when the terminal doesn't report px
# rtv reserves 2 rows for the HUD at 100x30 (see hud_rows_for in
# src/player.rs). The text-backend GIFs crop them out so every demo
# shows just the video, like the graphics backends do.
HUD_ROWS = 2
FIXTURE = "/tmp/rtv_gif_fixture.mp4"
GIF_MAX_W = 560
GIF_FPS = 10
GIF_SECONDS = 5

FONT_DIR = "/usr/share/fonts/truetype/dejavu"


def make_fixture():
    """Branded demo clip, entirely synthetic (lavfi):

      - animated diagonal gradient (navy -> blue -> violet -> cyan)
        with a faint 64 px grid;
      - staggered fade-ins: title, tagline, backend list;
      - running timecode + format stamp in the corners;
      - subtle vignette to focus the center.
    """
    if os.path.exists(FIXTURE):
        return
    dur = GIF_SECONDS + 2
    bold = f"{FONT_DIR}/DejaVuSans-Bold.ttf"
    sans = f"{FONT_DIR}/DejaVuSans.ttf"
    mono = f"{FONT_DIR}/DejaVuSansMono.ttf"
    vf = (
        f"[0:v]"
        f"drawgrid=w=64:h=64:t=1:c=white@0.04,"
        f"drawtext=fontfile={bold}:text='rtv':fontsize=190:fontcolor=white:"
        f"borderw=3:bordercolor=black@0.35:x=(w-text_w)/2:y=(h-text_h)/2-110:"
        f"alpha='min(1,t/0.8)',"
        f"drawtext=fontfile={sans}:text='terminal video player':fontsize=48:"
        f"fontcolor=white@0.92:x=(w-text_w)/2:y=h/2+58:"
        f"alpha='if(lt(t,0.6),0,min(1,(t-0.6)/0.8))',"
        f"drawtext=fontfile={mono}:"
        f"text='kitty · iTerm2 · sixel · blocks · ascii':fontsize=34:"
        f"fontcolor=0x9fd8ff:x=(w-text_w)/2:y=h/2+150:"
        f"alpha='if(lt(t,1.8),0,min(1,(t-1.8)/0.8))',"
        f"drawtext=fontfile={mono}:text='%{{pts\\:hms}}':fontsize=30:"
        f"fontcolor=white@0.55:x=w-text_w-40:y=h-text_h-28,"
        f"drawtext=fontfile={mono}:text='1280x720 · 30 fps':fontsize=30:"
        f"fontcolor=white@0.55:x=40:y=h-text_h-28,"
        f"vignette=PI/5,"
        f"fade=t=in:st=0:d=0.5[final]"
    )
    subprocess.run(
        [
            "ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
            "-f", "lavfi", "-i",
            f"gradients=size=1280x720:rate=30:duration={dur}:speed=0.05:"
            f"nb_colors=4:c0=0x0b1220:c1=0x1e3a8a:c2=0x6d28d9:c3=0x0891b2",
            "-f", "lavfi", "-i", f"sine=frequency=440:duration={dur}",
            "-filter_complex", vf,
            "-map", "[final]", "-map", "1:a",
            "-c:v", "libx264", "-preset", "fast", "-crf", "22",
            "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "96k",
            "-movflags", "+faststart",
            FIXTURE,
        ],
        check=True,
    )


def run_capture(binary, backend, snapshot_cb=None, chunk_cb=None):
    """Runs rtv in a pty and returns the full byte stream.

    If snapshot_cb is given it is called as snapshot_cb(stream_so_far)
    roughly every 1/GIF_FPS seconds (used by the pyte-based backends).
    """
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

    env = dict(os.environ)
    for var in ("KITTY_WINDOW_ID", "TERM_PROGRAM", "WT_SESSION", "COLORTERM",
                "LC_TERMINAL", "MLTERM"):
        env.pop(var, None)
    env["TERM"] = "xterm-256color"
    # The pty is not a real kitty: force the base64 transport so the
    # frames travel inside the escape stream (decodable offline).
    env["RTV_KITTY_NO_SHM"] = "1"

    p = subprocess.Popen(
        [binary, "--backend", backend, "--no-audio", FIXTURE],
        stdin=slave, stdout=slave, stderr=subprocess.DEVNULL,
        env=env, close_fds=True,
    )
    os.close(slave)

    # Continuous pty reader in a thread: without it the 64 KB pty
    # buffer fills while we decode, rtv blocks in write() and starts
    # dropping frames to keep realtime — harness latency, not the
    # player's (same lesson as tests/integration_resize.py).
    import threading
    chunks = []
    lock = threading.Lock()
    done = threading.Event()

    def reader():
        while not done.is_set():
            r, _, _ = select.select([master], [], [], 0.05)
            if not r:
                if p.poll() is not None:
                    break
                continue
            try:
                data = os.read(master, 1 << 18)
            except OSError:
                break
            if not data:
                break
            if chunk_cb:
                chunk_cb(data)
            else:
                with lock:
                    chunks.append(data)

    th = threading.Thread(target=reader, daemon=True)
    th.start()

    deadline = time.time() + GIF_SECONDS + 8
    next_snap = time.time() + 1.0 / GIF_FPS
    try:
        while time.time() < deadline:
            time.sleep(0.02)
            if snapshot_cb and time.time() >= next_snap:
                with lock:
                    snap_stream = b"".join(chunks)
                snapshot_cb(snap_stream)
                next_snap += 1.0 / GIF_FPS
            if p.poll() is not None and not th.is_alive():
                break
    finally:
        done.set()
        if p.poll() is None:
            p.terminate()
            try:
                p.wait(timeout=3)
            except subprocess.TimeoutExpired:
                p.kill()
        th.join(timeout=2)
        os.close(master)
    with lock:
        return b"".join(chunks)


# ---------------------------------------------------------------- kitty ---
KITTY_RE = re.compile(rb"\x1b_G([^;\x1b]*)(?:;([^\x1b]*))?\x1b\\")


def decode_kitty(stream):
    """Returns a list of PIL Images from the kitty graphics escapes."""
    frames = []
    pending = None  # (params_dict, payload_parts)
    for m in KITTY_RE.finditer(stream):
        params = dict(
            kv.split(b"=", 1) for kv in m.group(1).split(b",") if b"=" in kv
        )
        payload = m.group(2) or b""
        if params.get(b"a") == b"T":
            pending = (params, [payload])
            if params.get(b"m", b"0") == b"0":
                frames.append(_kitty_finish(pending))
                pending = None
        elif b"m" in params and pending is not None:
            pending[1].append(payload)
            if params[b"m"] == b"0":
                frames.append(_kitty_finish(pending))
                pending = None
    return [f for f in frames if f is not None]


def _kitty_finish(pending):
    params, parts = pending
    try:
        raw = base64.b64decode(b"".join(parts))
        if params.get(b"o") == b"z":
            raw = zlib.decompress(raw)
        w, h = int(params[b"s"]), int(params[b"v"])
        if len(raw) < w * h * 3:
            return None
        return Image.frombytes("RGB", (w, h), raw[: w * h * 3])
    except Exception:
        return None


# --------------------------------------------------------------- iterm2 ---
ITERM_RE = re.compile(rb"\x1b\]1337;File=[^:]*:([A-Za-z0-9+/=]+)\x07")


def decode_iterm2(stream):
    import io
    frames = []
    for m in ITERM_RE.finditer(stream):
        try:
            frames.append(
                Image.open(io.BytesIO(base64.b64decode(m.group(1)))).convert("RGB")
            )
        except Exception:
            pass
    return frames


# ---------------------------------------------------------------- sixel ---
SIXEL_RE = re.compile(rb"\x1bP[0-9;]*q.*?\x1b\\", re.DOTALL)


def decode_sixel(stream):
    frames = []
    for i, m in enumerate(SIXEL_RE.finditer(stream)):
        six_path = f"/tmp/_gif_sixel_{i}.six"
        png_path = f"/tmp/_gif_sixel_{i}.png"
        with open(six_path, "wb") as f:
            f.write(m.group(0))
        r = subprocess.run(["sixel2png", "-i", six_path, "-o", png_path],
                           capture_output=True)
        if r.returncode == 0 and os.path.exists(png_path):
            frames.append(Image.open(png_path).convert("RGB"))
        for pth in (six_path, png_path):
            try:
                os.unlink(pth)
            except FileNotFoundError:
                pass
    return frames


# --------------------------------------------------------- blocks/ascii ---
NAMED = {
    "black": (0, 0, 0), "red": (205, 49, 49), "green": (13, 188, 121),
    "brown": (229, 229, 16), "blue": (36, 114, 200), "magenta": (188, 63, 188),
    "cyan": (17, 168, 205), "white": (229, 229, 229), "default": None,
}


def _color(c, fallback):
    if c in NAMED:
        v = NAMED[c]
        return v if v is not None else fallback
    try:
        return tuple(int(c[i:i + 2], 16) for i in (0, 2, 4))
    except Exception:
        return fallback


def render_text_screen(screen, font):
    # Crop the HUD rows: the demo GIFs show only the video, on every
    # backend (the graphics decoders never see the HUD text either).
    vis_rows = ROWS - HUD_ROWS
    img = Image.new("RGB", (COLS * CELL_W, vis_rows * CELL_H), (0, 0, 0))
    dr = ImageDraw.Draw(img)
    for row in range(vis_rows):
        buf = screen.buffer[row]
        for col in range(COLS):
            ch = buf[col]
            fg = _color(ch.fg, (229, 229, 229))
            bg = _color(ch.bg, (0, 0, 0))
            if ch.reverse:
                fg, bg = bg, fg
            x, y = col * CELL_W, row * CELL_H
            if ch.data == "▀":
                dr.rectangle([x, y, x + CELL_W - 1, y + CELL_H // 2 - 1], fill=fg)
                dr.rectangle([x, y + CELL_H // 2, x + CELL_W - 1, y + CELL_H - 1],
                             fill=bg)
            elif ch.data == "▄":
                dr.rectangle([x, y, x + CELL_W - 1, y + CELL_H // 2 - 1], fill=bg)
                dr.rectangle([x, y + CELL_H // 2, x + CELL_W - 1, y + CELL_H - 1],
                             fill=fg)
            elif ch.data == "█":
                dr.rectangle([x, y, x + CELL_W - 1, y + CELL_H - 1], fill=fg)
            else:
                if bg != (0, 0, 0):
                    dr.rectangle([x, y, x + CELL_W - 1, y + CELL_H - 1], fill=bg)
                if ch.data and ch.data != " ":
                    dr.text((x, y), ch.data, fill=fg, font=font)
    return img


def capture_text_backend(binary, backend):
    import pyte
    font = None
    for cand in (
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
    ):
        if os.path.exists(cand):
            font = ImageFont.truetype(cand, CELL_H - 2)
            break
    if font is None:
        font = ImageFont.load_default()

    frames = []
    screen = pyte.Screen(COLS, ROWS)
    parser = pyte.Stream(screen)
    fed = [0]

    def snap(stream):
        try:
            parser.feed(stream[fed[0]:].decode("utf-8", "replace"))
            fed[0] = len(stream)
            frames.append(render_text_screen(screen, font))
        except Exception:
            pass

    run_capture(binary, backend, snapshot_cb=snap)
    return frames




def _shrink(img):
    """Downscales a decoded frame immediately (memory guard)."""
    w, h = img.size
    if w > GIF_MAX_W:
        img = img.resize((GIF_MAX_W, int(h * GIF_MAX_W / w)), Image.LANCZOS)
    return img


class IncrementalDecoder:
    """Buffers pty bytes, extracts complete escapes with a regex and
    decodes them on the fly, keeping only downscaled frames."""

    def __init__(self, regex, decode_one):
        self.regex = regex
        self.decode_one = decode_one
        self.buf = b""
        self.frames = []

    def feed(self, data):
        self.buf += data
        last_end = 0
        for m in self.regex.finditer(self.buf):
            img = self.decode_one(m)
            if img is not None:
                self.frames.append(_shrink(img))
            last_end = m.end()
        if last_end:
            self.buf = self.buf[last_end:]
        elif len(self.buf) > (64 << 20):
            # Pathological: no match in 64 MB — drop to stay alive.
            self.buf = self.buf[-(1 << 20):]


def _decode_kitty_match(m):
    """kitty is chunked (m=1/0): reassemble via a tiny state machine."""
    params = dict(kv.split(b"=", 1) for kv in m.group(1).split(b",") if b"=" in kv)
    payload = m.group(2) or b""
    st = _decode_kitty_match
    if params.get(b"a") == b"T":
        st.pending = (params, [payload])
        if params.get(b"m", b"0") == b"0":
            img, st.pending = _kitty_finish(st.pending), None
            return img
    elif b"m" in params and getattr(st, "pending", None) is not None:
        st.pending[1].append(payload)
        if params[b"m"] == b"0":
            img, st.pending = _kitty_finish(st.pending), None
            return img
    return None


def _decode_iterm2_match(m):
    import io
    try:
        return Image.open(io.BytesIO(base64.b64decode(m.group(1)))).convert("RGB")
    except Exception:
        return None


def _decode_sixel_match(m):
    six_path = "/tmp/_gif_sixel_cur.six"
    png_path = "/tmp/_gif_sixel_cur.png"
    with open(six_path, "wb") as f:
        f.write(m.group(0))
    r = subprocess.run(["sixel2png", "-i", six_path, "-o", png_path],
                       capture_output=True)
    img = None
    if r.returncode == 0 and os.path.exists(png_path):
        img = Image.open(png_path).convert("RGB")
        img.load()
    for pth in (six_path, png_path):
        try:
            os.unlink(pth)
        except FileNotFoundError:
            pass
    return img


def capture_graphics_backend(binary, backend):
    """Spools the raw pty stream to disk during playback (so the
    reader thread never stalls behind the decoder) and decodes the
    escapes offline afterwards."""
    spool = f"/home/user/_gif_spool_{backend}.bin"
    with open(spool, "wb", buffering=0) as f:
        run_capture(binary, backend, chunk_cb=f.write)
    dec = IncrementalDecoder(*{
        "kitty": (KITTY_RE, _decode_kitty_match),
        "iterm2": (ITERM_RE, _decode_iterm2_match),
        "sixel": (SIXEL_RE, _decode_sixel_match),
    }[backend])
    _decode_kitty_match.pending = None
    size = os.path.getsize(spool)
    print(f"  {size // 1024} KB spooled")
    with open(spool, "rb") as f:
        while True:
            data = f.read(1 << 22)
            if not data:
                break
            dec.feed(data)
    os.unlink(spool)
    return dec.frames


# ------------------------------------------------------------------ gif ---
def save_gif(frames, path):
    if not frames:
        print(f"  !! no frames for {path}")
        return False
    want = GIF_SECONDS * GIF_FPS
    if len(frames) > want:
        step = len(frames) / want
        frames = [frames[int(i * step)] for i in range(want)]
    w, h = frames[0].size
    if w > GIF_MAX_W:
        nh = int(h * GIF_MAX_W / w)
        frames = [f.resize((GIF_MAX_W, nh), Image.LANCZOS) for f in frames]
    frames = [f.quantize(colors=128, method=Image.MEDIANCUT) for f in frames]
    frames[0].save(
        path, save_all=True, append_images=frames[1:],
        duration=int(1000 / GIF_FPS), loop=0, optimize=True,
    )
    kb = os.path.getsize(path) // 1024
    print(f"  -> {path} ({len(frames)} frames, {kb} KB)")
    return True


def main():
    if len(sys.argv) not in (3, 4):
        print(__doc__)
        sys.exit(2)
    binary, outdir = sys.argv[1], sys.argv[2]
    os.makedirs(outdir, exist_ok=True)
    make_fixture()

    backends = ("kitty", "iterm2", "sixel", "blocks", "ascii")
    if len(sys.argv) == 4:
        backends = (sys.argv[3],)
    else:
        # The graphics protocols hold many uncompressed RGB frames in
        # memory: isolate each backend in its own process so the peak
        # never accumulates (the dev sandbox is memory-tight).
        rc = 0
        for b in backends:
            r = subprocess.run([sys.executable, __file__, binary, outdir, b])
            rc |= r.returncode
        sys.exit(rc)

    ok = True
    for backend in backends:
        print(f"[{backend}] capturing…")
        if backend in ("blocks", "ascii"):
            frames = capture_text_backend(binary, backend)
        else:
            frames = capture_graphics_backend(binary, backend)
        print(f"  {len(frames)} frames decoded")
        ok &= save_gif(frames, os.path.join(outdir, f"demo-{backend}.gif"))
        del frames
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
