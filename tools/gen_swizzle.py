#!/usr/bin/env python3
"""Generate crates/nativ64-gpu/src/texture/swizzle_tables.rs from AMD's addrlib.

Reads addrlib's `gfx10/gfx10SwizzlePattern.h` (MIT, part of Mesa, fetched into ref/mesa, see
docs/textures.md) and writes the GFX10.3 "RBPLUS" pattern tables our texture layouts use:
for each swizzle mode, the pattern rows of the two console configurations (Series X and
Series S) and of the unaligned base (used by the modes that do not XOR), plus the nibble
tables they index, renumbered so that only the entries in use remain.

    python -I tools/gen_swizzle.py \
        --header ref/mesa/src/amd/addrlib/src/gfx10/gfx10SwizzlePattern.h \
        --out crates/nativ64-gpu/src/texture/swizzle_tables.rs
"""

import argparse
import re
import sys

# Pattern tables we need: addrlib name -> Rust name. 3D "S3" and "D3" tables are the thick
# block patterns; "_1xaa" are the single-sample patterns (MSAA is not supported).
TABLES = [
    ("256_S", "SW_256B_S"),
    ("256_D", "SW_256B_D"),
    ("4K_S", "SW_4KB_S"),
    ("4K_D", "SW_4KB_D"),
    ("4K_S_X", "SW_4KB_S_X"),
    ("4K_D_X", "SW_4KB_D_X"),
    ("4K_S3", "SW_4KB_S_3D"),
    ("4K_S3_X", "SW_4KB_S_X_3D"),
    ("64K_S", "SW_64KB_S"),
    ("64K_D", "SW_64KB_D"),
    ("64K_S_T", "SW_64KB_S_T"),
    ("64K_D_T", "SW_64KB_D_T"),
    ("64K_S_X", "SW_64KB_S_X"),
    ("64K_D_X", "SW_64KB_D_X"),
    ("64K_R_X_1xaa", "SW_64KB_R_X"),
    ("64K_Z_X_1xaa", "SW_64KB_Z_X"),
    ("64K_S3", "SW_64KB_S_3D"),
    ("64K_S3_X", "SW_64KB_S_X_3D"),
    ("64K_S3_T", "SW_64KB_S_T_3D"),
    ("64K_D3_X", "SW_64KB_D_X_3D"),
]

# Console configurations: (pipes_log2, packers_log2). Series X, then Series S.
CONFIGS = [(5, 3), (3, 2)]
BPE_COUNT = 5  # 1, 2, 4, 8, 16 bytes per element (addrlib MaxNumOfBpp)

# addrlib's ADDR_BIT_SETTING: channel c (X=0, Y=1, Z=2, S=3), bit i is bit (c << 4) + i.
CHANNELS = {"X": 0, "Y": 1, "Z": 2, "S": 3}

MIT = """\
Copyright (C) 2007-2026 Advanced Micro Devices, Inc. All rights reserved.

Permission is hereby granted, free of charge, to any person obtaining a copy of this
software and associated documentation files (the "Software"), to deal in the Software
without restriction, including without limitation the rights to use, copy, modify, merge,
publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons
to whom the Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or
substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED,
INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR
PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE
FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE."""


def block_after(text, decl):
    """The text between the braces of the array declared by `decl`."""
    m = re.search(re.escape(decl) + r"\s*=\s*\{(.*?)\n\};", text, re.S)
    if not m:
        sys.exit(f"gen_swizzle: {decl} not found in the header")
    return m.group(1)


def term_mask(expr):
    """`Z0^X4^Y4` -> bit mask of ADDR_BIT_SETTING; `0` -> 0."""
    expr = expr.strip()
    if expr == "0":
        return 0
    mask = 0
    for tok in expr.split("^"):
        m = re.fullmatch(r"([XYZS])(\d+)", tok.strip())
        if not m:
            sys.exit(f"gen_swizzle: unknown pattern term {tok!r} in {expr!r}")
        mask |= 1 << ((CHANNELS[m.group(1)] << 4) + int(m.group(2)))
    return mask


def nibble_table(text, name, width):
    body = block_after(text, f"const UINT_64 GFX10_SW_PATTERN_{name}[][{width}]")
    rows = []
    for line in body.splitlines():
        m = re.match(r"\s*\{(.*)\}\s*,\s*//\s*(\d+)\s*$", line)
        if not m:
            continue
        cells = [c for c in m.group(1).split(",") if c.strip()]
        if len(cells) != width or int(m.group(2)) != len(rows):
            sys.exit(f"gen_swizzle: bad {name} row: {line!r}")
        rows.append([term_mask(c) for c in cells])
    return rows


