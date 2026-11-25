#!/usr/bin/env python3
"""Import map: which system functions a PE binary needs, and how far we are.

Reads the import tables (normal and delay-load) of one or more PE files,
groups every imported function by area (files, threads, graphics...), applies
a status file, and writes an HTML coverage map plus a JSON summary.

    python -I tools/import_map.py game/*.dll game/<title>.exe \
        --status tools/import-status.json --out reports/import-map.html

Statuses:
    native   implemented natively on Linux by us
    host     provided by the host OS: works, but not our code
    stub     exists but does nothing useful yet
    crash    called and failing (seen in logs)
    todo     not started (default)
    shipped  computed: provided by a DLL shipped with the game itself,
             so it runs as game code and needs no reimplementation

Only reads files; never executes or loads them. No dependencies.
"""

from __future__ import annotations

import argparse
import fnmatch
import json
import re
import struct
import sys
from collections import defaultdict
from pathlib import Path

STATUSES = ["native", "host", "stub", "crash", "todo", "shipped"]
# Area of each module, by pattern on the normalized module name (first match wins).
AREAS = [
    ("Files", ["api-ms-win-core-file*", "api-ms-win-core-handle*", "api-ms-win-core-io*"]),
    ("Threads & sync", ["api-ms-win-core-processthreads*", "api-ms-win-core-synch*",
                        "api-ms-win-core-fibers*", "api-ms-win-core-threadpool*",
                        "api-ms-win-core-interlocked*", "api-ms-win-core-processtopology*"]),
    ("Memory", ["api-ms-win-core-heap*", "api-ms-win-core-memory*", "xmem"]),
    ("Loader", ["api-ms-win-core-libraryloader*"]),
    ("C/C++ runtime", ["api-ms-win-crt-*", "vcruntime*", "msvcp*", "ucrtbase*",
                       "concrt*", "vcomp*", "vccorlib*"]),
    ("Graphics", ["d3d12*", "dxgi*", "xboxbvhbuilder", "amd_fidelityfx*"]),
    ("Audio & video", ["xaudio*", "xapo*", "mmdevapi*", "bink*"]),
    ("Input", ["gameinput*", "xinput*"]),
    ("Xbox services", ["xgameruntime*", "xgame*", "xbox*", "xuser*", "xstore*",
                       "api-ms-win-core-psm-*"]),
    ("Network & crypto", ["winhttp*", "ws2_32*", "xcurl*", "libhttpclient*",
                          "crypt32*", "bcrypt*", "ncrypt*"]),
    ("System", ["api-ms-win-*", "kernel32*", "kernelbase*", "ntdll*", "advapi32*",
                "ole32*", "oleaut32*", "combase*", "user32*"]),
]


class PEError(Exception):
    pass


class PE:
    """Minimal read-only PE32/PE32+ parser: imports, delay imports, exports."""

    def __init__(self, path: Path):
        self.path = path
        self.data = path.read_bytes()
        d = self.data
        if d[:2] != b"MZ":
            raise PEError("no MZ header")
        pe = struct.unpack_from("<I", d, 0x3C)[0]
        if d[pe:pe + 4] != b"PE\0\0":
            raise PEError("no PE signature")
        self.machine, nsec = struct.unpack_from("<HH", d, pe + 4)
        opt_size = struct.unpack_from("<H", d, pe + 20)[0]
        opt = pe + 24
        magic = struct.unpack_from("<H", d, opt)[0]
        if magic == 0x20B:
            self.is64 = True
            self.image_base = struct.unpack_from("<Q", d, opt + 24)[0]
            dirs = opt + 112
        elif magic == 0x10B:
            self.is64 = False
            self.image_base = struct.unpack_from("<I", d, opt + 28)[0]
            dirs = opt + 96
        else:
            raise PEError(f"unknown optional header magic {magic:#x}")
        ndirs = struct.unpack_from("<I", d, dirs - 4)[0]
        self.dirs = [struct.unpack_from("<II", d, dirs + 8 * i) for i in range(min(ndirs, 16))]
        self.sections = []
        for i in range(nsec):
            o = opt + opt_size + 40 * i
            vsize, va, rsize, roff = struct.unpack_from("<IIII", d, o + 8)
            self.sections.append((va, max(vsize, rsize), roff, rsize))

    def off(self, rva: int) -> int:
        for va, size, roff, rsize in self.sections:
            if va <= rva < va + size:
                if rva - va >= rsize:
                    raise PEError(f"rva {rva:#x} is past the section's raw data")
                return roff + rva - va
        raise PEError(f"rva {rva:#x} outside every section")

    def cstr(self, rva: int) -> str:
        o = self.off(rva)
        end = self.data.index(b"\0", o)
        return self.data[o:end].decode("ascii", "replace")

    def _thunks(self, rva: int):
        size, fmt, flag = (8, "<Q", 1 << 63) if self.is64 else (4, "<I", 1 << 31)
        o = self.off(rva)
        while True:
            v = struct.unpack_from(fmt, self.data, o)[0]
            if v == 0:
                return
            if v & flag:
                yield f"#{v & 0xFFFF}"
            else:
                yield self.cstr((v & 0x7FFFFFFF) + 2)
            o += size

    def imports(self) -> dict[str, list[str]]:
        out: dict[str, list[str]] = defaultdict(list)
        rva, size = self.dirs[1] if len(self.dirs) > 1 else (0, 0)
        if rva:
            o = self.off(rva)
            while True:
                oft, _, _, name, ft = struct.unpack_from("<IIIII", self.data, o)
                if name == 0:
                    break
                out[self.cstr(name)].extend(self._thunks(oft or ft))
                o += 20
        rva, size = self.dirs[13] if len(self.dirs) > 13 else (0, 0)
        if rva:
            o = self.off(rva)
            while True:
                attrs, name, _, _, int_rva = struct.unpack_from("<IIIII", self.data, o)
                if name == 0:
                    break
                fix = 0 if attrs & 1 else self.image_base
                out[self.cstr(name - fix)].extend(self._thunks(int_rva - fix))
                o += 32
        return out

    def exports(self) -> set[str]:
        rva, size = self.dirs[0] if self.dirs else (0, 0)
        if not rva:
            return set()
        o = self.off(rva)
        n_names = struct.unpack_from("<I", self.data, o + 24)[0]
        names_rva = struct.unpack_from("<I", self.data, o + 32)[0]
        names = set()
        if not n_names:
            return names
        base = self.off(names_rva)
        for i in range(n_names):
            names.add(self.cstr(struct.unpack_from("<I", self.data, base + 4 * i)[0]))
        return names


