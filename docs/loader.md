# nativ64-loader: the native PE loader

`crates/nativ64-loader` is the first chunk of nativ64's own base: it loads a PE32+ (x86-64)
executable without any compatibility layer. It does not run guest code yet.

## What it does

1. Parses the DOS/NT headers, sections and data directories, with bounds checks. Errors are
   `LoadError` values: bad magic, PE32 instead of PE32+, truncated file, section out of bounds.
2. Maps the image with `mmap`: the preferred ImageBase with `MAP_FIXED_NOREPLACE`, else any
   address. Copies headers and sections (the tail up to VirtualSize is zero), applies base
   relocations (`DIR64` and `ABSOLUTE`; anything else is an error) when it moved, then sets
   each section's protection (R/W/X) with `mprotect`.
3. Resolves the import table (names and ordinals) through `nativ64::lookup` and writes the
   address into the IAT. An unknown import gets the address of a per-import trap stub that
   prints `nativ64: unimplemented import MODULE!NAME` and aborts. The list of unresolved
   imports is kept in `Image::imports`.

`nativ64-run --check <exe>` maps and binds an image and prints the base, entry point, section
count and import statistics, plus the first 20 unresolved imports. Without `--check` it
prints "execution not implemented yet" and exits with 2.

## Not done yet

- A fake TEB/PEB reachable through `gs` (`arch_prctl(ARCH_SET_GS)`).
- TLS (directory, callbacks, slots).
- Structured exception handling (`.pdata` unwind tables, vectored handlers).
- Delay-load imports, forwarded exports, loading DLLs.
- The win64 call into the entry point (stack setup, SysV to win64 switch).
