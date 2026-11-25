# Import map

A coverage map of every system function the game calls, to track a Linux-native port
(no Wine) function by function.

## How it works

`tools/import_map.py` reads the import tables (normal and delay-load) of the game's
executable and DLLs, without running them, and lists every `module!function` they import.
Each function gets a status:

| Status | Meaning |
|---|---|
| 🟩 native | Implemented natively on Linux by us |
| 🟢 host | Provided by the host OS: works, but is not our code (like shipped, never counted as done unless a view says so) |
| 🟨 stub | Exists but does nothing useful yet |
| 🟥 crash | Called and failing (seen in logs) |
| ⬜ todo | Not started (default) |
| 🟦 shipped | Provided by a DLL shipped with the game (e.g. `msvcp140.dll`), so it runs as game code and needs no reimplementation |

Functions are grouped by area (Files, Threads & sync, Memory, Graphics, Xbox services...)
and drawn as one cell each. The headline percentage counts native functions out of
everything that is not shipped.

## Usage

```bash
python -I tools/import_map.py game/*.dll --out reports/import-map.html --json reports/import-map.json
```

Once the executable is available, add it:

```bash
python -I tools/import_map.py game/<title>.exe game/*.dll --out reports/import-map.html
```

`reports/` is not versioned: it lists the game's imports, which is derived from game files.

## Updating statuses

Edit `tools/import-status.json`:

```json
{
  "default": "todo",
  "functions": {
    "api-ms-win-core-synch!*": "stub",
    "api-ms-win-core-synch!EnterCriticalSection": "native",
    "d3d12_xs!*": "todo"
  }
}
```

Module names are normalized: lowercase, no `.dll`, and API-set version suffixes dropped
(`api-ms-win-core-file-l1-2-0.dll` becomes `api-ms-win-core-file`).

A status file may also set its `tab` name, the page's `title`, the `headline` under the
percentage, the `labels` of each status, and which statuses count as `done` for the
percentage.

`"shipped"` sets the precedence of shipped DLLs: `"first"` (default) marks a function
shipped before any rule is looked at; `"fallback"` applies the rules first (exact or glob)
and marks it shipped only when no rule matches, before the `default`. Use `"fallback"`
when the runtime prefers something else to the game's own DLLs.

## Tabs

Pass `--status` several times to get one tab per status file in a single page. Each tab
has its own percentage, labels and filters; the URL keeps the selected tab
(`import-map.html#Linux%20native`).

The page layout lives in `tools/import_map.html` (the script fills in the data).

## VIctory views

`tools/victory_status.py` reads a VIctory checkout (it never builds or runs it) and writes
a status file from how VIctory resolves each import:

1. its own implementation (`crates/victory-hle/src/modules`, registered in `all()`);
2. the host's Windows DLL, if `HostPolicy` allows it (`crates/victory-hle/src/host.rs`);
3. the DLLs shipped with the game;
4. nothing: the call traps.

Host pass-through beats shipped DLLs (`msvcp140*` and `vcruntime140*` are on VIctory's
allow list), so the Windows view sets `"shipped": "fallback"`.

Two views, shown as two tabs of the same page:

| Tab | 🟩 | 🟢 | 🟨 | ⬜ | 🟥 |
|---|---|---|---|---|---|
| **Windows · VIctory** (`--view windows`): what VIctory itself implements | Implemented by VIctory | Borrowed from host Windows | — | — | Unresolved (traps) |
| **Linux native** (`--view linux`): what a native port still needs | Portable VIctory code | — | VIctory code tied to Windows | Host Windows API to write | Unresolved even on Windows |

"Tied to Windows" means the module is registered on Windows only, or the function's body
calls Windows (`host!`, `windows_sys::`, `windows::`). This is a source heuristic, not a
build check.

```bash
# point --victory at your VIctory mirror checkout
python -I tools/victory_status.py --victory ../VIctory-mirror --view windows --out reports/status-victory-windows.json
python -I tools/victory_status.py --victory ../VIctory-mirror --view linux   --out reports/status-victory-linux.json
python -I tools/import_map.py game/*.dll \
    --status reports/status-victory-windows.json \
    --status reports/status-victory-linux.json \
    --out reports/import-map.html
```

Run it after each mirror sync to follow VIctory's progress. On the package DLLs alone
(VIctory `9aef3d9`, 2026-11-24), 611 imported functions:

- Windows view: 66 implemented by VIctory (10.8%, the headline), 543 borrowed from host
  Windows (host), 2 unresolved (`oleaut32` #4 and #6, they trap), 0 shipped. The percentage
  counts only VIctory's own code. It is not "the game runs": the borrowed functions
  work only if the host Windows really exports them, and the 2 unresolved ones crash
  if called.
- Linux view: 28 portable, 38 tied to Windows, 430 host APIs to write, 2 unresolved,
  113 shipped (28 of 498 = 5.6%).

## Library view

The `vn-win32` crate (`crates/vn-win32`, see `docs/library.md`) registers every function it
implements. Its `vn-win32-status` binary turns that registry into a status file, so a third
tab shows how much of the title's import list the library covers, whatever base loads the game:

```bash
cargo run -q -p vn-win32 --bin vn-win32-status > reports/status-vn-win32.json
python -I tools/import_map.py game/*.dll \
    --status reports/status-victory-windows.json \
    --status reports/status-victory-linux.json \
    --status reports/status-vn-win32.json \
    --out reports/import-map.html
```

Labels mirror the Linux view (`native`, `stub`, `todo`, shipped); only `native` counts for the
percentage. Functions the library registers but the game does not import are ignored.

## Limits

- Only static imports are seen. Functions the game looks up at runtime with
  `GetProcAddress` need a runtime log (a later step: count calls while the game runs,
  which also lets us weight cells by how often they are called).
- The map covers system APIs. Direct3D 12 methods and GPU shader instructions are
  separate maps.
- Host exports are not verified. VIctory resolves them with `GetProcAddress` at run time,
  so the Windows view counts every function of an allowed host DLL as borrowed without
  checking it exists. A borrowed function could still trap if the host does not export it.

## README chart

`--svg PATH` also writes a static SVG card of one view (`--svg-view NAME`, default the last tab;
`--svg-note TEXT` goes in the footer, e.g. the VIctory commit). It is embedded in `README.md`:

    python -I tools/import_map.py game/*.dll --status reports/status-victory-windows.json \
        --status reports/status-victory-linux.json --svg docs/import-map-linux.svg \
        --svg-view "Linux native" --svg-note "VIctory $(git -C ../VIctory-mirror rev-parse --short HEAD)"
