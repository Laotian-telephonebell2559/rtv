# rtv

*Read this in [English](README.md).*

Reproductor de vídeo para terminal, escrito en Rust. Rápido de verdad:
arranca en decenas de milisegundos, sincroniza audio y vídeo como un
reproductor serio (reloj de audio maestro, estilo ffplay) y aprovecha el
protocolo gráfico de tu terminal para sacar la máxima resolución posible.

```
rtv pelicula.mkv
```

Eso es todo. Detecta el terminal, elige el mejor backend de render, saca el
audio por el dispositivo por defecto y reproduce.

## Demos

Un clip por backend de render, capturado de la salida real de rtv al
terminal (scripts/capture_demo_gifs.py decodifica el flujo de escapes
exacto que emite el reproductor — protocolo gráfico de kitty, imágenes
inline de iTerm2, sixel, y los backends de texto rasterizados con un
emulador de terminal).

| Backend | Demo |
|---|---|
| `kitty` (píxel perfecto) | ![demo kitty](assets/demo-kitty.gif) |
| `iterm2` (imágenes inline) | ![demo iterm2](assets/demo-iterm2.gif) |
| `sixel` (gráficos DEC) | ![demo sixel](assets/demo-sixel.gif) |
| `blocks` (medias celdas) | ![demo blocks](assets/demo-blocks.gif) |
| `ascii` (texto plano) | ![demo ascii](assets/demo-ascii.gif) |
| Ventana GUI (`--gui`, egui/wgpu) — tooltip de tiempo al hover, pausa con espacio | ![demo gui](assets/demo-gui.gif) |

## Por qué existe

`mpv --vo=tct` funciona, pero arrastra el peso de mpv entero para pintar
celdas de colores, y `--vo=kitty` deja la sincronización fina en manos de un
camino de render que no fue pensado para terminales. rtv ataca el problema
desde el otro lado: un binario de ~1 MB que solo sabe hacer una cosa —
decodificar con FFmpeg y pintar en un terminal — y la hace con la menor
latencia y los menos bytes por frame que hemos conseguido.

La comparativa de abajo está verificada contra el código fuente real de mpv
(`vo_tct.c`, `vo_kitty.c`, `vo_sixel.c`, `terminal-unix.c`), no de oídas:

| | `mpv --vo=tct` / `--vo=kitty` | rtv |
|---|---|---|
| Arranque | ~150–300 ms | ~20–40 ms |
| Celdas half-block | SGR fg+bg emitido para cada celda, cada frame | delta-encoded (~30–50 % menos bytes) |
| Transporte kitty graphics | shm en local; base64 crudo por ssh (sin compresión) | shm en local; **zlib `o=z`** por ssh (~10× menos tráfico que el base64 crudo) |
| Overhead del protocolo kitty | `q=2`, chunking `m=1` (igual que rtv) | `q=2`, chunking `m=1` |
| Detección de tamaño de celda | solo `ioctl(TIOCGWINSZ)` — sin info de píxeles por ssh | `ioctl` + sondeo CSI `16t`/`14t` — resolución completa también por ssh |
| Dithering sixel | libsixel, paleta dinámica por histograma por frame | paleta fija 6×7×6 + Bayer ordenado (más barato por frame) |
| Binario (stripped) | ~40 MB | ~1.1 MB (FFmpeg dinámico) |
| Reloj maestro | audio (libmpv) | audio (cpal), o monotónico sin audio |
| Resolución de render | fija por vo | adaptativa al tamaño real de celda |

Medido con los binarios de este repo; los números exactos dependen de la
máquina y el terminal, pero los órdenes de magnitud se mantienen. Donde mpv
es mejor lo decimos: su camino sixel adapta la paleta por frame, lo que
puede verse mejor en algunos contenidos a cambio de más CPU.

## Características

- **Cualquier formato que trague FFmpeg** (vía `ffmpeg-the-third`, probado
  contra FFmpeg 7.1): H.264, HEVC, AV1, VP9… El decode de vídeo usa frame
  threading con todos los cores.
