# nativ64-gpu: the shader translator (G4)

The title ships its shaders compiled for the console GPU: RDNA2 (GFX10) machine code inside
a DXBC container. nativ64 has no DXIL to hand to a driver and no D3D12 to run it on, so each
program is translated at first use: GFX10 → our SSA IR → structured per-lane form → SPIR-V,
then a Vulkan pipeline. This file is the design; `docs/gpu.md` (Shaders bullet, G1
deviations, G4) places it in the GPU work.

Fact tags: **[seen]** checked on the title's own blobs (how is said once, in Sources);
**[llvm]** checked with `llvm-mc` (LLVM 23.1.5, `-mcpu=gfx1030` and `gfx1013` decode these blobs
identically); **[VIctory]** read in VIctory's `victory-shader` (GPL-3, facts only), not checked
here; **[spec]** from AMD/Khronos documents, not checked here; **[unverified]** a guess to
confirm.

## Sources

- **Blobs.** The executable embeds 10 DXBC containers (1 VS, 9 PS); the rest live in
  `shaders.rpf` (an `RPF9` archive, compressed or encrypted: no `DXBC` bytes visible), so they
  are only seen at pipeline creation. `pipeline.rs` keeps each stage's bytes (`Stage.bytes`)
  and the input layout. Chunk 1 adds a dump of every created pipeline's blobs; until then the
  10 embedded blobs are the corpus. Extraction was a throwaway script (scan for `DXBC`, check
  size and part table), never committed; game bytes never enter the repository or tests.
- **ISA.** AMD, *"RDNA 2" Instruction Set Architecture Reference Guide* (2020), public at
  `https://www.amd.com/content/dam/amd/en/documents/radeon-tech-docs/instruction-set-architectures/rdna2-shader-instruction-set-architecture.pdf`
  (the URL answers a plain `curl` with an empty page: read it in a browser). Machine-checkable
  oracle: `llvm-mc -arch=amdgcn -mcpu=gfx1030 -disassemble` (installed here, Apache-2.0 with
  LLVM exception; run as a tool, never linked). LLVM's `AMDGPUAsmGFX1030.rst` lists every form.
  Xbox2Windows calls the target gfx1013 **[unverified]**. The 10 embedded blobs decode the same
  under both LLVM targets, but a title run's 8634 blobs do not: they use `v_mad_f32`, `v_mac_f32`,
  `v_madak_f32` (gfx10.1 opcodes that gfx1030 lacks), and the same opcode can have another name
  (VOP3 0x140: `v_mad_legacy_f32` on gfx1013, `v_fma_legacy_f32` on gfx1030). **[llvm, seen]**
  The decoder's oracle is therefore `-mcpu=gfx1013`.
- **VIctory** (`../VIctory-mirror/crates/victory-shader`, GPL-3, targets DXIL): container
  offsets, stage data, root layout tables, the per-lane structuring argument. Facts only: no
  code, no module split, no names copied.
- **XCL** (`../xcl-mirror`, GPL-2, targets HLSL → DXIL): `docs/wave64-on-wave32.md` on running
  wave64 programs on 32-lane GPUs. Facts only.
- **Xbox2Windows** (`R0Y00/Xbox2Windows`): LICENSE is MIT (**[seen]**: raw LICENSE file, and
  the GitHub API reports `spdx_id: MIT`), so its code may be reused with its notice in
  `THIRD_PARTY.md`. It targets DXIL from Python/C++ tooling; it offers instruction-form
  coverage tables (`docs/coverage/rdna2.tsv`, `gfx1013.tsv`: 1968 forms, generated from LLVM's
  GFX10 asm docs), HLSL helpers (`wave64_emulate.hlsli`, `rgb9e5.hlsli`,
  `ffx_float_to_int.hlsli`) and hardware-measured fixtures (`tests/data/rdna_cvt_pk_u8_measured.tsv`,
  per-instruction `.s` tests). Use: its coverage table as our checklist, its measured data
  as test vectors (with notice). Its translator is HLSL-shaped; we keep our own.

## Container

```text
DXBC  +0x00 "DXBC", +0x04 digest[16], +0x14 u16 1, u16 0, +0x18 total size, +0x1c part count,
      +0x20 u32 part offsets; part = fourcc, u32 size, data                       [seen]
parts SFI0 ISG1 OSG1 PSV0 HASH DXIL PRIV, in that order on all 10 blobs            [seen]
DXIL  +0x00 u32 program version: low byte 0x66 (6.6), high half kind (0 PS, 1 VS)  [seen]
      +0x04 u32 size in dwords, +0x08 "DXIL", +0x0c u32 0x106, +0x10 bitcode offset, +0x14 size
      +0x18 "XDAT", u32 entry count (3), u32 0x24, then entries {u32 kind, u32 offset, u32 size}
            (offsets relative to the DXIL part data)                              [seen]
kind 0x12 16 bytes (hash?); kind 0x07 28-byte stub (stripped bitcode)             [seen]
kind 0x0d "XBCH", u32 version 3, u64 packed, u64 unpacked, then a zlib stream     [seen]
unpacked  "FSSM", u32 0x101, u64 payload size, payload                            [seen]
payload   +0x00 u64 6, +0x08..0x38 record offsets LS HS ES GS VS PS (absent:
          0xDEADBEEF_DEADBEEF), mesh at +0x48, compute at +0x60                    [seen VS/PS]
record    +0x00 u64 code offset, +0x08 u64 code size (payload-relative)            [seen]
          +0x14 u32 flags, bit 5 wave32 (clear on all 10: wave64)                  [VIctory]
          +0x28 u64 root layout table, +0x30 u64 used-resource table, +0x38/+0x3c counts [seen]
          +0x4c u32 LDS bytes, +0x50 u32 bits 0-9 VGPRs, bits 20-27 SGPRs          [seen]
layout    20 B: u32 root type, u32 table offset (dwords), u32 user SGPR count,
          u32 base register, u8 first user-data dword, u8 root slot, u16 0         [seen]
used      20 B: u8 kind (0 SRV, 1 sampler, 2 CBV, 3 UAV), u8 root type, u16 0,
          u32 table offset, u32 lower bound, u32 stride, u8 descriptor dwords,
          u8 first user-data dword, u8 root slot, u8 0                             [seen layout, VIctory kinds]
root type 0 constants, 1 root descriptor (64-bit address), 3 table (32-bit heap
          address), 4 static sampler, 5 resource heap, 6 sampler heap             [VIctory; 1 seen]
```

Stage data follows each record (E = record + 0x54):

```text
VS  E+0x24 32 inputs × 5 B {usage, signature row, channel mask, first VGPR, last channel},
    E+0xc4 count; E+0xc8 32 outputs × 6 B {usage, row, param n, row, slot, flags}, E+0x188 count
PS  E+0x24 32 inputs × 8 B {usage, row, mask, attr, default, 2nd row, param slot, flags:
    bit 0 flat, 1 linear, 3 fp16, 4 packed fp16}, E+0x124 count
    E+0x134 SPI_PS_INPUT_ADDR, E+0x138 DB_SHADER_CONTROL, E+0x148 patch size (dwords),
    E+0x14c 8 × {i32 code dword offset (-1 none), 10 variants × 10 dwords}
    variants: ZERO 32_R 32_GR 32_AR FP16 UNORM16 SNORM16 UINT16 SINT16 32_ABGR
```

**[seen]** on the corpus: VS output `{6, row 1, param 0}`; PS input `{6, row 1, mask 0xf,
attr 0, flags flat}`; `SPI_PS_INPUT_ADDR` 0x2; PS patch at dword 9 (size 6) whose 32_ABGR
variant is `s_nop ×4; exp mrt0 v2 done vm` in place of `v_cvt_pkrtz…; exp mrt0 … compr`.
Usage 6 = generic, 7 = clip distance **[VIctory]**. Merged stages (ES/GS, LS/HS, NGG) and mesh
keep other layouts **[VIctory: not decoded]**; the corpus has none.

