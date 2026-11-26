#!/usr/bin/env python3
"""Generates crates/nativ64-gpu/src/shader/gfx10/table.rs from `llvm-mc` (LLVM AMDGPU, as facts).

Every (encoding, opcode) is assembled into a probe instruction with distinctive field values,
disassembled with `llvm-mc -mcpu=gfx1013` (then gfx1030 for opcodes only that target has),
and the operand text is mapped back to the encoding fields that produced it. Usage:
    gen.py <out table.rs>          (needs llvm-mc on PATH)
"""
import re
import struct
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor

CPU = {'1030': '-mcpu=gfx1030', '1013': '-mcpu=gfx1013'}


def dis(b, cpu, wave=64):
    hexs = ','.join('0x%02x' % x for x in b)
    a = ['llvm-mc', '-arch=amdgcn', CPU[cpu], '-mattr=+wavefrontsize%d' % wave,
         '-disassemble', '-show-encoding']
    try:
        p = subprocess.run(a, input=hexs + '\n', capture_output=True, text=True, timeout=30)
    except Exception:
        return None
    out = [l for l in p.stdout.splitlines() if 'encoding:' in l]
    if len(out) != 1:
        return None
    t, e = out[0].split('; encoding:')
    if len(re.findall(r'0x', e)) != len(b):
        return None
    t = t.strip()
    if 'illegal' in t or 'Invalid' in t or 'invalid' in t:
        return None
    return t


def u(*dw):
    return b''.join(struct.pack('<I', x & 0xffffffff) for x in dw)


D, S0, S1, S2 = 16, 32, 48, 64
V = 256


def probes(enc, op):
    if enc == 'sop1':
        yield u(0xBE800000 | D << 16 | op << 8 | S0)
    elif enc == 'sop2':
        yield u(0x80000000 | op << 23 | D << 16 | S1 << 8 | S0)
    elif enc == 'sopc':
        yield u(0xBF000000 | op << 16 | S1 << 8 | S0)
    elif enc == 'sopk':
        yield u(0xB0000000 | op << 23 | D << 16 | 0x1234)
        yield u(0xB0000000 | op << 23 | D << 16 | 0x1234, 0x5678)
    elif enc == 'sopp':
        yield u(0xBF800000 | op << 16 | 0x1234)
        yield u(0xBF800000 | op << 16)
    elif enc == 'smem':
        yield u(0xF4000000 | op << 18 | D << 6 | 4, 0x40)
    elif enc == 'vop1':
        yield u(0x7E000000 | D << 17 | op << 9 | V + S0)
        yield u(0x7E000000 | op << 9)
        yield u(0x7E000000 | D << 17 | op << 9 | S0)
    elif enc == 'vop2':
        yield u(op << 25 | D << 17 | S1 << 9 | V + S0)
        yield u(op << 25 | D << 17 | S1 << 9 | S0)
        yield u(op << 25 | D << 17 | S1 << 9 | 255, 0xdeadbeef)
    elif enc == 'vopc':
        yield u(0x7C000000 | op << 17 | S1 << 9 | V + S0)
        yield u(0x7C000000 | op << 17 | S1 << 9 | S0)
    elif enc == 'vop3':
        for a in (V, 0):
            for b in (V, 0):
                for c in (V, 0):
                    yield u(0xD4000000 | op << 16 | D, (a + S0) | (b + S1) << 9 | (c + S2) << 18)
    elif enc == 'vintrp':
        yield u(0xC8000000 | D << 18 | op << 16 | 2 << 10 | 1 << 8 | 0)
    elif enc == 'mubuf':
        yield u(0xE0000000 | op << 18, 24 | 16 << 8 | 1 << 16)
    elif enc == 'exp':
        yield u(0xF8000000 | 0xf, 1 | 2 << 8 | 3 << 16 | 4 << 24)


NOPS = {'sop1': 256, 'sop2': 96, 'sopc': 128, 'sopk': 29, 'sopp': 128, 'smem': 256, 'vop1': 256,
        'vop2': 62, 'vopc': 256, 'vop3': 1024, 'vintrp': 3, 'mubuf': 256, 'exp': 1}

