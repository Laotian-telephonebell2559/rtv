#!/usr/bin/env python3
"""Test de CI (Termux): reproduce un vídeo CON AUDIO por el backend pulse
y verifica que el reloj de audio ancla y avanza.

Uso: termux_audio_check.py <ruta-a-rtv> <video-con-audio> [args-extra-de-rtv...]

Cómo verifica el audio REAL (no solo "no crashea"):
  * RTV_AUDIO_DEBUG=/tmp/rtv_audio_dbg.log — lo escribe SinkFeeder.fill()
    (el corazón compartido de los backends) UNA línea por callback/write
    con el PTS efectivo que se está oyendo. Si el backend pulse no
    conectó, no arrancó el writer, o el ring no fluye, el fichero queda
    vacío o congelado.
  * Se exige: >= 20 escrituras, PTS máximo >= 1.0 s y monotonía razonable
    (>= 95 % de deltas no-negativos: el limitador de tasa puede capar,
    nunca retroceder).
  * Además se exige exit 0 y que el proceso termine solo (fin del vídeo).
"""
import collections
import fcntl
import os
import pty
import signal
import struct
import sys
import termios
import threading
import time

TIMEOUT_SECS = 90
ROWS, COLS = 40, 120
# En Termux no existe /tmp: usar $TMPDIR (prefix propio) si está.
DBG = os.path.join(os.environ.get("TMPDIR") or "/tmp", "rtv_audio_dbg.log")


def main() -> int:
    rtv, video = sys.argv[1], sys.argv[2]
    if os.path.exists(DBG):
        os.unlink(DBG)

    pid, fd = pty.fork()
    if pid == 0:  # hijo
        os.environ.setdefault("TERM", "xterm-256color")
        os.environ["RTV_AUDIO_DEBUG"] = DBG
        # SIN --no-audio: el objetivo es el pipeline de audio completo.
        extra = sys.argv[3:]
        os.execvp(rtv, [rtv, "--verbose", "--backend", "ascii", *extra, video])

    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    os.kill(pid, signal.SIGWINCH)

    tail: collections.deque = collections.deque(maxlen=200)

    def drain() -> None:
        try:
            while True:
                d = os.read(fd, 65536)
                if not d:
                    return
                tail.append(d)
        except OSError:
            pass

    threading.Thread(target=drain, daemon=True).start()

    deadline = time.time() + TIMEOUT_SECS
    status = None
    while time.time() < deadline:
        done, st = os.waitpid(pid, os.WNOHANG)
        if done == pid:
            status = st
            break
        time.sleep(0.25)
    else:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
        sys.stderr.write("FAIL: timeout — rtv no terminó solo\n")
        _dump(tail)
        return 1

    code = os.waitstatus_to_exitcode(status)
    if code != 0:
        sys.stderr.write(f"FAIL: exit code {code}\n")
        _dump(tail)
        return 1

    # ---- Análisis del log del feeder ----
    if not os.path.exists(DBG):
        sys.stderr.write("FAIL: RTV_AUDIO_DEBUG no se creó — el sink de audio no arrancó\n")
        _dump(tail)
        return 1
    pts = []
    with open(DBG) as f:
        for line in f:
            # "12.3456 cb#7 buf=1920 pts_first=0.1234 rep_delay=0.0500 set=0.0734"
            for tok in line.split():
                if tok.startswith("set="):
                    try:
                        pts.append(float(tok[4:]))
                    except ValueError:
                        pass
    n = len(pts)
    if n < 20:
        sys.stderr.write(f"FAIL: solo {n} callbacks de audio (>=20 requeridos)\n")
        _dump(tail)
        return 1
    peak = max(pts)
    if peak < 1.0:
        sys.stderr.write(f"FAIL: el reloj de audio solo llegó a {peak:.3f}s (>=1.0 requerido)\n")
        return 1
    nonneg = sum(1 for a, b in zip(pts, pts[1:]) if b >= a - 1e-9)
    ratio = nonneg / max(1, n - 1)
    if ratio < 0.95:
        sys.stderr.write(f"FAIL: monotonía del reloj {ratio:.2%} (>=95% requerido)\n")
        return 1

    print(f"OK: {n} callbacks, PTS max {peak:.3f}s, monotonía {ratio:.2%}")
    return 0


def _dump(tail: collections.deque) -> None:
    data = b"".join(tail)[-6000:]
    sys.stderr.write("--- cola de salida de rtv ---\n")
    sys.stderr.flush()
    sys.stderr.buffer.write(data + b"\n")
    sys.stderr.flush()


if __name__ == "__main__":
    sys.exit(main())
