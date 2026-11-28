#!/bin/sh
# Drive the title's own start-up (legal notices, terms, onboarding, finish setup) with the
# scripted pad, dumping frames along the way.
#
#   scripts/drive-startup.sh <game.exe> [seconds] [out-dir]
#
# The sequences, learned from the frames of earlier runs:
#  - A at 5, 10, 15 and 20 seconds advance the legal notices (its list and Next).
#  - From 100 seconds, every 30 seconds: up, up, A (focus and tick the privacy row), down, A
#    (the terms row), down, A (Confirm) - the spacing that ticked both rows and enabled Confirm
#    in an earlier run. Its A also carries the onboarding and finish-setup screens.
# The frames land every 1800 presents in <out-dir> (or /tmp/nativ64-startup), with dump spacing
# from NATIV64_DUMP_EVERY; the run ends by timeout, which is the expected end.
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

script="0x4@5+2,0x4@10+2,0x4@15+2,0x4@20+2"
t=100
while [ "$t" -le "$seconds" ]; do
    for at in "$t:0x40:1" "$((t + 4)):0x40:1" "$((t + 8)):0x4:2" \
        "$((t + 15)):0x80:1" "$((t + 19)):0x4:2" "$((t + 26)):0x80:1" \
        "$((t + 30)):0x4:2"; do
        when=${at%%:*}
        rest=${at#*:}
        mask=${rest%%:*}
        hold=${rest##*:}
        script="$script,$mask@$when+$hold"
    done
    t=$((t + 30))
done

mkdir -p "$out"
echo "drive-startup: $seconds s, frames in $out"
cd "$root"
NATIV64_INPUT_SCRIPT="$script" \
NATIV64_DUMP_TARGETS="$out" \
NATIV64_DUMP_FROM=6000 \
NATIV64_DUMP_EVERY=${NATIV64_DUMP_EVERY:-1800} \
NATIV64_NATIVE_MODULES=acphal,chad,d3d12_xs,gameinput,tuxbox_xhttp,vehshim,xapu,xcrdapi,xg_xs,xgameruntime,xgameruntime_real,xmem \
NATIV64_WINDOW=wayland NATIV64_GAMEPAD=1 NATIV64_PATCH=0x2215c70=9090 \
exec ./target/release/nativ64-run --timeout "$seconds" "$exe"
