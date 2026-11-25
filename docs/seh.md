# Structured exception handling (SEH) and C++ exceptions

Status: chunks 1-3 implemented (2026-11-26). Chunks at the end; each is one Sonnet agent.

## Role

Guest code raises and catches exceptions the Windows x64 way: C++ `throw`/`catch`
(`_CxxThrowException` → `RaiseException`), `__try/__except/__finally`
(`__C_specific_handler`), hardware faults turned into exceptions, and code that walks its own
stack (`RtlCaptureContext` + `RtlLookupFunctionEntry` + `RtlVirtualUnwind`, used by
crash reporters and by the shipped CRT). Today none of it exists: `RaiseException` only knows
the thread-name request, vectored handlers are stored, never called.

## What the images import (our title and its package DLLs)

- Every image: `__C_specific_handler`, `RtlCaptureContext`, `RtlLookupFunctionEntry`,
  `RtlVirtualUnwind`, `UnhandledExceptionFilter`; most: `__CxxFrameHandler3/4`,
  `_CxxThrowException`, `__std_terminate`.
- The shipped `vcruntime140.dll` / `vcruntime140_1.dll` (genuine Microsoft builds) export the
  C++ side (`__CxxFrameHandler*`, `_CxxThrowException`, `__C_specific_handler`) and import only
  `RaiseException`, `RtlUnwindEx`, `RtlUnwind`, `RtlVirtualUnwind`, `RtlLookupFunctionEntry`,
  `RtlCaptureContext`, `RtlPcToFileHeader`, `UnhandledExceptionFilter`. The loader binds a
  shipped export first, so **C++ exceptions come for free once the ntdll level below exists**.
  We write the ntdll level, plus our own `__C_specific_handler` (a small, documented format)
  as the fallback for images bound to nativ64.

## Facts (public x64 ABI, Microsoft "x64 exception handling" documentation)

- `.pdata` = data directory 3: sorted `RUNTIME_FUNCTION { u32 begin, end, unwind_info }`
  (RVAs). A function without an entry is a leaf: `rip = [rsp]`, `rsp += 8`.
- `UNWIND_INFO`: `u8 version:3 | flags:5` (`EHANDLER` 1, `UHANDLER` 2, `CHAININFO` 4),
  `u8 prolog_size`, `u8 code_count`, `u8 frame_reg:4 | frame_offset:4` (offset × 16), then
  `code_count` u16 codes padded to an even count, then either `u32 handler RVA` + language
  data (EHANDLER/UHANDLER) or a chained `RUNTIME_FUNCTION` (CHAININFO). Version 1 and 2
  (version 2 adds `UWOP_EPILOG` codes, which unwinding skips).
- Codes `{ u8 prolog_offset, u8 op:4 | info:4 }`: `PUSH_NONVOL` 0, `ALLOC_LARGE` 1 (info 0:
  next slot × 8; info 1: next two slots as u32), `ALLOC_SMALL` 2 (info × 8 + 8), `SET_FPREG` 3
  (`rsp = frame_reg - frame_offset × 16`), `SAVE_NONVOL` 4 (next slot × 8), `SAVE_NONVOL_FAR`
  5 (next two slots), `EPILOG` 6, `SAVE_XMM128` 8 (next slot × 16), `SAVE_XMM128_FAR` 9,
  `PUSH_MACHFRAME` 10 (info 1: error code first). Codes whose prolog offset is beyond
  `rip - begin` are skipped (the prolog had not run them yet).
- Epilog: when `rip` is in an epilog (`add rsp, imm` / `lea rsp, [frame + imm]`, `pop` of
  non-volatile registers, then `ret`, `jmp` to outside the function or `rex.w jmp [mem]`),
  the unwind emulates the remaining epilog instructions instead of the codes.
- `CONTEXT` (0x4d0 bytes, 16-aligned): `ContextFlags` 0x30, `MxCsr` 0x34, `SegCs` 0x38,
  `EFlags` 0x44, `Rax` 0x78, `Rcx` 0x80, `Rdx` 0x88, `Rbx` 0x90, `Rsp` 0x98, `Rbp` 0xa0,
  `Rsi` 0xa8, `Rdi` 0xb0, `R8..R15` 0xb8..0xf0, `Rip` 0xf8, `FltSave` (FXSAVE area, 512
  bytes) 0x100, so `Xmm0..15` at 0x1a0. `KNONVOLATILE_CONTEXT_POINTERS`: 16 XMM pointers
  then 16 integer pointers (rax..r15 order).
