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

| Bits | Encoding | Size | First chunk |
|---|---|---|---|
| `0` | VOP2 | 4 (+4 literal, +DPP/SDWA 4/8) | 2 |
| `0111111` / `0111110` | VOP1 / VOPC | 4 (+…) | 2 |
| `10` (not below) | SOP2 | 4 (+4) | 2 |
| `1011` | SOPK | 4 | 2 |
| `101111101` / `…110` / `…111` | SOP1 / SOPC / SOPP | 4 (+4) | 2 |
| `110010` | VINTRP | 4 | 2 |
| `110011` | VOP3P | 8 (+4) | 9 |
| `110101` | VOP3 (VOP3a/b; VOP1/2/C opcodes at fixed bases) | 8 (+4) | 2 |
| `110110` | DS | 8 | 9 |
| `110111` | FLAT / GLOBAL / SCRATCH | 8 | 9 |
| `111000` / `111010` | MUBUF / MTBUF | 8 | 2 (MUBUF), 9 (MTBUF) |
| `111100` | MIMG (NSA adds dwords) | 8+ | 7 |
| `111101` | SMEM | 8 | 2 |
| `111110` | EXP | 8 | 2 |

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
`s_endpgm` stops a path; `s_setpc`/`s_swappc`/`s_getpc` + jump tables stop translation
(`s_getpc` alone is allowed: it addresses constant data).

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
  dead phis are removed. `Func` carries `idom` and `rpo` for chunk 4. Encodings the decoder
  does not read, and `s_setpc/swappc/rfe/call`, `s_movrel*`, `v_movrel*`, scalar atomics,
  MUBUF-to-LDS are typed `IrError`s. `EntryRegs` names the entry SGPR/VGPR ranges (EXEC is
  always one); `from_record` is a first guess from the root layout. `nativ64-shader --ir`
  prints the IR.
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
   the wave chunk (10).
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

## Inputs and outputs