**Chunk 1 check** (`shader/`, run on the 10 embedded blobs: all parse, 1 VS and 9 PS; VS and PS
fields above reproduced, `nativ64-shader` prints them). Precisions and corrections **[seen]**:

- The `u32 0x24` after the XDAT entry count is the offset of the entry table in the `DXIL` part
  (entries at part +0x24); the bitcode offset at `DXIL +0x10` counts from the `DXIL` magic (+8).
  The program version dword is `kind << 16 | major << 4 | minor` (0x66 = 6.6).
- `XBCH` packed size = bytes after the 24-byte header; `FSSM` payload size = unpacked − 16.
- Payload slot table: the first u64 (6) is read as a slot count; the pointers LS..PS sit at
  +0x08..+0x30, and +0x68 holds 2 on all blobs (meaning unknown, ignored).
- PS patch table: 8 entries of 404 bytes (`i32` code dword offset or -1, then 10 variants of
  10 dwords of which `patch size` are used). Only target 0 is patched on the corpus; the patch
  sits at dword 9, 10 or 12 depending on the program (not always 9), with size 6 everywhere.
  The 32_ABGR variant is four `s_nop` plus an `exp mrt0 … done vm` whose enable mask is the
  target's channel mask (0x1 or 0xf): `v2` only on 3 programs, 4 channels on the others.
- Used-resource kind 0 (`SRV`) names the VS's root descriptor (type 1) read as `s8:s9`, so
  `kind` does not separate buffers from views; tracing (Resources) decides.
- `ISG1`/`OSG1` elements are 32 bytes (stream, name offset, index, system value, component
  type, register, mask, read-write mask, min precision); names follow the table.
- VS inputs (stage data) are empty on the corpus: that layout is still unverified. Merged
  stages and compute records are not decoded (present slots are listed only).

**Code.** Starts with two 12-byte VOP3 words of opcode 0x180 (`v_nop` in VOP3 form with junk
operand fields and a literal: a marker or hash) and `s_inst_prefetch 3`; `llvm-mc` rejects the
two (bad operands for `v_nop`) **[seen, llvm]**. The program ends at `s_endpgm`, padded with
`s_code_end`; compilers may append constant data after it **[VIctory]**. So instructions are
found by following control flow from offset 0, never by a linear sweep, and `v_nop` decodes
whatever its operand bits say.

**Decisions.** One `Container` parser (DXBC) and one `NativeProgram` reader (XDAT → XBCH →
`miniz_oxide` inflate → FSSM → records), refusing any unknown version or an unpacked size over 16 MiB. A VS may carry two kind-0x0d
programs, compiled with and without a geometry stage **[VIctory]**: take the one with a VS
record. The PS export patch is applied with the 32_ABGR
variant (exports carry raw 32-bit values) and the render-target format is handled by Vulkan
(see Outputs). The digest is not checked (our checksum code is unverified).

## Decoding (GFX10)

Encodings, by the top bits of the first dword **[spec, llvm]**:

| Bits | Encoding | Size |
|---|---|---|
| `0` | VOP2 | 4 (+4 literal, +DPP/SDWA 4/8) |
| `0111111` / `0111110` | VOP1 / VOPC | 4 (+…) |
| `10` (not below) | SOP2 | 4 (+4) |
| `1011` | SOPK | 4 |
| `101111101` / `…110` / `…111` | SOP1 / SOPC / SOPP | 4 (+4) |
| `110010` | VINTRP | 4 |
| `110011` | VOP3P | 8 (+4) |
| `110101` | VOP3 (VOP3a/b; VOP1/2/C opcodes at fixed bases) | 8 (+4) |
| `110110` | DS | 8 |
| `110111` | FLAT / GLOBAL / SCRATCH | 8 |
| `111000` / `111010` | MUBUF / MTBUF | 8 |
| `111100` | MIMG (NSA adds dwords) | 8+ |
| `111101` | SMEM | 8 |
| `111110` | EXP | 8 |

Operand fields: SGPR 0-105, VCC 106-107, TTMP 108-123, M0 124, NULL 125, EXEC 126-127, inline
integers 128-208 (0..64, -1..-16), inline floats 240-248 (0.5, -0.5, 1, -1, 2, -2, 4, -4,
1/(2π)), `vccz`/`execz`/`scc` 251-253, literal 255, VGPRs 256-511 **[spec]**. One literal per
instruction, shared by every operand that names 255.

**Design.** A table-driven decoder: per encoding a `[Option<OpInfo>; N]` built at compile time
(`OpInfo` = name, operand kinds and widths, float/int class, side effects). `decode(code, at)
-> Instr { at, len, enc, op, operands, modifiers (abs/neg/clamp/omod/opsel), dpp/sdwa,
mem fields (offset, glc/slc/dlc, idxen/offen, dmask, dim, a16/d16) }`. Unknown opcode = error
`unknown GFX10 VOP3 opcode 0x1a3 at +0x40` (the "unknown slot = stop" rule). `disasm()`
prints LLVM syntax so tests diff it against `llvm-mc` output **[llvm]**. Flow following: from
offset 0, branches (`s_branch`, `s_cbranch_*`: target = next + 4·simm16) and fall-through;
`s_endpgm` stops a path; `s_setpc`/`s_swappc` are recorded as indirect jumps (`s_getpc`
alone is allowed: it addresses constant data). **[seen]** The whole corpus (8634 blobs, 20.25M
instructions) decodes with every encoding above, 0 diff against `llvm-mc` on the instructions
compared. Its indirect jumps are of two kinds only: 13 compute programs use the far jump
(`s_getpc_b64 s[n:n+1]`, `s_add_u32 sn, sn, literal`, carry, `s_setpc_b64 s[n:n+1]`), and 332
vertex programs open with `s_swappc_b64 s[4:5], s[4:5]`, the call to the fetch routine (Inputs
and outputs). No jump table.

## IR

SSA over 32-bit register dwords, one function per program:

- **Registers**: `S(n)`, `V(n)`, `VCC_LO/HI`, `EXEC_LO/HI`, `M0`, `SCC`. A 64-bit operand is a
  pair. Every vector instruction reads EXEC; a VGPR write also reads the old value (lanes
  outside EXEC keep it). SCC and carries are explicit results.
- **Blocks** split at branch targets and after branches; `s_nop`, `s_waitcnt*`,
  `s_inst_prefetch`, `s_clause`, `s_dcache_*`, `s_code_end`, `v_nop` vanish.
- **Instructions keep their decoded opcode** (no early raising); semantics live in one place,
  the emitter's per-opcode table. Phis are inserted by the usual dominance-frontier method.
- **Entry values**: every register live at entry is an `Arg(reg)`; I/O raising and resource
  tracing give them meaning (user SGPRs, system SGPRs, input VGPRs). A live-in that nothing
  explains is an error naming the register.