def patinfo_rows(text, table):
    body = block_after(text, f"const ADDR_SW_PATINFO GFX10_SW_{table}_RBPLUS_PATINFO[]")
    rows = []
    for line in body.splitlines():
        m = re.match(
            r"\s*\{\s*(\d+),\s*(\d+),\s*(\d+),\s*(\d+),\s*(\d+),\s*\}\s*,\s*//\s*(.*)$", line
        )
        if m:
            rows.append(([int(g) for g in m.group(2, 3, 4, 5)], m.group(6)))
    return rows


def config_base(pipes_log2, pkrs_log2):
    """addrlib's m_colorBaseIndex for a GB_ADDR_CONFIG (HwlInitGlobalParams)."""
    base = pipes_log2 * BPE_COUNT
    if pkrs_log2 >= 2:
        base += (2 * pkrs_log2 - 2) * BPE_COUNT
    return base


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--header", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    with open(args.header, encoding="utf-8") as f:
        text = f.read()

    nib = {
        "01": nibble_table(text, "NIBBLE01", 8),
        "2": nibble_table(text, "NIBBLE2", 4),
        "3": nibble_table(text, "NIBBLE3", 4),
        "4": nibble_table(text, "NIBBLE4", 4),
    }
    used = {k: {} for k in nib}  # old index -> new index, in order of first use

    def renumber(kind, old):
        return used[kind].setdefault(old, len(used[kind]))

    tables = []
    for table, rust in TABLES:
        rows = patinfo_rows(text, table)
        out = []
        # row groups: the unaligned base (rows 0..5), then each console configuration
        starts = [0] + [config_base(p, k) for p, k in CONFIGS]
        for start in starts:
            group = []
            for i in range(BPE_COUNT):
                fields, comment = rows[start + i]
                if start:
                    want = re.search(r"(\d+) pipes \((\d+) PKRs\) (\d+) bpe", comment)
                    cfg = CONFIGS[starts.index(start) - 1]
                    ok = (
                        want
                        and int(want.group(1)) == 1 << cfg[0]
                        and int(want.group(2)) == 1 << cfg[1]
                        and int(want.group(3)) == 1 << i
                    )
                    if not ok:
                        sys.exit(f"gen_swizzle: {table} row {start + i} is {comment!r}")
                n01, n2, n3, n4 = fields
                group.append(
                    (
                        renumber("01", n01),
                        renumber("2", n2),
                        renumber("3", n3),
                        renumber("4", n4),
                    )
                )
            out.append(group)
        tables.append((rust, table, out))

    w = []
    w.append("//! Swizzle patterns of GFX10.3 (RB+), generated by `tools/gen_swizzle.py` from AMD's addrlib")
    w.append("//! (`gfx10/gfx10SwizzlePattern.h`). Do not edit; see `docs/textures.md` and `THIRD_PARTY.md`.")
    w.append("//!")
    for line in MIT.splitlines():
        w.append(("//! " + line).rstrip())
    w.append("")
    w.append("/// One pattern row: indices into the four nibble tables below (address bits 0-7, 8-11,")
    w.append("/// 12-15 and 16-19).")
    w.append("#[derive(Clone, Copy, Debug)]")
    w.append("pub struct PatInfo {")
    w.append("    pub n01: u16,")
    w.append("    pub n2: u16,")
    w.append("    pub n3: u16,")
    w.append("    pub n4: u16,")
    w.append("}")
    w.append("")
    w.append("/// Configuration groups of a pattern table: the unaligned base (modes without XOR), Series X,")
    w.append("/// Series S. Each holds one row per element size (1, 2, 4, 8, 16 bytes).")
    w.append("pub type Rows = [[PatInfo; 5]; 3];")
    w.append("")
    w.append("// Address bit terms: bit `(channel << 4) + index`, channel X=0, Y=1, Z=2, S=3; the terms of")
    w.append("// one address bit are XORed.")
    for kind, width, name in (("01", 8, "NIBBLE01"), ("2", 4, "NIBBLE2"), ("3", 4, "NIBBLE3"), ("4", 4, "NIBBLE4")):
        order = sorted(used[kind], key=lambda o: used[kind][o])
        w.append("")
        w.append(f"pub static {name}: [[u64; {width}]; {len(order)}] = [")
        for old in order:
            cells = ", ".join(f"0x{m:x}" for m in nib[kind][old])
            w.append(f"    [{cells}],")
        w.append("];")
    for rust, table, groups in tables:
        w.append("")
        w.append(f"/// addrlib `GFX10_SW_{table}_RBPLUS_PATINFO`.")
        w.append(f"pub static {rust}: Rows = [")
        for group in groups:
            w.append("    [")
            for r in group:
                w.append(f"        PatInfo {{ n01: {r[0]}, n2: {r[1]}, n3: {r[2]}, n4: {r[3]} }},")
            w.append("    ],")
        w.append("];")
    w.append("")
    with open(args.out, "w", encoding="utf-8") as f:
        f.write("\n".join(w))


if __name__ == "__main__":
    main()