- **Audio real** con `cpal`: WASAPI en Windows, ALSA/PulseAudio en Linux,
  CoreAudio en macOS. Cualquier layout/formato de origen se convierte a f32
  estéreo al sample rate nativo del dispositivo con `libswresample`.
- **Sincronización A/V estilo ffplay**: el reloj de audio (posición real del
  sink, con la latencia de salida compensada y suavizada) pilota el vídeo.
  `compute_target_delay` con los mismos umbrales que ffplay. En la práctica:
  avdiff mediano de 0–2 ms en régimen estable, incluso con AV1 4K por
  software en 2 cores.
- **Seeks instantáneos estilo mpv**: `←`/`→` aterrizan en el keyframe ≤
  target y el audio salta exactamente al PTS real de aterrizaje del vídeo.
  Sin decodificar GOPs enteros en silencio, sin desincronización tras
  ráfagas de seeks.
- **Escalado adaptativo por celda real**: al arrancar se sondea el terminal
  (CSI `16t`/`14t` en kitty, WezTerm, Ghostty, iTerm2, foot, Konsole, xterm)
  para conocer el tamaño en píxeles de cada celda y escalar el vídeo a la
  resolución máxima que la ventana puede mostrar. Más ventana, más nitidez —
  también por ssh, donde la detección solo-ioctl de mpv no obtiene ninguna
  información de píxeles.
- **Resize en caliente, instantáneo**: el redibujo tras redimensionar tarda
  ~1 ms (espera inter-frame interrumpible por eventos + reescalado inmediato
  del frame en pantalla), y el decoder reajusta `sws_scale` sin drenar su
  colchón de pre-decode ni perder el sync.
- **Cinco backends de render** con auto-detección: Kitty graphics protocol
  (píxeles reales comprimidos con zlib; Kitty/Ghostty/WezTerm), **iTerm2
  inline images** (OSC 1337, BMP en memoria — también vía ssh con
  `LC_TERMINAL`), **Sixel** real (paleta fija 6×7×6 + dithering Bayer
  ordenado + RLE; mlterm/foot/contour/xterm `-ti vt340`), half-blocks
  truecolor (`▀`, 2 px por celda) y ASCII.
- **También una GUI de ventana**: `rtv --gui vídeo.mp4` abre una ventana
  nativa (winit + wgpu + egui) con el mismo núcleo de reproducción que el
  terminal — mismos relojes, mismo motor de sync, mismo protocolo de
  seek — más un HUD en pantalla con barra de progreso clicable y con
  scrubbing, timestamps al pasar el ratón, pantalla completa y controles
  que se ocultan solos.
- **Subtítulos softsub (opt-in)**: por defecto no se muestra ningún
  subtítulo. Con `--sub` (sin valor) se usa la pista de texto embebida del
  contenedor (MKV/MP4), y con `--sub fichero.srt` (o `.ass`) se carga un
  fichero externo. El texto se pinta centrado, en negrita y blanco
  brillante, pegado justo debajo de la imagen (si hay letterbox) o en las
  2 filas reservadas encima del HUD, sin tocar el pipeline de vídeo: la
  pista embebida se carga en un hilo aparte con demux solo-subtítulos
  (`AVDISCARD_ALL` en el resto de streams) y el lookup por tiempo es una
  búsqueda binaria por frame. Tags ASS `{\...}` y HTML de SRT fuera.
- **HUD discreto**: barra de progreso, tiempo y volumen en 1–2 líneas
  (con `--stats` añade backend, resolución, celda, fps y drops)
  que se adaptan al ancho. Solo se repinta cuando cambia (nada de parpadeo)
  y desaparece si la ventana es demasiado pequeña para ser legible.
- **Terminal siempre limpio**: alt-screen, autowrap desactivado durante la
  reproducción, y restauración completa al salir — también con `Ctrl+C` o
  con el audio sonando. Los logs de libav (`libdav1d`, `libaom`…) van
  silenciados para que ningún `error parsing obu data` ensucie el TUI.

## Instalación

### Binarios precompilados (recomendado)

Cada [release](../../releases) incluye paquetes **autocontenidos** para
Windows (x86_64), Linux (x86_64/arm64), macOS (Intel/Apple Silicon) y
**Termux/Android (aarch64/x86_64)** con **FFmpeg 7.1 empaquetado dentro**
— no hace falta instalar FFmpeg ni nada más:

```bash
tar -xzf rtv-*-linux-x86_64.tar.gz && cd rtv-*/ && ./rtv vídeo.mp4
```

En Windows: descomprime el `.zip` y ejecuta `rtv.exe vídeo.mp4` (las DLLs de
FFmpeg van junto al exe). En Termux: descomprime
`rtv-*-termux-aarch64.tar.gz` en tu `$HOME` de Termux y ejecuta el
lanzador `rtv` incluido (trae el cierre completo de librerías — FFmpeg,
dav1d, gnutls — validado en CI contra un contenedor Termux limpio; para
audio: `pkg install pulseaudio` + `pulseaudio --start`). El binario debe
quedarse junto a su carpeta `lib/`/`libs/` (Linux/macOS/Termux) o junto a
las DLLs (Windows). Los paquetes se generan en CI
([`.github/workflows/build.yml`](.github/workflows/build.yml));
el workflow se lanza manualmente (Actions → build → *Run workflow*) o al
crear un tag `v*`. También puedes bajar los artefactos de cualquier
ejecución del workflow.

### Compilar desde fuente

Necesitas Rust (edition 2021) y las librerías de desarrollo de FFmpeg.

#### Linux (Debian/Ubuntu)

```bash
sudo apt install libavformat-dev libavcodec-dev libavutil-dev \
                 libswscale-dev libswresample-dev libclang-dev \
                 libasound2-dev pkg-config
cargo build --release
```

#### macOS

```bash
# OJO: ffmpeg@7, no ffmpeg — brew ya sirve FFmpeg 8.x, que NO compila con
# ffmpeg-the-third 5.0 (misma razón que en BUILD-WINDOWS.md).
brew install ffmpeg@7 pkg-config
export FFMPEG_DIR="$(brew --prefix ffmpeg@7)"   # ffmpeg@7 es keg-only
cargo build --release
```

#### Windows

Guía completa en [`BUILD-WINDOWS.md`](BUILD-WINDOWS.md). En corto:

1. Descarga `ffmpeg-n7.1-latest-win64-lgpl-shared.zip` de
   [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds/releases).
