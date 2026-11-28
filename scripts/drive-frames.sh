#!/bin/sh
# Drive the title's own start-up with a pad script timed in **presented frames**.
#
#   scripts/drive-frames.sh <game.exe> [seconds] [out-dir]
#
# While this build loads, frames are minutes apart; a wall-clock script presses long before or
# after the screen it means (scripts/drive-startup.sh says why it exists for a fast run). This
# one presses by frame number (`<mask>@f<frame>+<frames>`, see the gameinput module docs):
#
#  - a cycle every 240 frames from 7000 to 32000: A, a right-stick scroll of 30 frames (a legal
#    document), A, down, down, A that turns the page - the pattern that carried the legal notices
#    in the fast runs, repeated so screens that appear later still get their presses;
#  - from 32000 A every 300 frames (onboarding, finish setup, the logo), up to 120000.
#
# Frames land every 1500 presents in <out-dir> (or /tmp/nativ64-startup-frames), and a run ends
# by its timeout, which is the expected end.
set -eu

if [ $# -lt 1 ]; then
    sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'
    exit 64
fi
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
exe=$1
shift
seconds=7200
case ${1:-} in
    '' | *[!0-9]*) ;;
    *) seconds=$1; shift ;;
esac
out=/tmp/nativ64-startup-frames
case ${1:-} in
    '' | -*) ;;
    *) out=$1; shift ;;
esac
if [ ! -f "$exe" ]; then
    echo "drive-frames: $exe: no such file" >&2
    exit 1
fi
mkdir -p "$out"

script=""
f=7000
while [ "$f" -lt 32000 ]; do
    script="$script,4@f$f+2,ry=-1@f$((f + 8))+30,4@f$((f + 60))+2"
    script="$script,0x80@f$((f + 90))+2,0x80@f$((f + 100))+2,4@f$((f + 120))+2"
    f=$((f + 240))
done
f=32000
while [ "$f" -lt 120000 ]; do
    script="$script,4@f$f+2"
    f=$((f + 300))
done
script=${script#,}

cd "$root"
echo "drive-frames: $exe for up to $seconds s, frames into $out"
NATIV64_INPUT_SCRIPT="$script" \
NATIV64_GAMEPAD=${NATIV64_GAMEPAD:-1} \
NATIV64_NATIVE_MODULES=${NATIV64_NATIVE_MODULES:-acphal,chad,d3d12_xs,gameinput,tuxbox_xhttp,vehshim,xapu,xcrdapi,xg_xs,xgameruntime,xgameruntime_real,xmem} \
NATIV64_WINDOW=${NATIV64_WINDOW:-wayland} \
NATIV64_PATCH=${NATIV64_PATCH:-0x2215c70=9090} \
NATIV64_DUMP_TARGETS="$out" \
NATIV64_DUMP_FROM=1500 \
NATIV64_DUMP_EVERY=${NATIV64_DUMP_EVERY:-1500} \
NATIV64_PROFILE=${NATIV64_PROFILE:-1} \
exec ./target/release/nativ64-run --timeout "$seconds" "$exe"
