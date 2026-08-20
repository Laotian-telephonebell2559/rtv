# Compilar rtv en Windows

> **¿Solo quieres usar rtv?** No hace falta compilar nada: el CI
> ([`.github/workflows/build.yml`](.github/workflows/build.yml), job
> `windows`) publica un `.zip` autocontenido (`rtv.exe` + las 5 DLLs de
> FFmpeg + yt-dlp) en cada [release](../../releases) y como artefacto de
> cada ejecución del workflow. Esta guía documenta **el mismo
> procedimiento que ejecuta ese job**, para reproducirlo en local.

## ⚠️ IMPORTANTE: versión de FFmpeg

Este proyecto usa `ffmpeg-the-third 5.0`, que **soporta FFmpeg 5.1–8.1** en
teoría, pero en la práctica **requiere FFmpeg 7.1.x en Windows** por dos
motivos:

1. Las variantes `V410` / `V308` / `V408` de `AVCodecID` (que la crate usa
   sin gate `#[cfg]`) sólo existen desde FFmpeg 7.1.
2. Los campos legacy del `AVCodec` (`supported_samplerates`, `sample_fmts`,
   `pix_fmts`, `ch_layouts`) que la crate lee directamente **fueron eliminados
   en FFmpeg 8.0** (sustituidos por `avcodec_get_supported_config`).

→ La única versión que satisface ambas condiciones es **FFmpeg 7.1.x**.

## Ruta rápida y probada (5 minutos, sin vcpkg) — la misma del CI

Todos los comandos de esta guía son de **PowerShell** (ejecútalos desde
Windows Terminal). El job `windows` del CI hace exactamente estos pasos
con `shell: pwsh` en un runner `windows-latest`.

### 1) Descargar FFmpeg **7.1 shared** (con .dll + .lib + include)

El CI descarga este build oficial de BtbN (release `latest`):

```powershell
$url = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n7.1-latest-win64-lgpl-shared-7.1.zip"
Invoke-WebRequest -Uri $url -OutFile ffmpeg.zip
```

(También puedes bajarlo a mano de
<https://github.com/BtbN/FFmpeg-Builds/releases> — cualquier
`ffmpeg-n7.1*-win64-lgpl-shared*.zip` vale.)

⚠️ **Tiene que llevar `shared` en el nombre y ser rama `n7.1`**. Los
`static` no traen los `.lib` que necesita el linker. `master` (que es 8.x)
NO compila con esta versión de la crate.

### 2) Descomprimir en una ruta sin espacios

Descomprime en algún sitio como:

```
C:\ffmpeg
```

Tras descomprimir tienes que ver:

```
C:\ffmpeg\
   ├── bin\      (ffmpeg.exe, avcodec-*.dll, avformat-*.dll, ...)
   ├── include\  (libavcodec\, libavformat\, ...)
   └── lib\      (avcodec.lib, avformat.lib, ...)
```

El zip deja una carpeta interna tipo
`ffmpeg-n7.1-latest-win64-lgpl-shared-7.1\`: hay que mover su **contenido**
directamente a `C:\ffmpeg\` (queremos `C:\ffmpeg\include`, no
`C:\ffmpeg\ffmpeg-n7.1-...\include`). Es lo que hace el CI:

```powershell
Expand-Archive ffmpeg.zip -DestinationPath C:\ffdl
$inner = Get-ChildItem C:\ffdl -Directory | Select-Object -First 1
Move-Item $inner.FullName C:\ffmpeg
```

Nota sobre https: los builds BtbN compilan con `--enable-schannel` (TLS
nativo de Windows), así que la reproducción por red (`rtv https://…`)
funciona sin instalar nada más.

### 3) Fijar las variables de entorno

En PowerShell **como el usuario que usas normalmente** (no admin):

```powershell
# Para la sesión actual:
$env:FFMPEG_DIR = "C:\ffmpeg"
$env:PATH = "C:\ffmpeg\bin;" + $env:PATH

# Para dejarlo permanente:
[System.Environment]::SetEnvironmentVariable("FFMPEG_DIR", "C:\ffmpeg", "User")
$oldPath = [System.Environment]::GetEnvironmentVariable("PATH", "User")
[System.Environment]::SetEnvironmentVariable("PATH", "C:\ffmpeg\bin;$oldPath", "User")
```

Cierra y abre PowerShell de nuevo.

Verifica:

```powershell
$env:FFMPEG_DIR    # → C:\ffmpeg
ffmpeg -version    # → debería imprimir "ffmpeg version n7.1..."
```

### 4) libclang: LLVM ≤ 18 (solo si bindgen crashea)

`ffmpeg-sys-the-third` genera bindings con bindgen, que necesita
`libclang.dll`. `clang-sys 1.8.1` (fijado por el `Cargo.lock`) soporta
libclang **hasta la versión 18**: con LLVM 19/20 el build script puede
crashear con `STATUS_ACCESS_VIOLATION` (le pasaba al runner
`windows-latest`, que trae LLVM 20 preinstalado). Si te ocurre, haz lo
mismo que el CI — instalar LLVM 18.1.8 y apuntar `LIBCLANG_PATH` a él:

```powershell
$url = "https://github.com/llvm/llvm-project/releases/download/llvmorg-18.1.8/LLVM-18.1.8-win64.exe"
curl.exe -fsSL --retry 5 -o llvm18.exe $url
7z x llvm18.exe -oC:\llvm18 "bin\libclang.dll" "lib\clang"   # solo lo necesario
$env:LIBCLANG_PATH = "C:\llvm18\bin"
```