2. Descomprime en `C:\ffmpeg\` (deben quedar `include`, `lib` y `bin`).
3. `$env:FFMPEG_DIR = "C:\ffmpeg"` y añade `C:\ffmpeg\bin` al `PATH`.
4. `cargo clean && cargo build --release`.

Ejecuta rtv desde **Windows Terminal** (u otro terminal moderno): soporta
truecolor, half-blocks, Sixel (≥ 1.22) y WASAPI sin problemas. Evita la
consola clásica `cmd.exe` — no puede mostrar salida no-ASCII
correctamente, lo que descarta todos los backends menos `ascii`.

#### Termux (Android)

rtv funciona de forma nativa en Termux (sin proot ni contenedores). El
script [`scripts/build-termux.sh`](scripts/build-termux.sh) lo hace todo:

```bash
pkg install -y git
git clone https://github.com/Santitub/rtv.git && cd rtv
bash scripts/build-termux.sh
rtv vídeo.mp4
```

Detalles que resuelve el script:

- **FFmpeg**: los repos de Termux ya sirven FFmpeg 8.x, que **no compila**
  con `ffmpeg-the-third 5.0`. El script compila FFmpeg **7.1.5** desde
  fuente (solo decode, con dav1d) en `~/rtv-ffmpeg` — tarda un rato la
  primera vez, pero queda cacheado para builds siguientes.
- **Audio**: cpal (AAudio/oboe) no funciona en procesos de consola de
  Termux, así que en Termux rtv usa un backend propio de **PulseAudio**
  (carga `libpulse-simple` en runtime, sin dependencia de build). Para
  tener sonido:

  ```bash
  pkg install -y pulseaudio
  pulseaudio --start
  ```

  Sin servidor PulseAudio el vídeo se reproduce igualmente sin sonido
  (mismo comportamiento que `--no-audio`).
- **Instalación**: deja un wrapper `rtv` en `$PREFIX/bin` que exporta el
  `LD_LIBRARY_PATH` del FFmpeg compilado.

El backend de terminal recomendado en Termux es el que auto-detecta rtv
(`blocks`/truecolor); con teclado táctil, los controles de ratón del HUD
(tap en la barra de progreso) funcionan si el terminal reporta eventos
de ratón.

Todo esto se valida en CI sin dispositivo físico dentro del workflow
[`build.yml`](.github/workflows/build.yml) (jobs `termux-*`), que compila
y testea rtv (incluido el camino real de audio por PulseAudio) dentro de
imágenes [`termux/termux-docker`](https://github.com/termux/termux-docker)
para x86_64 y aarch64, y publica paquetes `rtv-*-termux-*` como artefactos
(y en las releases, junto al resto de plataformas).

## Uso

```
rtv <fichero|URL> [opciones]
```

La entrada puede ser un fichero local, una **URL http/https directa**
(mp4, mkv, HLS `.m3u8`… — los protocolos de red van integrados en el
FFmpeg empaquetado, TLS incluido) o la **página de un sitio de vídeo**
(YouTube, Twitch, Vimeo, Dailymotion) si tienes
[yt-dlp](https://github.com/yt-dlp/yt-dlp) instalado:

```bash
rtv https://ejemplo.com/video.mp4                # URL directa
rtv https://www.youtube.com/watch?v=aqz-KE-bpKQ  # vía yt-dlp
rtv --ytdl https://cualquier-sitio-que-yt-dlp-soporte/…
```

**Los paquetes de release de Linux/Windows/macOS ya incluyen yt-dlp**
(el standalone oficial, junto al binario de rtv): funciona nada más
descomprimir. rtv busca por este orden: `$RTV_YTDLP` → `yt-dlp` del
`PATH` (si instalaste uno con pip/winget/brew, se prefiere por ser más
actualizable) → el incluido en el paquete. El incluido se actualiza
solo con `yt-dlp -U`. En Termux no existe build de yt-dlp para Android:
`pkg install python-pip && pip install yt-dlp`.

Con YouTube, el default pide **vídeo hasta 1080p + audio en streams
DASH separados** (doble input: dos conexiones, una por demuxer, audio
de reloj maestro) con fallback a muxed. `--ytdl-format b` fuerza una
sola conexión. También puedes montar tú el doble input a mano:

```bash
rtv película.mkv --audio-file comentario_del_director.m4a
```

| Opción | Efecto |
|---|---|
| `--gui` | Abre en ventana nativa (winit + wgpu + egui) en vez del terminal — mismo motor de reproducción, HUD en pantalla con barra de progreso clicable y pantalla completa |
| `--info` | NO reproduce: imprime la información del fichero — nombre, tamaño, fecha, formato, duración, bitrate, metadatos, pista de vídeo (codec, resolución + calidad `1080p/4K…`, fps), todas las pistas de audio (codec, canales, Hz, idioma) y de subtítulos (idioma, texto/bitmap), y capítulos. Salida pipeable (sin ANSI si stdout no es un terminal) |
| `--backend <kitty\|iterm2\|sixel\|blocks\|ascii>` | Fuerza un backend (por defecto se auto-detecta) |
| `--scale <0.1..1.0>` | Limita la resolución de render. Útil en terminales 4K donde el decode software no da abasto |
| `--loop-video` | Reinicia al llegar al final |
| `--stats` | Telemetría en el HUD: backend, resolución, tamaño de celda, FPS mostrados/decodificados y drops (sin el flag el HUD es limpio: transporte + volumen) |
| `--no-audio` | Sin audio; el vídeo usa reloj monotónico |
| `--audio-backend <auto\|cpal\|pulse\|none>` | Backend de salida de audio. `auto` (default) usa cpal (ALSA/WASAPI/CoreAudio) y en Termux prueba primero PulseAudio; `pulse` fuerza PulseAudio (vía `libpulse-simple`, cargada en runtime); `none` equivale a `--no-audio`. Un backend forzado que no arranca ⇒ vídeo sin audio (no hay fallback silencioso) |
| `--sub [fichero.srt\|.ass]` | Activa subtítulos: sin valor usa la pista de texto embebida del contenedor; con fichero carga subtítulos externos. Sin `--sub` no se muestran subtítulos |
| `--no-subs` | Desactiva subtítulos aunque se pase `--sub` (compatibilidad) |
| `--aid <N>` / `--alang <idioma>` | Pista de audio inicial: por índice 1-based dentro de las pistas de audio (`--aid 2` = segunda) o por idioma (`--alang spa`), como mpv. Sin match → pista "best" de FFmpeg |
| `--sid <N>` / `--slang <idioma>` | Pista de subtítulos embebida inicial (por índice de pista de texto / por idioma). Implican subtítulos ON aunque no se pase `--sub` |
| `--hwdec <auto\|none\|vaapi\|cuda\|qsv\|d3d11va\|dxva2\|videotoolbox\|vulkan\|drm\|vdpau>` | Decode por hardware. `auto` (default) prueba los hwaccels de la plataforma y cae a software si ninguno funciona; `none` fuerza software |
| `--ytdl` | Fuerza la resolución con yt-dlp para CUALQUIER URL (los sitios grandes —YouTube, Twitch, Vimeo, Dailymotion— se detectan solos, sin flag) |
| `--ytdl-format <FMT>` | Formato que se pide a yt-dlp (su sintaxis `-f`). Default `bv*[height<=?1080]+ba/b`: mejor vídeo ≤1080p + mejor audio en streams separados (doble input), con fallback a muxed. `b` fuerza muxed (una sola conexión) |
| `--audio-file <fichero\|URL>` | Reproduce el AUDIO desde otro fichero/URL (doble input), como el `--audio-file` de mpv. Prioridad sobre el audio separado de yt-dlp |
| `--verbose` | Deja los logs de FFmpeg en stderr (debugging) y lista los hwaccels compilados |

### Comparativa de backends (calidad / velocidad)

Medido con `tests/bench_backends.py` (testsrc2 1280×720@30, pty 100×30,
5 s por backend, binario release):

| Backend | Resolución efectiva | KB emitidos/frame | CPU rtv | Dónde funciona |
|---|---|---:|---:|---|
| `kitty` local | píxeles reales (la mejor) | **0,06** (shm `t=s`) | ~40 % | kitty en Linux o macOS local |
| `kitty`  | píxeles reales (la mejor) | 129 (zlib `o=z`) | 69 % | Kitty, Ghostty, WezTerm (y vía ssh) |
| `iterm2` | píxeles reales | 1331 | 40 % | iTerm2 (y vía ssh con `LC_TERMINAL`) |
| `sixel`  | píxeles, paleta 252 colores + dithering | 178 | 91 % | mlterm, foot, contour, xterm `-ti vt340` |
| `blocks` | 1×2 px/celda (`▀` truecolor) | 32 | 20 % | cualquier terminal truecolor |
| `ascii`  | 1×2 px/celda, sin color fino | 2 | 18 % | cualquier cosa |

Lecturas rápidas:

- **kitty/iterm2** dan la máxima calidad. En **kitty local (Linux y
  macOS)** rtv usa el transporte por **memoria compartida** del
  protocolo graphics (`t=s`): el frame se escribe a un objeto POSIX
  shm (Linux: `/dev/shm`; macOS: `shm_open`+`mmap`) y el escape solo
  lleva el nombre del objeto (~60 bytes/frame) — ni zlib ni base64,
  el mínimo consumo posible con calidad píxel exacta. Por ssh (o en
  Ghostty/WezTerm) cae automáticamente a zlib (`o=z`), que ya baja el
  tráfico 10× frente al base64 crudo (129 vs 1324 KB/frame) — la
  salida kitty de mpv también soporta shm en local, pero por ssh envía
  base64 **sin comprimir**, y ese chorro es el que limitaba los fps
  mostrados por debajo de los del vídeo aunque el decode fuese sobrado.
  Opt-out del shm con `RTV_KITTY_NO_SHM=1`. iTerm2 no tiene compresión
  equivalente en su protocolo.
- **Windows Terminal** no implementa el protocolo kitty graphics (ni
  compresión ni shm): allí rtv usa `sixel` (WT ≥ 1.22 lo soporta) o
  `blocks`. No hay transporte equivalente que acelerar — es un límite
  del terminal, no de rtv.
- **blocks** es el fallback universal: funciona en cualquier terminal
  truecolor (Windows Terminal, gnome-terminal, alacritty, konsole…)
  con un coste mínimo.
- **sixel** re-codifica la imagen entera cada frame y el dithering es
  caro: es el backend más intensivo en CPU. La paleta fija + Bayer de
  rtv es más barata por frame que la paleta dinámica de libsixel que
  usa mpv, a cambio de colores algo menos fieles en ciertos contenidos.

### Decode por hardware (`--hwdec`)

Con NVIDIA en Linux, `auto` prioriza **CUDA/NVDEC nativo**: la VAAPI
que exista ahí es la capa de traducción `nvidia-vaapi-driver` (más
lenta y frágil), pero abre sin error, así que sin esta prioridad el
hwdec "funcionaba" sin aportar nada.

Con `--hwdec auto` (el default) rtv intenta descargar el decode a la GPU
y cae a software de forma transparente si no hay hwaccel utilizable
(sin GPU, headless, codec no soportado por el driver…). El HUD muestra
el hwaccel activo junto al backend (p.ej. `kitty+vaapi`); si el hwaccel
muere a mitad de reproducción (reset de driver), rtv reabre el decoder
en software desde el punto exacto de reproducción sin cortar audio ni
sync, y la etiqueta del HUD vuelve a mostrar solo el backend.

Los frames decodificados en GPU se copian a RAM (`av_hwframe_transfer_data`
→ NV12) porque el sink es un terminal: las celdas se generan en CPU sí o
sí. El ahorro está en el decode (la parte cara de AV1/HEVC 4K), no en el
escalado.

Matriz de soporte orientativa (depende del FFmpeg enlazado y del driver):

| SO | Orden de prueba en `auto` | Notas |
|---|---|---|
| Linux | VAAPI → CUDA/NVDEC → QSV → VDPAU → Vulkan → DRM | VAAPI cubre Intel y AMD (Mesa); necesita acceso a `/dev/dri` |
| Windows | D3D11VA → DXVA2 → CUDA → QSV → Vulkan | D3D11VA funciona sin libs extra en cualquier GPU moderna |
| macOS | VideoToolbox | Apple Silicon e Intel |

Por codec: H.264/HEVC están soportados por prácticamente cualquier GPU de
la última década; **AV1** solo por GPUs recientes (Intel Xe/Arc, AMD
RDNA2+, NVIDIA RTX 30+). El fallback es por negociación, no global: si el
decoder AV1 no anuncia el hwaccel, ese vídeo va por software aunque otro
H.264 en la misma máquina vaya por GPU.

> Nota: la ganancia con GPU real (CPU%/fps con y sin `--hwdec`) está
> pendiente de medir fuera del sandbox de CI (que no tiene `/dev/dri`;
> ahí solo se valida el camino de fallback).

### Controles

| Tecla | Acción |
|---|---|
| `Espacio` | Pausa / reanudar (también el audio) |
| `←` / `→` | Seek ±5 s |
| `↑` / `↓` | Volumen ±5 (0–200 %) |
| `a` / `#` (`A` = atrás) | Cicla la pista de AUDIO en caliente, sin cortar el playback (el HUD muestra la pista con un OSD de ~2.5 s) |
| `j` (`J` = atrás) | Cicla subtítulos: off → [externa `--sub`] → pistas embebidas → off |
| `q` / `Esc` / `Ctrl+C` | Salir |
| 🖱️ Click / arrastre en la barra del HUD | Salta a esa posición del vídeo (seek proporcional; arrastrar hace scrubbing) |

