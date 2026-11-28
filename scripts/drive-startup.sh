#!/bin/sh
# Drive the title's own start-up (legal notices, terms, onboarding, finish setup) with the
# scripted pad, dumping frames along the way.
#
#   scripts/drive-startup.sh <game.exe> [seconds] [out-dir]
#
# The start-up screens come up at an unpredictable frame (the legal notices between frame 6000
# and 9000 in this session's runs), so the script's clock is anchored to a presented frame
# (`NATIV64_INPUT_WAIT_FRAME`, default 6800) and then runs once:
#  - each legal page opens its document when A lands on the row: A, then the right stick scrolls
#    the document down (two 12-second holds), then A accepts it, then down, down focuses Next and
#    A turns the page - once for Copyright (at 2 s) and once for Photosensitivity (at 44 s);
#  - at 86 to 115 seconds the terms sequence up, up, A (privacy), down, A (terms), down, A
#    (Confirm) with the 4/7/7 second spacing that ticked both rows and enabled Confirm;
#  - A at 140, 165 and 190 seconds carry the onboarding and finish-setup screens.
# The frames land every 1500 presents in <out-dir> (or /tmp/nativ64-startup); the run ends by
# timeout, which is the expected end.
set -eu

if [ $# -lt 1 ]; then
    sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'
    exit 64
fi

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
exe=$1
shift
seconds=800
case ${1:-} in
    '' | *[!0-9]*) ;;
    *) seconds=$1; shift ;;
esac
out=/tmp/nativ64-startup
case ${1:-} in
    '' | -*) ;;
    *) out=$1; shift ;;
esac
if [ ! -f "$exe" ]; then
    echo "drive-startup: $exe: no such file" >&2
    exit 1
fi

script=""
for at in "2:0x4:2" "6:ry=-1:12" "22:0x4:2" "28:0x80:1" "31:0x80:1" "34:0x4:2" \
    "44:0x4:2" "48:ry=-1:12" "64:0x4:2" "70:0x80:1" "73:0x80:1" "76:0x4:2" \
    "86:0x40:1" "90:0x40:1" "94:0x4:2" "101:0x80:1" "105:0x4:2" "111:0x80:1" \
    "115:0x4:2" "140:0x4:2" "165:0x4:2" "190:0x4:2"; do
    when=${at%%:*}
    rest=${at#*:}
    mask=${rest%%:*}
    hold=${rest##*:}
    script="$script,$mask@$when+$hold"
done

mkdir -p "$out"
echo "drive-startup: $seconds s, frames in $out"
cd "$root"
NATIV64_INPUT_SCRIPT="$script" \
NATIV64_DUMP_TARGETS="$out" \
NATIV64_DUMP_FROM=6000 \
NATIV64_DUMP_EVERY=${NATIV64_DUMP_EVERY:-1500} \
NATIV64_INPUT_WAIT_FRAME=${NATIV64_INPUT_WAIT_FRAME:-6800} \
NATIV64_NATIVE_MODULES=acphal,chad,d3d12_xs,gameinput,tuxbox_xhttp,vehshim,xapu,xcrdapi,xg_xs,xgameruntime,xgameruntime_real,xmem \
NATIV64_WINDOW=wayland NATIV64_GAMEPAD=1 NATIV64_PATCH=0x2215c70=9090 \
exec ./target/release/nativ64-run --timeout "$seconds" "$exe"