# field value -> slot name, per encoding
FIELDS = {
    'sop1': {D: 'SDst', S0: 'SSrc0'},
    'sop2': {D: 'SDst', S0: 'SSrc0', S1: 'SSrc1'},
    'sopc': {S0: 'SSrc0', S1: 'SSrc1'},
    'sopk': {D: 'SDst'},
    'sopp': {},
    'smem': {D: 'SData', 8: 'SBase'},
    'vop1': {D: 'VDst', S0: 'Src0'},
    'vop2': {D: 'VDst', S0: 'Src0', S1: 'VSrc1'},
    'vopc': {S0: 'Src0', S1: 'VSrc1'},
    'vop3': {D: 'VDst', S0: 'Src0', S1: 'Src1', S2: 'Src2', 0: 'SDstB'},
    'vintrp': {},
    'mubuf': {D: 'Vdata'},
    'exp': {},
}

TOK = re.compile(r'^(v|s|ttmp)(?:(\d+)|\[(\d+):(\d+)\])$')


def split_ops(s):
    out, depth, cur = [], 0, ''
    for ch in s:
        if ch in '([':
            depth += 1
        elif ch in ')]':
            depth -= 1
        if ch == ',' and depth == 0:
            out.append(cur.strip())
            cur = ''
        else:
            cur += ch
    if cur.strip():
        out.append(cur.strip())
    return out


def parse(text):
    name, _, rest = text.partition(' ')
    toks = split_ops(rest)
    if toks and ' ' in toks[-1] and '(' not in toks[-1]:
        toks[-1] = toks[-1].split(' ')[0]
    return name, toks


def slots_of(enc, toks, wave_toks):
    """Map tokens to slots. Returns (slots list, immfmt) or None when not representable."""
    fields = FIELDS[enc]
    slots = []
    lits = 0
    imm = None
    for i, t in enumerate(toks):
        m = TOK.match(t)
        if m:
            cls, a, lo, hi = m.groups()
            n = int(a if a is not None else lo)
            w = 1 if a is not None else int(hi) - int(lo) + 1
            f = fields.get(n)
            if f is None:
                return None
            # width in wave32 text
            mw = TOK.match(wave_toks[i]) if i < len(wave_toks) else None
            if mw:
                a2, lo2, hi2 = mw.group(2), mw.group(3), mw.group(4)
                w32 = 1 if a2 is not None else int(hi2) - int(lo2) + 1
                if w32 != w:
                    w = 0  # wave-size dependent mask
            if f == 'VDst' and cls == 's':
                f = 'VDstS'
            if f == 'VDst' and enc == 'vop3' and cls == 'v':
                pass
            slots.append(('R', f, w))
        elif re.match(r'^attr\d+\.[xyzw]$', t):
            slots.append(('A', 'Attr', 0))
        elif t in ('vcc', 'vcc_lo', 'exec', 'exec_lo', 'null', 'off'):
            slots.append(('F', 'vcc' if t.startswith('vcc') else t, 0))
        elif t.startswith('0xdeadbeef') or t.startswith('0xbeef'):
            lits += 1
            slots.append(('L', None, 0))
        elif re.match(r'^-?\d+$', t) or re.match(r'^0x[0-9a-f]+$', t) or t.startswith('hwreg'):
            slots.append(('I', t, 0))
        else:
            return None
    return slots


def run(args):
    enc, op = args
    got = {}
    for cpu in ('1013', '1030'):
        for b in probes(enc, op):
            t = dis(b, cpu, 64)
            if t:
                got[cpu] = (b, t)
                break
    if not got:
        return enc, op, None
    cpu = '1013' if '1013' in got else '1030'
    b, t = got[cpu]
    t32 = dis(b, cpu, 32) or t
    extra = 0
    if enc == 'smem':
        g = dis(u(struct.unpack('<I', b[:4])[0] | 1 << 16, struct.unpack('<I', b[4:8])[0]), cpu)
        d = dis(u(struct.unpack('<I', b[:4])[0] | 1 << 14, struct.unpack('<I', b[4:8])[0]), cpu)
        extra = (4 if g and ' glc' in g else 0) | (8 if d and ' dlc' in d else 0)
    if enc == 'vop3':
        d0, d1 = struct.unpack('<II', b[:8])
        has_sdst = bool(re.search(r' s\[0:1\]| s0,', t))
        for k in range(3):
            ok = False
            if not has_sdst:
                a = dis(u(d0 | 0x100 << k, d1), cpu)
                ok = bool(a and '|' in a)
            a = dis(u(d0, d1 | 1 << (29 + k)), cpu)
            ok = ok or bool(a and ('-' in a or 'neg(' in a) and a != t)
            if ok:
                extra |= 1 << (16 + k)
        if not has_sdst:
            a = dis(u(d0 | 0x800, d1), cpu)
            if a and 'op_sel' in a:
                extra |= 32
                extra |= (a.split('op_sel:[')[1].split(']')[0].count(',') + 1) << 12  # entries
        a = dis(u(d0 | 0x8000, d1), cpu)
        if a and 'clamp' in a:
            extra |= 256
        a = dis(u(d0, d1 | 3 << 27), cpu)
        if a and ('mul:' in a or 'div:' in a):
            extra |= 512
    lb = None
    LIT = 0xdeadbeef
    if enc == 'vop1':
        lb = u(0x7E000000 | D << 17 | op << 9 | 255, LIT)
    elif enc == 'vop2':
        lb = u(op << 25 | D << 17 | S1 << 9 | 255, LIT)
    elif enc == 'vopc':
        lb = u(0x7C000000 | op << 17 | S1 << 9 | 255, LIT)
    elif enc == 'vop3':
        lb = u(0xD4000000 | op << 16 | D, 255 | (V + S1) << 9 | (V + S2) << 18, LIT)
    elif enc in ('sop1', 'sop2', 'sopc'):
        if enc == 'sop1':
            lb = u(0xBE800000 | D << 16 | op << 8 | 255, LIT)
        elif enc == 'sop2':
            lb = u(0x80000000 | op << 23 | D << 16 | S1 << 8 | 255, LIT)
        else:
            lb = u(0xBF000000 | op << 16 | S1 << 8 | 255, LIT)
    if lb:
        lt = dis(lb, cpu)
        if lt and '0xbeef' in lt.replace('0xdeadbeef', ''):
            extra |= 16
    return enc, op, (cpu, b, t, t32, extra)