- **As built** (`ir.rs`, chunk 3). Registers are `S(n)`, `T(n)` (ttmp), `V(n)`, `VccLo/Hi`,
  `ExecLo/Hi`, `M0`, `Scc`; wave32 only has the `Lo` halves. An `Inst` keeps its decoded `Instr`
  plus `dsts` (explicit dwords, then EXEC for `*_saveexec` / `v_cmpx`, then SCC), `srcs` (one
  `Vec<Src>` per explicit source operand, one `Src` per dword) and `imps` (implicit reads:
  EXEC for every vector instruction except `v_readlane/writelane`, VCC for `v_div_fmas`, SCC
  for `s_cselect/addc/subb/cmov`, M0 for `v_interp*` / `s_sendmsg`, and the old value of every
  written VGPR, which is `undef` when nothing defined it). `s_cbranch_*` are `Term::Branch`
  with their condition registers as reads. A `Src` is a `Val`, a `Const`, `CodeLo(off)` /
  `CodeHi` (the program's own address: `s_getpc_b64` + offsets; the code is assumed aligned so
  the low add never carries, SCC from it folds to 0) or `Undef` (phi input of a path that never
  defined the register). Folded instructions leave no `Inst`; their SCC folds too. Phis go on
  the iterated dominance frontier of the registers that are read somewhere, then trivial and
  dead phis are removed. `Func` carries `idom` and `rpo` for chunks 4-5. Encodings the decoder
  does not read, and `s_setpc/swappc/rfe/call`, `s_movrel*`, `v_movrel*`, scalar atomics,
  MUBUF-to-LDS are typed `IrError`s.
  **Chunk 4b** (`ir.rs`): MIMG, MTBUF, DS, VOP3P and FLAT/GLOBAL/SCRATCH are lowered. Data dwords
  follow the decoder (dmask channels, halved by d16, plus one dword for tfe or lwe, which only
  loads accept); NSA address lists name each register; resource and sampler are SGPR quads
  (8 dwords, 4 for a BVH ray). Stores have no `dsts`; an atomic (MIMG, FLAT) defines its old
  value only with glc, half of the data for compare-and-swap. Every written VGPR keeps its old
  value as an implicit read. DS reads M0 only for the `gds` bit, `ds_ordered_count`,
  `ds_append`/`ds_consume` and `*_addtid` (LLVM: `ldsRequiresM0Init` is generation < 9); GWS
  opcodes, MIMG r128 (outside BVH) and tfe/lwe on a store or atomic, LDS loads (FLAT) are typed
  errors. The flat-scratch base of FLAT/SCRATCH is hidden state, not an IR register.
  **Chunk 4** (`ir.rs`, `native.rs`, `gfx10/flow.rs`): CS and mesh stage data are decoded like
  the VS/PS ones (`CsData`: `RSRC2` and group size, refused when a dimension is 0 or the group
  over 1024 threads; `MeshData`: outputs, lanes, vertex and primitive maxima, user SGPRs,
  topology). `EntryRegs::from_stage(record)` replaces the root-layout guess with the table of
  Inputs and outputs. `s_setpc_b64` of a `CodeLo` / `CodeHi` pair is a direct jump: `build`
  follows the code, folds, and when a `s_setpc_b64` folds to byte `t` follows `t` and builds
  again (`gfx10::follow_with`), so the far-jump targets need no separate analysis. A VS whose
  stage data lists attributes and whose first instruction is `s_swappc_b64 s[4:5], s[4:5]`
  gets that call as an `Inst` reading `s[4:5]` (entry values) and defining the attribute VGPRs
  (first VGPR + channel), then `s4`, `s5`; any other jump through a register, and the same call
  without attributes, is `IrErrorKind::Indirect`. Scalar compares of constants (`s_cmp_*`)
  fold their SCC. After SSA, `prune` turns an `s_cbranch_*` whose condition folded to a
  constant into a jump (a wave64 mask is decided when either half is a non-zero constant),
  drops the blocks only it reached, removes the dead phi inputs (the phis left with one value
  fold, which can decide more branches), recomputes `idom` / `rpo` and drops the arguments
  nothing reads any more. `nativ64-shader --ir --stats` reports, per stage, the programs that
  build and the errors by kind. `nativ64-shader --ir` prints the IR.
- **Analyses on SSA**: constant folding of scalar values (literals, `s_getpc` + offset, loads
  from appended constant data), lane-mask typing, resource tracing, uniform/lane classification.
  Emission does not need phis (below).

## Structuring: EXEC masks → per-lane structured flow

SPIR-V's `Shader` capability requires structured control flow; GFX10 code is a CFG whose
divergence is in EXEC. Each SPIR-V invocation runs one console lane, so EXEC becomes one
boolean per invocation.

