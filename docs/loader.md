# nativ64-loader: the native PE loader

`crates/nativ64-loader` is the first chunk of nativ64's own base: it loads a PE32+ (x86-64)
executable without any compatibility layer, and runs its entry point.

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

4. Static TLS (`tls.rs`): the TLS directory is validated at load (every range must be inside the
   image, else `LoadError::BadTls`) and index 0 is written to `AddressOfIndex` before the final
   `mprotect`. Per thread, `ThreadTls` copies the raw data, zero-fills `SizeOfZeroFill` and builds
   the `ThreadLocalStoragePointer` array (slot 0 = block).
5. Fake TEB/PEB (`teb.rs`): one zeroed PEB per process (ImageBaseAddress 0x10, ProcessHeap 0x30:
   filled from nativ64's `GetProcessHeap` export when it exists, else 0) and a 0x2000-byte TEB per
   thread (ExceptionList 0x00 = -1, StackBase 0x08, StackLimit 0x10, Self 0x30, ClientId 0x40/0x48,
   TLS pointer 0x58, PEB 0x60, LastError 0x68), installed with `arch_prctl(ARCH_SET_GS)`. Rust and
   glibc use `fs` on x86-64 Linux, so `gs` is free. `teb::install_for_current_thread(stack_base,
   stack_limit)` is the entry point for any thread that runs guest code.
6. `Image::run_main()` starts a thread with a 64 MiB stack, installs the TEB and TLS with that
   thread's real stack bounds, calls each TLS callback `(base, DLL_PROCESS_ATTACH, 0)`, then calls
   the entry point as `extern "win64" fn() -> u32`; the return value is the result. An image
   with no entry point gives `LoadError::NoEntryPoint`.

`nativ64-run <exe>` maps, binds and runs the image; the entry's return value is the exit code.
`--timeout SECONDS` (default 20, 0 = none) prints `nativ64: timeout` and exits 124. A
SIGSEGV/SIGILL/SIGBUS/SIGFPE in guest code (handled on a sigaltstack, see `fault.rs`) prints the
signal, fault address, RIP and `image+0xOFFSET` when RIP is inside the image, then exits 139. A
trap stub aborts (SIGABRT). `nativ64-run --check <exe>` only maps and binds, printing the base,
entry point, section count and import statistics, plus the first 20 unresolved imports.

## Not done yet

- Structured exception handling (`.pdata` unwind tables, vectored handlers).
- Delay-load imports, ordinal imports (they are bound to a trap stub), forwarded exports.
- Threads created by nativ64 (`CreateThread`) do not get a TEB or TLS yet: they must call
  `teb::install_for_current_thread` and allocate a `ThreadTls` on the new thread.
- Packaging and loading DLLs (guest DLLs, their TLS callbacks, `DLL_THREAD_ATTACH`).
