# Console texture layouts (GFX10.3)

Textures on the console live in title memory in the GPU's own tiled ("swizzled") layout, and
the title's archives store texture data already in that layout. So the sizes we report
(`GetResourceAllocationInfo`, `xg_xs`), the offsets the title writes to, and our detiler for
Vulkan uploads must all follow the hardware's addressing exactly. This document specifies our
own implementation in `crates/nativ64-gpu/src/texture/` and `xg_xs.rs`.

## Sources

- **AMD addrlib** (MIT, part of Mesa: `src/amd/addrlib`), the GPU vendor's own addressing
  library. Fetch it read-only, sparse, into `ref/mesa` (git-ignored):
  `git clone --depth 1 --filter=blob:none --sparse https://gitlab.freedesktop.org/mesa/mesa.git
  ref/mesa && git -C ref/mesa sparse-checkout set src/amd/addrlib`. Our swizzle tables are
  **generated** from its `gfx10/gfx10SwizzlePattern.h` by `tools/gen_swizzle.py`; the
  generated Rust file carries addrlib's MIT notice (also listed in a `THIRD_PARTY.md`). The
  algorithms (block dimensions, mip tail, equations) are written by us from its
  `core/addrlib2.cpp` and `gfx10/gfx10addrlib.cpp`.
- **VIctory** and **XCL** (GPL, read only, facts): which modes the title uses, the `xg_xs`
  interface, the console's pipe configuration. XCL checked its sizes against the console's own
  library on sample textures; where the two differ, XCL wins.

## Facts

- **Swizzle modes** (addrlib numbering): 0 LINEAR, 1/2 256B_S/D, 5/6 4KB_S/D, 9/10 64KB_S/D,
  17/18 64KB_S_T/D_T, 21-23 4KB_S_X/D_X/R_X, 24-27 64KB_Z_X/S_X/D_X/R_X, 28-31 VAR (not
  supported: refuse). Alignment = block size (256 B, 4 KiB, 64 KiB); linear: 256.
- **Resource `Layout` field**: 0 = the library chooses, 1 = linear, `0x100 | mode` = explicit.
  `XGSuggestSwizzleMode` writes `0x100 | mode`.
- **Library choice** (layout 0): 1D → linear; depth/stencil (flag 0x2) → 64KB_Z_X; render
  target (0x1) → 64KB_R_X; UAV only (0x4, no RT) → 4KB_S_X when bits per pixel × w × h ≤
  0x80000, else 64KB_R_X; anything else → 4KB_S. Flag bits 0x100000 and 0x200000 are
  console-only and do not affect the layout (both references ignore them).