**User SGPRs.** A VS/PS/CS receives user-data dword i in `s[i]`; merged stages and mesh have 8
system SGPRs first (`s[i+8]`) **[VIctory]**. Seen: the VS's root descriptor (layout `user
data 8, 2 SGPRs`) is read as `s8:s9`; the PS gets the primitive mask in `s10`, right after
its user SGPRs (`s_mov_b32 m0, s10` before `v_interp`) **[seen]**. What `s0..s7` hold is
**[unverified]** (no program of the corpus reads them; a read is an error naming the SGPR).

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
Opus review after each. Module paths are in `crates/nativ64-gpu/src/shader/`.

1. **Container and dump** (done; `container.rs`, `native.rs`, `bin/nativ64-shader.rs`,
   `pipeline.rs` hook). DXBC parts, DXIL header, XDAT, XBCH inflate, FSSM records, layout/used
   tables, VS/PS stage data, signature parts (`ISG1`/`OSG1`: rows, names, indices, masks), PS
   patch. `NATIV64_DUMP_SHADERS`. Tests: synthetic containers (every refusal path). Then run
   the title with the dump and record which pipelines the first frames draw with: that list
   sets the instruction order of chunks 2-7.
2. **Decoder** (`gfx10/`). SOP1/2/C/K/P, SMEM, VOP1/2/C/3 (incl. literal, inline constants,
   modifiers; DPP/SDWA fields decoded), VINTRP, EXP, MUBUF; flow following; `disasm()` in LLVM
   syntax. Tests: per encoding from `llvm-mc` fixtures; the corpus diff (skipping the 24-byte
   marker) when `NATIV64_SHADER_CORPUS` is set.
3. **IR** (done; `ir.rs`): blocks, SSA dwords, implicit operands, phis, entry `Arg`s, constant
   folding, `s_getpc` data. Tests: small programs, SSA shape asserted textually.
4. **Structuring** (`structure.rs`): mask typing, per-lane branches, guards and their
   removal, discard → demote, Ramsey tree, multi-level break lowering plan. Tests: if, if/else,
   nested ifs, loop with break and continue, a 2-level break, an irreducible graph (error), a
   cross-lane use (error).
5. **SPIR-V builder + ALU** (`spirv/`): module builder, types/constants, registers as
   variables, structured flow from chunk 4, scalar and VOP1/2/3/C ALU for f32/i32/u32 and the
   exact-semantics list, `v_cvt_pkrtz`. Tests: `spirv-val` on each test program; opcode
   vectors through a small CPU evaluator of our IR (same table semantics) where a GPU is absent.
6. **I/O** (`io.rs`): user SGPRs and root block, VS system VGPRs and vertex attributes, PS
   input VGPRs and `v_interp` raising, exports, linking by semantic. Milestone: the corpus VS
   and the 9 PS translate and validate.
7. **Resources** (`resource.rs`): tracing table, BDA buffer loads with bounds, view table
   format, MIMG sample/load/resinfo, static samplers. Tests: synthetic programs for each
   `Source`; one error test per refusal.
8. **Vulkan pipelines and draws** (`backend/vk/pipeline.rs`): bindless set and view table
   upkeep at submit, root block per draw, `VkPipeline` from translated stages + `GraphicsState`
   (blend/raster/depth decoded from `raw`), cache, `vkCmdDraw*` replay. Real validation: the
   splash screen against the expected frame.
9. **Widening, driven by the corpus report**: VOP3P and f16, DS, FLAT/GLOBAL, MTBUF and typed
   buffers, remaining VOP3, MIMG gather/compare/derivatives.
10. **Wave ops**: DPP quad permutes → `OpGroupNonUniformQuad*` (fragment) or derivatives,
    `v_readfirstlane` of uniform values = identity, ballot-style mask uses through subgroup ops
    with wave64 emulated in 32-lane hosts as XCL describes; until then they are errors.

Later: compute (`GLCompute`, thread ids, LDS, `USER_DATA` spill of root params past 16 user
SGPRs **[VIctory]**), merged stages and NGG (decode their layouts first), mesh, ray queries.

## Open questions

- Which hardware stages the archive's programs use: if the splash VS is NGG (GS slot) the
  first chunks need merged-stage stage data, not decoded yet. Chunk 1's dump answers it.
- Wave32 bit (record +0x14 bit 5) and the VS system VGPR layout are VIctory's readings, not
  confirmed by the corpus (all wave64, VS reads only v0).
- Stages seen in a title run (8634 distinct blobs): 3297 pixel, 2734 compute, 1569 mesh, 1034
  vertex. Mesh shaders are common, so mesh (and compute) stage data and merged layouts are not
  optional later work: the plan's "Later" list needs reordering. Of these, 1705 programs
  (233315 instructions) decode fully with chunk 2; the rest stop at MIMG (4250 programs), MTBUF
  (1567), DS (640), VOP3P (427), FLAT (45): chunks 7 and 9 come first. 332 programs contain
  `s_setpc`/`s_swappc`/`s_call` (indirect jumps).
- Root-layout and used-resource tables have up to a few hundred entries (the chunk 1 cap of 64
  refused 2486 vertex/pixel blobs; now 512, bounds-checked against the payload).
- VOP3 interpolation (`v_interp_*_e64`) decodes; the f16 forms print `high`, not decoded.
- `s0..s7` of the hardware VS/PS: content unknown; a read stops translation. With entry SGPRs
  from s8 only, 941 of the 1705 fully decoded programs fail on an `s0..s7` read (compute and
  mesh programs read them: 1363 build when s0 is allowed); 332 stop at an indirect jump.
- Kind 0x12 XDAT entry, `PRIV` part (12 bytes), program-record bytes not listed: meaning unknown,
  ignored.
- Bindless limits and `VK_EXT_shader_demote_to_helper_invocation` on lavapipe and the Intel
  driver: to check with `nativ64-vkcheck` before chunk 8.
