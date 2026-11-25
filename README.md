# vice-native

**A Linux-native runtime for an Xbox Series title: no Wine, no Proton, function by function.**

Compatibility layers already run the title's own x86-64 code on a Windows PC. Underneath,
they all lean on the same thing: hundreds of Windows APIs borrowed from the host
(`kernel32`, `ntdll`, the C runtime, threads, files, sockets...). vice-native rewrites
those APIs natively on Linux, as a standalone library that any of these layers can load
through a thin adapter.

![Linux-native coverage](docs/import-map-linux.svg)

Of the 498 system functions the title needs from Windows (excluding the 113 its own DLLs
ship), 28 already have portable code in [VIctory](https://github.com/Marvisak/VIctory),
38 have VIctory code that still calls Windows, and **430 are host Windows APIs a native
port must write**. This covers the package DLLs; the executable comes next.

## How it fits

```
 compatibility layer (the "base": VIctory today, any other tomorrow)
        │  the title calls CreateFileW, VirtualAlloc, AcquireSRWLockExclusive...
   ┌────▼─────┐
   │ adapter   │  small, the only part tied to one base
   └────┬─────┘
 vn-win32: the Windows APIs, reimplemented on Linux (Rust, Windows x64 calling convention)
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
git clone https://github.com/Ybucaille/vice-native.git
cd vice-native
cargo test
```

To draw the import map you need your own, legally obtained copy of the title's files
and a VIctory checkout next to this repository. See [`docs/import-map.md`](docs/import-map.md).

## Contributing

The map is the to-do list: pick a grey cell, implement the function in `vn-win32` with
its tests, open a pull request. [`CONTRIBUTING.md`](CONTRIBUTING.md) explains how, and
[`docs/library.md`](docs/library.md) covers the ABI rules (calling convention, 32-bit
`LONG`, UTF-16 strings, `SetLastError`).

Good first contributions: C runtime string and math functions, timing and system-info
calls, synchronization primitives on futexes.

## Layout

| Path | What |
|---|---|
| `crates/vn-win32/` | The library: Windows APIs on Linux, one module per Windows module |
| `tools/` | Import map scripts and page template |
| `docs/` | Documentation |
| `game/` | Your local game files (not versioned) |
| `reports/` | Generated pages (not versioned: derived from game files) |

## Legal

This project contains no game files, no console system files and nothing extracted from
them, and never will: you supply your own legally obtained copy. It is not affiliated
with Microsoft, the title's publisher or any compatibility-layer project.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option,
so any base can use the library, whatever its own license.