- **Console configuration**: GFX10.3 with RB+ (addrlib's "RBPLUS" tables). Series X: 32
  pipes, 8 packers (`pipes_log2 = 5`, `pkrs_log2 = 3`); Series S: 8 pipes, 4 packers (3, 2).
  We report a Series X (`NATIV64_CONSOLE=series-s` later if needed). Only the XOR modes
  (`_T`, `_X`, mode ≥ 16) depend on it; the pattern group index follows addrlib's
  `colorBaseIndex` rule. The pipe/bank XOR value of a surface comes from the low bits of its
  base address (`base & (block - 1)`), as addrlib's `ComputeOffsetFromEquation` expects.
- **Block dimensions**: thin blocks from `elements_log2 = block_log2 - elem_log2 -
  samples_log2` split between x and y (x gets the extra bit); thick (3D) blocks for every S/D
  mode of a 3D texture (`_S`, `_D`, `_S_X`, `_D_X`, `_S_T`, `_D_T`, addrlib's `isStd` /
  `isDisp`, with its dedicated 3D tables); only `_Z_X` and `_R_X` stay thin in 3D. 4KB_R_X
  (mode 23) has no pattern in addrlib and is refused. Example: 64 KiB, 4-byte
  elements → 128 × 128.
- **Mip chain**: each mip padded to whole blocks; one block holds the **mip tail** (the
  smallest mips) at offset 0 of the slice, then mips from smallest to largest, mip 0 last.
  Tail rules (first tail mip, max mips in tail, position of each tail mip) as in addrlib's
  `GetMaxNumMipsInTail` / `GetMipTailDim`; single-mip textures have no tail. Array slices
  follow each other at the slice size.
- **Formats**: element size and block-compressed (BC: 4 × 4 texels in 8 or 16 bytes) per DXGI
  format, plus console-only formats 116-120 and 189-191 (to fill as the title uses them).
  Depth + stencil formats have two planes (depth, then stencil aligned to its plane alignment).
- **Metadata planes**: a single-mip render target has a CMASK plane: (w and h padded to 1024)
  × / 128 bytes per slice, aligned 0x2000, after the colour plane; a depth buffer has an
  HTILE plane (same / 16, aligned 64 KiB). Included in the reported size (what the console
  library returns). Never read by us; only reserved.
- **Reference value**: 3840 × 2160 R8G8B8A8_UNORM, render target + UAV, 1 mip, layout 0 →
  64KB_R_X, 30 × 17 blocks of 128 × 128 = 33,423,360 bytes, + CMASK 98,304 at that offset →
  size 33,521,664, alignment 65,536.
- **`xg_xs` exports**: `XGCreateTextureComputer(const D3D12_RESOURCE_DESC *, computer **)` and
  `XGSuggestSwizzleMode(desc, flags (0), u32 *out)`. The computer has no QueryInterface: slot
  0 AddRef, 1 Release, 2 `GetResourceLayout(XG_RESOURCE_LAYOUT *)` (0x1B40 bytes: totals,
  then 4 planes of 0x6C8, each with up to 15 mips of 0x70; field offsets in the module),
  3 size, 4 base alignment, 5 mip offset, 6 `GetTexelElementOffsetBytes(plane, mip, x, y,
  z_or_slice, sample, u32 *bit)`, 7 texel coordinate (stub), 8 `CopyIntoSubresource`,
  9 `CopyFromSubresource`, 10 `GetResourceTiling` (64 KiB modes), 11-12 stubs. The title calls
  1, 2, 6, 8, 9, 10. VIctory and XCL disagree on slot 9's arguments: confirm at a call site.

## Design

| Module | Content |
|---|---|
| `texture/format.rs` | DXGI (+ console) format → element bytes, BC block, planes. |
| `texture/swizzle_tables.rs` | Generated from addrlib (MIT notice): RB+ pattern info, nibble tables. |
| `texture/equation.rs` | Per mode × element size × config: the address equation as three per-axis tables, so `offset = base_xor ^ ox[x] ^ oy[y] ^ oz[z]` within a block (GF(2)-linear, as the hardware). |
| `texture/layout.rs` | `Layout::new(desc) -> Result<Layout>`: mode choice, block dims, padded mips, tail, planes, metadata, size, alignment, per-subresource offset/pitch. Pure, no I/O. |
| `texture/tile.rs` | `detile(layout, plane, subresource, tiled, linear_out, row_pitch)` and `tile(...)`, whole blocks at a time; multi-threaded above 1 MiB later. |
| `xg_xs.rs` | The two exports and the texture computer object (our `xobj`, `Iface::XgTextureComputer`, no QI). |
| `d3d12/resource.rs` | `GetResourceAllocationInfo` and resource layouts use `texture::Layout`. |

Errors are values (`LayoutError`: unsupported mode, MSAA, VAR, too large), turned into
`E_INVALIDARG`/`E_NOTIMPL` at the API, with one stderr line naming what is missing. MSAA is
refused until the title needs it.

## Tests

- The reference value above, plus a table of hand-checked cases (linear pitch padding,
  4KB_S small texture, BC1/BC3 mip chains with a tail, a depth+stencil two-plane format, a 3D
  thick texture, an array) with both references agreeing.
- Properties: within a block the equation is a bijection onto the block; every texel offset of
  every subresource lies inside the plane and no two subresources overlap; `tile` then
  `detile` returns the input for every supported mode, element size and a few sizes.
- `xg_xs`: layout struct offsets (size asserts), a computer's numbers equal `Layout`'s, error
  cases (null desc/out, VAR, MSAA).
- Later, an oracle: build addrlib (MIT) as a test-only tool and compare sizes and offsets over
  many random descriptions.

## Chunks

1. T1: `format`, generator + tables, `equation`, `layout` (sizes, tail, planes) with tests;
   `GetResourceAllocationInfo` uses it. The title's run passes the texture-size stop.
2. T2: `xg_xs` exports and the texture computer (slots above), `tile`/`detile`.
3. T3 (with G3): uploads to Vulkan images through `detile`.