1. **Mask typing.** A wave-sized SGPR value (64-bit pair in wave64, 32-bit in wave32) is a
   *mask* when it comes from `v_cmp*`, `v_cmpx*`, carry-outs, EXEC, VCC, `s_*_saveexec`,
   or bitwise ops (`and/or/xor/andn2/orn2/not/mov/cselect`) of masks and of constants 0/-1.
   A mask becomes a `bool` (this lane's bit). Uses allowed: EXEC write, `v_cndmask`, carry-in,
   `s_cbranch_vcc*/exec*`, SCC from a mask op (= the bit, see 2). Any other use (`s_bcnt1`,
   `s_ff1`, shifts, arithmetic, memory, `v_readlane` of a mask) is *cross-lane*: error until
   the wave chunk (7).
2. **Branches.** A wave-wide test of a mask (`s_cbranch_execz`, `vccz`, SCC of a mask op)
   becomes a test of the lane's own bit. Sound because compiled code skips only work the
   lanes outside the mask would not change, so each lane can decide alone **[VIctory]**. A
   branch on a scalar non-mask value is uniform: same in every invocation. A scalar value
   defined in a region some lanes skip and used after the join would diverge: error.
3. **Guards.** Each vector instruction is guarded by `active` (EXEC's bit). A path-sensitive
   pass removes guards it proves true (after `s_and_saveexec` + `s_cbranch_execz`, the usual
   if-pattern). What stays: ALU writes `v = active ? new : old` (`OpSelect`); stores, atomics,
   exports and `image_store` inside `if (active)`.
4. **Pixel discard.** `s_andn2 live, live, exec` then a branch when no lane is live, and
   `s_wqm` keeping helpers, become `OpDemoteToHelperInvocation` at that point: the lane keeps
   running as a helper (derivatives read it), its outputs and stores are dropped **[VIctory
   for the pattern; spec for demote]**. The final `exp … done` with the lane's bit clear is a
   demote too.
5. **Structured tree.** The CFG must be reducible (else error). A dominator-tree algorithm
   (Ramsey, *Beyond Relooper*, JFP 2022) turns it into a tree of `Block(label)`, `Loop(label)`,
   `If`, `Br(label)`, `Return`. Lowering to SPIR-V: `If` = `OpSelectionMerge`; `Loop` =
   `OpLoopMerge` with a continue block that branches back; `Block` = a one-trip loop (merge =
   label, continue block unreachable). A `Br` to the innermost loop's merge/continue is a
   plain branch; a `Br` out of k levels sets a function variable `target`, breaks, and each
   enclosing merge re-tests `target`. Always valid; `spirv-val` checks it in tests.
6. **Registers in emission** are `Function` `OpVariable`s (one per register dword the program
   touches, `bool` for masks), loaded and stored around each instruction: no phis to place
   after restructuring, and drivers promote them. SSA stays for analysis.

- **As built** (`structure.rs`, chunk 5). `structure(&Func)` returns `Structured`: `masks`
  (typing, `scc_of`, the cross-lane uses), `branches` (per block: `Uniform` or `Lane(LaneTest)`),
  `guards` (per instruction: `Scalar` / `Proved` / `Active`), `demotes` and `tree`. Errors are
  typed: `CrossLane { at, name }` (first in program order), `DivergentScalar`, `Irreducible`.
  Deviations from the steps above:
  1. Typing is per dword and optimistic (loop-carried mask phis type). A mask operation with a
     non-constant, non-mask input (`s_and_b32 s0, exec_lo, s5`) is reported as a cross-lane use
     by that instruction. Cross-lane by nature, whatever the operands: `v_readlane`,
     `v_readfirstlane`, `v_writelane`, `v_mbcnt*`, `v_permlane*`, `ds_swizzle/permute/bpermute`,
     any DPP instruction. Carry-in and `v_cndmask` take a mask in their last source only.
  2. A lane-wise branch whose mask is built only from one EXEC value and scalar selects
     (`vccz` of `exec & ~(scc ? exec : 0)`) and is not an EXEC value is not split-checked: every
     lane that runs on decides alike. `DivergentScalar` looks at the phis of the immediate
     post-dominator of each other lane-wise branch (a differing scalar, `undef` included). It
     is strict: it also stops the usual `v_cmpx` + `s_cbranch_execz` over scalar loads whose
     result is read after the join (corpus: 1321 CS, 683 PS, 50 VS, 8 mesh once cross-lane
     uses are set aside; resolved by speculation, below).
  3. Guards are proved from per-value facts (EXEC of the entry, the bit a branch tests on its
     edges, copies and and/or/andn2 of known values). The join after an if/else (`exec | saved`
     where `saved` came out of an xor) is not proved: it needs bit-set reasoning, so those
     guards stay `Active` (correct, only less sparse).
  4. The discard is marked at the `s_andn2 live, live, exec` (`Demote`) when its SCC branch
     reaches a block that only does scalar work and a `done` export to `null`. The final `exp
     done` with the lane's bit clear is not marked yet.
  5. The tree is Ramsey's, with exits inlined in the `Loop` when a loop exit has one
     predecessor. `Br { to, depth }` carries the number of constructs left; each `Block` /
     `Loop` lists in `forwards` the targets that leave through it (the `target` re-tests).
     Irreducible means a retreating edge whose target does not dominate its source.
  `nativ64-shader --structure --stats` reports per stage and per error kind, then the same
  ignoring cross-lane uses, and the cross-lane instructions by name.

**Decisions after chunk 5** (corpus of 8610 programs with IR: 2862 structure; cross-lane uses
stop 2023 CS, 1687 PS, all 1569 mesh, 270 VS; a divergent scalar stops the rest; none is
irreducible):

- **Speculated scalar regions.** Scalar instructions with no side effect (SALU, SMEM loads)
  inside a region that lanes skip are run by every invocation, unguarded. The wave runs that
  region when at least one lane enters it, so a scalar phi at the join selects the region's
  value when `OpGroupNonUniformAny(entered)` and the old value otherwise: exact, and it replaces
  the `DivergentScalar` error for these. SMEM loads are safe to speculate because every load is
  bounds-checked (Resources). A scalar store, atomic or `s_sendmsg` in such a region stays an
  error.
- **Scalar bits as lane masks.** A mask operation with a uniform non-mask scalar
  (`s_and_b32 exec_lo, exec_lo, s5`) is not cross-lane: lane n reads bit n of the scalar
  (`n` = the console lane index, below). Only reading a whole mask as data (`v_mov_b32 v, s_mask`,
  `s_bcnt1`, `s_ff1` of a mask) needs a ballot.
- **Wave ops are on the main path** (chunk 7, right after the SPIR-V builder). Revised by the
  wave survey below: no workgroup-memory emulation of the 64-lane wave for now.

**Chunk 7 design (Opus, after the wave survey).** `nativ64-shader --structure --waves` classifies
every cross-lane instruction of the corpus (8610 programs with IR). Programs with a cross-lane
use: cs 2023, ps 1687, vs 270, mesh 1569. With the survey's first, strict uniformity test, the
programs whose uses are all size-agnostic were cs 270, ps 1116, vs 226, mesh 0. What the uses
are:

- **SGPR spills into VGPR lanes**: `v_writelane v, s, k` / `v_readlane s, v, k` with a constant
  `k` on a VGPR that nothing else touches (cs: writelane in 775 programs, readlane at a constant
  lane < 32 in 1047, seen on a sample). Not a lane exchange at all.
- **`v_readfirstlane` of a value that is uniform over the active lanes** (cs 1691 programs, ps
  877, vs 232, mesh 426 by the strict test). The mesh "other" cases seen on a sample are uniform
  too, but the strict test misses them: a system entry VGPR, a `buffer_load` whose address is
  an SGPR, a `v_cndmask` on a mask `scc ? exec : 0`.
- **Waterfall loops** (`v_readfirstlane` → `v_cmp_eq` with the same VGPR; cs 1137, ps 135, vs
  25, mesh 34).
- **Lane index**: `v_mbcnt_lo/hi(-1, ·)` (all 1569 mesh, cs 270).
- **Whole-wave sections** (`s_or_saveexec -1` … restore): cs 923 programs, ps 284. They hold
  the DPP row ops, `v_permlanex16`, DPP8 and the final `v_readlane` of HLSL wave reductions and
  prefix sums: 37.6 k cross-lane instructions in cs, 4.7 k in ps.
- **Quad ops** in pixel shaders: `ds_swizzle` quad mode (916), DPP `quad_perm` (124).
- **Masks as data**: `s_ff1` (cs 787), `s_bcnt1`, a few moves and compares; `mbcnt` of a real
  mask (cs 100); `ds_bpermute` (cs 6). No `row_bcast`, `wave_*` DPP or `v_permlane16` anywhere.

Decision: **keep the per-lane model of chunk 5** (one invocation per console lane, structured
per-lane flow) and give each use a meaning that does not depend on the host's subgroup size.
XCL's split form (two wave32 halves meeting through groupshared, uniform flow per console wave)
would need a second translation model and costs 1.1–4× on its benchmark, while the survey shows
that almost every use has a size-independent reading:

1. **Uniformity** (`structure/uniform.rs`): a value is *uniform* when it is the same in every
   active lane. Constants, SGPRs (scalar code is uniform by construction once speculation, below,
   is in), `v_readfirstlane` results, VALU of uniform operands, `v_cndmask` whose mask is EXEC or
   0 chosen by a uniform condition (and whose values are uniform), vector loads (MUBUF, MTBUF,
   GLOBAL, MIMG without derivatives) whose every address and resource operand is uniform (not
   atomics, not after a store to the same resource in the same block: refused as not uniform),
   and system entry VGPRs that the stage table marks wave-uniform (mesh `v5` and the like, from
   Inputs and outputs; an unknown one is not uniform). `v_readfirstlane` / `v_readlane` of a
   uniform value is a copy: no subgroup op.
2. **Spill slots**: a VGPR whose every definition is `v_writelane` at a constant lane and whose
   every read is `v_readlane` at a constant lane becomes one scalar variable per lane used.
   Exact. A slot read before any write reads `undef`, as on the console.
3. **Waterfall**: `v_readfirstlane` in a waterfall loop takes this lane's own value; the
   compare is then true, the body runs once per lane, and the resource index it feeds is marked
   `NonUniform` (Resources). Exact per lane.
4. **Console lane index**: `v_mbcnt_lo(-1, x)` = `min(lane, 32) + x`, `v_mbcnt_hi(-1, x)` =
   `max(lane - 32, 0) + x`, where `lane` = the console lane index: in compute and mesh
   `LocalInvocationIndex % 64` (wave64; `% 32` in wave32), never the host's subgroup id. In PS
   and VS the console lane is not observable; lane-index uses there are refused by name.
5. **Whole-wave sections** are raised to one subgroup operation (VIctory's approach, read from
   `emit/wave.rs`, no code copied): simulate the section on 64 symbolic lanes; a value it
   leaves that turns out to be the reduction, inclusive or exclusive prefix of one associative
   operation over the lanes' masked inputs becomes `OpGroupNonUniform{IAdd,FAdd,UMin,...}`
   `Reduce` / `InclusiveScan` / `ExclusiveScan` over the host's active invocations. That is the
   HLSL meaning (`WaveActiveSum`, `WavePrefixSum`), which a size-agnostic program accepts; any
   step the simulation does not model leaves its values unknown and the program refused by
   name.
6. **Quad ops** (`ds_swizzle` quad mode, DPP `quad_perm`): `OpGroupNonUniformQuadBroadcast` /
   `QuadSwap`, or `Shuffle` to `(id & ~3) | sel` with `subgroupQuadOperationsInAllStages`
   unset. Exact on any host (quads are 4 lanes everywhere). Helper lanes stay active for them.
7. **Masks as data**: the ballot of the lane's bits over the host's active invocations
   (`OpGroupNonUniformBallot`). Allowed where the meaning is size-agnostic: `s_bcnt1` (count),
   `s_ff1` (first active lane, then only fed to `v_readlane`, which becomes `BroadcastFirst`),
   `mbcnt` of a mask (`BallotExclusiveBitCount`), comparing a mask with 0 or with EXEC. Lane
   numbers from a ballot are host lanes: they must never meet a console lane index (item 4)
   in the same program; refused by name if they do.
8. **The rest is refused by name**: `v_readlane` at a computed lane outside 7, `v_writelane`
   outside 2, `ds_bpermute`, DPP or `v_permlanex16` outside a whole-wave section. If the corpus
   count after items 1–7 shows such programs on the first screen's path, the split form (XCL) is
   the fallback, for compute and mesh only.

Requirements: SPIR-V `GroupNonUniform`, `…Ballot`, `…Arithmetic`, `…Quad`, `…Shuffle`, and
`SPV_KHR_maximal_reconvergence` (`MaximallyReconvergesKHR`) on every program with a subgroup
op, so the host's active set at the op is the set of lanes whose structured flow reached it
(NVIDIA, ANV and llvmpipe here all have `shaderMaximalReconvergence`). A subgroup op sits
inside `if (active)` where its guard is not `Proved`, so lanes whose EXEC bit is clear do not
take part.

Tests: the probe runs one lane; chunk 7 adds a **wave evaluator** that runs the `Cpu` backend
over 64 lanes (and over a host width of 8, 32 or 64, the subgroup ops computed over that host
width) so a test checks both "console-exact on a 64-wide host" and "same result on narrower
hosts" for size-agnostic programs. On a device (`NATIV64_SPIRV_RUN=1`) the probe gets a
`LocalSize` of 64 and compares per lane (llvmpipe: 8-wide subgroups, so only size-agnostic
cases run there).

Sub-chunks, each with its corpus count (`--structure --waves` and `--structure --stats`):
- **7a — IR and structure, no wave op**: `s_andn1/andn2_wrexec_*` write EXEC in the IR; far
  jumps resolved in one pass; speculated scalar regions (`OpGroupNonUniformAny` select at the
  join) replacing `DivergentScalar`; scalar bits as masks.
- **7b — uniformity, spill slots, waterfall, lane index** (items 1–4), with the wave evaluator.
- **7c — whole-wave sections, quad ops, ballots** (items 5–7), then the refusals of item 8 and
  the corpus count that decides on the split form.
- **Far jumps** resolve in one pass over the code before building (one program chains more than
  16: the per-build loop of chunk 4 is quadratic there).

**As built (7a)** (corpus of 8634 blobs; IR 8611 programs, was 8610):
- **`s_andn1/andn2_wrexec_*`**: the IR lists the SGPR then EXEC as destinations, reads EXEC, sets
  SCC; mask typing and the ALU (CPU and SPIR-V) treat them as the saveexec family. The semantics
  (`exec = sdst = ~src & exec`, `src & ~exec`; SCC = result != 0) are the ISA text, **[unverified]**.
  No corpus program uses them: no number moved. The bit-set facts of `s_andn2_saveexec` now
  combine as `src & ~exec` (they had `exec & ~src`).
- **Far jumps**: `resolve_jumps` runs a forward worklist over the instructions carrying the folded
  scalar values (`fold`), keeping at a join what every path agrees on; the build then runs once
  with every known target resolved. The per-build loop remains as the fallback for a target only
  the SSA form knows. The CS chaining more than 16 jumps now builds (IR 8611/8634; the 23
  refusals left are `v_movrels/movreld` indexed registers).
- **Speculated scalar regions**: deviation from the select at the join. The decision is on the
  branch, not on each phi: `BranchKind::Wave` is a lane-wise branch whose join has a scalar phi
  with differing inputs and whose region has no scalar store, atomic or `s_sendmsg`. Every
  invocation takes it on `OpGroupNonUniformAny(bit)` (capability `GroupNonUniformVote`), the
  region's scalar instructions run in all of them, its vector instructions stay `Active`, and
  no EXEC fact is drawn from its edges. This is the same value as the select (the region's when
  any lane entered, else the old one), with registers staying variables and no phi to place.
  The probe has one lane, so the vote is the lane's bit (the CPU evaluator reads `Wave` as
  `Lane`). Structure 2862 -> 3060 of 8611 (cs 693, ps 1607, vs 760, mesh 0); ignoring cross-lane
  uses 6549 -> 8603 of 8611; `divergent scalar` is gone (984 speculated branches), what is left
  is 8 irreducible cs, which it used to hide.
- **Scalar bits as lane masks**: a mask operation takes a non-mask scalar (or a constant other
  than 0 / -1) beside at least one mask; scalars alone still do not make a mask. The ALU reads
  bit 0 of the scalar's first dword (the probe's lane), for every dword of a wave64 mask. The
  mask operations (`s_and_b32`, `s_andn2_b32`, `s_or_b32`, `s_and_saveexec_*`, ...) leave the
  cross-lane list; the program counts do not move (the same programs still hold a readlane,
  mbcnt or `s_ff1`): cs 2024, ps 1687, mesh 1569, vs 270. ALU coverage 8525/8611 programs
  (3006/3060 structured).