#### Cambio de pista en runtime

El cambio de audio reutiliza el protocolo de seek: se bumpean los
seriales de los relojes (los chunks de la pista vieja que queden en el
ring se silencian sin tocar el reloj), el hilo de audio reabre el
decoder sobre el stream nuevo — cada pista puede tener codec,
sample-rate y layout distintos; el resampler normaliza siempre al
formato fijo del sink — y aterriza en el instante actual con recorte
sample-accurate. El vídeo ni se entera: entra en el hold estándar de
master desanclado y continúa en sync al primer chunk de la pista nueva
(|avdiff| mediano medido tras el cambio: <1 ms).

Los subtítulos son más simples: cada pista embebida se decodifica en un
hilo propio de demux-solo-subs al seleccionarla, y `off` simplemente
suelta la pista (las 2 filas reservadas se devuelven al vídeo).

## Arquitectura

Pipeline productor–consumidor con un hilo por etapa, comunicados por canales
`crossbeam` acotados:

```
                 ┌──────────────┐
                 │  fichero.mp4 │
                 └──────┬───────┘
                        │
           ┌────────────┴────────────┐
           ▼                         ▼
   ┌───────────────┐         ┌──────────────┐
   │ demux vídeo   │         │ demux audio  │
   │ decode + sws  │         │ decode + swr │
   │ (hilo 1)      │         │ (hilo 2)     │
   └──────┬────────┘         └──────┬───────┘
          │ RGB24                   │ f32 estéreo
          │ cola por presupuesto    │ ring buffer
          │ de memoria (~48 MB)     │
          ▼                         ▼
   ┌───────────────┐         ┌──────────────┐
   │ loop principal│         │ callback cpal│
   │ · sync ffplay │◄────────│ · alimenta el│
   │ · render      │ AudioClk│   reloj audio│
   │ · HUD e input │ (master)└──────────────┘
   └───────────────┘
```

