# vn-win32: the host API library

`crates/vn-win32` reimplements, natively on Linux x86_64, the Windows host APIs the title's
binaries import (kernel32, `api-ms-win-core-*`, `api-ms-win-crt-*`, ...). It does not depend on
the compatibility layer ("base") that loads the game: VIctory today, possibly others later.

## Layout

- One Rust module per Windows module, named like `tools/import_map.py` normalizes it
  (lowercase, no `.dll`, API-set version suffix dropped, `-` becomes `_`):
  `api-ms-win-core-synch` -> `src/api_ms_win_core_synch.rs`.
- `src/registry.rs`: `Export { module, name, addr, status }`, `Status::{Native, Stub}`,
  `lookup(module, name)`, `normalize_module`, and the C function `vn_win32_lookup`.
- `src/lib.rs`: `TABLES` lists each module's `EXPORTS`; `exports()` iterates them all.
- `src/bin/vn-win32-status.rs`: prints the status file for the import map
  (see "Library view" in `import-map.md`).

## Adding a function

Declare it in the module file with the `win32!` macro; the declaration is also the
registration. The macro registers each function under the API-set name and under its
`kernel32` forwarder.

```rust
win32! {
    module "api-ms-win-core-synch" => "kernel32";
    Native fn Sleep(milliseconds: u32) { /* ... */ }
}
```

Use `Stub` for a function that exists but does nothing useful yet. For a new module, create
the file and add its `EXPORTS` to `TABLES`. Write a unit test that calls through the win64
function. Then check the new tab of the map.

## ABI rules

- Every export is `unsafe extern "win64"`: the Windows x64 convention, whatever the host's.
- Use Windows sizes: `DWORD` = `u32`, `BOOL` = `i32`, `LONG` = `i32` (never `i64`; Linux
  `long` is 64-bit), `LONGLONG` = `i64`, `HANDLE` = pointer-sized.
- Strings are UTF-16 (`u16`) for `W` functions; `A` functions take the ANSI code page.
- Failures return the documented value and set the error with `set_last_error` (read back by
  `GetLastError`, thread-local), e.g. `ERROR_NOACCESS` (998) for a bad pointer.
- Never unwind across the boundary: no panics in exports.

## Base adapters

A base plugs in through a thin adapter. It must:

1. For each import `module!function` (or ordinal) of the loaded binaries, call
   `vn_win32_lookup(module, name)` (C) or `vn_win32::lookup` (Rust). Module names may be given
   as in the import table; the library normalizes them.
2. Patch the returned address into the import thunk. NULL means "not implemented here":
   the base falls back to its own implementation or traps.
3. Call the functions with the win64 convention (on Linux, the base's thunk or JIT must
   handle the switch from System V).
4. Own what the library does not: loading PE images, TLS setup, and anything outside the
   Windows host APIs (Direct3D, GPU).