def norm_module(name: str) -> str:
    """'API-MS-WIN-CORE-FILE-L1-2-0.dll' -> 'api-ms-win-core-file'."""
    n = name.lower()
    if n.endswith(".dll"):
        n = n[:-4]
    if n.startswith(("api-ms-", "ext-ms-")):
        n = re.sub(r"-l\d+-\d+-\d+$", "", n)
    return n


def area_of(module: str) -> str:
    for area, patterns in AREAS:
        if any(fnmatch.fnmatch(module, p) for p in patterns):
            return area
    return "Other"


DEFAULT_VIEW = {
    "tab": "Linux native",
    "title": "Linux-native port",
    "headline": "system functions run natively on Linux",
    "labels": {
        "native": "Native Linux",
        "host": "Provided by host OS",
        "stub": "Stub",
        "crash": "Called, failing",
        "todo": "Not started",
        "shipped": "Shipped with the game",
    },
    "done": ["native"],
    "shipped": "first",
}


def load_status(path: Path | None) -> tuple[str, list[tuple[str, str]], dict]:
    """Returns (default status, rules, view). A status file may override the
    view: its title, headline, status labels, which statuses count as done and
    the "shipped" precedence ("first": shipped DLL exports beat every rule;
    "fallback": rules apply first, shipped only when no rule matches)."""
    view = json.loads(json.dumps(DEFAULT_VIEW))
    if not path or not path.exists():
        return "todo", [], view
    cfg = json.loads(path.read_text(encoding="utf-8-sig"))
    default = cfg.get("default", "todo")
    rules = []
    for pattern, status in cfg.get("functions", {}).items():
        if status not in STATUSES:
            raise SystemExit(f"{path}: unknown status {status!r} for {pattern!r}")
        rules.append((pattern.lower(), status))
    for k in ("tab", "title", "headline", "done", "shipped"):
        if k in cfg:
            view[k] = cfg[k]
    view["labels"].update(cfg.get("labels", {}))
    if any(s not in STATUSES for s in view["done"]):
        raise SystemExit(f"{path}: 'done' may only list {STATUSES}")
    if view["shipped"] not in ("first", "fallback"):
        raise SystemExit(f"{path}: 'shipped' must be \"first\" or \"fallback\"")
    return default, rules, view


def match_rule(key: str, rules: list[tuple[str, str]]) -> str | None:
    """Exact matches win over globs; among globs, the longest pattern wins."""
    k = key.lower()
    best, best_len = None, -1
    for pattern, status in rules:
        if pattern == k:
            return status
        if fnmatch.fnmatch(k, pattern) and len(pattern) > best_len:
            best, best_len = status, len(pattern)
    return best


def status_of(key: str, default: str, rules: list[tuple[str, str]]) -> str:
    return match_rule(key, rules) or default