def build():
    jobs = [(e, o) for e in NOPS for o in range(NOPS[e])]
    res = {}
    with ThreadPoolExecutor(8) as ex:
        for enc, op, r in ex.map(run, jobs):
            if r:
                res.setdefault(enc, {})[op] = r
    return res


SLOT_RS = {
    'SDst': 'SDst', 'SSrc0': 'SSrc0', 'SSrc1': 'SSrc1', 'VDst': 'VDst', 'VDstS': 'VDstS',
    'Src0': 'Src0', 'VSrc1': 'VSrc1', 'Src1': 'Src1', 'Src2': 'Src2', 'SDstB': 'SDstB',
    'SBase': 'SBase', 'SData': 'SData', 'Vdata': 'Vdata',
}


def imm_style(name, enc):
    # how a 16-bit immediate prints
    return None


def emit(res, out):
    L = []
    L.append('// @generated by tools/gen_gfx10_table.py from `llvm-mc` output: do not edit.')
    L.append('// One row per (encoding, opcode) that LLVM disassembles for gfx1030, plus the opcodes only')
    L.append('// gfx1013 has (flag G13). Operand slots are the encoding fields in print order.')
    L.append('use super::ops::{build, OpInfo, Slot};')
    L.append('')
    skipped = []
    fixtures = []
    # fp-modifier support of the e32 forms follows their VOP3 siblings
    vop3_mods = {}
    for op, (cpu, b, t, t32, extra) in res.get('vop3', {}).items():
        vop3_mods[parse(t)[0].replace('_e64', '')] = (extra >> 16) & 7
    for enc in NOPS:
        rows = []
        for op, (cpu, b, t, t32, extra) in sorted(res.get(enc, {}).items()):
            name, toks = parse(t)
            _, toks32 = parse(t32)
            suffix = ''
            for s in ('_e32', '_e64'):
                if name.endswith(s):
                    name, suffix = name[:-4], s
            fl = 0
            if suffix:
                fl |= 1
            if cpu == '1030':
                fl |= 2
            fl |= extra
            sl = emit_slots(enc, name, toks, toks32, cpu, op)
            if sl is None:
                skipped.append((enc, op, t))
                continue
            if enc in ('vop1', 'vop2', 'vopc'):
                fl |= vop3_mods.get(name, 0) << 16
            flags = str(fl)
            fixtures.append('%s\t%s' % (b.hex(), t))
            rows.append('    (0x%02x, OpInfo { name: "%s", slots: &[%s], flags: %s }),'
                        % (op, name, ', '.join(sl), flags))
        size = {'sop1': 256, 'sop2': 128, 'sopc': 128, 'sopk': 32, 'sopp': 128, 'smem': 256,
                'vop1': 256, 'vop2': 64, 'vopc': 256, 'vop3': 1024, 'vintrp': 4, 'mubuf': 256,
                'exp': 1}[enc]
        L.append('pub static %s: [Option<OpInfo>; %d] = build(&[' % (enc.upper(), size))
        L.extend(rows)
        L.append(']);')
        L.append('')
    open(out, 'w').write('\n'.join(L))
    if len(sys.argv) > 2:
        open(sys.argv[2], 'w').write('\n'.join(fixtures) + '\n')
    for s in skipped:
        print('skipped', s, file=sys.stderr)


