# nativ64

**Xbox Series games on Linux, natively: no Wine, no Proton, function by function.**

Compatibility layers and recompilers already run Xbox Series code on a Windows PC.
Underneath, they all lean on the same thing: hundreds of Windows APIs borrowed from the
host (`kernel32`, `ntdll`, the C runtime, threads, files, sockets...). nativ64 rewrites
those APIs natively on Linux, as a standalone library that any of these projects can
load through a thin adapter.

The library is not tied to one game: the functions it implements are the same for every
title. Priorities come from the import map, which reads a game's import tables and shows
what is done and what is left. The first title measured is a large open-world game; more
titles can be added to the map, and functions shared by many games come first.

![Linux-native coverage](docs/import-map-linux.svg)

For that first title, 498 system functions are needed from Windows (excluding the 113
its own DLLs ship). 28 already have portable code in
[VIctory](https://github.com/Marvisak/VIctory), the compatibility layer used as the first
base, 38 have VIctory code that still calls Windows, and **430 are host Windows APIs a
native port must write**. This covers the package DLLs; the executable comes next.

## How it fits

```
 compatibility layer (the "base": VIctory today, any other tomorrow)
        │  the title calls CreateFileW, VirtualAlloc, AcquireSRWLockExclusive...
   ┌────▼─────┐
   │ adapter   │  small, the only part tied to one base
   └────┬─────┘
 nativ64: the Windows APIs, reimplemented on Linux (Rust, Windows x64 calling convention)
        │
      Linux (syscalls, futexes, Vulkan, PipeWire...)
```

- **Base-agnostic.** The library knows nothing about the base above it. Switching base
  means writing a new adapter, not redoing the work.
- **Usable from Rust and C/C++.** Functions are exported with the Windows x64 calling
  convention and looked up by `module!function`, from Rust or through a C entry point.
- **Testable without the game.** Each function is checked against documented Windows
  behaviour on its own.
- **Measured.** The import map reads the title's import tables and shows, cell by cell,
  what is done and what is left, in one tab per base and one for this library.

## Status

Early. The import map is done, and the library has a first set of functions with tests.
Next: the biggest areas on the map, C runtime (165 functions), system (136), threads and
sync (84).

| Area | To write |
|---|---|
| C/C++ runtime (`api-ms-win-crt-*`) | 165 |
| System | 136 |
| Threads & sync | 84 |
| Network & crypto | 21 |
| Files | 14 |
| Memory, loader, audio | 10 |

Also needed, outside the library: SEH and TLS support in the base's loader, and a
Direct3D 12 to Vulkan path for the GPU.

## Getting started

Requirements: Linux x86-64, Rust (stable), Python 3 (no dependencies).

```bash
git clone https://github.com/Ybucaille/nativ64.git
cd nativ64
cargo test
```

To draw the import map you need your own, legally obtained copy of a game's files and,
for the base tabs, a VIctory checkout next to this repository. See [`docs/import-map.md`](docs/import-map.md).

## Contributing

The map is the to-do list: pick a grey cell, implement the function in `nativ64` with
its tests, open a pull request. [`CONTRIBUTING.md`](CONTRIBUTING.md) explains how, and
[`docs/library.md`](docs/library.md) covers the ABI rules (calling convention, 32-bit
`LONG`, UTF-16 strings, `SetLastError`).

Good first contributions: C runtime string and math functions, timing and system-info
calls, synchronization primitives on futexes.

## Layout

| Path | What |
|---|---|
| `crates/nativ64/` | The library: Windows APIs on Linux, one module per Windows module |
| `tools/` | Import map scripts and page template |
| `docs/` | Documentation |
| `game/` | Your local game files (not versioned) |
| `reports/` | Generated pages (not versioned: derived from game files) |

## Legal

This project contains no game files, no console system files and nothing extracted from
them, and never will: you supply your own legally obtained copy. It is not affiliated
with Microsoft, any game publisher or any compatibility-layer project.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option,
so any base can use the library, whatever its own license.