def build(paths: list[Path], status_files: list[Path | None]) -> dict:
    """Read the imports once, then apply each status file as its own view (tab)."""
    parsed: dict[str, PE] = {}
    errors = {}
    for p in paths:
        try:
            parsed[p.name.lower()] = PE(p)
        except (PEError, OSError, struct.error, ValueError) as e:
            errors[p.name] = str(e)

    shipped_exports = {}
    for name, pe in parsed.items():
        try:
            shipped_exports[norm_module(name)] = pe.exports()
        except (PEError, struct.error, ValueError) as e:
            errors[name] = f"exports: {e}"

    funcs: dict[str, dict] = {}
    for name, pe in parsed.items():
        try:
            imps = pe.imports()
        except (PEError, struct.error, ValueError) as e:
            errors[name] = f"imports: {e}"
            continue
        for module, names in imps.items():
            m = norm_module(module)
            for fn in names:
                key = f"{m}!{fn}"
                f = funcs.setdefault(key, {"module": m, "function": fn, "area": area_of(m),
                                           "importers": []})
                if pe.path.name not in f["importers"]:
                    f["importers"].append(pe.path.name)
    funcs = dict(sorted(funcs.items()))

    views = []
    for status_file in status_files:
        default, rules, view = load_status(status_file)
        statuses = {}
        for key, f in funcs.items():
            is_shipped = f["function"] in shipped_exports.get(f["module"], ())
            if is_shipped and view["shipped"] == "first":
                statuses[key] = "shipped"
            else:
                rule = match_rule(key, rules)
                statuses[key] = rule or ("shipped" if is_shipped else default)
        totals = {s: 0 for s in STATUSES}
        for st in statuses.values():
            totals[st] += 1
        views.append(view | {"totals": totals, "statuses": statuses})

    return {
        "inputs": sorted(p.name for p in paths),
        "errors": errors,
        "functions": funcs,
        "views": views,
    }


def render_html(report: dict) -> str:
    """Fill the HTML template (tools/import_map.html) with the report data."""
    template = Path(__file__).with_name("import_map.html").read_text(encoding="utf-8")
    data = json.dumps(report, ensure_ascii=False).replace("</", "<\\/")
    return template.replace("/*__REPORT__*/null", data)

SVG_COLORS = {"native": "#0ca30c", "host": "#a6e3a1", "stub": "#fab219",
              "crash": "#d03b3b", "todo": "#3a3a37", "shipped": "#3987e5"}


