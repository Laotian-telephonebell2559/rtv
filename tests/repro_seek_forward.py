#!/usr/bin/env python3
"""Bug repro: pressing → repeatedly should advance through the WHOLE
video. Runs rtv in a pty, sends → every INTERVAL seconds N times and
analyzes RTV_SYNC_LOG: prints the master position before each seek
and the landing PTS after it.
"""
import os, pty, sys, time, subprocess, select, fcntl, termios, struct

VIDEO = sys.argv[1]
N = int(sys.argv[2]) if len(sys.argv) > 2 else 30
INTERVAL = float(sys.argv[3]) if len(sys.argv) > 3 else 1.0
BIN = os.environ.get("RTV_BIN") or os.path.join(
    os.path.dirname(__file__), "..", "target", "release", "rtv")
LOG = "/tmp/rtv_seek_repro.log"

if os.path.exists(LOG):
    os.remove(LOG)

env = dict(os.environ)
env["RTV_SYNC_LOG"] = LOG
env["TERM"] = "xterm-256color"

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))

proc = subprocess.Popen(
    [BIN, VIDEO, "--backend", "ascii"],
    stdin=slave, stdout=slave, stderr=subprocess.DEVNULL, env=env,
)

RIGHT = b"\x1b[C"

def drain():
    while select.select([master], [], [], 0)[0]:
        try:
            os.read(master, 65536)
        except OSError:
            break

try:
    # warmup
    t0 = time.monotonic()
    while time.monotonic() - t0 < 3.0:
        drain(); time.sleep(0.05)
    for i in range(N):
        os.write(master, RIGHT)
        t0 = time.monotonic()
        while time.monotonic() - t0 < INTERVAL:
            drain(); time.sleep(0.05)
        if proc.poll() is not None:
            print(f"[i={i}] process ended (EOF reached?)")
            break
    os.write(master, b"q")
    time.sleep(1.0); drain()
finally:
    try:
        proc.terminate()
    except Exception:
        pass
    proc.wait()

# Analizar el log
seeks = []       # (wall, target=now+5, now, anchored)
frames = []      # (wall, master, pts)
for line in open(LOG):
    parts = line.split()
    if line.startswith("# SEEK"):
        d = dict(p.split("=") for p in parts[2:])
        seeks.append((float(d["wall"]), float(d["now"]), d["anchored"]))
    else:
        try:
            frames.append((float(parts[0]), float(parts[1]), float(parts[2])))
        except (ValueError, IndexError):
            pass

# Video duration (to tolerate the end clamp: seeks with
# now >= duration-6 can no longer advance a full +5s).
import json, subprocess as sp
dur = float(json.loads(sp.check_output([
    "ffprobe", "-v", "error", "-show_entries", "format=duration",
    "-of", "json", VIDEO]))["format"]["duration"])

print(f"Total seeks: {len(seeks)}, frames logged: {len(frames)}, dur={dur:.1f}")
ok = True
reached_end = False
prev_land = None
for i, (w, now, anch) in enumerate(seeks):
    land = next((f for f in frames if f[0] >= w), None)
    lp = land[2] if land else float("nan")
    at_end = now >= dur - 6.0
    reached_end = reached_end or at_end
    print(f"seek#{i:2d} wall={w:8.2f} now_before={now:7.2f} anchored={anch} landing_pts={lp:7.2f}{' [end]' if at_end else ''}")
    if prev_land is not None and land and lp < prev_land + 1.0 and not at_end:
        ok = False
        print(f"   ^^ DID NOT ADVANCE (prev landing {prev_land:.2f})")
    if land:
        prev_land = lp
if not reached_end and len(seeks) * 5.0 + 10.0 > dur:
    ok = False
    print("FAIL: never reached the end of the video")
print("RESULT:", "OK — seek always advances" if ok else "FAIL — seek gets stuck")
sys.exit(0 if ok else 1)