## Inputs and outputs

**User SGPRs.** A VS/PS/CS receives user-data dword i in `s[i]`; merged stages and mesh have 8
system SGPRs first (`s[i+8]`) **[VIctory]**. Seen: the VS's root descriptor (layout `user
data 8, 2 SGPRs`) is read as `s8:s9`; the PS gets the primitive mask in `s10`, right after
its user SGPRs (`s_mov_b32 m0, s10` before `v_interp`) **[seen]**.

**Entry SGPRs below s8, per stage** (first reads before any write, whole corpus **[seen]**;
meanings **[VIctory]** unless said):

| Stage | Reads in s0..s7 | Meaning |
|---|---|---|
| VS | `s[4:5]`, 332 of 1034, only as `s_swappc_b64 s[4:5], s[4:5]` at the entry | fetch routine address: the call is replaced by the attribute values (Vertex input) |
| PS | none | — |
| Mesh | `s3`, all 1569, by `s_bfe_u32` | merged wave info: wave index in the group, bits 24-27 |
| CS | `s4..s7` in most of the 2734 (`s[4:5]` as an SMEM base, `s6`/`s7` multiplied by group sizes), a few `s0..s3` | user SGPRs then group id x/y/z and group info, as `COMPUTE_PGM_RSRC2` (stage data E+0x24) enables them; scratch offset last |

So `EntryRegs` comes from the stage data, not from a guess: CS from `RSRC2` (user SGPR count
bits 5:1, group id x/y/z bits 7-9, group info bit 10, scratch bit 0) and the thread-group size
(E+0x28, 3 × u32); mesh from its record (E+0x448: group size in lanes, maximum vertices and
primitives, user SGPRs, topology; code at E+0x45c) plus `s3` and `v5` (group id × group size,
read through `v_readfirstlane`). A live-in still unexplained is an error naming the register.
That 332 VS read the fetch address while 702 pull their vertices themselves is seen; that the
call appears exactly when the stage data lists attributes is **[unverified]**.

**Root block.** Per draw the backend builds, from the recorded root arguments (`RootTable`,
`RootConstants`, `RootView` in `command.rs`) and the program's layout table, a *root block*:

```text
u32 user_data[32]   the values the console would load into user SGPRs (constants as given,
                    root descriptor = its 64-bit address, table = low 32 bits of its address)
u64 param_base[N]   per root slot: table → address of its first descriptor in heap memory,
                    root descriptor → its address; heaps (types 5, 6) → heap start