Piezas clave:

- **`playback.rs`** — el núcleo de reproducción compartido por los dos
  frontends: sondeo de pistas, construcción del pipeline (decoder +
  audio + relojes), la compuerta de arranque de audio, la ventana de
  seek y el planificador de frames (`plan_frame`, la decisión
  drop/espera de ffplay). El reproductor de terminal y la GUI consumen
  exactamente el mismo motor.
- **`decoder.rs`** — demux + decode + `sws_scale` de vídeo. Los seeks van
  con serial: cada seek incrementa un contador y los frames con serial viejo
  se descartan aguas abajo. El resize es un store atómico de las dims
  destino que el escalador lee antes de cada frame.
- **`audio.rs`** — demux + decode + `swr_convert` de audio, ring buffer
  lock-free hacia el callback de cpal. El callback alimenta el reloj de
  audio con el PTS de la muestra que se está oyendo (latencia de salida
  descontada, suavizada con EMA, con limitador de tasa contra los bursts de
  prebuffer de PulseAudio).
- **`clock.rs`** — relojes estilo ffplay (`FfClock`) con seriales, anclaje,
  staleness y `compute_target_delay`.
- **`player.rs`** — el frontend de terminal: input, sync, decisión de
  drop/espera, render y HUD. Las esperas se hacen con `event::poll`, así
  que cualquier tecla o resize interrumpe la espera y se atiende al
  instante.
