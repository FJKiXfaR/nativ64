#!/bin/sh
# Play the title with a real controller and no frame dumps.
#
#   scripts/play.sh <game.exe> [seconds]
#
# Sets the pieces a plain run of the title needs (the genuine package DLLs next to the exe are
# answered by nativ64's own modules, the watchdog's hang patch, the Wayland window) and turns the
# gamepad on: `NATIV64_GAMEPAD=1` reads the pad's joystick and scans /proc/bus/input/devices for
# its evdev node (a pad reports most of its buttons only there). `NATIV64_GAMEPAD_LOG=1` in the
# environment prints every button the pad sends. Frame dumps are deliberately off: reading frames
# back during a loading burst can cost the GPU device (see native-handoff.md).
set -eu

if [ $# -lt 1 ]; then
    sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
    exit 64
fi

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
exe=$1
shift
seconds=${1:-7200}
case $seconds in
    '' | *[!0-9]*) echo "play: the seconds must be a number" >&2; exit 64 ;;
esac
if [ ! -f "$exe" ]; then
    echo "play: $exe: no such file" >&2
    exit 1
fi

cd "$root"
echo "play: $exe for up to $seconds s; Ctrl-C ends it"
NATIV64_GAMEPAD=${NATIV64_GAMEPAD:-1} \
NATIV64_NATIVE_MODULES=${NATIV64_NATIVE_MODULES:-acphal,chad,d3d12_xs,gameinput,tuxbox_xhttp,vehshim,xapu,xcrdapi,xg_xs,xgameruntime,xgameruntime_real,xmem} \
NATIV64_WINDOW=${NATIV64_WINDOW:-wayland} \
NATIV64_PATCH=${NATIV64_PATCH:-0x2215c70=9090} \
exec ./target/release/nativ64-run --timeout "$seconds" "$exe"