u64 view_table      address of the view table (Resources)
```

It reaches the shader as a push constant holding its device address (8 bytes; BDA is already
required by our backend). A user SGPR read is `user_data[i]`.

**Vertex input.** The console prefixes a fetch routine that loads attribute k's channels into
VGPR `first + k` **[VIctory]**. Ours: each VS input (stage data) → its `ISG1` row → semantic
name and index → the pipeline's `InputElement` with that name and index → a Vulkan vertex
attribute (location = input index, format from the DXGI format, binding = input slot, offset),
strides set at draw (`vkCmdBindVertexBuffers2`). The VGPRs start as those attribute values. The
corpus VS has no inputs and pulls its vertices itself through a raw buffer **[seen]**. System
VGPRs of a hardware VS: v0 vertex id, v3 instance id (v2 primitive id) **[VIctory]** →
`VertexIndex` / `InstanceIndex` (minus the bases, D3D ids start at 0).

**Pixel input.** `SPI_PS_INPUT_ADDR` enables input VGPR groups in a fixed order (persp sample,
center, centroid i/j pairs; pull model; linear sample/center/centroid; line stipple; pos
x/y/z/w; front face; ancillary; sample coverage; fixed pos) **[spec]**. Raising:
`v_interp_p1` + `v_interp_p2` of `attr n` with one barycentric pair = an `Input` variable at
that attribute's location: perspective center = default, centroid = `Centroid`, sample =
`Sample`, linear = `NoPerspective`; `v_interp_mov p0` = `Flat` (D3D and Vulkan both take the
first vertex as provoking); f16 forms read the half. Barycentric VGPRs used other than by
`v_interp` = error (or `VK_KHR_fragment_shader_barycentric`, later). Position → `FragCoord`,
front face → `FrontFacing`, ancillary → `SampleId`, coverage → `SampleMask`.

**Outputs.** VS `exp pos0` → `Position` (D3D clip space; the backend sets a negative viewport
height so y matches); `exp pos1..` with usage 7 → `ClipDistance`; `exp param n` → `Output` at
the location linked below. PS `exp mrt n` (after the 32_ABGR patch: raw 32-bit channels) →
`Output` location n, typed float/uint/int from the pipeline's `target_formats[n]` (Vulkan does
the UNORM/FP16 conversion); `exp mrtz` → `FragDepth` / stencil (`SPV_EXT_shader_stencil_export`)
/ `SampleMask` per its enable mask; `exp null` → nothing.

**Linking.** Pipelines are translated per VS+PS pair, so locations are assigned once: each VS
output row (`OSG1`) is matched by semantic name and index with a PS input row (`ISG1`), both
sides get the same location; an unmatched PS input reads its stage-data default.

## Resources

Our heaps do not hold RDNA2 descriptors: a view or sampler slot holds `"nVEW"` and a registry
index (`descriptor.rs`), slots 32 bytes (views) and 16 (samplers) (`info.rs`). A translated
shader must therefore never interpret the words it loads from a heap; it finds *which slot* a
descriptor operand comes from and looks the view up by its index.

**Tracing.** Every descriptor operand (SMEM `sbase`, MUBUF/MTBUF `srsrc`, MIMG `srsrc`/`ssamp`)
and every SMEM address is traced through SSA (copies, phis that agree, adds of constants or of
one computed value) to a `Source`:

| Source | Seen as | Translation |
|---|---|---|
| `Loaded { param, offset, dyn }` | `s_load_dwordx4/x8` from a table or heap param | slot address = `param_base + offset + dyn`; index = u32 at slot + 4 (BDA load); view = `view_table[index]` |
| `Built { addr, words }` | code ORs a root descriptor's address into a V# (`s_or_b32 s1, s9, 0x10000`, `s2 = -1`, `s3` constant) **[seen]** | raw buffer at that address, size from `num_records` (−1 = unbounded), stride from dword 1 |
| `Static(slot)` | root type 4 | sampler created at pipeline build from `RootSignature`'s static sampler |
| `Code(offset)` | `s_getpc` + offset | constant data, folded at translation |
| `Memory` | address loaded from memory or root constants | raw BDA at the computed address |

The layout and used tables say which root slot each user SGPR range is and how many dwords a
descriptor has; tracing cross-checks them and refuses mismatches. A loaded descriptor whose
words are used as numbers (bit extraction, compare) is an error first; fallback if a title
needs it: the view table also holds a synthesized RDNA2 descriptor per view.

**View table.** At submit the backend appends, for each registry entry not yet materialized,
a 32-byte record `{u32 kind, u32 format, u64 address, u32 size or width/height, u32
depth/layers/mips, u32 image slot, u32 sampler slot}` to a device buffer, and creates the
Vulkan object: `VkImageView` into `textures[]` (sampled) or `images[]` (storage), `VkSampler`
into `samplers[]`. One descriptor set, three `UPDATE_AFTER_BIND | PARTIALLY_BOUND` arrays sized
from device limits, bound once; slot = registry index (one namespace, holes allowed).
Buffers (CBV, raw/structured SRV/UAV) never need a descriptor: they are title memory, read by
`PhysicalStorageBuffer` pointers. Typed buffers (`buffer_load_format`, MTBUF) use the format
in the record and convert in the shader (chunk 9).

**Instructions.** SMEM `s_load*`/`s_buffer_load*` → BDA loads (bounds-checked against the
record size: out of range reads 0, as RDNA2 robust buffers do **[spec]**); MUBUF
`buffer_load/store_dword*` (`offen`/`idxen`, `inst_offset`, `soffset`) → BDA with the same
check; MIMG `image_sample*` → `OpImageSample*` on `OpSampledImage(textures[i], samplers[j])`
(`_l`, `_b`, `_d`, `_c`, `_o`, `_lz` variants), `image_load/store` → `OpImageRead/Write` on
`images[i]`, `image_get_resinfo` → `OpImageQuerySize(Lod)`; `dmask`, `dim`, `a16`, `d16`
shape the operands. FLAT/GLOBAL → BDA at the address. DS → a `Workgroup` u32 array of the
record's LDS size (compute later).

## SPIR-V emission

- **Our own builder** over the `spirv` crate (0.4.0+sdk-1.4.341.0, Apache-2.0 **[seen on
  crates.io]**: enums and constants only). A `Module` with sections (capabilities, extensions,
  memory model, entry points, decorations, types/constants deduplicated by key, functions),
  `finish() -> Vec<u32>`.
- **Target**: SPIR-V 1.5 (Vulkan 1.2, our floor), `PhysicalStorageBuffer64` addressing, GLSL450
  memory model; capabilities `Shader`, `PhysicalStorageBufferAddresses`, `RuntimeDescriptorArray`,
  `SampledImageArrayNonUniformIndexing` (`NonUniform` on indices from `dyn`), `Int64`,
  `DemoteToHelperInvocationEXT`, others on use (`Float16`, `ImageQuery`, `StorageImage…`).
- **Values are bits.** Registers are `u32`; each instruction bitcasts to its type (`f32`, `i32`,
  `f16x2`), applies modifiers (abs, neg, clamp, omod, opsel) and bitcasts back.
- **Exact semantics where they differ**: `v_mul_legacy_f32`/`v_mac_legacy` (0·x = 0),
  `v_min/max_f32` (IEEE mode: a NaN operand gives the other), `v_med3`, `v_cvt_pkrtz_f16_f32`
  (round toward zero: integer code), `v_rcp/rsq/sqrt` (host precision accepted), `v_frexp*`,
  `v_ldexp`, `v_bfe`/`v_bfi`/`v_alignbit`, 24-bit multiplies, carries of `v_add_co`/`v_addc`,
  `s_*` SCC rules, denorm/round mode from `s_setreg`/`s_round_mode`/`s_denorm_mode` (fixed
  per program: if it changes mid-program, error). One table entry per opcode, unit-tested on
  vectors (Xbox2Windows' measured data where it has some).
- **Stages**: hardware VS → `Vertex`, PS → `Fragment` (`OriginUpperLeft`, `DepthReplacing` if
  it writes depth, `EarlyFragmentTests` from `DB_SHADER_CONTROL`, later), CS → `GLCompute`
  (later). Wave64 and wave32 translate the same; the wave size only sizes masks.

## Validation and cache

- **Tests**: every translation test runs `spirv-val --target-env vulkan1.2` when it is in
  `PATH` (installed here: SPIRV-Tools v2026.4) and skips that step otherwise; CI installs it.
  Decoder tests diff `disasm()` with `llvm-mc` when present. Test inputs are hand-written GFX10
  sequences (assembled once with `llvm-mc -show-encoding`, checked in as hex with their
  source) and synthetic containers built by a test helper; never game bytes. A corpus test,
  `#[ignore]`, runs over `$NATIV64_SHADER_CORPUS` (the dump directory) and prints a coverage
  report: programs translated, first error per program grouped by message.
