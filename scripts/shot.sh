#!/usr/bin/env bash
# Build and capture the Linux app on a private X11 display.
set -euo pipefail

fail_usage() {
    printf '%s\n' "$*" >&2
    exit 2
}

require() {
    command -v "$1" >/dev/null || fail_usage "Missing dependency: $1"
}

check_xvfb_socket() {
    # Xvfb uses abstract Unix sockets on Linux. TCP is disabled below.
    python3 - <<'PYTHON'
import errno
import os
import socket
import sys

try:
    with socket.socket(socket.AF_UNIX) as listener:
        listener.bind(b"\0expos-shot-" + os.urandom(8).hex().encode())
        listener.listen()
except OSError as error:
    if error.errno not in (errno.EACCES, errno.EPERM, errno.EAFNOSUPPORT):
        raise
    print(f"Xvfb Unix sockets are unavailable: {error}", file=sys.stderr)
    sys.exit(77)
PYTHON
}

if [[ ${1-} != --run-under-timeout ]]; then
    [[ $(uname -s) == Linux ]] || fail_usage 'scripts/shot.sh requires Linux.'
    [[ $# -ge 2 && $# -le 3 ]] || fail_usage 'usage: scripts/shot.sh <fixture-dir> <scenario.json> [out-dir]'

    fixture=$(realpath -e -- "$1") && [[ -d $fixture ]] || fail_usage "Invalid fixture directory: $1"
    scenario=$(realpath -e -- "$2") && [[ -f $scenario ]] || fail_usage "Invalid scenario file: $2"
    temp_root=$(realpath -m -- "${TMPDIR:-/tmp}")
    filename=${2##*/}
    stem=${filename%.*}
    out=$(realpath -m -- "${3:-$temp_root/expos-shots/${stem:-$filename}}")
    if [[ $out == "$fixture" || $out == "${fixture%/}/"* ||
          $temp_root == "$fixture" || $temp_root == "${fixture%/}/"* ]]; then
        fail_usage 'The output and temporary directories must be outside the source fixture.'
    fi

    require timeout
    require python3
    check_xvfb_socket
    umask 077
    run=$(mktemp -d "$temp_root/expos-shot.XXXXXXXX")
    cleanup() {
        local status=$?
        if (( status != 0 )) && [[ -s $run/xvfb.log ]]; then
            tail -n 20 -- "$run/xvfb.log" >&2
        fi
        rm -rf -- "$run"
    }
    trap cleanup EXIT

    # Supervise the child group while keeping cleanup outside it, so a timeout
    # or a TERM from the caller cannot interrupt removal of the private state.
    timeout --kill-after=1s 900s "$BASH" "$0" --run-under-timeout "$run" "$fixture" "$scenario" "$out" &
    child=$!
    stop_capture() {
        kill -TERM "$child" 2>/dev/null || true
        wait "$child" || true
        exit 1
    }
    trap stop_capture HUP INT TERM
    if wait "$child"; then
        exit 0
    else
        status=$?
    fi
    case $status in
        2|77) exit "$status" ;;
        124|137) echo 'Headless capture exceeded the 900-second run deadline.' >&2 ;;
    esac
    exit 1
fi
[[ $# == 5 ]] || fail_usage 'Missing supervised capture arguments.'
run=$2
fixture=$3
scenario=$4
out=$5

for program in cargo cc xvfb-run Xvfb xauth xdpyinfo import; do
    require "$program"
done

icd=${EXPOS_SHOT_VK_DRIVER:-}
if [[ -z $icd ]]; then
    for candidate in /usr/share/vulkan/icd.d/lvp*.json; do
        if [[ -r $candidate ]]; then
            icd=$candidate
            break
        fi
    done
fi
icd=$(realpath -e -- "$icd") && [[ -f $icd && -r $icd ]] ||
    fail_usage 'Mesa Lavapipe Vulkan ICD is missing. See docs/headless.md.'

unsafe_entry=$(find "$fixture" ! -type d ! -type f -print -quit)
[[ -z $unsafe_entry ]] || fail_usage "Fixture must contain only directories and regular files: $unsafe_entry"

mkdir -p -- "$run"/{cache,config,data,fixture,fonts,home,link,runtime,state,tmp} "$out"
cp -a -- "$fixture/." "$run/fixture/"
chmod -R u+rwX -- "$run/fixture"

# Minimal installations may supply the runtime sonames only.
for entry in xcb:1 xkbcommon:0 xkbcommon-x11:0; do
    name=lib${entry%:*}.so
    [[ $(cc -print-file-name="$name") == "$name" ]] || continue
    soname=$name.${entry#*:}
    installed=$(cc -print-file-name="$soname")
    [[ -f $installed ]] || fail_usage "Missing runtime library: $soname"
    ln -s -- "$(realpath -- "$installed")" "$run/link/$name"
done

have_fonts=false
for font in \
    /usr/share/fonts/opentype/ipaexfont-gothic/ipaexg.ttf \
    /usr/share/fonts/opentype/ipaexfont-mincho/ipaexm.ttf \
    /usr/share/fonts/truetype/dejavu/DejaVuSans.ttf \
    /usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf \
    /usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf; do
    [[ -f $font ]] || continue
    ln -s -- "$font" "$run/fonts/"
    have_fonts=true
done
if [[ $have_fonts == true ]]; then
    # A relative font directory also works when TMPDIR contains XML punctuation.
    cat >"$run/fonts.conf" <<'FONTS'
<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
<fontconfig>
  <dir prefix="relative">fonts</dir>
  <cachedir prefix="xdg">fontconfig</cachedir>
  <alias><family>sans-serif</family><prefer><family>DejaVu Sans</family><family>IPAexGothic</family></prefer></alias>
  <alias><family>serif</family><prefer><family>DejaVu Serif</family><family>IPAexMincho</family></prefer></alias>
  <alias><family>monospace</family><prefer><family>DejaVu Sans Mono</family></prefer></alias>
</fontconfig>
FONTS
    export FONTCONFIG_FILE="$run/fonts.conf"
else
    unset FONTCONFIG_FILE
fi
unset FONTCONFIG_PATH

# Preserve server-supplied toolchain and build caches when HOME is replaced.
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export HOME="$run/home" TMPDIR="$run/tmp" TMP="$run/tmp" TEMP="$run/tmp"
export XDG_CONFIG_HOME="$run/config" XDG_STATE_HOME="$run/state" XDG_RUNTIME_DIR="$run/runtime"
export XDG_CACHE_HOME="$run/cache" XDG_DATA_HOME="$run/data"
unset DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET ZED_HEADLESS LOCALAPPDATA GPUI_FILER_PSEUDO_LOCALE
unset XAUTHORITY VK_ICD_FILENAMES
export LANG=C.UTF-8 LC_ALL=C.UTF-8 TZ=UTC VK_DRIVER_FILES="$icd" GPUI_X11_SCALE_FACTOR=1

root=$(cd -- "$(dirname -- "$0")/.." && pwd -P)
cd -- "$root"
# Read Cargo's executable path so CARGO_TARGET_DIR and configured targets work.
client=$(
    RUST_FONTCONFIG_DLOPEN=1 LIBRARY_PATH="$run/link${LIBRARY_PATH:+:$LIBRARY_PATH}" \
        cargo build -p expos --message-format=json-render-diagnostics |
        python3 -c '
import json
import sys

client = None
for line in sys.stdin:
    event = json.loads(line)
    if event.get("reason") == "compiler-artifact" and event.get("executable"):
        target = event["target"]
        if target["name"] == "expos" and "bin" in target["kind"]:
            client = event["executable"]
if client is not None:
    print(client)
'
)
[[ -n $client && -x $client ]] || { echo 'Cargo did not produce the expos executable.' >&2; exit 1; }

cd -- "$run/fixture"
if xvfb-run -a -e "$run/xvfb.log" -s '-screen 0 3840x2160x24 -dpi 96 -nolisten tcp' \
    sh -c 'xdpyinfo >/dev/null || exit 1; exec "$@"' expos-shot \
    "$client" --headless-shot "$scenario" "$out" "$run/fixture"; then
    exit 0
else
    status=$?
fi
if grep -Eqi '(unix|xservtrans|bind).*(permission denied|operation not permitted)' "$run/xvfb.log" 2>/dev/null; then
    exit 77
fi
[[ $status == 2 ]] && exit 2
exit 1