- **`gui.rs`** — el frontend de ventana (winit + wgpu + egui) sobre el
  mismo núcleo `playback`: subida de texturas, HUD en pantalla con barra
  de progreso clicable y con scrubbing, timestamps al pasar el ratón,
  pantalla completa y controles con auto-ocultado.
- **`renderer.rs`** — los cinco backends de terminal (kitty, iTerm2,
  Sixel, halfblocks, ascii). Todos recortan a los límites reales del
  área de vídeo, de modo que un frame con dimensiones desfasadas (resize
  en vuelo) nunca desborda la pantalla.
- **`subs.rs`** — subtítulos softsub: parsers SRT/ASS puros en Rust para
  archivos externos (`--sub`) y un hilo demuxer/decoder propio para la
  pista embebida del contenedor (con `AVDISCARD_ALL` en el resto de
  streams para que el demux de subs sea casi gratis). El player consulta
  los eventos activos por PTS con búsqueda binaria en cada refresco.
- **`terminfo.rs`** — sondeo del tamaño de celda (CSI `16t`/`14t`) con
  timeout de 20 ms y lista blanca de terminales que responden; heurística
  8×16 para el resto. En Windows nunca se sondea.

## Estructura del repo

```
rtv/
├── Cargo.toml
├── README.md                # este README en inglés
├── README_ES.md
├── BUILD-WINDOWS.md         # guía de build para Windows
├── todo.md                  # notas de trabajo y plan de las tareas
├── src/
│   ├── main.rs              # CLI, init de FFmpeg, silenciado de logs
│   ├── playback.rs          # núcleo compartido (pipeline+relojes+planificador)
│   ├── player.rs            # frontend de terminal: loop principal y sync
│   ├── gui.rs               # frontend de ventana (winit+wgpu+egui)
│   ├── decoder.rs           # hilo de vídeo
│   ├── hwdec.rs             # decode por hardware (unsafe FFmpeg aislado)
│   ├── audio.rs             # hilo de audio + sink cpal
│   ├── clock.rs             # relojes ffplay-style
│   ├── renderer.rs          # backends de render + HUD
│   ├── subs.rs              # subtítulos softsub SRT/ASS (externos y embebidos)
│   ├── tracks.rs            # inventario de pistas + selección --aid/--alang/--sid/--slang
│   ├── terminfo.rs          # detección del tamaño de celda
│   └── input.rs             # eventos de teclado/resize
└── tests/
    ├── integration_sync.py       # sync A/V + seeks, en pty real
    ├── integration_resize.py     # tormenta de resizes + seeks + pausa
    ├── integration_resize_ux.py  # latencia de resize, parpadeo, límites
    ├── integration_grow_quality.py # recuperación de calidad al agrandar
    ├── integration_hwdec.py      # --hwdec: fallback transparente y CLI
    ├── integration_backends_subs.py # Sixel/iTerm2 reales + subs SRT/ASS/embebidos
    ├── integration_tracks.py     # cambio de pista audio/subs en runtime + CLI
    └── stress_exit_hang.py       # salida limpia bajo decode saturado (HEVC)
```