(Si no tienes `7z`, instala LLVM 18 con su instalador normal y usa
`LIBCLANG_PATH = ruta\bin`. Con Visual Studio o un LLVM ≤ 18 ya en el
sistema normalmente no hace falta tocar nada.)

### 5) Compilar

```powershell
cd rtv
cargo clean            # importante — para que reintente la detección
cargo build --release --locked   # --locked: mismas versiones que el CI
```

La primera build tarda ~30-60 s (bindgen + LTO fat). Salidas:

```
target\release\rtv.exe
```

Sobre el audio: `cpal` en Windows enlaza con **WASAPI** que forma parte del
sistema — **no hay que instalar nada más** para el audio.

### 6) Ejecutar

**Importante**: el `.exe` es dinámicamente enlazado contra las DLL de FFmpeg,
así que necesita encontrarlas al arrancar. Como pusiste `C:\ffmpeg\bin` en el
PATH en el paso 3, ya funciona. Para un paquete **portable** haz lo que
hace el CI al empaquetar: copiar junto al `.exe` las DLL de las **5
familias que rtv enlaza** (sin avfilter/avdevice) — Windows resuelve las
DLL desde el directorio del propio exe:

```powershell
foreach ($fam in "avutil","avcodec","avformat","swscale","swresample") {
  Copy-Item (Get-ChildItem "C:\ffmpeg\bin\$fam-*.dll") .\target\release\
}
```

(Con FFmpeg 7.1 son `avutil-59`, `avcodec-61`, `avformat-61`, `swscale-8`
y `swresample-5`.) El CI verifica esto con un smoke test: copia el paquete
a otra ruta, quita `C:\ffmpeg\bin` del PATH y comprueba que
`rtv.exe --version` arranca.

Ejemplo de uso:

```powershell
.\target\release\rtv.exe "C:\videos\prueba.mp4"                # audio + vídeo
.\target\release\rtv.exe "C:\videos\prueba.mp4" --stats        # con FPS detallados
.\target\release\rtv.exe "C:\videos\prueba.mp4" --no-audio     # sólo vídeo
.\target\release\rtv.exe "C:\videos\prueba.mp4" --loop-video   # loop
```

## Alternativa: vcpkg

Si prefieres vcpkg:

```powershell
git clone https://github.com/microsoft/vcpkg C:\vcpkg
C:\vcpkg\bootstrap-vcpkg.bat
# Importante: la rama de vcpkg debe corresponder a FFmpeg 7.1
C:\vcpkg\vcpkg install ffmpeg[avcodec,avformat,swscale,swresample]:x64-windows
$env:VCPKG_ROOT = "C:\vcpkg"
cargo clean
cargo build --release
```

`vcpkg install` compila FFmpeg desde cero → tarda **20-40 minutos**. Por eso
recomiendo BtbN.

## Decode por hardware en Windows

`--hwdec auto` prueba **D3D11VA → DXVA2 → CUDA → QSV → Vulkan**. Buenas
noticias: **D3D11VA y DXVA2 no necesitan ninguna librería extra** — van
contra las API de Windows que ya están en el sistema, y los builds de BtbN
las traen habilitadas. Con cualquier GPU moderna (Intel/AMD/NVIDIA) el
decode de H.264/HEVC debería salir por GPU sin hacer nada; AV1 solo con
GPUs recientes (Intel Arc, AMD RDNA2+, NVIDIA RTX 30+).

En Linux, en cambio, VAAPI requiere `libva-dev` (y drivers Mesa/iHD) en
la máquina de build si compilas FFmpeg tú mismo; con el FFmpeg del sistema
(`libavcodec-dev` de la distro) ya viene incluido.

## Terminal recomendado

| Terminal | Compatible | Notas |
|---|---|---|
| **Windows Terminal** | ✅ Recomendado | Truecolor + half-blocks + Sixel (≥ 1.22) + audio WASAPI. |
| **Alacritty / WezTerm** | ✅✅ | WezTerm además responde a CSI 16t (mejor escalado adaptativo) e implementa kitty graphics. |
| **cmd.exe (consola clásica)** | ❌ Evítalo | El host de consola legacy no muestra salida no-ASCII correctamente: descarta todos los backends menos `ascii`. Usa Windows Terminal (PowerShell dentro de WT funciona perfecto). |

## Problemas comunes

- **`avcodec-61.dll` no se encuentra al ejecutar** → o falta `C:\ffmpeg\bin`
  en el PATH, o esas DLL no están junto al `.exe`.
- **`LINK : fatal error LNK1181: cannot open input file 'avcodec.lib'`** →
  descargaste el zip `static`. Usa el `shared`.
- **`error[E0599]: no associated function or constant named V410`** →
  Descargaste FFmpeg <7.1 (típicamente 7.0.x). Usa 7.1.x.
- **`error[E0609]: no field 'supported_samplerates'`** → Descargaste
  FFmpeg 8.x. Usa 7.1.x (`master`/`8.1` NO valen para esta crate).
- **`Package 'alsa' not found`** → sólo en Linux. Instala `libasound2-dev`.
- **Los colores se ven con líneas verticales gordas** → tu fuente no es
  cuadrada. Añade `--scale 0.5` o cambia la fuente a Cascadia Mono / Consolas.
- **No suena el audio pero aparece 🔊** → puede que el device por defecto no
  soporte la sample rate del vídeo. Prueba `--verbose` para ver el error.
  `--no-audio` deshabilita el audio si te da problemas.
