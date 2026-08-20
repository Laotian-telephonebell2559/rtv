#!/data/data/com.termux/files/usr/bin/bash
# build-termux.sh — compila rtv NATIVAMENTE dentro de Termux (Android).
#
# Uso (desde la raíz del repo, en una sesión de Termux):
#   bash scripts/build-termux.sh
#
# Qué hace:
#   1. Instala las dependencias de build con pkg (rust, clang, etc.).
#   2. Compila FFmpeg 7.1.5 desde fuente (config mínima LGPL, solo decode)
#      en $HOME/rtv-ffmpeg — el ffmpeg de los repos de Termux es 8.x y
#      ffmpeg-the-third 5.0 NO compila contra 8.x (mismos motivos que
#      BUILD-WINDOWS.md). El resultado se cachea: re-ejecutar el script
#      no lo recompila.
#   3. Compila rtv con `--no-default-features --features pulse`:
#      cpal/AAudio no funciona en un proceso de consola Termux; el audio
#      sale por PulseAudio (pkg install pulseaudio, ver README).
#
# Resultado: target/release/rtv (enlazado contra $HOME/rtv-ffmpeg/lib)
# y un wrapper `rtv` en $PREFIX/bin con el LD_LIBRARY_PATH ya puesto.
set -euo pipefail

FF_VERSION="${FF_VERSION:-7.1.5}"
FF_PREFIX="${FF_PREFIX:-$HOME/rtv-ffmpeg}"
NPROC="$(nproc 2>/dev/null || echo 2)"

say() { printf '\033[1;36m== %s\033[0m\n' "$*"; }

if [ -z "${TERMUX_VERSION:-}" ] && ! echo "${PREFIX:-}" | grep -q com.termux; then
    echo "AVISO: esto no parece Termux (PREFIX=${PREFIX:-?}). Continuando..." >&2
fi

# En Termux no existe /bin/sh: los shebangs #!/bin/sh se reescriben con el
# shim termux-exec, que el login shell carga vía LD_PRELOAD. Si entramos
# sin login (docker exec del CI, ssh directo...), lo cargamos a mano.
if [ -z "${LD_PRELOAD:-}" ] && [ -n "${PREFIX:-}" ]; then
    for _so in "$PREFIX/lib/libtermux-exec-ld-preload.so" \
               "$PREFIX/lib/libtermux-exec.so"; do
        [ -f "$_so" ] && { export LD_PRELOAD="$_so"; break; }
    done
fi

# ---------------------------------------------------------------- deps --
say "Instalando dependencias de build (pkg)"
# binutils: ar/ranlib para las crates *-sys. libdav1d = decode AV1 decente.
# libgnutls: TLS para el protocolo https de libavformat (reproducción
# por red / yt-dlp). LGPLv2.1+, misma licencia que el resto del build.
# termux-pkg.sh ROTA de mirror hasta dar con uno consistente: los
# mirrors de Termux se desincronizan a menudo (índices con "unexpected
# size" o dependencias imposibles tipo "ncurses (= X) but Y is to be
# installed") y la única cura fiable es probar otro mirror.
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
sh "$SCRIPT_DIR/termux-pkg.sh" rust clang make pkg-config binutils curl tar xz-utils libdav1d libgnutls

# nasm solo hace falta para el asm x86 (termux-docker x86_64 del CI; en un
# móvil aarch64 el asm NEON se ensambla con clang, sin nasm). Si no se
# puede instalar, se desactiva el asm x86 (build de test, no de release).
ARCH="$(uname -m)"
X86ASM_FLAG=""
if [ "$ARCH" = "x86_64" ] || [ "$ARCH" = "i686" ]; then
    if ! (pkg install -y nasm >/dev/null 2>&1 && command -v nasm >/dev/null); then
        X86ASM_FLAG="--disable-x86asm"
    fi
fi

# -------------------------------------------------------------- FFmpeg --
if [ -f "$FF_PREFIX/.rtv-ffmpeg-$FF_VERSION" ]; then
    say "FFmpeg $FF_VERSION ya compilado en $FF_PREFIX (cacheado)"
else
    say "Compilando FFmpeg $FF_VERSION desde fuente (tarda un rato)"
    SRC_DIR="$(mktemp -d)"
    # Espejo de GitHub primario (ffmpeg.org resetea conexiones a veces).
    curl -fsSL --retry 5 --retry-all-errors --retry-delay 5 -o "$SRC_DIR/ff.tar" \
        "https://github.com/FFmpeg/FFmpeg/archive/refs/tags/n${FF_VERSION}.tar.gz" ||
    curl -fsSL --retry 5 --retry-all-errors --retry-delay 5 -o "$SRC_DIR/ff.tar" \
        "https://ffmpeg.org/releases/ffmpeg-${FF_VERSION}.tar.xz"
    tar -xf "$SRC_DIR/ff.tar" -C "$SRC_DIR" --strip-components=1
    cd "$SRC_DIR"
    # Config mínima para rtv: solo DECODE (sin avfilter/avdevice ni
    # encoders/muxers). Igual que el build Linux del CI pero sin VAAPI
    # (no hay /dev/dri accesible en Android sin root).
    # `sh ./configure`: el shebang del tarball es #!/bin/sh, que no existe
    # en Termux (cinturón extra por si el LD_PRELOAD de arriba no aplica).
    sh ./configure --prefix="$FF_PREFIX" \
        --enable-shared --disable-static \
        --disable-programs --disable-doc \
        --disable-avdevice --disable-avfilter \
        --disable-encoders --disable-muxers \
        --disable-xlib --disable-libxcb \
        --disable-vulkan \
        --enable-libdav1d --enable-gnutls $X86ASM_FLAG
    # https compilado o nada: sin TLS no hay reproducción por red.
    grep -q "CONFIG_HTTPS_PROTOCOL 1" config_components.h
    make -j"$NPROC"
    make install
    cp COPYING.LGPLv2.1 "$FF_PREFIX/LICENSE.txt" 2>/dev/null || true
    touch "$FF_PREFIX/.rtv-ffmpeg-$FF_VERSION"
    cd - >/dev/null
    rm -rf "$SRC_DIR"
fi

# ------------------------------------------------------------------ rtv --
say "Compilando rtv (backend de audio: pulse)"
export FFMPEG_DIR="$FF_PREFIX"
# bindgen necesita libclang; en Termux vive en $PREFIX/lib.
export LIBCLANG_PATH="${LIBCLANG_PATH:-$PREFIX/lib}"
cargo build --release --locked --no-default-features --features pulse

# Wrapper de conveniencia: rtv en el PATH con LD_LIBRARY_PATH puesto.
say "Instalando wrapper en \$PREFIX/bin/rtv"
BIN="$(pwd)/target/release/rtv"
cat > "$PREFIX/bin/rtv" << WRAP
#!/data/data/com.termux/files/usr/bin/bash
export LD_LIBRARY_PATH="$FF_PREFIX/lib:\${LD_LIBRARY_PATH:-}"
exec "$BIN" "\$@"
WRAP
chmod +x "$PREFIX/bin/rtv"

say "Listo. Prueba:  rtv --version"
echo "Para AUDIO instala y arranca PulseAudio:"
echo "  pkg install pulseaudio"
echo "  pulseaudio --start --exit-idle-time=-1"
echo "y reproduce normalmente:  rtv video.mp4"