- `EXCEPTION_RECORD` (0x98): `u32 code, u32 flags, *record, *address, u32 nparams, pad,
  u64 info[15]`. Flags: `NONCONTINUABLE` 1, `UNWINDING` 2, `EXIT_UNWIND` 4, `NESTED_CALL`
  0x10, `TARGET_UNWIND` 0x20, `COLLIDED_UNWIND` 0x40.
- `DISPATCHER_CONTEXT` (0x50): `ControlPc, ImageBase, FunctionEntry, EstablisherFrame,
  TargetIp, ContextRecord, LanguageHandler, HandlerData, HistoryTable` (8 bytes each),
  `u32 ScopeIndex, u32 Fill0`.
- Handler: `EXCEPTION_DISPOSITION handler(record, establisher_frame, context, dispatcher)`:
  `ContinueExecution` 0, `ContinueSearch` 1, `NestedException` 2, `CollidedUnwind` 3.
  The establisher frame is the frame's `rsp` after its prolog (or `frame_reg -
  offset` with a frame pointer).
- `__C_specific_handler` language data: `u32 count`, then `count × { u32 begin, end, handler,
  target }` (RVAs). `target == 0`: a `__finally` (`handler(abnormal = 1, frame)` during
  unwind); else `handler == 1` means `EXCEPTION_EXECUTE_HANDLER`, otherwise a filter
  `filter(EXCEPTION_POINTERS *, frame)` returning 1 (execute), 0 (search), -1 (continue).

## Design

### Module `nativ64::seh` (rtlsupport + errorhandling exports)

1. **Function tables.** On `register_image_range`, read the image's exception directory and
   keep `(base, size, *RUNTIME_FUNCTION, count)`. `RtlAddFunctionTable` /
   `RtlDeleteFunctionTable` add dynamic tables (tests use them for generated code).
   `RtlLookupFunctionEntry(pc, *base, history)`: binary search; history ignored.
2. **`RtlVirtualUnwind(type, base, pc, entry, ctx, *handler_data, *establisher, ptrs)`** as
   in the facts: returns the handler (EHANDLER for type 1, UHANDLER for type 2, when
   `pc - begin` is past the prolog), follows chained info, fills context pointers.
3. **`RtlCaptureContext(ctx)`**: naked asm, the caller's registers, `Rip` = return address,
   `Rsp` = caller's rsp after return; `ContextFlags = CONTROL|INTEGER|FLOATING_POINT`.
   **`RtlRestoreContext(ctx, record)`**: naked asm, loads every register and jumps to
   `Rip` on `Rsp`.
4. **Dispatch** (`dispatch(record, ctx) -> bool`): vectored handlers in order (`PVECTORED_
   EXCEPTION_HANDLER(EXCEPTION_POINTERS *)`, -1 = continue execution, 0 = search); then walk
   frames from `ctx` with `RtlVirtualUnwind(UNW_FLAG_EHANDLER)`, calling each handler with a
   `DISPATCHER_CONTEXT`. ContinueExecution → restore the (possibly changed) context;
   NestedException / collided unwind as Windows; a frame outside every image, or the stack
   limit from the TEB, ends the walk. Nothing handled → `UnhandledExceptionFilter`, which
   calls the title's top-level filter (`SetUnhandledExceptionFilter`), then reports and stops
   like a fault (`fatal exception 0x… at image+0x…`).
5. **`RaiseException(code, flags, n, args)`**: builds the record (address = return
   address), captures the caller's context, dispatches; returns when a handler continues
   execution (`NONCONTINUABLE` → `STATUS_NONCONTINUABLE_EXCEPTION`). The thread-name request
   keeps its fast path only when no handler takes it (dispatch first, as on Windows).
6. **`RtlUnwindEx(target_frame, target_ip, record, retval, ctx, history)`** and `RtlUnwind`:
   second pass, frame by frame, calling UHANDLER handlers with `UNWINDING` (+
   `TARGET_UNWIND` on the target frame, `EXIT_UNWIND` when `target_frame` is NULL); then
   `Rax = retval`, `Rip = target_ip`, restore.
7. **`__C_specific_handler`** (ours, fallback): scope table walk for both passes, with
   `ScopeIndex` resumption for nested unwinds.
8. **Faults → exceptions** (loader `fault.rs`): SIGSEGV/SIGBUS → `0xC0000005` (info[0] = 0
   read, 1 write, 8 execute; info[1] = address), SIGILL → `0xC000001D`, SIGFPE divide →
   `0xC0000094`, SIGTRAP after `int3` → `0x80000003` (Rip moved back to the `int3`). The
   signal handler copies `CONTEXT` + record onto the **guest stack** (below the faulting
   `rsp`, 16-aligned, plus home space), points `rip` at a dispatcher trampoline and returns
   from the signal; the dispatch then runs as ordinary guest-thread code (gs and TEB as
   usual). The dispatcher never returns: it restores a context or stops. Faults inside
   nativ64 code (not in a guest image) keep today's report. `NATIV64_NO_SEH=1` keeps the
   plain report, for debugging.

### Implementation notes (chunk 2)

- `RaiseException`, `RtlUnwindEx`, `RtlUnwind` are assembly entries (`context_entry!` in
  `seh.rs`) that capture the caller's context on their own frame, then run Rust and resume
  through `RtlRestoreContext`.
- Our dispatcher frames have no unwind info. A second pass whose walk leaves the images
  (called from our `__C_specific_handler`, or from a shipped handler that our dispatcher
  called) continues from the context of the innermost exception being dispatched (`ACTIVE`,
  per thread); the entries a landing abandons are dropped.
- A handler returning `ContinueExecution` for a non-continuable exception raises
  `STATUS_NONCONTINUABLE_EXCEPTION` (non-continuable, same context); any other bad value raises
  `STATUS_INVALID_DISPOSITION`. A non-continuable escalation that is continued again stops.
- In test builds the report-and-stop step records `(code, address)` (`take_stopped`) and
  returns, so a raise returns to its caller after an unhandled exception.

### Implementation notes (chunk 3)

- `fault.rs::on_fault` keeps the clzero emulation first, then `classify` maps the signal (error
  code from `uc_mcontext` gives read/write/execute) and, for a `rip` in a guest image or DLL
  (`dispatchable`), `divert_to_dispatch` writes `CONTEXT` + `EXCEPTION_RECORD` 128 bytes
  (red zone) below the faulting `rsp`, with home space and a null return address, and sets
  `rip = nativ64::seh::dispatch_fault(rec, ctx)` (rcx, rdx) before the signal returns. The
  `int3` record and context carry the `int3` address (`Rip` moved back by 1).
- The loader installs `report_unhandled` as the fatal hook: an exception nobody handled prints
  `fatal exception 0x... at address ..., rip ...` plus `image+0xOFF` / `dll+0xOFF`, exit 139.
- Not dispatched (plain report): host-code faults, `NATIV64_NO_SEH=1`, a stack whose pages below
  the fault are not mapped (`mincore` check), signals without counterpart (single-step).

### Shortcuts

No `__CxxFrameHandler`/`_CxxThrowException` of our own (the shipped vcruntime ones);
history table unused; guard-page exceptions and stack overflow (`0xC00000FD`) not raised;
SIGFPE other than integer overflow is reported as divide by zero (no x87/SSE exception codes); no stack-limit check from the TEB (the walks stop outside the images, when the stack does not
climb, or after 100000 frames); `NestedException` only sets `NESTED_CALL` (the handler's own
record is not chained); the `RtlUnwindEx` context argument is only scratch, the unwind runs on
its own capture; exit unwind (`target_frame` NULL) resumes at `target_ip` in the last context
reached; x87 state not saved beyond FXSAVE; `RtlUnwind` on the ARM-style `UNWIND_HISTORY_TABLE` cache
never filled.

## Chunks

1. Function tables, `RtlLookupFunctionEntry`, `RtlVirtualUnwind`, `RtlCaptureContext`,
   `RtlRestoreContext`, `RtlAddFunctionTable`/`RtlDeleteFunctionTable`. Tests: machine code
   generated into executable memory with hand-written unwind info (every op, chained info,
   epilog, frame pointer, leaf), registered with `RtlAddFunctionTable`, unwound from known
   `rip` values; capture/restore round trip.
2. Dispatch, `RaiseException`, `RtlUnwindEx`/`RtlUnwind`, vectored handlers,
   `UnhandledExceptionFilter`, `__C_specific_handler`. Tests: generated functions with scope
   tables (`__try/__except` filter values, `__finally` on unwind, nested), vectored handler
   continuing execution.
3. Faults → exceptions in the loader. Test: a guest-like function touching NULL under an
   `__except` resumes at its handler. Then real run: the title and package DLLs' C++
   exceptions go through the shipped vcruntime (check `--trace-imports`).