- **Runtime check**: `NATIV64_SHADER_VALIDATE=1` runs `spirv-val` on each new module and
  prints the error with the program's hash.
- **Cache**: `$XDG_CACHE_HOME/nativ64/shaders/<hash>.spv` (fallback `~/.cache`). Key = 64-bit
  FNV-1a hash (in-repo, no new crate) of {translator version, each stage blob, linking
  inputs: RT format classes, input layout}. The file starts with a header: magic, translator
  version, total input length and a second 64-bit hash of the same inputs (other seed),
  compared on load; a mismatch or a bad file is a miss, never an error. Bump the version on any
  translator change.
- **Dump**: `NATIV64_DUMP_SHADERS=dir` writes each pipeline's blobs at creation
  (`<hash>.<stage>.dxbc`) plus `pipelines.txt` (pipeline id, stage hashes, root signature,
  formats, first draw frame); `nativ64-shader` (a bin of `nativ64-gpu`) prints a blob's parts,
  stage data, layout and disassembly, and translates it.

## Real validation

The milestone check (CLAUDE.md) is the title's first splash/loading screen, compared with what
the title shows on the console (Legal/Terms screens; frames by `NATIV64_DUMP_FRAMES`). The
order below makes the programs that screen draws with work first: chunk 1's dump names them.

## Chunk plan

One Sonnet agent per chunk, each with its tests (filtered output: failures and summary only);
Opus review after each. Module paths are in `crates/nativ64-gpu/src/shader/`. The corpus is
41% pixel, 32% compute, 18% mesh, 12% vertex: compute and mesh are part of the main path, not
later work. The order serves the first screen: each chunk lands for all four stages unless
it says otherwise.

1. **Container and dump** (done; `container.rs`, `native.rs`, `bin/nativ64-shader.rs`,
   `pipeline.rs` hook). `NATIV64_DUMP_SHADERS`.
2. **Decoder** (done; `gfx10/`). Every encoding of the table, flow following, `disasm()` in
   LLVM syntax, `llvm-mc` fixtures per encoding (`tools/gen_gfx10_ext.py` for the vector-memory
   and packed ones). The whole corpus decodes.
3. **IR** (done; `ir.rs`): blocks, SSA dwords, implicit operands, phis, entry `Arg`s, constant
   folding, `s_getpc` data.
4. **Entry and flow cleanup** (`ir.rs`, `native.rs`): CS and mesh stage data (`RSRC2`,
   group size, mesh record), `EntryRegs` from stage data for the four stages (table in Inputs
   and outputs), far jump → direct jump, VS fetch call → fetch point, then **constant-branch
   pruning**: an `s_cbranch_scc*/vcc*/exec*` whose condition folded to a constant becomes a
   jump, unreachable blocks go, phis are recomputed (dominators change). Pruning sits here,
   before structuring, so chunk 5 never sees dead edges (fewer `If`s, fewer irreducible false
   alarms). Acceptance: `--ir` on the corpus, every program builds or stops on a named
   unsupported opcode, none on an unexplained live-in or a register jump; the report gives the
   count per error.
4b. **IR lowering of the memory and packed encodings** (`ir.rs`): MIMG, MTBUF, DS, VOP3P,
   FLAT/GLOBAL/SCRATCH get their `dsts`, `srcs` and implicit reads (EXEC, M0 for DS, the old
   value of written VGPRs; stores and atomics as side effects). After chunk 4 they are the only
   reason 6947 of 8634 programs do not build (MIMG 4035, MTBUF 1881, DS 633, VOP3P 349, FLAT
   47). Acceptance: the corpus report shows only named unsupported opcodes, a handful at most.
5. **Structuring** (`structure.rs`): mask typing, per-lane branches, guards and their
   removal, discard → demote, Ramsey tree, multi-level break lowering plan. Tests: if, if/else,
   nested ifs, loop with break and continue, a 2-level break, an irreducible graph (error), a
   cross-lane use (error); corpus count of programs that structure.
6. **SPIR-V builder + ALU** (done; `spirv/`): module builder, types/constants, registers as
   variables, structured flow from chunk 5, scalar and VOP1/2/3/C ALU for f32/i32/u32 and the
   exact-semantics list, `v_cvt_pkrtz`, `v_fma_mix*` (2179 + 135 uses in the corpus: the only
   VOP3P seen). Tests: `spirv-val` on each test program; opcode vectors through a small CPU
   evaluator of our IR where a GPU is absent.
   Scope decisions (Opus, before chunk 6): I/O and resources come later, so chunk 6 emits a
   **probe entry**: `GLCompute`, `LocalSize 1 1 1`, one invocation = one lane; every entry
   register is a `Private` `u32` variable initialised from a `StorageBuffer` input array (one
   dword per live-in, index in `EntryRegs` order), and every register still live at
   `s_endpgm` is written to an output array, so tests run vectors on a GPU when one exists
   (lavapipe in CI) and on the CPU evaluator otherwise, with the same bytes. EXEC and masks
   are a `bool` per lane (the lane is always active in the probe); SCC/VCC a `bool`/`u32`
   per the mask typing of chunk 5. Memory, export, interpolation, `s_sendmsg` and wave
   instructions are not emitted: the translator stops on the first one with
   `Unsupported { chunk: 7|8|9, op }`. `nativ64-shader --spirv --stats` reports, over the
   structured programs, those that translate and validate, and the others by first
   unsupported op; `--spirv --alu-stats` counts programs whose every **ALU** instruction is
   supported (non-ALU ops ignored): the chunk's acceptance is ALU coverage, not full
   translation. `--spirv <blob> -o x.spv` writes a probe module.
   **As built** (`spirv/`: `builder.rs` module builder over the `spirv` crate, `alu.rs` the
   semantics, `emit.rs` the translation, `gpu.rs` the optional device runner of the tests).
   * One table, two backends. `alu::exec` is the per-opcode table, written once over a
     `Backend` of ~20 primitives on 32-bit bits (`ibin`, `fma`, `fcmp`, `select`, `pack_half`...);
     `Cpu` (the evaluator the unit tests run on) and the SPIR-V `Gen` both implement it, and
     `Dry` asks whether an opcode is supported (`--alu-stats`). Everything else (abs, neg,
     clamp, omod, opsel, SDWA selects, guards `v = active ? new : old`) is in the table.
   * Probe layout (`Probe`): bindings 0 / 1 of set 0, `inputs` = one dword per entry register
     sorted by `Reg` (EXEC's slot unused, EXEC starts true), `outputs` = one dword per
     register variable `(Reg, is_mask)`, written at each `s_endpgm`; a mask is 0 or 1. A wave64
     mask pair holds the lane's bit in both dwords (a pair is one bit, as the branch tests
     read it); the lane is console lane 0, so a scalar read as a mask is its bit 0.
   * Flow: `Block` and `Loop` are `OpLoopMerge` with a continue block that only goes back;
     a multi-level `Br` stores a `target` code (block * 2 + kind + 1) in a function variable,
     breaks, and the `forwards` re-tests after each merge (reset to 0 where it resolves).
     Phi dsts need no copies: they share the register variable with their inputs; only a
     phi input that is a folded constant is stored, at the end of its predecessor, after the
     branch condition is read. Demotes are ignored (the probe only honours EXEC).
   * ALU covered: SOP1/2/C/K (32-bit, `_b64` moves, shifts, mask ops, `saveexec`, SCC rules,
     `s_setreg` of anything but the f32 round / denormal bits of MODE, which is `Unsupported`),
     VOP1/2/3/C for f32 / i32 / u32 (+ i64/u64 compares and shifts, 16-bit shifts / compares /
     add), SDWA (select, sext, abs, neg, `dst_sel`, `dst_unused`), `v_cvt_pkrtz_f16_f32`
     (integer round-toward-zero code), `v_cvt_f16_f32` / `v_cvt_f32_f16` (`PackHalf2x16`),
     `v_pack_b32_f16`, `v_fma_mix*`, `v_cube*`, `v_cmp_class_f32`, `v_perm_b32`, `v_bfe/bfi`,
     `v_alignbit/byte`, carries, 24-bit multiplies. f16 arithmetic, `v_pk_*`, f64 and
     `v_swap_b32` are `Unsupported { chunk: 6 }` (plan item 11).
   * Tests (23 in `spirv/tests.rs`): `fixtures.txt` holds the `llvm-mc` hex of every
     instruction used (`[w64] ` prefix for wavefrontsize64); each case gives vectors, runs
     them on the evaluator, translates and validates with `spirv-val --target-env vulkan1.2`
     (skipped when absent). `NATIV64_SPIRV_RUN=1 NATIV64_SPIRV_DEVICE=llvmpipe cargo test -p
     nativ64-gpu spirv -- --test-threads 1` also runs every vector (and the integer sweeps)
     on the device and compares register by register: all 23 agree on llvmpipe.
   * Corpus (8610 programs with IR, 2862 structure): `--spirv --alu-stats`: **8524** programs
     have every ALU instruction supported (2810 of the 2862 structured ones; cs 2633/2717, ps
     3292/3294, vs 1030/1030, mesh 1569/1569); 2176 of 15.3 M ALU instructions are not (f16
     and `v_pk_*`: 38 programs at most per opcode, `v_swap_b32` 10, f64 5).
     `--spirv --stats`: 5 programs translate and validate (all cs); the other structured
     ones stop on their first non-ALU instruction: `s_load_dwordx4/x8/x16/dword` (chunk 9;
     1973 programs), `s_swappc_b64` (the VS fetch call, chunk 8; 324), `v_interp_*` (261),
     `exp` (295), `ds_write*` (4).
   * Two IR gaps found: `s_bitset0/1_b32` did not read their destination (fixed in `ir.rs`:
     it is now an old-value read like `s_cmov`), and `s_andn1/andn2_wrexec_b32/b64` (52 k uses)
     write EXEC but the IR lists only their SGPR destination: they are `Unsupported` chunk 7
     until `ir.rs` models the EXEC write (`exec = sdst = src & ~exec`, `andn1`: `~src & exec`,
     per the ISA text, unverified).
