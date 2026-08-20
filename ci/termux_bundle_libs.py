#!/usr/bin/env python3
"""Empaqueta el cierre TRANSITIVO de librerías de un binario en Termux.

Se ejecuta DENTRO del userland Termux (contenedor termux-docker o un
dispositivo real). Recorre las entradas NEEDED (readelf -d) del binario y,
recursivamente, de cada librería resuelta; copia a un directorio de salida
todo lo que viva en los dirs de búsqueda dados (p.ej. el FFmpeg propio y
$PREFIX/lib — cosas instaladas con pkg que en el móvil del usuario pueden
NO estar, como libdav1d). Lo que no aparece en ningún dir de búsqueda debe
ser una librería del sistema Android (bionic: libc/libm/libdl/liblog...),
presente en cualquier dispositivo; si no está en esa lista blanca, error.

Motivación: el paquete termux solo llevaba las libs de FFmpeg, pero ese
FFmpeg enlaza libdav1d del pkg de Termux -> en un móvil sin
`pkg install libdav1d` el linker fallaba con "library ... not found".

Uso:
    python3 termux_bundle_libs.py <binario> <outdir> <searchdir>...
"""
import os
import re
import shutil
import subprocess
import sys

# Librerías del SISTEMA Android (bionic), garantizadas en todo dispositivo.
# Cualquier NEEDED no resuelto que no case aquí es un bug de empaquetado.
BIONIC_OK = re.compile(
    r"^(libc|libm|libdl|liblog|libz|libstdc\+\+|ld-android|libdl_android)\.so$"
)


def needed(path: str) -> list[str]:
    out = subprocess.run(
        ["readelf", "-d", path], capture_output=True, text=True, check=True
    ).stdout
    return re.findall(r"\(NEEDED\)\s+Shared library: \[([^\]]+)\]", out)


def main() -> int:
    if len(sys.argv) < 4:
        print(__doc__)
        return 2
    binary, outdir, searchdirs = sys.argv[1], sys.argv[2], sys.argv[3:]
    os.makedirs(outdir, exist_ok=True)

    bundled: dict[str, str] = {}
    system: list[str] = []
    queue = [binary]
    seen_files = set()

    while queue:
        f = queue.pop()
        real = os.path.realpath(f)
        if real in seen_files:
            continue
        seen_files.add(real)
        for name in needed(f):
            if name in bundled or name in system:
                continue
            for d in searchdirs:
                cand = os.path.join(d, name)
                if os.path.exists(cand):
                    src = os.path.realpath(cand)
                    # Copiar el CONTENIDO real bajo el nombre del soname
                    # (los symlinks .so.61 -> .so.61.x.x no sobreviven al
                    # docker cp/tar de forma fiable).
                    shutil.copy2(src, os.path.join(outdir, name))
                    bundled[name] = src
                    queue.append(src)
                    break
            else:
                if BIONIC_OK.match(name):
                    system.append(name)
                else:
                    print(f"ERROR: '{name}' no está en los dirs de búsqueda "
                          f"ni es una lib bionic conocida", file=sys.stderr)
                    return 1

    print(f"empaquetadas ({len(bundled)}):")
    for n, src in sorted(bundled.items()):
        print(f"  {n:28} <- {src}")
    print(f"del sistema Android ({len(system)}): {', '.join(sorted(system))}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
