#!/data/data/com.termux/files/usr/bin/sh
# Instala paquetes de Termux ROTANDO de mirror hasta que uno funcione.
#
#   sh scripts/termux-pkg.sh paquete1 paquete2 ...
#
# Por qué existe: `pkg` elige un mirror ALEATORIO y los mirrors se
# desincronizan a menudo — dos modos de fallo reales vistos en CI:
#   a) "File has unexpected size ... Mirror sync in progress?" (índice
#      y .deb de versiones distintas)  → exit 100 del apt update
#   b) "ncurses-ui-libs : Depends: ncurses (= X) but Y is to be
#      installed" → el propio mirror principal a MEDIO sincronizar
#      publica un índice inconsistente y el install revienta
# En ambos casos la cura es la misma: probar OTRO mirror (cada uno
# sincroniza en momentos distintos, siempre hay alguno consistente).
set -u

PREFIX="${PREFIX:-/data/data/com.termux/files/usr}"

# Mirrors oficiales (lista de termux-tools), ordenados: primario,
# CDN Cloudflare, y espejos independientes en regiones distintas
# (cuanto más repartidos, menos probable que TODOS estén a medio sync).
MIRRORS="
https://packages.termux.dev/apt/termux-main
https://packages-cf.termux.dev/apt/termux-main
https://grimler.se/termux/termux-main
https://mirror.mwt.me/termux/main
https://ftp.fau.de/termux/termux-main
https://plug-mirror.rcac.purdue.edu/termux/termux-main
https://mirror.csclub.uwaterloo.ca/termux/termux-main
https://mirrors.tuna.tsinghua.edu.cn/termux/apt/termux-main
"

# Sin chosen_mirrors `pkg`/apt respetan sources.list tal cual (con él,
# pkg re-sortea un mirror aleatorio cada pocas horas y pisa lo nuestro).
rm -f "$PREFIX/etc/termux/chosen_mirrors" 2>/dev/null || true

for m in $MIRRORS; do
    echo "==> probando mirror: $m"
    if ! echo "deb $m stable main" > "$PREFIX/etc/apt/sources.list" 2>/dev/null; then
        # Sin permiso de escritura (raro): al menos intentar con el actual.
        echo "    (no puedo escribir sources.list; uso el mirror ya configurado)"
    fi
    if ! apt-get update -y; then
        echo "==> apt update falló con $m — siguiente mirror"
        continue
    fi
    if apt-get install -y -o Dpkg::Options::=--force-confnew "$@"; then
        echo "==> OK: paquetes instalados desde $m"
        exit 0
    fi
    echo "==> install falló con $m (¿índice inconsistente?) — siguiente mirror"
done

echo "Ningún mirror de Termux sirvió ahora mismo. Reintenta en unos" >&2
echo "minutos o elige mirror a mano con 'termux-change-repo'." >&2
exit 1
