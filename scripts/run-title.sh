#!/bin/sh
# Reproduce a native run of the title and collect the evidence of one run.
#
#   scripts/run-title.sh <game.exe> [seconds] [out-dir] [-- <nativ64-run args>...]
#
# Builds the release loader, runs it headless with frame dumps and the profile on, and prints a
# one-screen summary of the log (frames presented, profile, skips, first fatal/refusal lines).
# `seconds` defaults to 120 (a timeout is the expected end of a run). `out-dir` defaults to
# /tmp/nativ64-run and receives run.log plus frames/.
#
# The game's genuine package files must sit next to the exe. A directory that also holds Wine-era
# replacement DLLs is fine: NATIV64_NATIVE_MODULES makes nativ64 answer those modules itself.
set -eu

if [ $# -lt 1 ]; then
    sed -n '2,6p' "$0" | sed 's/^# \{0,1\}//'
    exit 64
fi

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
exe=$1
shift
seconds=120
case ${1:-} in
    '' | *[!0-9]*) ;;
    *) seconds=$1; shift ;;
esac
out=/tmp/nativ64-run
case ${1:-} in
    '' | -*) ;;
    *) out=$1; shift ;;
esac
if [ "${1:-}" = "--" ]; then
    shift
fi

if [ ! -f "$exe" ]; then
    echo "run-title: $exe: no such file" >&2
    exit 1
fi

# Modules whose on-disk files are the abandoned Wine route's replacements: nativ64 must answer
# them, or the loader binds the old shims and the run is not the native path.
: "${NATIV64_NATIVE_MODULES:=acphal,chad,d3d12_xs,gameinput,tuxbox_xhttp,vehshim,xapu,xcrdapi,xg_xs,xgameruntime,xgameruntime_real,xmem}"
export NATIV64_NATIVE_MODULES
export NATIV64_WINDOW=${NATIV64_WINDOW:-headless}
export NATIV64_DUMP_FRAMES="$out/frames"
export NATIV64_PROFILE=${NATIV64_PROFILE:-1}
export NATIV64_DEBUG_STRING=${NATIV64_DEBUG_STRING:-1}
export NATIV64_SEH_LOG=${NATIV64_SEH_LOG:-1}

mkdir -p "$out"
rm -rf "$out/frames"
mkdir -p "$out/frames"

echo "run-title: building the release loader"
(cd "$root" && cargo build --release -p nativ64-loader --bin nativ64-run)

echo "run-title: $exe, ${seconds}s, out $out"
set +e
(cd "$root" && timeout $((seconds + 30)) ./target/release/nativ64-run --timeout "$seconds" "$@" "$exe") \
    >"$out/run.log" 2>&1
status=$?
set -e

echo
echo "--- summary ($out/run.log) ---"
grep -aE "PresentX frame " "$out/run.log" | tail -3 || true
grep -aE "nativ64-profile: frame" "$out/run.log" | tail -1 || true
grep -aE "nativ64-gpu: frame [0-9]+:" "$out/run.log" | tail -1 || true
grep -acE "^nativ64-gpu: replay skips" "$out/run.log" | sed 's/^/skip reasons logged once: /' || true
grep -aE "nativ64: fatal|nativ64: unimplemented|a console value$" "$out/run.log" | head -5 || true
echo "frames: $(find "$out/frames" -name '*.png' 2>/dev/null | wc -l) in $out/frames"
echo "exit: $status (124 = the requested timeout, the normal end of a run)"