def render_svg(report: dict, view_name: str | None = None, note: str = "") -> str:
    """Static SVG card of one view (same layout and dark colors as the HTML map)."""
    from xml.sax.saxutils import escape

    views = report["views"]
    v = next((x for x in views if x["tab"] == view_name), None) if view_name else views[-1]
    if v is None:
        raise SystemExit(f"--svg-view: no view named {view_name!r} (have: "
                         + ", ".join(x['tab'] for x in views) + ")")
    st, totals = v["statuses"], v["totals"]
    W, PAD = 900, 28
    inner = W - 2 * PAD
    fmt = "{:,}".format
    font = 'font-family="ui-sans-serif,system-ui,-apple-system,\'Segoe UI\',Roboto,sans-serif"'

    need = len(report["functions"]) - totals["shipped"]
    done = sum(totals[s] for s in v["done"])
    pct = 100 * done / need if need else 100

    by_area: dict[str, dict[str, list[str]]] = {}
    for key, f in report["functions"].items():
        by_area.setdefault(f["area"], {}).setdefault(f["module"], []).append(key)

    out: list[str] = []
    y = PAD + 14
    out.append(f'<text x="{PAD}" y="{y}" fill="#8f8e86" font-size="13" {font}>'
               f'{escape(v["title"])}</text>')
    y += 52
    out.append(f'<text x="{PAD}" y="{y}" fill="#f0efec" font-size="52" font-weight="700" {font}>'
               f'{pct:.1f}<tspan font-size="26" fill="#8f8e86">%</tspan></text>')
    out.append(f'<text x="{PAD + 190}" y="{y - 6}" fill="#c3c2b7" font-size="15" {font}>'
               f'<tspan font-weight="700" fill="#f0efec">{fmt(done)}</tspan> of '
               f'<tspan font-weight="700" fill="#f0efec">{fmt(need)}</tspan> '
               f'{escape(v["headline"])}</text>')
    y += 26
    x = PAD
    for s in (s for s in STATUSES if totals[s]):
        label = f'{v["labels"][s]} {fmt(totals[s])}'
        out.append(f'<rect x="{x}" y="{y - 10}" width="11" height="11" rx="2" fill="{SVG_COLORS[s]}"/>')
        out.append(f'<text x="{x + 16}" y="{y}" fill="#c3c2b7" font-size="12" {font}>{escape(label)}</text>')
        x += 16 + int(len(label) * 6.4) + 18
        if x > W - PAD - 120 and s != STATUSES[-1]:
            x, y = PAD, y + 20
    y += 30

    # Area tiles: 3 columns, one stacked bar of the non-shipped functions per area
    TGAP, TH, BPAD = 12, 62, 12
    tw = (inner - 2 * TGAP) / 3
    bw = tw - 2 * BPAD
    tiles = []
    for name, mods in by_area.items():
        keys = [k for m in mods.values() for k in m]
        c = {s: sum(1 for k in keys if st[k] == s) for s in STATUSES}
        n = len(keys) - c["shipped"]
        tiles.append((name, c, n, sum(c[s] for s in v["done"])))
    tiles.sort(key=lambda t: (-t[2], t[0]))
    top = y - 14
    for i, (name, c, n, ad) in enumerate(tiles):
        tx = PAD + (i % 3) * (tw + TGAP)
        ty = top + (i // 3) * (TH + TGAP)
        bx, by = tx + BPAD, ty + 30
        out.append(f'<rect x="{tx:.1f}" y="{ty}" width="{tw:.1f}" height="{TH}" rx="8" '
                   f'fill="#232322" stroke="#2f2f2d"/>')
        out.append(f'<text x="{bx:.1f}" y="{ty + 20}" fill="#f0efec" font-size="13" font-weight="600" {font}>'
                   f'{escape(name)}</text>')
        out.append(f'<text x="{tx + tw - BPAD:.1f}" y="{ty + 20}" text-anchor="end" fill="#8f8e86" '
                   f'font-size="12" {font}>{ad} / {n}</text>')
        out.append(f'<clipPath id="b{i}"><rect x="{bx:.1f}" y="{by}" width="{bw:.1f}" height="10" rx="3"/></clipPath>')
        segs = [(s, c[s]) for s in STATUSES if s != "shipped" and c[s]] if n else [("shipped", 1)]
        total = sum(k for _, k in segs)
        sx = bx
        out.append(f'<g clip-path="url(#b{i})">')
        for j, (s, k) in enumerate(segs):
            w = (bx + bw - sx) if j == len(segs) - 1 else max(2.0, bw * k / total)
            out.append(f'<rect x="{sx:.1f}" y="{by}" width="{w:.1f}" height="10" fill="{SVG_COLORS[s]}"/>')
            sx += w
        out.append('</g>')
        sub = f"{100 * ad / n:.0f}% done" if n else "all shipped"
        if n and c["shipped"]:
            sub += f" · {c['shipped']} shipped"
        out.append(f'<text x="{bx:.1f}" y="{ty + 55}" fill="#8f8e86" font-size="10.5" {font}>{escape(sub)}</text>')
    rows = -(-len(tiles) // 3)
    y = top + rows * (TH + TGAP) + 12

    y += 2
    foot = "Generated by tools/import_map.py" + (f" · {note}" if note else "")
    out.append(f'<text x="{PAD}" y="{y}" fill="#8f8e86" font-size="11" {font}>{escape(foot)}</text>')
    H = y + 18
    title = escape(f'{v["title"]}: {pct:.1f}% ({done} of {need} {v["headline"]})')
    return (f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" '
            f'role="img" aria-label="{title}">\n<title>{title}</title>\n'
            f'<rect width="{W}" height="{H}" rx="14" fill="#1a1a19"/>\n'
            + "\n".join(out) + "\n</svg>\n")


def main() -> int:
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(errors="replace")  # tab names may not fit a legacy console code page
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("files", nargs="+", type=Path, help="PE files (exe, dll)")
    ap.add_argument("--status", type=Path, action="append",
                    help="status file; repeat it for several tabs (default: tools/import-status.json)")
    ap.add_argument("--out", type=Path, default=Path("reports/import-map.html"))
    ap.add_argument("--svg", type=Path, help="also write a static SVG card of one view (for a README)")
    ap.add_argument("--svg-view", help="tab name of the view drawn by --svg (default: the last one)")
    ap.add_argument("--svg-note", default="", help="text appended to the SVG footer (e.g. a commit)")
    ap.add_argument("--json", type=Path, help="also write the full report as JSON")
    args = ap.parse_args()

    report = build(args.files, args.status or [Path(__file__).with_name("import-status.json")])
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(render_html(report), encoding="utf-8")
    if args.svg:
        args.svg.parent.mkdir(parents=True, exist_ok=True)
        args.svg.write_text(render_svg(report, args.svg_view, args.svg_note), encoding="utf-8")
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(report, indent=1), encoding="utf-8")

    for v in report["views"]:
        t = v["totals"]
        print(f"[{v['tab']}] {sum(t.values())} imported functions: "
              + ", ".join(f"{s} {t[s]}" for s in STATUSES))
    for name, err in report["errors"].items():
        print(f"warning: {name}: {err}", file=sys.stderr)
    print(f"map written to {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
