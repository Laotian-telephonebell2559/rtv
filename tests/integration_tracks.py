#!/usr/bin/env python3
"""Integration test: runtime audio/subtitle track switching.

Builds an MKV with:
  * test video (smptebars 640x360 25fps, 30 s)
  * 2 audio tracks with distinct TONES (440 Hz eng / 880 Hz spa)
  * 2 SRT subtitle tracks (eng: "ENGLISH ...", spa: "SPANISH ...")

Checks (real pty):
  1. `j` key cycles subtitles: off -> eng -> spa; the on-screen text
     changes ("ENGLISH" then "SPANISH" is searched in the pty output)
     and the "Subs [" OSD shows up.
  2. `a` key cycles the audio track WITHOUT breaking sync: after the
     switch, the sync-log median |avdiff| stays < 60 ms and the player
     keeps rendering frames (no freeze).
  3. --aid/--alang/--sid/--slang pick the track at startup.
  4. `a` with a single audio track: informative OSD, nothing breaks.
  5. Clean exit with `q` (exit 0) in every case.
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
import termios
import time
import fcntl

RTV = os.environ.get("RTV_BIN") or os.path.join(
    os.path.dirname(__file__), "..", "target", "release", "rtv")
FAIL = 0


def check(name, ok, detail=""):
    global FAIL
    tag = "PASS" if ok else "FAIL"
    print(f"[{tag}] {name}" + (f" — {detail}" if detail else ""))
    if not ok:
        FAIL = 1


def make_media(tmp):
    """MKV with 2 audio tracks (440/880 tones) + 2 subs (eng/spa)."""
    mkv = os.path.join(tmp, "multi.mkv")
    srt_en = os.path.join(tmp, "en.srt")
    srt_es = os.path.join(tmp, "es.srt")
    with open(srt_en, "w") as f:
        for i in range(30):
            f.write(f"{i+1}\n00:00:{i:02d},000 --> 00:00:{i:02d},900\nENGLISH LINE {i}\n\n")
    with open(srt_es, "w") as f:
        for i in range(30):
            f.write(f"{i+1}\n00:00:{i:02d},000 --> 00:00:{i:02d},900\nSPANISH LINE {i}\n\n")
    subprocess.run(
        [
            "ffmpeg", "-y", "-v", "error",
            "-f", "lavfi", "-i", "smptebars=size=640x360:rate=25",
            "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100",
            "-f", "lavfi", "-i", "sine=frequency=880:sample_rate=44100",
            "-i", srt_en, "-i", srt_es,
            "-map", "0:v", "-map", "1:a", "-map", "2:a", "-map", "3:s", "-map", "4:s",
            "-t", "30",
            "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
            "-c:a", "aac", "-b:a", "96k",
            "-c:s", "srt",
            "-metadata:s:a:0", "language=eng",
            "-metadata:s:a:1", "language=spa",
            "-metadata:s:s:0", "language=eng",
            "-metadata:s:s:1", "language=spa",
            mkv,
        ],
        check=True,
    )
    return mkv


def spawn(args, cols=100, rows=30, env_extra=None):
    env = dict(os.environ)
    env["TERM"] = "xterm-256color"
    if env_extra:
        env.update(env_extra)
    m, s = pty.openpty()
    fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    p = subprocess.Popen(
        [RTV] + args, stdin=s, stdout=s, stderr=subprocess.DEVNULL,
        env=env, close_fds=True,
    )
    os.close(s)
    return p, m


def read_pty(m, buf, dur):
    """Reads from the pty for `dur` s, accumulating into buf (bytearray)."""
    end = time.time() + dur
    while time.time() < end:
        r, _, _ = select.select([m], [], [], 0.1)
        if r:
            try:
                data = os.read(m, 65536)
            except OSError:
                break
            if not data:
                break
            buf.extend(data)
    return buf


def finish(p, m, timeout=5.0):
    try:
        os.write(m, b"q")
    except OSError:
        pass
    t0 = time.time()
    while p.poll() is None and time.time() - t0 < timeout:
        # Drain the pty so rtv does not block on write().
        r, _, _ = select.select([m], [], [], 0.05)
        if r:
            try:
                os.read(m, 65536)
            except OSError:
                break
    # Careful: if draining the pty fails with EIO (the process just
    # exited), `p.poll()` can still return None for a moment (zombie
    # not reaped yet) — using wait() with the remaining time avoids
    # flagging clean exits as -9.
    remaining = max(0.5, timeout - (time.time() - t0))
    try:
        rc = p.wait(timeout=remaining)
    except subprocess.TimeoutExpired:
        p.kill()
        p.wait()
        rc = -9
    try:
        os.close(m)
    except OSError:
        pass
    return rc


def strip_ansi(b):
    txt = b.decode("utf-8", "replace")
    txt = re.sub(r"\x1b\][^\x07\x1b]*(\x07|\x1b\\)", "", txt)  # OSC
    txt = re.sub(r"\x1b[PX^_][^\x1b]*\x1b\\", "", txt)  # DCS/PM/APC
    txt = re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", txt)  # CSI
    return txt


def parse_sync_log(path):
    rows = []
    if not os.path.exists(path):
        return rows
    with open(path) as f:
        for ln in f:
            if ln.startswith("#"):
                continue
            parts = ln.split()
            if len(parts) >= 4:
                try:
                    rows.append((float(parts[0]), float(parts[3])))
                except ValueError:
                    pass
    return rows


def median(xs):
    if not xs:
        return float("nan")
    s = sorted(xs)
    return s[len(s) // 2]


def main():
    tmp = tempfile.mkdtemp(prefix="rtvtracks_")
    mkv = make_media(tmp)

    # ---------- 1. Subtitle cycling with `j` ----------
    p, m = spawn([mkv, "--backend", "ascii"])
    buf = bytearray()
    read_pty(m, buf, 2.5)          # startup, subs off
    pre = strip_ansi(bytes(buf))
    os.write(m, b"j")              # -> embedded track 1 (eng)
    buf2 = bytearray()
    read_pty(m, buf2, 3.0)
    after_j1 = strip_ansi(bytes(buf2))
    os.write(m, b"j")              # -> embedded track 2 (spa)
    buf3 = bytearray()
    read_pty(m, buf3, 3.0)
    after_j2 = strip_ansi(bytes(buf3))
    rc = finish(p, m)
    check("subs: exit 0 after cycling with j", rc == 0, f"rc={rc}")
    check("subs: no text before enabling", "ENGLISH LINE" not in pre)
    check("subs: eng track visible after 1×j", "ENGLISH LINE" in after_j1,
          "ENGLISH never showed up")
    check("subs: spa track visible after 2×j", "SPANISH LINE" in after_j2,
          "SPANISH never showed up")
    check("subs: feedback OSD", "Subs [" in after_j1 or "Subs [" in after_j2)

    # ---------- 2. Audio cycling with `a` + sync ----------
    slog = os.path.join(tmp, "sync.log")
    p, m = spawn([mkv, "--backend", "ascii"], env_extra={"RTV_SYNC_LOG": slog})
    buf = bytearray()
    read_pty(m, buf, 3.0)
    os.write(m, b"a")              # -> track 2 (spa 880 Hz)
    buf2 = bytearray()
    read_pty(m, buf2, 4.0)
    osd_txt = strip_ansi(bytes(buf2))
    os.write(m, b"a")              # -> back to track 1
    buf3 = bytearray()
    read_pty(m, buf3, 4.0)
    rc = finish(p, m)
    check("audio: exit 0 after cycling with a", rc == 0, f"rc={rc}")
    check("audio: feedback OSD", "Audio [" in osd_txt,
          "'Audio [' never showed up in the HUD")
    rows = parse_sync_log(slog)
    check("audio: sync-log has frames", len(rows) > 50, f"{len(rows)} frames")
    if rows:
        # Frames from the last 3 s (after both switches): stable sync.
        t_end = rows[-1][0]
        tail = [d for (w, d) in rows if w >= t_end - 3.0]
        med = abs(median(tail))
        check("audio: median |avdiff| post-switch < 60 ms", med < 60.0,
              f"{med:.1f} ms over {len(tail)} frames")
        # The player did not freeze: there are frames AFTER the 2nd switch.
        n_after = sum(1 for (w, d) in rows if w >= t_end - 2.0)
        check("audio: still rendering frames after the switches", n_after >= 20,
              f"{n_after} frames in the last 2 s")

    # ---------- 3. Initial selection via CLI ----------
    for args, name in [
        (["--aid", "2"], "--aid 2"),
        (["--alang", "spa"], "--alang spa"),
        (["--sid", "2"], "--sid 2"),
        (["--slang", "eng"], "--slang eng"),
    ]:
        p, m = spawn([mkv, "--backend", "ascii"] + args)
        buf = bytearray()
        read_pty(m, buf, 2.5)
        txt = strip_ansi(bytes(buf))
        rc = finish(p, m)
        check(f"CLI {name}: plays and exit 0", rc == 0, f"rc={rc}")
        if name == "--sid 2":
            check("CLI --sid 2: shows the spa track", "SPANISH LINE" in txt)
        if name == "--slang eng":
            check("CLI --slang eng: shows the eng track", "ENGLISH LINE" in txt)

    # ---------- 4. `a` with a single track (mono-audio video) ----------
    mono = os.path.join(tmp, "mono.mp4")
    subprocess.run(
        ["ffmpeg", "-y", "-v", "error",
         "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=25",
         "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100",
         "-t", "8", "-c:v", "libx264", "-preset", "ultrafast",
         "-pix_fmt", "yuv420p", "-c:a", "aac", mono],
        check=True,
    )
    p, m = spawn([mono, "--backend", "ascii"])
    buf = bytearray()
    read_pty(m, buf, 2.0)
    os.write(m, b"a")
    buf2 = bytearray()
    read_pty(m, buf2, 2.0)
    txt = strip_ansi(bytes(buf2))
    rc = finish(p, m)
    check("mono: `a` breaks nothing (exit 0)", rc == 0, f"rc={rc}")
    check("mono: 'only track' OSD", "only track" in txt, "no informative OSD")

    print("\n" + ("ALL OK" if FAIL == 0 else "FAILURES"))
    sys.exit(FAIL)


if __name__ == "__main__":
    signal.alarm(300)
    main()