## Tests

Los tests de integración ejecutan el binario release dentro de un pty real,
le inyectan teclas, resizes (`TIOCSWINSZ` + `SIGWINCH`) y analizan tanto el
log de sincronía (`RTV_SYNC_LOG`) como el propio stream de escape sequences
(con [pyte](https://github.com/selectel/pyte) como emulador de terminal):

```bash
cargo build --release
python3 tests/integration_sync.py       video.mp4
python3 tests/integration_resize.py     video.mp4 [ascii|blocks]
python3 tests/integration_resize_ux.py  video.mp4
python3 tests/integration_grow_quality.py video.mp4
python3 tests/integration_hwdec.py      video.mp4
```

Verifican, entre otras cosas: |avdiff| en régimen y tras cada seek, latencia
del primer frame post-seek, supervivencia a tormentas de 60+ resizes con
tamaños degenerados (4×3), latencia de redibujo tras resize, que ninguna
secuencia de cursor escriba fuera de los límites del terminal y que el HUD
no se repinte más de lo necesario.

## Estado y hoja de ruta

Hecho:

- [x] Audio con cpal + swresample, reloj de audio maestro
- [x] Motor de sync estilo ffplay (drop/duplicado con umbrales de ffplay)
- [x] Seeks instantáneos con aterrizaje en keyframe y audio alineado al PTS real
- [x] Escalado adaptativo por tamaño real de celda
- [x] Resize en caliente instantáneo, sin perder sync ni colchón de decode
- [x] HUD adaptativo sin parpadeo; oculto en ventanas minúsculas
- [x] Decode por hardware (`--hwdec`): VAAPI/CUDA/QSV (Linux),
      D3D11VA/DXVA2 (Windows), VideoToolbox (macOS), con fallback
      transparente a software incluso a mitad de stream
- [x] Barra de progreso clicable con ratón (HUD de terminal y GUI)
- [x] GUI de ventana (`--gui`, winit + wgpu + egui) compartiendo el mismo
      núcleo de reproducción que el terminal

Pendiente:

- [ ] Medir la ganancia de `--hwdec` en una máquina con GPU real
      (el sandbox de CI no tiene `/dev/dri`)

## Licencia

Doble licencia: MIT o Apache-2.0, a tu elección.