7. **Wave ops and speculation** (`structure.rs`, `structure/`, `spirv/`): "Chunk 7 design"
   above, in three sub-chunks (7a IR and speculation, 7b uniformity / spill slots / waterfall /
   lane index, 7c whole-wave sections / quad ops / ballots). Acceptance: `--structure --stats`
   near the IR count, the remaining refusals named by `--structure --waves`.
8. **I/O** (`io.rs`): user SGPRs and root block; VS system VGPRs and vertex attributes; PS
   input VGPRs and `v_interp` raising; exports and linking by semantic; CS (`GLCompute`,
   `LocalSize` from the group size, thread id VGPRs, group id SGPRs); mesh (`MeshEXT`, one
   invocation per lane, `exp pos/param` per vertex, `exp prim` → `PrimitiveTriangleIndicesEXT`
   and cull, per-primitive params, `s_sendmsg GS_ALLOC_REQ` → `SetMeshOutputsEXT`).
   Milestone: one program of each stage translates and validates.
9. **Resources and memory** (`resource.rs`): tracing table, BDA buffer loads with bounds
   (MUBUF, MTBUF typed formats, `global_load`), view table format, MIMG sample/load/resinfo/
   store, static samplers, DS → `Workgroup` LDS (compute, mesh). Tests: synthetic programs for
   each `Source`; one error test per refusal.
10. **Vulkan pipelines, draws and dispatches** (`backend/vk/pipeline.rs`): bindless set and view
   table upkeep at submit, root block per draw, graphics (VS+PS) and mesh (MS+PS,
   `VK_EXT_mesh_shader`) pipelines from translated stages + `GraphicsState`, compute pipelines,
   cache, `vkCmdDraw*` / `vkCmdDrawMeshTasks*` / `vkCmdDispatch*` replay. Real validation: the
   first screen against the expected frame.
11. **Widening, driven by the corpus report**: f16 and `v_pk_*`, remaining VOP3, MIMG
    gather/compare/derivatives, FLAT/SCRATCH stores and atomics, ray queries, merged stages if a
    title uses them (none in this corpus: its stages are VS, PS, CS, mesh only).

## Open questions

- Unverified ALU semantics (no hardware data, the table follows the ISA text / LLVM): sign of
  the zero of `v_mul_legacy_f32` and `v_mad_legacy_f32` (the product's sign here), `v_med3_f32`
  with a NaN (the minimum of the others), `v_cube*` (LLVM's description), `v_frexp_*` of
  denormals (treated like zero), `v_cvt_pk_u8_f32` (truncates, saturates to 255), `v_perm_b32`
  selectors 8..11 (sign bits 15, 31, 47, 63), `v_bfe_i32` with offset + width past 32 (the
  arithmetic shift fills the field), 16-bit VOP2 ops zeroing the high half of the destination.
- f32 denormals and rounding: the emitted code uses the host's (SPIR-V has no per-instruction
  mode; `DenormPreserve` / `RoundingModeRTE` need `SPV_KHR_float_controls`, core in 1.4).
  A program that writes the f32 bits of MODE is refused; the default MODE of a stage is not
  decoded yet. `PackHalf2x16` rounds as the host does (llvmpipe: to nearest even);
  `v_mad_f32` / `v_mac_f32` are `OpFMul` + `OpFAdd` without `NoContraction`, so a driver may
  fuse them.
- `s_wqm` is a move in the probe; in a pixel shader it is a quad-wise OR (chunk 8).
- Wave32 bit (record +0x14 bit 5) and the VS system VGPR layout are VIctory's readings, not
  confirmed by the corpus (all wave64, VS reads only v0).
- Root-layout and used-resource tables have up to a few hundred entries (the chunk 1 cap of 64
  refused 2486 vertex/pixel blobs; now 512, bounds-checked against the payload).
- VOP3 interpolation (`v_interp_*_e64`) decodes; the f16 forms print `high`, not decoded.
- The VS fetch routine: its address arrives in `s[4:5]`, its code is not in the blob. That it
  only loads attributes into the stage data's VGPRs (and keeps every other register) is
  VIctory's reading; chunk 4 checks that the stage data of each of the 332 lists attributes.
- Compute programs reading `s0..s3` (about 50 `s_mov` / `s_and`): user data, or system SGPRs
  before it when `RSRC2` says few user SGPRs? Chunk 4 answers it by decoding `RSRC2`.
- Kind 0x12 XDAT entry, `PRIV` part (12 bytes), program-record bytes not listed: meaning unknown,
  ignored.
- Bindless limits, `VK_EXT_shader_demote_to_helper_invocation` and `VK_EXT_mesh_shader` on
  lavapipe and the Intel driver: to check with `nativ64-vkcheck` before chunk 10 (mesh is 18%
  of the programs: without the extension those pipelines cannot be built at all).
