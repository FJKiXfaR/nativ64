#!/bin/sh
# Regenerate the host-API coverage map: what the title imports and how much nativ64 answers.
#
#   scripts/import-map.sh <game.exe> [package.dll...] [--out DIR]
#
# Writes reports/status-nativ64.json (the status of every export nativ64 registers) and
# reports/import-map.json / .html (the map). Pass the executable and the game's *genuine* package
# DLLs - not the Wine-era replacement DLLs from an old run directory, whose imports are not the
# title's. The map is derived from game files: it is git-ignored, never commit it.
set -eu

out=reports
files=
count=0
while [ $# -gt 0 ]; do
    case $1 in
        --out)
            out=$2
            shift 2
            ;;
        --out=*)
            out=${1#--out=}
            shift
            ;;
        -*)
            echo "import-map: unknown option $1" >&2
            exit 64
            ;;
        *)
            files="$files $1"
            count=$((count + 1))
            shift
            ;;
    esac
done
if [ "$count" -eq 0 ]; then
    sed -n '2,6p' "$0" | sed 's/^# \{0,1\}//'
    exit 64
fi
for f in $files; do
    if [ ! -f "$f" ]; then
        echo "import-map: $f: no such file" >&2
        exit 1
    fi
done

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
mkdir -p "$root/$out"
cd "$root"

echo "import-map: collecting nativ64's registrations"
cargo run -q -p nativ64-status --bin nativ64-status >"$out/status-nativ64.json"

echo "import-map: reading the import tables of $count file(s)"
# shellcheck disable=SC2086
python3 -I tools/import_map.py $files \
    --status "$out/status-nativ64.json" \
    --out "$out/import-map.html" \
    --json "$out/import-map.json"

python3 - "$out/import-map.json" <<'EOF'
import json
import sys

d = json.load(open(sys.argv[1]))
for view in d["views"]:
    t = view["totals"]
    done = sum(t[k] for k in view.get("done", ["native"]))
    counted = sum(v for k, v in t.items() if k != "shipped")
    print(f"{view['tab']}: {len(d['functions'])} imported, {done}/{counted} done, totals {t}")
EOF
echo "import-map: wrote $out/status-nativ64.json, $out/import-map.json, $out/import-map.html"
