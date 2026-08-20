#!/usr/bin/env python3
"""Smoke test de CI: reproduce un vídeo COMPLETO en un pty real y exige exit 0.

Uso: smoke_pty.py <ruta-a-rtv> <ruta-a-video> [args-extra-de-rtv...]

Los args extra se insertan tal cual (p.ej. --audio-backend none).

Sustituye a `script -qec` (el `script` de util-linux 2.37 de ubuntu-22.04
maneja mal el stdin no-TTY de los runners y el hijo moría con SIGABRT).
Aquí el pty nace con winsize definido (120x40), la salida se drena en
continuo (un pty con el buffer lleno bloquea al hijo — lección aprendida en
tests/integration_resize.py) y, si el proceso no sale limpio, se vuelca la
cola de la salida para poder diagnosticar el fallo desde el log de Actions.
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

TIMEOUT_SECS = 60
ROWS, COLS = 40, 120


def dump_tail(tail: collections.deque) -> None:
    data = b"".join(tail)[-8000:]
    sys.stderr.write("--- cola de salida de rtv (diagnóstico) ---\n")
    sys.stderr.flush()
    sys.stderr.buffer.write(data + b"\n")
    sys.stderr.flush()


def main() -> int:
    rtv, video = sys.argv[1], sys.argv[2]
    pid, fd = pty.fork()
    if pid == 0:  # hijo
        os.environ.setdefault("TERM", "xterm-256color")
        # --verbose: NO silenciar stderr -> un panic de Rust queda en el log.
        extra = sys.argv[3:]
        os.execvp(rtv, [rtv, "--verbose", "--no-audio", "--backend", "ascii", *extra, video])

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
        wpid, st = os.waitpid(pid, os.WNOHANG)
        if wpid:
            status = st
            break
        time.sleep(0.2)
    if status is None:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
        print(f"FALLO: rtv no terminó en {TIMEOUT_SECS}s", file=sys.stderr)
        dump_tail(tail)
        return 1

    code = os.waitstatus_to_exitcode(status)  # negativo = señal
    if code != 0:
        print(f"FALLO: rtv salió con {code} (negativo = señal)", file=sys.stderr)
        dump_tail(tail)
        return 1
    print("smoke OK: reproducción completa hasta EOF, exit 0")
    return 0


if __name__ == "__main__":
    sys.exit(main())
