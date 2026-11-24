#!/usr/bin/env python3
"""Turn VIctory's source into import-map status files.

VIctory resolves every function the game imports in this order
(see its docs/architecture.md):
  1. its own implementation (crates/victory-hle/src/modules, registered with
     hle_export! or HleExport), some of it compiled on Windows only;
  2. the host's own Windows DLL, if HostPolicy allows that DLL
     (crates/victory-hle/src/host.rs);
  3. a DLL shipped with the game (import_map's "shipped"; the windows view
     sets "shipped": "fallback" so rules 1-2 win over it);
  4. nothing: a generated stub that traps when called.

This script reads those two places (it never builds or runs anything) and
writes status files for tools/import_map.py:

  --view windows   what VIctory itself implements on Windows
      native  implemented by VIctory (the only "done" status)
      host    borrowed from the host's Windows DLL (exports unchecked)
      crash   not resolved: traps if the game calls it
  --view linux     what a Linux-native port still needs
      native  VIctory code that does not touch Windows (portable)
      stub    VIctory code that is registered on Windows only, or whose
              body calls Windows (host!, windows_sys, windows::): needs porting
      todo    Windows API VIctory borrows from the host (must be written)
      crash   not resolved even on Windows

    python -I tools/victory_status.py --victory ../VIctory --view linux \
        --out reports/status-victory-linux.json
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

VIEWS = {
    "windows": {
        "tab": "Windows · VIctory",
        "title": "VIctory coverage on Windows",
        "headline": "functions implemented by VIctory itself",
        "labels": {
            "native": "Implemented by VIctory",
            "host": "Borrowed from host Windows",
            "crash": "Unresolved (traps)",
            "todo": "Not started",
            "shipped": "Shipped with the game",
        },
        "done": ["native"],
        "shipped": "fallback",  # host pass-through beats DLLs shipped with the game
    },
    "linux": {
        "tab": "Linux native",
        "title": "Linux-native port: what's left",
        "headline": "functions with portable code",
        "labels": {
            "native": "Portable VIctory code",
            "stub": "VIctory code tied to Windows",
            "crash": "Unresolved even on Windows",
            "todo": "Host Windows API to write",
            "shipped": "Shipped with the game",
        },
        "done": ["native"],
    },
}


def strip_comments(src: str) -> str:
    src = re.sub(r"/\*.*?\*/", "", src, flags=re.S)
    return re.sub(r"//[^\n]*", "", src)


def blocks(src: str, opener: str):
    """Yield the text of every `opener { ... }` block, braces balanced."""
    for m in re.finditer(re.escape(opener) + r"\s*\{", src):
        depth, i = 0, m.end() - 1
        for j in range(i, len(src)):
            if src[j] == "{":
                depth += 1
            elif src[j] == "}":
                depth -= 1
                if depth == 0:
                    yield src[i + 1:j]
                    break


def to_module_glob(pattern: str) -> str:
    """VIctory DLL pattern -> import_map module glob.

    'kernel32.dll' -> 'kernel32'; 'api-ms-win-core-libraryloader-*' ->
    'api-ms-win-core-libraryloader*' (import_map drops API-set versions).
    """
    p = pattern.lower()
    if p.endswith(".dll"):
        p = p[:-4]
    if p.endswith("-*"):
        p = p[:-2] + "*"
    return p


def registered_modules(mod_rs: str) -> dict[str, bool]:
    """Modules that `all()` registers -> True if registered on Windows only."""
    src = strip_comments(mod_rs)
    body = next(blocks(src, "fn all() -> Vec<HleModule>"), "")
    always = re.findall(r"(\w+)::module\(\)", re.search(r"vec!\[(.*?)\]", body, re.S).group(1))
    win = re.findall(r"#\[cfg\(windows\)\]\s*all\.push\(\s*(\w+)::module\(\)", body)
    out = {m: False for m in always}
    out.update({m: True for m in win})
    return out


# Signs that a function body leans on the host's Windows APIs.
WINDOWS_CALLS = re.compile(r"\bhost!\s*\(|\bwindows_sys::|\bwindows::|\bhost::function\s*\(")


def fn_body(src: str, name: str) -> str:
    m = re.search(rf"\bfn\s+{re.escape(name)}\s*[(<]", src)
    if not m:
        return ""
    start = src.find("{", m.end())
    depth = 0
    for j in range(start, len(src)):
        if src[j] == "{":
            depth += 1
        elif src[j] == "}":
            depth -= 1
            if depth == 0:
                return src[start:j + 1]
    return ""


def hle_modules(modules_dir: Path) -> list[dict]:
    registered = registered_modules((modules_dir / "mod.rs").read_text(encoding="utf-8"))
    found = []
    for rs in sorted(modules_dir.rglob("*.rs")):
        top = rs.relative_to(modules_dir).parts[0].removesuffix(".rs")
        if top not in registered:
            continue
        src = strip_comments(rs.read_text(encoding="utf-8"))
        file_uses_windows = bool(WINDOWS_CALLS.search(src))
        # Struct literals only: skip the `fn module() -> HleModule {` body itself.
        for m in re.finditer(r"(?<!-> )\bHleModule\s*\{", src):
            body = next(blocks(src[m.start():], "HleModule"))
            dlls_m = re.search(r"dlls\s*:\s*&\[(.*?)\]", body, re.S)
            if not dlls_m:
                continue
            dlls = re.findall(r'"([^"]+)"', dlls_m.group(1))
            name_m = re.search(r'^\s*name\s*:\s*"([^"]+)"', body, re.M)
            exports = {}
            for e in re.findall(r"hle_export!\(\s*(\w+)\s*\)", body):
                b = fn_body(src, e)
                exports[e] = bool(WINDOWS_CALLS.search(b)) if b else file_uses_windows
            for e in re.findall(r'HleExport\s*\{\s*name\s*:\s*"([^"]+)"', body):
                exports.setdefault(e, file_uses_windows)
            found.append({
                "name": name_m.group(1) if name_m else top,
                "file": str(rs.relative_to(modules_dir)),
                "windows_only": registered[top],
                "dlls": dlls,
                "exports": exports,
            })
    return found


def host_allow(host_rs: Path) -> list[str]:
    src = strip_comments(host_rs.read_text(encoding="utf-8"))
    fn = next(blocks(src, "fn default() -> Self"), "")
    arr = re.search(r"let\s+allow\s*=\s*\[(.*?)\]\s*;", fn, re.S)
    if not arr:
        raise SystemExit(f"{host_rs}: could not find HostPolicy's allow list")
    return re.findall(r'"([^"]+)"', arr.group(1))


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--victory", type=Path, required=True, help="path to a VIctory checkout")
    ap.add_argument("--view", choices=sorted(VIEWS), required=True)
    ap.add_argument("--out", type=Path, required=True)
    args = ap.parse_args()

    hle_dir = args.victory / "crates" / "victory-hle" / "src"
    if not (hle_dir / "modules" / "mod.rs").exists():
        raise SystemExit(f"{args.victory}: not a VIctory checkout (no crates/victory-hle)")

    mods = hle_modules(hle_dir / "modules")
    allow = host_allow(hle_dir / "host.rs")
    view = VIEWS[args.view]

    rules: dict[str, str] = {}
    # Host pass-through: globs, so any exact HLE entry below wins over them.
    for p in allow:
        rules[f"{to_module_glob(p)}!*"] = "host" if args.view == "windows" else "todo"
    for m in mods:
        for e, calls_windows in m["exports"].items():
            if args.view == "windows":
                status = "native"
            else:
                status = "stub" if m["windows_only"] or calls_windows else "native"
            for dll in m["dlls"]:
                key = f"{to_module_glob(dll)}!{e.lower()}"
                # The same name may be served by two modules; keep the best.
                if rules.get(key) != "native":
                    rules[key] = status

    out = {
        "_comment": f"Generated by tools/victory_status.py from {args.victory.resolve().name}. "
                    "Do not edit by hand: re-run the script.",
        "tab": view["tab"],
        "title": view["title"],
        "headline": view["headline"],
        "labels": view["labels"],
        "done": view["done"],
        **({"shipped": view["shipped"]} if "shipped" in view else {}),
        "default": "crash",
        "functions": dict(sorted(rules.items())),
        "victory": {
            "hle_modules": [{k: m[k] for k in ("name", "file", "windows_only", "dlls")}
                            | {"exports": len(m["exports"])} for m in mods],
            "host_allow": allow,
        },
    }
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(out, indent=1), encoding="utf-8")
    n_exp = sum(len(m["exports"]) for m in mods)
    n_win = sum(1 for m in mods for w in m["exports"].values() if m["windows_only"] or w)
    print(f"{len(mods)} VIctory modules, {n_exp} implemented functions "
          f"({n_win} tied to Windows), {len(allow)} host DLL patterns -> {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
