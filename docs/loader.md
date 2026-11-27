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
3. Resolves the import table (names and ordinals) through `nativ64::lookup`, then `nativ64_gpu::lookup` and writes the
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

7. Package DLLs (`image.rs`, `exports.rs`): `LoadOptions::search_dirs` is the list of directories
   searched for the DLLs the image imports (`nativ64-run` passes the exe's directory, then
   `--data DIR` (repeatable) and `NATIV64_PACKAGE_DIRS`). An import module that exists there as a
   file (name compared ignoring case) is mapped with the same loader, recursively, each DLL once
   (cycle-safe: a module still loading binds to nativ64), dependencies first. Its export
   directory is parsed (names, ordinals, forwarders `mod.func` / `mod.#n` resolved to another
   package DLL, else to nativ64; an unresolvable one is left out, so importers get a trap stub)
   with every array bounds-checked (`LoadError::BadExports`; a failing DLL is `LoadError::Dll`).
   An import binds to the shipped DLL's export first, then `nativ64::lookup` /
   `nativ64_gpu::lookup`, then a trap stub. `LoadOptions::native_modules` (env
   `NATIV64_NATIVE_MODULES`, comma-separated names) forces nativ64 for those modules. Each
   image with static TLS gets its own index (main image 0, DLLs 1, 2...; at most 64 images).
   `run_main` registers the DLLs with `nativ64::process::register_module` (so `GetModuleHandle*`
   returns the real base and `GetProcAddress` resolves their exports, by name or ordinal), then,
   on the guest thread with the TEB installed and before the exe's own TLS callbacks, runs for
   each DLL in dependency order its TLS callbacks and `DllMain(base, DLL_PROCESS_ATTACH, 0)`; a
   FALSE return is `LoadError::DllInitFailed`. `--trace-imports` also covers the DLLs' imports.

8. Guest threads (`threads.rs`): `run_main` registers thread hooks with nativ64
   (`nativ64::process::set_thread_hooks`) and the exe's `SizeOfStackReserve`. A thread made by
   `CreateThread` / `_beginthreadex` runs `attach` before its start routine: signal stack, TEB
   (`install_for_current_thread` with the real stack bounds), a `ThreadTls` for every image, then
   for each image in load order (DLLs, then the exe) its TLS callbacks and, for a DLL,
   `DllMain(base, DLL_THREAD_ATTACH, 0)` (skipped when the DLL called `DisableThreadLibraryCalls`,
   recorded in `nativ64::process`; TLS callbacks still run). After the routine returns, or the
   thread ended with `_endthreadex` (it jumps back to the same spot), `detach` runs
   `DLL_THREAD_DETACH` in reverse order, then frees the TLS blocks and the TEB. Without hooks
   (nativ64 alone) threads run as before. Stack size (`process::thread_stack_size`): the
   `stack` argument when `STACK_SIZE_PARAM_IS_A_RESERVATION` is set, otherwise the larger of it
   and the exe's `SizeOfStackReserve` (so 0 gives the default); at least 1 MiB, at most 1 GiB.

`nativ64-run <exe>` maps, binds and runs the image; the entry's return value is the exit code.
`--timeout SECONDS` (default 20, 0 = none) prints `nativ64: timeout` and exits 124. A
SIGSEGV/SIGILL/SIGBUS/SIGFPE/SIGTRAP (an `int3`) in guest code (handled on a sigaltstack, see `fault.rs`) prints the
signal, fault address, RIP and `image+0xOFFSET` when RIP is inside the image, then exits 139. In guest code (and `NATIV64_NO_SEH` unset) the fault first becomes a Windows exception
(docs/seh.md); only an unhandled one prints that report. A
trap stub aborts (SIGABRT). `nativ64-run --check <exe>` only maps and binds, printing the base,
entry point, section count and import statistics, the first 20 unresolved imports, the loaded
package DLLs (base, path, import and unresolved counts) and the unresolved imports per module
for the exe and each DLL.
`--trace-imports` (`LoadOptions::trace_imports`) routes every resolved import through a
trampoline that logs `nativ64: call module!name` on entry (argument registers, xmm0-3 and stack
arguments are preserved; the return value is not logged). Off by default. Imports by ordinal
resolve through `nativ64::lookup_ordinal` (only `ws2_32` has a table).

## CPU instructions the host may lack

The console CPU is an AMD Zen 2; the host may not be. `clzero` (zero the 64-byte line at `rax`) traps
as SIGILL on other CPUs, so the fault handler in `fault.rs` emulates it and resumes. The two loops
compilers build around it (`clzero; add rax,0x40; cmp rax,b; jb`, and `mov rax,a; add a,0x40; clzero;
cmp a,b; jb`) are done whole in one trap, leaving registers and flags as their last pass would.
The same handler emulates SSE4a `extrq` / `insertq` (register forms, immediate or fields in a register) on the
saved XMM registers; the title uses them unconditionally. `monitorx` / `mwaitx` are accepted as no-ops.
`clwb` (`66 0f ae /6`), `clflushopt` (`66 0f ae /7`) and `clflush` (`0f ae /7`) are cache write-back
hints with nothing a program can observe on coherent x86 caches, so they are skipped like no-ops
(the title uses them before handing CPU-written memory to the GPU, which reads the same memory).
A whole buffer-flush loop (`clwb [a]; add a,0x40; cmp a,b; jbe top`) is recognised and its
arithmetic and flags are applied in one trap: skipping it per line would cost a signal per
64 bytes, and the title flushes resources megabytes at a time.
Other Zen-only instructions are reported as faults, not emulated, until a title needs them.

## Not done yet

- Structured exception handling is done (docs/seh.md); guard-page faults and stack overflow
  (`0xC00000FD`) are not raised, and the shipped vcruntime C++ exceptions were not run on the
  real title yet.
- Delay-load imports. Ordinal imports resolve only against a package DLL (nativ64 has an
  ordinal table for `ws2_32` only).
- `LoadLibrary*` of a package DLL that was not an import: only the DLLs mapped at start are
  found (run-time loading would need `DllMain`, TLS slots on every live thread and a loader
  hook).
- Package DLLs: `DLL_PROCESS_DETACH`, `FreeLibrary` reference
  counts, forwarder cycles between DLLs (the module still loading binds to nativ64), the
  `image+0xOFF` fault report for DLL addresses.
- `ExitThread` is not implemented (only `_endthreadex`); each guest thread leaks its 64 KiB
  signal stack.
