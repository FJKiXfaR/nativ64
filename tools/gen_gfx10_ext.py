#!/usr/bin/env python3
"""Generates the vector-memory and packed-math tables of the GFX10 decoder from `llvm-mc`:
crates/nativ64-gpu/src/shader/gfx10/table_ext.rs (MIMG, MTBUF, DS, VOP3P, FLAT) and the fixtures
file next to it (one probe per opcode, then random field patterns with llvm-mc's text).
Same method as gen_gfx10_table.py: LLVM's AMDGPU tables are facts, nothing is linked. Usage:
    gen_gfx10_ext.py <table_ext.rs> <fixtures_ext.txt>      (needs llvm-mc on PATH)
"""
import os
import random
import re
import sys
from concurrent.futures import ThreadPoolExecutor

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from gen_gfx10_table import TOK, dis, split_ops, u  # noqa: E402

POOL = ThreadPoolExecutor(8)
FAMILIES = []


def family(f):
    FAMILIES.append(f)
    return f


def pmap(f, xs):
    return list(POOL.map(f, xs))


def table(name, size, rows):
    L = ['pub static %s: [Option<OpInfo>; %d] = build(&[' % (name, size)]
    for op, nm, slots, flags in rows:
        L.append('    (0x%02x, OpInfo { name: "%s", slots: &[%s], flags: %s }),'
                 % (op, nm, ', '.join(slots), flags))
    L.append(']);')
    L.append('')
    return L


# ---------------------------------------------------------------- MIMG

IMG_FLAGS = {
    'SAMP': 1 << 6, 'GATHER': 1 << 7, 'ATOMIC': 1 << 10, 'CMPSWAP': 1 << 11, 'OFFSET': 1 << 12,
    'BIAS': 1 << 13, 'COMPARE': 1 << 14, 'DERIV': 1 << 15, 'LOD': 1 << 19, 'CLAMP': 1 << 20,
    'MIP': 1 << 21, 'G16': 1 << 22, 'NOCOORD': 1 << 23,
}
COORDS = [1, 2, 3, 3, 2, 3, 3, 4]
GRADS = [1, 2, 3, 2, 1, 2, 2, 2]


def mimg_dw(op, dim=1, dmask=0xf, d1=None, extra=0):
    d0 = 0xF0000000 | (op >> 7) | dim << 3 | dmask << 8 | (op & 0x7f) << 18 | extra
    return [d0, 4 | 3 << 8 | 1 << 16 if d1 is None else d1]


def img_flags(name):
    toks = name[len('image_'):].split('_')
    f = []
    head = toks[0]
    rest = toks[1:]
    if head in ('sample', 'gather4'):
        f.append('SAMP')
        if head == 'gather4':
            f.append('GATHER')
        for t, k in (('c', 'COMPARE'), ('b', 'BIAS'), ('l', 'LOD'), ('cl', 'CLAMP'),
                     ('o', 'OFFSET')):
            if t in rest:
                f.append(k)
        if 'd' in rest or 'cd' in rest:
            f.append('DERIV')
        if 'g16' in rest:
            f.append('G16')
    elif head in ('gather4h',) or (head == 'get' and rest == ['lod']):
        f.append('SAMP')
        if head == 'gather4h':
            f.append('GATHER')
    elif head == 'get':
        f.append('NOCOORD')
    elif head == 'atomic':
        f.append('ATOMIC')
        if 'cmpswap' in rest or 'fcmpswap' in rest:
            f.append('CMPSWAP')
    elif head in ('load', 'store') and 'mip' in rest:
        f.append('MIP')
    return f