def W(w):
    return str(w)


def emit_slots(enc, name, toks, toks32, cpu='1030', opcode=0):
    """Slot constructors for one op; None if the form is not representable."""
    if enc == 'exp':
        return []
    if enc == 'vintrp':
        return []
    if enc in ('sopp',):
        global SOPP_KIND
        SOPP_KIND = sopp_imm_kind(cpu, opcode)
        return imm_slots_sopp(name, toks)
    if enc == 'sopk':
        return sopk_slots(name, toks, toks32)
    if enc == 'smem':
        return smem_slots(name, toks, toks32)
    if enc == 'mubuf':
        return mubuf_slots(name, toks, toks32)
    s = slots_of(enc, toks, toks32)
    if s is None:
        return None
    out = []
    lit_seen = 0
    for kind, f, w in s:
        if kind == 'R':
            out.append('Slot::%s(%d)' % (f, w))
        elif kind == 'A':
            out.append('Slot::Attr')
        elif kind == 'F':
            out.append('Slot::Fixed("%s")' % f)
        elif kind == 'L':
            lit_seen += 1
            out.append('Slot::Lit')
        elif kind == 'I':
            # immediates in SOP2-slot ops (s_movk etc. are SOPK; 0-opcode encodings do not take them)
            return None
    # K-literal ops print the src0 literal first and the constant second; the src0 literal is
    # not a slot of its own.
    if lit_seen == 2:
        first = out.index('Slot::Lit')
        out[first] = 'Slot::Src0(1)'
    return out


SOPP_KIND = None


def TOKIMM_DEC(toks):
    return False


def imm_slots_sopp(name, toks):
    if not toks:
        return []
    t = toks[0]
    if name == 's_waitcnt':
        return ['Slot::Waitcnt']
    if name in ('s_sendmsg', 's_sendmsghalt'):
        return ['Slot::Sendmsg']
    if name == 's_endpgm':
        return ['Slot::Endpgm']
    return [SOPP_KIND]


def sopp_imm_kind(cpu, op):
    # decimal, hex or "decimal up to 64" rendering of a 16-bit immediate
    kinds = []
    for imm in (0x13, 0x64):
        t = dis(u(0xBF800000 | op << 16 | imm), cpu)
        kinds.append(t.split(' ')[-1].startswith('0x') if t else None)
    if kinds == [False, False]:
        return 'Slot::Dec16'
    if kinds == [True, True]:
        return 'Slot::Hex16'
    return 'Slot::Imm16'


def sopk_slots(name, toks, toks32):
    out = []
    if name == 's_version':
        return ['Slot::Version']
    for t in toks:
        if TOK.match(t):
            m = TOK.match(t)
            w = 1 if m.group(2) is not None else int(m.group(4)) - int(m.group(3)) + 1
            out.append('Slot::SDst(%d)' % w)
        elif t.startswith('hwreg'):
            out.append('Slot::Hwreg')
        elif t.startswith('0x5678'):
            out.append('Slot::Lit')
        elif t.startswith('0x1234'):
            out.append('Slot::Hex16')
        elif t == '4660':
            out.append('Slot::Dec16')
        else:
            return None
    return out


def smem_slots(name, toks, toks32):
    out = []
    for t in toks:
        m = TOK.match(t)
        if m:
            n = int(m.group(2) if m.group(2) is not None else m.group(3))
            w = 1 if m.group(2) is not None else int(m.group(4)) - int(m.group(3)) + 1
            if n == D:
                out.append('Slot::SData(%d)' % w)
            elif n == 8:
                out.append('Slot::SBase(%d)' % w)
            elif n == 0:
                pass  # soffset: printed by the encoding
            else:
                return None
        elif t == '16':
            out.append('Slot::SDataImm')
        elif t.startswith('offset:') or t == 's0':
            pass
        else:
            return None
    return out


def mubuf_slots(name, toks, toks32):
    out = []
    for t in toks:
        m = TOK.match(t)
        if m and (m.group(2) is not None and int(m.group(2)) == D or m.group(3) is not None and int(m.group(3)) == D):
            w = 1 if m.group(2) is not None else int(m.group(4)) - int(m.group(3)) + 1
            out.append('Slot::Vdata(%d)' % w)
    return out


if __name__ == '__main__':
    r = build()
    emit(r, sys.argv[1])
    print({k: len(v) for k, v in r.items()})