def img_naddr(flags, dim, a16):
    """Mirror of the decoder's rule, to pick consistent NSA sizes."""
    n = 0
    if 'NOCOORD' in flags:
        return 1
    for k in ('OFFSET', 'BIAS', 'COMPARE'):
        n += k in flags
    if 'DERIV' in flags:
        n += 2 * ((GRADS[dim] + 1) // 2) if 'G16' in flags else 2 * GRADS[dim]
    packed = COORDS[dim] + sum(k in flags for k in ('LOD', 'CLAMP', 'MIP'))
    return n + ((packed + 1) // 2 if a16 else packed)


@family
def mimg(fix, rnd):
    texts = pmap(lambda op: dis(u(*mimg_dw(op, 1, 0xf)), '1013'), range(256))
    rows = []
    info = {}
    for op, t in enumerate(texts):
        if not t:
            # atomics refuse some masks: retry with a single channel
            t = dis(u(*mimg_dw(op, 1, 1)), '1013')
        if not t:
            continue
        name = t.split()[0]
        fl = img_flags(name)
        slots = ['Slot::ImgData', 'Slot::ImgAddr', 'Slot::ImgRsrc']
        if 'SAMP' in fl:
            slots.append('Slot::ImgSamp')
        flags = sum(IMG_FLAGS[k] for k in fl)
        rows.append((op, name, slots, flags))
        info[op] = (name, fl)
        pm = (3 if 'CMPSWAP' in fl else 1) if 'ATOMIC' in fl else 0xf
        fix.append((u(*mimg_dw(op, 0 if 'ATOMIC' in fl else 1, pm)),))
    # the ray-intersection opcodes only decode with fixed mask, dimension and unorm / r128 bits
    for op, nm, bits in ((0xe6, 'bvh_intersect_ray', 0), (0xe7, 'bvh64_intersect_ray', 1)):
        d0 = 0xF0000000 | (op >> 7) | 3 << 1 | 0xf << 8 | 1 << 12 | 1 << 15 | (op & 0x7f) << 18
        t = dis(u(d0, 0x27 | 0x27 << 8 | 7 << 16, 0x201e1824, 0x3e2c3f29, 0), '1013')
        assert t and 'bvh' in t, t
        rows.append((op, 'image_' + nm, ['Slot::ImgData', 'Slot::ImgAddr', 'Slot::ImgRsrc'],
                     (1 << 24) | (bits << 25)))
        for _ in range(8):
            fix.append((u(d0 | rnd.getrandbits(1) << 14,
                          rnd.randrange(200) | rnd.randrange(200) << 8 | rnd.randrange(8) << 16,
                          rnd.getrandbits(32), rnd.getrandbits(32), rnd.getrandbits(32)),))
    # random patterns: every opcode, every dimension it is valid for
    cases = []
    for _ in range(2600):
        op = rnd.choice(list(info))
        name, fl = info[op]
        dim = rnd.randrange(8)
        if dim >= 6 and ('SAMP' in fl or 'MIP' in fl):
            continue
        if 'ATOMIC' in fl and dim != 0:
            continue
        a16 = rnd.random() < 0.3
        d16 = rnd.random() < 0.2
        if 'ATOMIC' in fl:
            dmask = rnd.choice([3, 15] if 'CMPSWAP' in fl else [1, 3])
        elif 'GATHER' in fl:
            dmask = rnd.choice([1, 2, 4, 8])
        else:
            dmask = rnd.randrange(1, 16)
        n = img_naddr(fl, dim, a16)
        d0tfe = rnd.getrandbits(1)
        if 'GATHER' in fl and d16 and d0tfe:
            # LLVM prints nonsense for a d16 gather with tfe (wrong data and address widths)
            d0tfe = 0
        nsa = 0
        if n > 1 and rnd.random() < 0.5:
            nsa = (n - 1 + 3) // 4
        d0 = (0xF0000000 | (op >> 7) | nsa << 1 | dim << 3 | rnd.getrandbits(1) << 7
              | dmask << 8 | rnd.getrandbits(6) << 12 & ~(1 << 16) | d0tfe << 16 | (op & 0x7f) << 18
              | rnd.getrandbits(1) << 25)
        d0 &= ~(1 << 14)
        d1 = (rnd.randrange(200) | rnd.randrange(200) << 8 | rnd.randrange(8) << 16
              | rnd.randrange(8) << 21 | a16 << 30 | d16 << 31)
        w = [d0, d1]
        for _ in range(nsa):
            w.append(rnd.getrandbits(32))
        # LLVM reads the sampler / lwe bits only for the opcodes that have them
        cases.append(u(*w))
    fix.extend((c,) for c in cases)
    return ('MIMG', 256, rows)


# ---------------------------------------------------------------- MTBUF


def mtbuf_dw(op, fmt=4, **kw):
    d0 = 0xE8000000 | (op & 7) << 16 | fmt << 19 | kw.get('d0', 0)
    d1 = 0x00010304 | (op >> 3) << 21 | 4 << 24 | kw.get('d1', 0)
    return [d0, d1]


@family
def mtbuf(fix, rnd):
    rows = []
    for op in range(16):
        t = dis(u(*mtbuf_dw(op)), '1013')
        if not t:
            continue
        name = t.split()[0]
        m = re.search(r'^\S+ v(?:\[(\d+):(\d+)\]|(\d+))', t)
        w = int(m.group(2)) - int(m.group(1)) + 1 if m.group(1) else 1
        rows.append((op, name, ['Slot::Vdata(%d)' % w], 0))
        fix.append((u(*mtbuf_dw(op)),))
    for _ in range(900):
        op = rnd.randrange(16)
        d0 = (rnd.getrandbits(12) | rnd.getrandbits(4) << 12 | rnd.getrandbits(3) << 16
              | rnd.randrange(128) << 19 | 0xE8000000)
        d0 = (d0 & ~(7 << 16)) | (op & 7) << 16
        d1 = (rnd.randrange(200) | rnd.randrange(200) << 8 | rnd.randrange(8) << 16
              | (op >> 3) << 21 | rnd.getrandbits(1) << 22 | rnd.randrange(256) << 24)
        fix.append((u(d0, d1),))
    # the unified format names, by value
    names = []
    for f in range(128):
        t = dis(u(*mtbuf_dw(0, f)), '1013')
        m = re.search(r'format:\[(\w+)\]', t or '')
        names.append(m.group(1) if m else '')
    last = max(i for i, n in enumerate(names) if n)
    FORMATS[:] = names[:last + 1]
    return ('MTBUF', 16, rows)


FORMATS = []


# ---------------------------------------------------------------- DS

DS_FIELDS = {4: 'addr', 16: 'data0', 32: 'data1', 48: 'vdst'}


def ds_dw(op, off, gds, mask, regs=(4, 16, 32, 48)):
    d1 = 0
    for k in range(4):
        if mask >> k & 1:
            d1 |= regs[k] << (8 * (k if k < 3 else 3))
    d1 = ((regs[0] if mask & 1 else 0) | (regs[1] if mask & 2 else 0) << 8
          | (regs[2] if mask & 4 else 0) << 16 | (regs[3] if mask & 8 else 0) << 24)
    return [0xD8000000 | op << 18 | off | gds << 17, d1]


def ds_form(op):
    """The accepted field mask of an opcode (max operands), whether it takes gds, its text."""
    best = None
    for gds in (0, 1):
        for mask in range(16):
            t = dis(u(*ds_dw(op, 0x0201, gds, mask)), '1013')
            if t and t.startswith('ds_') and (best is None or bin(mask).count('1')
                                              > bin(best[1]).count('1')):
                best = (gds, mask, t)
    return best


@family
def ds(fix, rnd):
    rows = []
    forms = pmap(ds_form, range(256))
    for op, f in enumerate(forms):
        if not f:
            continue
        gds, mask, t = f
        name = t.split()[0]
        toks = split_ops(t.partition(' ')[2])
        slots = []
        for tok in toks:
            m = TOK.match(tok.split(' ')[0])
            if not m:
                continue
            n = int(m.group(2) if m.group(2) is not None else m.group(3))
            w = 1 if m.group(2) is not None else int(m.group(4)) - int(m.group(3)) + 1
            slots.append({4: 'Slot::DsAddr', 16: 'Slot::DsData0(%d)', 32: 'Slot::DsData1(%d)',
                          48: 'Slot::VDst(%d)'}[n].replace('%d', str(w)))
        flags = 0
        if 'offset0:' in t:
            flags |= 1 << 6
        if 'swizzle(' in t:
            flags |= 1 << 7
        rows.append((op, name, slots, flags))
        fix.append((u(*ds_dw(op, 0x0201, gds, mask)),))
        for _ in range(8):
            off = rnd.getrandbits(16)
            if flags & 128:
                kind = rnd.randrange(5)
                if kind == 0:
                    off = 0x8000 | rnd.getrandbits(8)
                elif kind == 1:
                    off = 0x1f | (1 << rnd.randrange(5)) << 10
                elif kind == 2:
                    off = 0x1f | ((1 << rnd.randrange(1, 6)) - 1) << 10
                elif kind == 3:
                    g = 1 << rnd.randrange(1, 6)
                    off = (0x1f - g + 1) | rnd.randrange(g) << 5
                else:
                    off &= 0x7fff
            regs = [rnd.randrange(200) for _ in range(4)]
            g = (rnd.random() < 0.3) or gds
            fix.append((u(*ds_dw(op, off, g, mask, regs)),))
    return ('DS', 256, rows)


def run():
    out_t, out_f = sys.argv[1], sys.argv[2]
    rnd = random.Random(1013)
    L = ['// @generated by tools/gen_gfx10_ext.py from `llvm-mc` output: do not edit.',
         '// Vector-memory and packed-math encodings: one row per opcode LLVM disassembles for gfx1013.',
         'use super::ops::{build, OpInfo, Slot};', '']
    fixtures = []
    for f in FAMILIES:
        fix = []
        name, size, rows = f(fix, rnd)
        L.extend(table(name, size, rows))
        if name == 'MTBUF':
            L.append('/// Unified buffer format names by value (empty: LLVM has none). Value 1 is the default and')
            L.append('/// is not printed.')
            L.append('pub static UFMT: [&str; %d] = [' % len(FORMATS))
            L.extend('    "%s",' % n for n in FORMATS)
            L.append('];')
            L.append('')
        for (b,) in fix:
            fixtures.append(b)
        print(name, len(rows), 'rows', len(fix), 'cases', file=sys.stderr)
    texts = pmap(lambda b: dis(b, '1013'), fixtures)
    lines = ['%s\t%s' % (b.hex(), ' '.join(t.split())) for b, t in zip(fixtures, texts) if t]
    open(out_t, 'w').write('\n'.join(L))
    open(out_f, 'w').write('\n'.join(lines) + '\n')
    print(len(lines), 'of', len(fixtures), 'fixtures decode', file=sys.stderr)


if __name__ == '__main__':
    run()
