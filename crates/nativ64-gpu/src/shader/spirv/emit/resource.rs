//! Memory and resources of the vertex and pixel stages (chunk 9a, `docs/shaders.md`,
//! Resources and Chunk 9 design): which SMEM loads read a descriptor table, scalar and buffer
//! loads and stores as bounds-checked device-address accesses, and `image_sample*` on the
//! bindless arrays.
//!
//! A descriptor a table slot holds is never interpreted: the load that reads it writes the
//! slot's registry index into the first dword of its result (the rest is 0), and the
//! operands that use the descriptor find the object by that index. The words of such a
//! descriptor used as numbers are refused, so nothing can see the stand-in.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use spirv::{Capability, Decoration, GlslStd450Op as GL, Op, StorageClass};

use super::{bad, Gen, Opd, SpirvError, Tr, R};
use crate::shader::gfx10::Enc;
use crate::shader::io::{Class, StageIo};
use crate::shader::ir::{Func, Inst, Reg, Src, Term, Val, ValDef};
use crate::shader::resource::SamplerDesc;
use crate::shader::spirv::alu::{Backend, Ctx, Unsupported};
use crate::shader::spirv::builder::inst;

/// A buffer format's numeric convention (the last word of its `BUF_FMT_` name).
#[derive(Clone, Copy, PartialEq, Eq)]
enum BufNum {
    Unorm,
    Snorm,
    Uint,
    Sint,
    Float,
}

/// The typed buffer formats a runtime-format load converts, most used first: the DXGI format,
/// the components' bit widths and their numeric convention (16-bit floats as half).
const KNOWN_FORMATS: &[(u32, &[u32], BufNum)] = &[
    (41, &[32], BufNum::Float),
    (39, &[32], BufNum::Float),
    (42, &[32], BufNum::Uint),
    (43, &[32], BufNum::Sint),
    (57, &[16], BufNum::Uint),
    (62, &[8], BufNum::Uint),
    (2, &[32, 32, 32, 32], BufNum::Float),
    (17, &[32, 32], BufNum::Uint),
    (10, &[16, 16, 16, 16], BufNum::Float),
    (12, &[16, 16, 16, 16], BufNum::Uint),
    (36, &[16, 16], BufNum::Uint),
    (50, &[8, 8], BufNum::Uint),
    (61, &[8], BufNum::Unorm),
    (54, &[16], BufNum::Float),
    (27, &[8, 8, 8, 8], BufNum::Unorm),
    (28, &[8, 8, 8, 8], BufNum::Unorm),
    (11, &[16, 16, 16, 16], BufNum::Unorm),
    (35, &[16, 16], BufNum::Unorm),
    (49, &[8, 8], BufNum::Unorm),
    (56, &[16], BufNum::Unorm),
];

/// Components (bits each) and the numeric convention of an MTBUF format byte.
fn ufmt(f: u8) -> R<(Vec<u32>, BufNum)> {
    let name = crate::shader::gfx10::table_ext::UFMT
        .get(usize::from(f))
        .copied()
        .unwrap_or("");
    let name = name.strip_prefix("BUF_FMT_").unwrap_or("");
    if name.is_empty() {
        // the table leaves the default format (8_UNORM) unnamed
        if f == 1 {
            return Ok((vec![8], BufNum::Unorm));
        }
        return refuse(&format!("buffer format {f}"));
    }
    let mut parts: Vec<&str> = name.split('_').collect();
    let num = match parts.pop().unwrap_or("") {
        "UNORM" => BufNum::Unorm,
        "SNORM" => BufNum::Snorm,
        "UINT" => BufNum::Uint,
        "SINT" => BufNum::Sint,
        "FLOAT" => BufNum::Float,
        other => return refuse(&format!("buffer format {name} ({other})")),
    };
    let mut bits = Vec::new();
    for p in parts {
        bits.push(match p {
            "8" => 8,
            "16" => 16,
            "32" => 32,
            other => return refuse(&format!("buffer format {name} ({other})")),
        });
    }
    if bits.is_empty() {
        return refuse(&format!("buffer format {name}"));
    }
    Ok((bits, num))
}

fn refuse<T>(op: &str) -> R<T> {
    Err(SpirvError::Unsupported(Unsupported {
        chunk: 9,
        op: op.to_string(),
    }))
}

// ---------------------------------------------------------------------------------------
// Tracing

/// What the program's SMEM loads and descriptor operands are, found before emission.
pub(in crate::shader::spirv) struct Analysis {
    /// The SMEM loads that read a descriptor table: `(block, instruction)` → user SGPR.
    table_loads: HashMap<(usize, usize), u16>,
    /// Where each loaded descriptor dword comes from: the table's user SGPR and the byte
    /// offset in the table, when the program loads it at a constant one (chunk 10c: the used
    /// rows say whether such a slot is an SRV or a UAV).
    origin: HashMap<Val, (u16, u32)>,
    /// The loaded descriptor dwords the program uses as plain numbers (to build another
    /// descriptor out of them, say): the load emits that descriptor's V# words for them.
    numbers: HashSet<Val>,
    /// Every value that holds a dword of a loaded descriptor (the loads' results, and copies
    /// and phis of them).
    loaded: HashSet<Val>,
    /// A raw scalar load or a built buffer has a base that does not trace to user data: an
    /// address the program read from memory or computed from it, which the backend cannot
    /// translate (`Translated::memory_addresses`).
    memory_addresses: bool,
    /// The SMEM loads that read the program's own code (a literal table at `s_getpc` plus a
    /// computed offset): they are served from the code words the module embeds.
    pub(super) code_loads: HashSet<(usize, usize)>,
    /// The code bytes those loads reach (a window of the code, so a program's whole code is
    /// not embedded): its first byte.
    pub(super) code_window: u32,
}

/// Whether `s` is computed from user data and constants only (scalar arithmetic, copies and
/// phis of such): a root descriptor's address, or a flag word of a buffer the code builds.
fn user_derived(f: &Func, s: Src, depth: u32) -> bool {
    let Src::Val(v) = s else {
        return matches!(s, Src::Const(_));
    };
    if depth > 24 {
        return false;
    }
    let info = f.vals[v.0 as usize];
    match info.def {
        ValDef::Arg => matches!(info.reg, Reg::S(_)),
        ValDef::Phi { block, index } => f.blocks[block].phis[index]
            .args
            .iter()
            .all(|(_, a)| *a == s || user_derived(f, *a, depth + 1)),
        ValDef::Inst { .. } => {
            let Some(i) = def_inst(f, v) else {
                return false;
            };
            matches!(i.instr.enc, Enc::Sop1 | Enc::Sop2 | Enc::Sopk)
                && i.srcs
                    .iter()
                    .flatten()
                    .all(|a| user_derived(f, *a, depth + 1))
        }
    }
}

/// Whether the program reads memory through an address that is not user data: a raw scalar
/// load, or a buffer the code builds (not loaded from a table), whose words are not
/// user-derived.
fn reads_memory_addresses(
    f: &Func,
    loaded: &HashSet<Val>,
    table_loads: &HashMap<(usize, usize), u16>,
) -> bool {
    let is_loaded = |s: &Src| matches!(s, Src::Val(v) if loaded.contains(v));
    for (b, blk) in f.blocks.iter().enumerate() {
        for (k, i) in blk.insts.iter().enumerate() {
            let name = i.instr.name();
            let words: Option<&[Src]> = match i.instr.enc {
                Enc::Smem if name.starts_with("s_load_dword") => {
                    if table_loads.contains_key(&(b, k)) {
                        None
                    } else {
                        i.srcs.first().map(Vec::as_slice)
                    }
                }
                Enc::Smem if name.starts_with("s_buffer_load") => i.srcs.first().map(Vec::as_slice),
                Enc::Mubuf | Enc::Mtbuf => {
                    i.srcs.len().checked_sub(2).map(|n| i.srcs[n].as_slice())
                }
                _ => None,
            };
            let Some(w) = words else { continue };
            if w.first().is_some_and(is_loaded) {
                continue;
            }
            if !w.iter().all(|s| user_derived(f, *s, 0)) {
                return true;
            }
        }
    }
    false
}

fn def_inst(f: &Func, v: Val) -> Option<&Inst> {
    match f.vals[v.0 as usize].def {
        ValDef::Inst { block, index } => Some(&f.blocks[block].insts[index]),
        _ => None,
    }
}

/// The table user SGPR whose value `s` is plus an offset: the SGPR itself, copies, additions
/// of anything to such a value, phis whose inputs all agree.
fn table_of(f: &Func, tables: &BTreeSet<u16>, s: Src, depth: u32) -> Option<u16> {
    let Src::Val(v) = s else {
        return None;
    };
    if depth > 24 {
        return None;
    }
    let info = f.vals[v.0 as usize];
    match info.def {
        ValDef::Arg => match info.reg {
            Reg::S(n) if tables.contains(&n) => Some(n),
            _ => None,
        },
        ValDef::Phi { block, index } => {
            let mut found: Option<u16> = None;
            for (_, a) in &f.blocks[block].phis[index].args {
                if *a == s {
                    continue;
                }
                let n = table_of(f, tables, *a, depth + 1)?;
                if found.is_some_and(|x| x != n) {
                    return None;
                }
                found = Some(n);
            }
            found
        }
        ValDef::Inst { .. } => {
            let i = def_inst(f, v)?;
            if i.dsts.first().map(|d| d.1) != Some(v) {
                return None;
            }
            let at = |a: usize| i.srcs.get(a).and_then(|s| s.first()).copied();
            match i.instr.name() {
                "s_mov_b32" => table_of(f, tables, at(0)?, depth + 1),
                "s_add_u32" | "s_add_i32" => table_of(f, tables, at(0)?, depth + 1)
                    .or_else(|| table_of(f, tables, at(1)?, depth + 1)),
                _ => None,
            }
        }
    }
}

/// The table user SGPR whose value `s` is and the constant added to it: `table_of` where
/// every addition is of a constant.
fn table_addend(f: &Func, tables: &BTreeSet<u16>, s: Src, depth: u32) -> Option<(u16, u32)> {
    let Src::Val(v) = s else {
        return None;
    };
    if depth > 24 {
        return None;
    }
    let info = f.vals[v.0 as usize];
    match info.def {
        ValDef::Arg => match info.reg {
            Reg::S(n) if tables.contains(&n) => Some((n, 0)),
            _ => None,
        },
        ValDef::Phi { block, index } => {
            let mut found: Option<(u16, u32)> = None;
            for (_, a) in &f.blocks[block].phis[index].args {
                if *a == s {
                    continue;
                }
                let t = table_addend(f, tables, *a, depth + 1)?;
                if found.is_some_and(|x| x != t) {
                    return None;
                }
                found = Some(t);
            }
            found
        }
        ValDef::Inst { .. } => {
            let i = def_inst(f, v)?;
            if i.dsts.first().map(|d| d.1) != Some(v) {
                return None;
            }
            let at = |a: usize| i.srcs.get(a).and_then(|s| s.first()).copied();
            match i.instr.name() {
                "s_mov_b32" => table_addend(f, tables, at(0)?, depth + 1),
                "s_add_u32" | "s_add_i32" => {
                    let (a, b) = (at(0)?, at(1)?);
                    match (a, b) {
                        (x, Src::Const(k)) | (Src::Const(k), x) => {
                            let (n, c) = table_addend(f, tables, x, depth + 1)?;
                            Some((n, c.wrapping_add(k)))
                        }
                        _ => None,
                    }
                }
                _ => None,
            }
        }
    }
}

/// The high dword of a table address is 0: the constant, or the carry of an addition that
/// started from 0.
fn high_is_zero(f: &Func, s: Src, depth: u32) -> bool {
    match s {
        Src::Const(0) => true,
        Src::Val(v) if depth <= 24 => {
            let info = f.vals[v.0 as usize];
            match info.def {
                ValDef::Phi { block, index } => f.blocks[block].phis[index]
                    .args
                    .iter()
                    .all(|(_, a)| *a == s || high_is_zero(f, *a, depth + 1)),
                ValDef::Inst { .. } => {
                    let Some(i) = def_inst(f, v) else {
                        return false;
                    };
                    let at = |a: usize| i.srcs.get(a).and_then(|s| s.first()).copied();
                    match i.instr.name() {
                        "s_mov_b32" => at(0).is_some_and(|a| high_is_zero(f, a, depth + 1)),
                        "s_addc_u32" => [at(0), at(1)]
                            .into_iter()
                            .all(|a| a.is_some_and(|a| high_is_zero(f, a, depth + 1))),
                        _ => false,
                    }
                }
                ValDef::Arg => false,
            }
        }
        _ => false,
    }
}

fn is_copy(i: &Inst) -> bool {
    matches!(i.instr.name(), "s_mov_b32" | "s_mov_b64")
}

/// Whether operand `j` of `i` (of `n`) is a place a descriptor goes.
fn descriptor_operand(i: &Inst, j: usize, n: usize) -> bool {
    match i.instr.enc {
        Enc::Smem => i.instr.name().starts_with("s_buffer_load") && j == 0,
        Enc::Mubuf | Enc::Mtbuf => j + 2 == n,
        Enc::Mimg => j + 2 >= n,
        _ => false,
    }
}

pub(in crate::shader::spirv) fn analyze(
    f: &Func,
    tables: &BTreeSet<u16>,
) -> Result<Analysis, SpirvError> {
    let mut a = Analysis {
        table_loads: HashMap::new(),
        origin: HashMap::new(),
        loaded: HashSet::new(),
        numbers: HashSet::new(),
        memory_addresses: false,
        code_loads: HashSet::new(),
        code_window: 0,
    };
    for (b, blk) in f.blocks.iter().enumerate() {
        for (k, i) in blk.insts.iter().enumerate() {
            let name = i.instr.name();
            if i.instr.enc != Enc::Smem || !name.starts_with("s_load_dword") {
                continue;
            }
            let Some(&[lo, hi]) = i.srcs.first().map(Vec::as_slice) else {
                continue;
            };
            let Some(n) = table_of(f, tables, lo, 0) else {
                continue;
            };
            if !high_is_zero(f, hi, 0) {
                return refuse("a table address with a high dword");
            }
            a.table_loads.insert((b, k), n);
            a.loaded.extend(i.dsts.iter().map(|d| d.1));
            // the slot's constant offset: what the program added to the table's address,
            // the immediate and a constant SGPR offset
            let soff = match i.srcs.get(1).and_then(|s| s.first()) {
                None => Some(0),
                Some(Src::Const(k)) => Some(*k),
                Some(_) => None,
            };
            if let (Some((t, add)), Some(soff)) = (table_addend(f, tables, lo, 0), soff) {
                let at = add
                    .wrapping_add(soff)
                    .wrapping_add(i.instr.mem.offset as u32);
                a.origin.extend(i.dsts.iter().map(|d| (d.1, (t, at))));
            }
        }
    }
    if a.loaded.is_empty() {
        a.memory_addresses = reads_memory_addresses(f, &a.loaded, &a.table_loads);
        return Ok(a);
    }
    // copies and phis of loaded dwords are loaded dwords
    loop {
        let before = (a.loaded.len(), a.origin.len());
        for blk in &f.blocks {
            for p in &blk.phis {
                if !p.args.is_empty()
                    && p.args
                        .iter()
                        .all(|(_, s)| matches!(s, Src::Val(v) if a.loaded.contains(v)))
                {
                    a.loaded.insert(p.dst);
                    let mut from = p.args.iter().filter_map(|(_, s)| match s {
                        Src::Val(v) if *v != p.dst => Some(a.origin.get(v).copied()),
                        _ => None,
                    });
                    if let Some(Some(o)) = from.next() {
                        if from.all(|x| x == Some(o)) {
                            a.origin.insert(p.dst, o);
                        }
                    }
                }
            }
            for i in blk.insts.iter().filter(|i| is_copy(i)) {
                for (j, (reg, val)) in i.dsts.iter().enumerate() {
                    let from = i.srcs.first().and_then(|s| s.get(j));
                    if *reg != Reg::Scc && matches!(from, Some(Src::Val(v)) if a.loaded.contains(v))
                    {
                        a.loaded.insert(*val);
                        if let Some(Src::Val(v)) = from {
                            if let Some(&o) = a.origin.get(v) {
                                a.origin.insert(*val, o);
                            }
                        }
                    }
                }
            }
        }
        if (a.loaded.len(), a.origin.len()) == before {
            break;
        }
    }
    let is_loaded = |s: &Src| matches!(s, Src::Val(v) if a.loaded.contains(v));
    // the loaded dwords some instruction uses as a plain number: the load emits the console
    // V# words of the descriptor's view for them (a program builds a new descriptor out of
    // them, say)
    let mut numbers: HashSet<Val> = HashSet::new();
    for blk in &f.blocks {
        for i in blk.insts.iter().filter(|i| !is_copy(i)) {
            let n = i.srcs.len();
            for (j, s) in i.srcs.iter().enumerate() {
                if descriptor_operand(i, j, n) {
                    continue;
                }
                for x in s {
                    if let Src::Val(v) = x {
                        if a.loaded.contains(v) {
                            numbers.insert(*v);
                        }
                    }
                }
            }
        }
    }
    // a phi may carry such numbers (the words of two descriptors, say), but not a descriptor
    // a descriptor operand still resolves by its slot. A phi nothing reads cannot be one: the
    // title's programs keep dead phis of half-built descriptors.
    let numeric = |s: &Src| matches!(s, Src::Val(v) if numbers.contains(v));
    // the values that reach a descriptor operand: a phi of loaded words is only refused when
    // it (or a phi it feeds) is used as a descriptor
    let mut descriptors: HashSet<Val> = HashSet::new();
    for blk in &f.blocks {
        for i in &blk.insts {
            let n = i.srcs.len();
            for (j, s) in i.srcs.iter().enumerate() {
                if descriptor_operand(i, j, n) {
                    for x in s {
                        if let Src::Val(v) = x {
                            descriptors.insert(*v);
                        }
                    }
                }
            }
        }
    }
    loop {
        let before = descriptors.len();
        for blk in &f.blocks {
            for p in &blk.phis {
                if descriptors.contains(&p.dst) {
                    for (_, s) in &p.args {
                        if let Src::Val(v) = s {
                            descriptors.insert(*v);
                        }
                    }
                }
            }
        }
        if descriptors.len() == before {
            break;
        }
    }
    let mut used: HashSet<Val> = HashSet::new();
    for blk in &f.blocks {
        for i in &blk.insts {
            for s in i.srcs.iter().flatten() {
                if let Src::Val(v) = s {
                    used.insert(*v);
                }
            }
        }
        for p in &blk.phis {
            for (_, s) in &p.args {
                if let Src::Val(v) = s {
                    used.insert(*v);
                }
            }
        }
        if let Term::Branch { cond, .. } = &blk.term {
            for (_, s) in cond {
                if let Src::Val(v) = s {
                    used.insert(*v);
                }
            }
        }
    }
    for blk in &f.blocks {
        for p in &blk.phis {
            if a.loaded.contains(&p.dst) || !used.contains(&p.dst) || !descriptors.contains(&p.dst)
            {
                continue;
            }
            let sources = |f: &dyn Fn(&Src) -> bool| p.args.iter().any(|(_, s)| f(s));
            if sources(&is_loaded) && !p.args.iter().all(|(_, s)| numeric(s) || !is_loaded(s)) {
                return refuse("a loaded descriptor's words mixed into a phi");
            }
        }
        if let Term::Branch { cond, .. } = &blk.term {
            if cond.iter().any(|(_, s)| is_loaded(s)) {
                return refuse("a loaded descriptor's words in a branch condition");
            }
        }
    }
    a.numbers = numbers;
    // the program's own address (`s_getpc_b64`): values computed from it point into the code
    let mut code_vals: HashSet<Val> = HashSet::new();
    let mut code_window = u32::MAX;
    loop {
        let before = code_vals.len();
        for blk in &f.blocks {
            for i in &blk.insts {
                for s in i.srcs.iter().flatten() {
                    if let Src::CodeLo(o) = s {
                        code_window = code_window.min(o & !15);
                    }
                }
                let from_code = i.srcs.iter().flatten().any(|s| {
                    matches!(s, Src::CodeLo(_) | Src::CodeHi)
                        || matches!(s, Src::Val(v) if code_vals.contains(v))
                });
                if from_code {
                    for (_, v) in &i.dsts {
                        code_vals.insert(*v);
                    }
                }
            }
        }
        if code_vals.len() == before {
            break;
        }
    }
    for (b, blk) in f.blocks.iter().enumerate() {
        for (k, i) in blk.insts.iter().enumerate() {
            if i.instr.enc != Enc::Smem || !i.instr.name().starts_with("s_load_dword") {
                continue;
            }
            let code = i.srcs.first().is_some_and(|s| {
                s.iter()
                    .any(|x| matches!(x, Src::Val(v) if code_vals.contains(v)))
            });
            if code {
                a.code_loads.insert((b, k));
            }
        }
    }
    if code_window != u32::MAX {
        a.code_window = code_window;
    }
    a.memory_addresses = reads_memory_addresses(f, &a.loaded, &a.table_loads);
    Ok(a)
}

// ---------------------------------------------------------------------------------------
// Emission state

/// The resource variables of the entry point, created on first use.
pub(in crate::shader::spirv) struct Res {
    pub(super) analysis: Analysis,
    /// The root block pointer, computed in the entry block.
    pub(super) root: Option<u32>,
    /// Sampled-image arrays by image dimension: `(variable, image type)`.
    textures: BTreeMap<u8, (u32, u32)>,
    sampler_array: Option<u32>,
    /// Storage-image arrays by image dimension: `(variable, image type)` (set 0, binding 1).
    images: BTreeMap<u8, (u32, u32)>,
    /// The workgroup memory (compute) and its size in dwords.
    lds: Option<u32>,
    /// Static samplers: set 1, binding k = `samplers[k]`.
    pub(super) samplers: Vec<SamplerDesc>,
    /// The import table variable and its struct type (set 0, binding 3).
    imports: Option<(u32, u32)>,
    /// The ids already decorated NonUniform.
    non_uniform: HashSet<u32>,
    static_vars: Vec<u32>,
    /// A `Function` variable the guarded loads write their result to.
    tmp: Option<u32>,
}

impl Res {
    /// Whether the entry reads a texture or a sampler of the bindless set, and whether it reads
    /// memory through an address that is not user data.
    pub(in crate::shader::spirv) fn flags(&self) -> (bool, bool, bool) {
        (
            !self.textures.is_empty()
                || !self.images.is_empty()
                || self.sampler_array.is_some()
                || !self.samplers.is_empty(),
            self.analysis.memory_addresses,
            !self.images.is_empty(),
        )
    }

    pub(super) fn new(analysis: Analysis) -> Self {
        Self {
            analysis,
            root: None,
            textures: BTreeMap::new(),
            sampler_array: None,
            images: BTreeMap::new(),
            lds: None,
            samplers: Vec::new(),
            imports: None,
            non_uniform: HashSet::new(),
            static_vars: Vec::new(),
            tmp: None,
        }
    }
}

/// A buffer: base address and size in bytes (both 64-bit), stride (32-bit).
struct Buf {
    base: u32,
    size: u32,
    stride: u32,
}

/// Workgroup-memory dwords a program that uses LDS without declaring any gets.
const MIN_LDS: u32 = 256;

/// `(set, binding)` of the bindless arrays (`docs/shaders.md`, Resources).
const SET_BINDLESS: u32 = 0;
const BIND_SAMPLED: u32 = 0;
const BIND_STORAGE: u32 = 1;
const BIND_SAMPLERS: u32 = 2;
/// The set of the static samplers.
const SET_STATIC: u32 = 1;

impl<'a> Tr<'a> {
    fn res(&mut self) -> &mut Res {
        &mut self.st().res
    }

    fn t64(&mut self) -> u32 {
        self.g.m.capability(Capability::Int64);
        self.g.m.global(Op::TypeInt, None, &[64, 0])
    }

    fn k64(&mut self, v: u64) -> u32 {
        let t = self.t64();
        self.g
            .m
            .global(Op::Constant, Some(t), &[v as u32, (v >> 32) as u32])
    }

    fn zext(&mut self, x: u32) -> u32 {
        let t = self.t64();
        self.g.op(Op::UConvert, t, &[x])
    }

    fn add64(&mut self, a: u32, b: u32) -> u32 {
        let t = self.t64();
        self.g.op(Op::IAdd, t, &[a, b])
    }

    fn add_k64(&mut self, a: u32, k: u64) -> u32 {
        let k = self.k64(k);
        self.add64(a, k)
    }

    fn root(&self) -> u32 {
        self.stage
            .as_ref()
            .and_then(|s| s.res.root)
            .expect("the root block")
    }

    /// A `u32` at a device address.
    fn bda_u32(&mut self, addr: u32) -> u32 {
        let tu = self.g.t_u32();
        let p = self.g.t_ptr(StorageClass::PhysicalStorageBuffer, tu);
        let ptr = self.g.op(Op::ConvertUToPtr, p, &[addr]);
        // MemoryAccess Aligned, 4
        self.g.op(Op::Load, tu, &[ptr, 2, 4])
    }

    fn bda_store_u32(&mut self, addr: u32, v: u32) {
        let tu = self.g.t_u32();
        let p = self.g.t_ptr(StorageClass::PhysicalStorageBuffer, tu);
        let ptr = self.g.op(Op::ConvertUToPtr, p, &[addr]);
        self.g.op0(Op::Store, &[ptr, v, 2, 4]);
    }

    /// `table_base[n]` (member 1 of the root block).
    fn table_base(&mut self, n: u16) -> u32 {
        let t = self.t64();
        let p = self.g.t_ptr(StorageClass::PhysicalStorageBuffer, t);
        let root = self.root();
        let (one, k) = (self.g.k(1), self.g.k(u32::from(n)));
        let a = self.g.op(Op::AccessChain, p, &[root, one, k]);
        self.g.op(Op::Load, t, &[a, 2, 8])
    }

    /// `view_table` (member 2 of the root block).
    fn view_table(&mut self) -> u32 {
        let t = self.t64();
        let p = self.g.t_ptr(StorageClass::PhysicalStorageBuffer, t);
        let root = self.root();
        let two = self.g.k(2);
        let a = self.g.op(Op::AccessChain, p, &[root, two]);
        self.g.op(Op::Load, t, &[a, 2, 8])
    }

    /// The value at `addr` when `valid`, else 0.
    fn guarded_load(&mut self, addr: u32, valid: u32) -> R<u32> {
        let tmp = match self.res().tmp {
            Some(v) => v,
            None => {
                let tu = self.g.t_u32();
                let pf = self.g.t_ptr(StorageClass::Function, tu);
                let v = self.g.m.id();
                self.g
                    .locals
                    .extend(inst(Op::Variable, &[pf, v, StorageClass::Function as u32]));
                self.res().tmp = Some(v);
                v
            }
        };
        let tu = self.g.t_u32();
        let zero = self.g.k(0);
        self.g.store(tmp, zero);
        self.guarded(Some(valid), |t| {
            let v = t.bda_u32(addr);
            t.g.store(tmp, v);
            Ok(())
        })?;
        Ok(self.g.load(tu, tmp))
    }

    fn and(&mut self, a: u32, b: Option<u32>) -> u32 {
        match b {
            Some(b) => {
                let tb = self.g.t_bool();
                self.g.op(Op::LogicalAnd, tb, &[a, b])
            }
            None => a,
        }
    }

    // -----------------------------------------------------------------------------------
    // Buffers

    /// A buffer a registry index names: the view table's record (`{kind, format, address,
    /// size, stride, 0, 0}`, 32 bytes). A record of another kind is an empty buffer.
    fn loaded_buf(&mut self, index: u32) -> Buf {
        let (tb, t64) = (self.g.t_bool(), self.t64());
        let table = self.view_table();
        let i64 = self.zext(index);
        let five = self.g.k(5);
        let off = self.g.op(Op::ShiftLeftLogical, t64, &[i64, five]);
        let rec = self.add64(table, off);
        let at = |t: &mut Self, o: u64| {
            let a = t.add_k64(rec, o);
            t.bda_u32(a)
        };
        let kind = at(self, 0);
        let (lo, hi) = (at(self, 8), at(self, 12));
        let (size32, stride) = (at(self, 16), at(self, 20));
        let (lo, hi) = (self.zext(lo), self.zext(hi));
        let thirty_two = self.g.k(32);
        let hi = self.g.op(Op::ShiftLeftLogical, t64, &[hi, thirty_two]);
        let base = self.g.op(Op::BitwiseOr, t64, &[lo, hi]);
        let (all, one) = (self.g.k(u32::MAX), self.g.k(1));
        let unbounded = self.g.op(Op::IEqual, tb, &[size32, all]);
        let (s64, max) = (self.zext(size32), self.k64(u64::MAX));
        let size = self.g.op(Op::Select, t64, &[unbounded, max, s64]);
        let is_buf = self.g.op(Op::IEqual, tb, &[kind, one]);
        let none = self.k64(0);
        let size = self.g.op(Op::Select, t64, &[is_buf, size, none]);
        Buf { base, size, stride }
    }

    /// The dword at byte `off` of the view table record `index` (a runtime value: the record
    /// says what the descriptor names).
    fn record_dword(&mut self, index: u32, off: u64) -> u32 {
        let table = self.view_table();
        let i64 = self.zext(index);
        let five = self.g.k(5);
        let t64 = self.t64();
        let sh = self.g.op(Op::ShiftLeftLogical, t64, &[i64, five]);
        let rec = self.add64(table, sh);
        let a = self.add_k64(rec, off);
        self.bda_u32(a)
    }

    /// The dword of `words` that holds the `bits`-wide component starting at byte `bit / 8`.
    fn extract_bits(&mut self, words: &[u32], bit: u32, bits: u32) -> u32 {
        let w = words[(bit / 32) as usize];
        let shift = bit % 32;
        let tu = self.g.t_u32();
        let raw = if shift == 0 {
            w
        } else {
            let sh = self.g.k(shift);
            self.g.op(Op::ShiftRightLogical, tu, &[w, sh])
        };
        let mask = self.g.k(if bits >= 32 {
            u32::MAX
        } else {
            (1u32 << bits) - 1
        });
        self.g.op(Op::BitwiseAnd, tu, &[raw, mask])
    }

    /// A half-float component at bit `bit` of `words` as f32 bits (`UnpackHalf2x16`).
    fn unpack_half(&mut self, words: &[u32], bit: u32) -> u32 {
        let w = words[(bit / 32) as usize];
        let which = (bit % 32) / 16;
        let t2 = self.g.t_v2();
        let v = self.g.ext(t2, GL::UnpackHalf2x16, &[w]);
        let tf = self.g.t_f32();
        let f = self.g.op(Op::CompositeExtract, tf, &[v, which]);
        self.g.as_bits(f)
    }

    /// A buffer built by the program: its four words are an RDNA2 V# (base in word 0 and
    /// the low 16 bits of word 1, stride in bits 16-29 of word 1, `num_records` in word 2;
    /// a stride scales `num_records`, -1 is unbounded).
    fn built_buf(&mut self, w: [u32; 4]) -> Buf {
        let (tb, tu, t64) = (self.g.t_bool(), self.g.t_u32(), self.t64());
        let (mask16, sixteen, mask14) = (self.g.k(0xffff), self.g.k(16), self.g.k(0x3fff));
        let hi = self.g.op(Op::BitwiseAnd, tu, &[w[1], mask16]);
        let (lo, hi) = (self.zext(w[0]), self.zext(hi));
        let thirty_two = self.g.k(32);
        let hi = self.g.op(Op::ShiftLeftLogical, t64, &[hi, thirty_two]);
        let base = self.g.op(Op::BitwiseOr, t64, &[lo, hi]);
        let sh = self.g.op(Op::ShiftRightLogical, tu, &[w[1], sixteen]);
        let stride = self.g.op(Op::BitwiseAnd, tu, &[sh, mask14]);
        let (all, zero) = (self.g.k(u32::MAX), self.g.k(0));
        let unbounded = self.g.op(Op::IEqual, tb, &[w[2], all]);
        let raw = self.g.op(Op::IEqual, tb, &[stride, zero]);
        let (n64, s64) = (self.zext(w[2]), self.zext(stride));
        let scaled = self.g.op(Op::IMul, t64, &[n64, s64]);
        let bytes = self.g.op(Op::Select, t64, &[raw, n64, scaled]);
        let max = self.k64(u64::MAX);
        let size = self.g.op(Op::Select, t64, &[unbounded, max, bytes]);
        Buf { base, size, stride }
    }

    /// The buffer a descriptor operand names: loaded from a table, or built by the code.
    fn buffer_of(&mut self, rsrc: &[Src], vals: &[Opd<Gen>]) -> R<Buf> {
        let loaded =
            matches!(rsrc.first(), Some(Src::Val(v)) if self.res().analysis.loaded.contains(v));
        if loaded {
            let index = self.bits_of(vals[0]);
            return Ok(self.loaded_buf(index));
        }
        if vals.len() < 4 {
            return bad("a buffer resource of fewer than four dwords");
        }
        let w = [0, 1, 2, 3].map(|k| self.bits_of(vals[k]));
        Ok(self.built_buf(w))
    }

    /// Whether the dword at byte `off + 4 * k` of `buf` is inside it.
    fn in_range(&mut self, buf: &Buf, off: u32, k: u64) -> u32 {
        let tb = self.g.t_bool();
        let end = self.add_k64(off, 4 * k + 4);
        self.g.op(Op::ULessThanEqual, tb, &[end, buf.size])
    }

    /// The SMEM address operands: the immediate plus the optional SGPR offset, as a `u64`.
    fn smem_offset(&mut self, c: &Ctx<Gen>) -> u32 {
        let imm = i64::from(c.inst.instr.mem.offset) as u64;
        let mut off = self.k64(imm);
        if let Some(&s) = c.srcs.get(1).and_then(|s| s.first()) {
            let s = self.bits_of(s);
            let z = self.zext(s);
            off = self.add64(off, z);
        }
        off
    }

    // -----------------------------------------------------------------------------------
    // Instructions

    /// The memory and resource instructions this chunk translates; `None` leaves the rest to
    /// the table, which refuses them as chunk 9.
    pub(super) fn resource_exec(
        &mut self,
        c: &Ctx<Gen>,
        b: usize,
        k: usize,
    ) -> R<Option<Vec<Opd<Gen>>>> {
        match c.inst.instr.enc {
            Enc::Smem => self.smem(c, b, k),
            Enc::Mubuf | Enc::Mtbuf => self.mubuf(c),
            Enc::Mimg if c.inst.instr.name().starts_with("image_load") => self.image_load(c),
            Enc::Mimg if c.inst.instr.name().starts_with("image_store") => self.image_store(c),
            Enc::Mimg => self.image_sample(c),
            Enc::Ds => self.ds(c),
            Enc::Flat => self.flat(c),
            _ => Ok(None),
        }
    }

    /// `x` of `s_load_dwordx` / `s_buffer_load_dwordx`: the dwords loaded.
    fn dwords(name: &str, prefix: &str) -> Option<usize> {
        match name.strip_prefix(prefix)? {
            "" => Some(1),
            "x2" => Some(2),
            "x4" => Some(4),
            "x8" => Some(8),
            "x16" => Some(16),
            _ => None,
        }
    }

    fn smem(&mut self, c: &Ctx<Gen>, b: usize, k: usize) -> R<Option<Vec<Opd<Gen>>>> {
        let name = c.inst.instr.name();
        let buffer = name.starts_with("s_buffer_load");
        let Some(n) = Self::dwords(
            name,
            if buffer {
                "s_buffer_load_dword"
            } else {
                "s_load_dword"
            },
        ) else {
            return Ok(None);
        };
        if c.inst.dsts.len() != n {
            return bad("an SMEM load whose results do not match its width");
        }
        let tu = self.g.t_u32();
        let off = self.smem_offset(c);
        let mut out = Vec::new();
        if buffer {
            let buf = self.buffer_of(&c.inst.srcs[0], &c.srcs[0])?;
            for j in 0..n {
                let valid = self.in_range(&buf, off, j as u64);
                let a = self.add_k64(buf.base, 4 * j as u64);
                let a = self.add64(a, off);
                out.push(Opd::V(self.guarded_load(a, valid)?));
            }
            return Ok(Some(out));
        }
        if self.res().analysis.code_loads.contains(&(b, k)) {
            // a literal table in the program's own code: the address is a code offset (the
            // module embeds the code words)
            let Some(&lo) = c.srcs.first().and_then(|s| s.first()) else {
                return bad("a code load without its address");
            };
            let lo = self.bits_of(lo);
            let mut out = Vec::new();
            for j in 0..n as u32 {
                let off = if j == 0 {
                    lo
                } else {
                    let k = self.g.k(4 * j);
                    self.g.op(Op::IAdd, tu, &[lo, k])
                };
                let w = self.code_word(off);
                out.push(Opd::V(w));
            }
            return Ok(Some(out));
        }
        if let Some(t) = self.res().analysis.table_loads.get(&(b, k)).copied() {
            // the slot: the table's base plus what the program added to its low dword
            let lo = self.bits_of(c.srcs[0][0]);
            let root = self.root();
            let user = self.user_dword(root, u32::from(t));
            let delta = self.g.op(Op::ISub, tu, &[lo, user]);
            let delta = self.zext(delta);
            let slot = self.table_base(t);
            let slot = self.add64(slot, delta);
            let slot = self.add64(slot, off);
            // our slots hold `"nVEW"` and the registry index
            let at = self.add_k64(slot, 4);
            let index = self.bda_u32(at);
            if c.inst
                .dsts
                .iter()
                .any(|d| self.res().analysis.numbers.contains(&d.1))
            {
                // the words are used as numbers (to build another descriptor out of them):
                // the console V# of each view the records describe (a wide load reads the
                // descriptors of consecutive table slots)
                if n % 4 != 0 {
                    return refuse("the words of a descriptor used as numbers, of an odd width");
                }
                let tu = self.g.t_u32();
                for g in 0..(n / 4) as u32 {
                    let slot_g = self.add_k64(slot, u64::from(32 * g));
                    let index_g = if g == 0 {
                        index
                    } else {
                        let at = self.add_k64(slot_g, 4);
                        self.bda_u32(at)
                    };
                    let lo = self.record_dword(index_g, 8);
                    let hi = self.record_dword(index_g, 12);
                    let stride = self.record_dword(index_g, 20);
                    let size = self.record_dword(index_g, 16);
                    let mask = self.g.k(0xffff);
                    let hi16 = self.g.op(Op::BitwiseAnd, tu, &[hi, mask]);
                    let sh = self.g.k(16);
                    let st = self.g.op(Op::ShiftLeftLogical, tu, &[stride, sh]);
                    let word1 = self.g.op(Op::BitwiseOr, tu, &[hi16, st]);
                    // the record count: elements for a structured buffer, bytes for a raw one
                    let zero = self.g.k(0);
                    let tb = self.g.t_bool();
                    let raw = self.g.op(Op::IEqual, tb, &[stride, zero]);
                    let count = self.g.op(Op::UDiv, tu, &[size, stride]);
                    let word2 = self.g.select(raw, size, count);
                    out.push(Opd::V(lo));
                    out.push(Opd::V(word1));
                    out.push(Opd::V(word2));
                    out.push(Opd::V(self.g.k(0)));
                }
                return Ok(Some(out));
            }
            out.push(Opd::V(index));
            for _ in 1..n {
                out.push(Opd::V(self.g.k(0)));
            }
            return Ok(Some(out));
        }
        // a raw load from an address the program has: a root descriptor or computed
        let (lo, hi) = (self.bits_of(c.srcs[0][0]), self.bits_of(c.srcs[0][1]));
        let (lo, hi) = (self.zext(lo), self.zext(hi));
        let t64 = self.t64();
        let thirty_two = self.g.k(32);
        let hi = self.g.op(Op::ShiftLeftLogical, t64, &[hi, thirty_two]);
        let base = self.g.op(Op::BitwiseOr, t64, &[lo, hi]);
        let base = self.add64(base, off);
        for j in 0..n {
            let a = self.add_k64(base, 4 * j as u64);
            out.push(Opd::V(self.bda_u32(a)));
        }
        Ok(Some(out))
    }

    /// The result `new` of a vector instruction: the old value in the lanes that are off.
    fn lane_result(&mut self, c: &Ctx<Gen>, j: usize, new: u32) -> Opd<Gen> {
        let Some(act) = c.active else {
            return Opd::V(new);
        };
        let reg = c.inst.dsts[j].0;
        let old = match c.imps.iter().find(|(r, _)| *r == reg) {
            Some((_, Opd::V(v))) => *v,
            _ => new,
        };
        Opd::V(self.g.select(act, new, old))
    }

    fn mubuf(&mut self, c: &Ctx<Gen>) -> R<Option<Vec<Opd<Gen>>>> {
        let i = &c.inst.instr;
        let name = i.name();
        let (load, n) = match name {
            "buffer_load_dword" => (true, 1),
            "buffer_load_dwordx2" => (true, 2),
            "buffer_load_dwordx3" => (true, 3),
            "buffer_load_dwordx4" => (true, 4),
            "buffer_store_dword" => (false, 1),
            "buffer_store_dwordx2" => (false, 2),
            "buffer_store_dwordx3" => (false, 3),
            "buffer_store_dwordx4" => (false, 4),
            "buffer_load_ubyte" | "buffer_load_sbyte" | "buffer_load_ushort"
            | "buffer_load_sshort" => (true, 1),
            _ => {
                if let Some(op) = name.strip_prefix("buffer_atomic_") {
                    return self.mubuf_atomic(c, op);
                }
                if let Some((load, rest)) = name
                    .strip_prefix("buffer_load_format_")
                    .map(|r| (true, r))
                    .or_else(|| {
                        name.strip_prefix("buffer_store_format_")
                            .map(|r| (false, r))
                    })
                {
                    let n = Self::format_components(rest)?;
                    return self.mubuf_format(c, load, n, false);
                }
                // a typed buffer load (`tbuffer_*`) carries its format in the instruction and
                // always uses its address VGPR
                if let Some((load, rest)) = name
                    .strip_prefix("tbuffer_load_format_")
                    .map(|r| (true, r))
                    .or_else(|| {
                        name.strip_prefix("tbuffer_store_format_")
                            .map(|r| (false, r))
                    })
                {
                    let n = Self::format_components(rest)?;
                    return self.mubuf_format(c, load, n, true);
                }
                return Ok(None);
            }
        };
        let m = i.mem;
        if m.tfe {
            return refuse(&format!("{name} with tfe"));
        }
        let ns = c.srcs.len();
        if ns < 2 || (load && c.inst.dsts.len() != n) {
            return bad("a buffer instruction whose operands do not match its width");
        }
        let buf = self.buffer_of(&c.inst.srcs[ns - 2], &c.srcs[ns - 2])?;
        if std::env::var_os("NATIV64_ORIGINS").is_some() {
            eprintln!(
                "origin: {name} idxen {} offen {} offset {}",
                m.idxen, m.offen, m.offset
            );
        }
        let t64 = self.t64();
        // offset = soffset + inst_offset + voffset (offen) + index * stride (idxen)
        let soff = self.bits_of(c.srcs[ns - 1][0]);
        let mut off = self.zext(soff);
        off = self.add_k64(off, u64::from(m.offset as u32));
        let addr = if load { 0 } else { 1 };
        let mut at = c
            .srcs
            .get(addr)
            .filter(|_| m.offen || m.idxen)
            .map(|v| v.iter());
        if m.idxen {
            let Some(&ix) = at.as_mut().and_then(|a| a.next()) else {
                return bad("a buffer index operand is missing");
            };
            let ix = self.bits_of(ix);
            let (ix, st) = (self.zext(ix), self.zext(buf.stride));
            let scaled = self.g.op(Op::IMul, t64, &[ix, st]);
            off = self.add64(off, scaled);
        }
        if m.offen {
            let Some(&vo) = at.as_mut().and_then(|a| a.next()) else {
                return bad("a buffer offset operand is missing");
            };
            let vo = self.bits_of(vo);
            let vo = self.zext(vo);
            off = self.add64(off, vo);
        }
        let base = self.add64(buf.base, off);
        let mut out = Vec::new();
        for j in 0..n {
            let valid = self.in_range(&buf, off, j as u64);
            let a = self.add_k64(base, 4 * j as u64);
            if load {
                let ok = self.and(valid, c.active);
                let v = self.guarded_load(a, ok)?;
                // a byte or a word of the view: its low bits, extended
                let v = match name {
                    "buffer_load_ubyte" => self.narrow(v, 8, false),
                    "buffer_load_sbyte" => self.narrow(v, 8, true),
                    "buffer_load_ushort" => self.narrow(v, 16, false),
                    "buffer_load_sshort" => self.narrow(v, 16, true),
                    _ => v,
                };
                out.push(self.lane_result(c, j, v));
            } else {
                let Some(&d) = c.srcs[0].get(j) else {
                    return bad("a buffer store with too few data dwords");
                };
                let d = self.bits_of(d);
                let ok = self.and(valid, c.active);
                self.guarded(Some(ok), |t| {
                    t.bda_store_u32(a, d);
                    Ok(())
                })?;
            }
        }
        Ok(Some(out))
    }

    /// `buffer_atomic_*`: a 32-bit atomic on the buffer at the address. Device scope,
    /// relaxed ordering; the old value is written back only with `glc` (an atomic's
    /// destination is otherwise not a result).
    fn mubuf_atomic(&mut self, c: &Ctx<Gen>, op: &str) -> R<Option<Vec<Opd<Gen>>>> {
        let i = &c.inst.instr;
        let m = i.mem;
        let (spirv_op, operands) = match op {
            "add" => (Op::AtomicIAdd, 1),
            "sub" => (Op::AtomicISub, 1),
            "smin" => (Op::AtomicSMin, 1),
            "umin" => (Op::AtomicUMin, 1),
            "smax" => (Op::AtomicSMax, 1),
            "umax" => (Op::AtomicUMax, 1),
            "and" => (Op::AtomicAnd, 1),
            "or" => (Op::AtomicOr, 1),
            "xor" => (Op::AtomicXor, 1),
            "exchange" | "swap" => (Op::AtomicExchange, 1),
            "inc" => (Op::AtomicIIncrement, 0),
            "dec" => (Op::AtomicIDecrement, 0),
            "cmpswap" => (Op::AtomicCompareExchange, 2),
            _ => return refuse(&format!("buffer_atomic_{op}")),
        };
        let ns = c.srcs.len();
        if ns < 2 {
            return bad("a buffer atomic without a descriptor");
        }
        let buf = self.buffer_of(&c.inst.srcs[ns - 2], &c.srcs[ns - 2])?;
        let tu = self.g.t_u32();
        let t64 = self.t64();
        let soff = self.bits_of(c.srcs[ns - 1][0]);
        let mut off = self.zext(soff);
        off = self.add_k64(off, u64::from(m.offset as u32));
        let mut at = c
            .srcs
            .get(1)
            .filter(|_| m.offen || m.idxen)
            .map(|v| v.iter());
        if m.idxen {
            let Some(&ix) = at.as_mut().and_then(|a| a.next()) else {
                return bad("a buffer index operand is missing");
            };
            let ix = self.bits_of(ix);
            let ix = self.zext(ix);
            let stride = self.zext(buf.stride);
            let step = self.g.op(Op::IMul, t64, &[ix, stride]);
            off = self.add64(off, step);
        }
        if m.offen {
            let Some(&vo) = at.as_mut().and_then(|a| a.next()) else {
                return bad("a buffer offset operand is missing");
            };
            let vo = self.bits_of(vo);
            let vo = self.zext(vo);
            off = self.add64(off, vo);
        }
        let base = self.add64(buf.base, off);
        let inside = self.in_range(&buf, off, 0);
        let ok = self.and(inside, c.active);
        let p = self.g.t_ptr(StorageClass::PhysicalStorageBuffer, tu);
        let ptr = self.g.op(Op::ConvertUToPtr, p, &[base]);
        let scope = self.g.k(1);
        let relaxed = self.g.k(0);
        let value = if operands >= 1 {
            Some(self.bits_of(c.srcs[0][0]))
        } else {
            None
        };
        let old = match spirv_op {
            Op::AtomicCompareExchange => {
                let cmp = value.unwrap_or(0);
                let new = self.bits_of(c.srcs[0][1]);
                self.g
                    .op(spirv_op, tu, &[ptr, scope, relaxed, relaxed, cmp, new])
            }
            Op::AtomicIIncrement | Op::AtomicIDecrement => {
                self.g.op(spirv_op, tu, &[ptr, scope, relaxed])
            }
            _ => {
                let Some(v) = value else {
                    return bad("a buffer atomic without a value");
                };
                self.g.op(spirv_op, tu, &[ptr, scope, relaxed, v])
            }
        };
        if c.inst.dsts.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let zero = self.g.k(0);
        let picked = self.g.select(ok, old, zero);
        Ok(Some(vec![self.lane_result(c, 0, picked)]))
    }

    /// The component count of a `*_format_x/y/z/w` opcode suffix.
    fn format_components(suffix: &str) -> R<usize> {
        Ok(match suffix {
            "x" => 1,
            "xy" => 2,
            "xyz" => 3,
            "xyzw" => 4,
            other => return refuse(&format!("buffer format op '{other}'")),
        })
    }

    /// `buffer_load_format_*` / `buffer_store_format_*`: `n` components of the instruction's
    /// buffer format at the address (the descriptor's stride steps between elements).
    fn mubuf_format(
        &mut self,
        c: &Ctx<Gen>,
        load: bool,
        n: usize,
        address_used: bool,
    ) -> R<Option<Vec<Opd<Gen>>>> {
        let i = &c.inst.instr;
        let m = i.mem;
        // a `buffer_*_format_*` op has no format of its own: the descriptor's (a runtime value
        // in the view record) names it, so the components are only known per candidate format
        let runtime = m.format == 0 && !address_used;
        let (bits, num) = if runtime {
            (Vec::new(), BufNum::Uint)
        } else {
            ufmt(m.format)?
        };
        if !runtime {
            if bits.len() < n {
                return bad("a buffer format op wider than its format");
            }
            if bits.iter().any(|b| b % 8 != 0) {
                return refuse("a packed buffer format");
            }
        }
        let elem: u32 = if runtime {
            // the widest format the runtime chain reads
            16
        } else {
            bits.iter().map(|b| b / 8).sum()
        };
        let ns = c.srcs.len();
        if ns < 2 || (load && c.inst.dsts.len() != n) {
            return bad("a buffer format op whose operands do not match its width");
        }
        let buf = self.buffer_of(&c.inst.srcs[ns - 2], &c.srcs[ns - 2])?;
        let t64 = self.t64();
        let soff = self.bits_of(c.srcs[ns - 1][0]);
        let mut off = self.zext(soff);
        off = self.add_k64(off, u64::from(m.offset as u32));
        let addr = usize::from(load);
        let mut at = c
            .srcs
            .get(addr)
            .filter(|_| m.offen || m.idxen || address_used)
            .map(|v| v.iter());
        if m.idxen {
            let Some(&ix) = at.as_mut().and_then(|a| a.next()) else {
                return bad("a buffer index operand is missing");
            };
            let ix = self.bits_of(ix);
            let ix = self.zext(ix);
            let stride = self.zext(buf.stride);
            let scaled = self.g.op(Op::IMul, t64, &[ix, stride]);
            // a descriptor with no stride steps by the format's own size
            let tb = self.g.t_bool();
            let zero = self.g.k(0);
            let raw = self.g.op(Op::IEqual, tb, &[buf.stride, zero]);
            let size = self.k64(u64::from(elem));
            let by_format = self.g.op(Op::IMul, t64, &[ix, size]);
            let step = self.g.op(Op::Select, t64, &[raw, by_format, scaled]);
            off = self.add64(off, step);
        }
        // a typed buffer's address is a byte offset when it is not an element index
        if m.offen || (address_used && !m.idxen) {
            let Some(&vo) = at.as_mut().and_then(|a| a.next()) else {
                return bad("a buffer offset operand is missing");
            };
            let vo = self.bits_of(vo);
            let vo = self.zext(vo);
            off = self.add64(off, vo);
        }
        let base = self.add64(buf.base, off);
        let tb = self.g.t_bool();
        let end = self.add_k64(off, u64::from(elem));
        let inside = self.g.op(Op::ULessThanEqual, tb, &[end, buf.size]);
        let tu = self.g.t_u32();
        let dwords = elem.div_ceil(4);
        let runtime_fmt = if runtime {
            if !matches!(
                c.inst.srcs[ns - 2].first(),
                Some(Src::Val(v)) if self.res().analysis.loaded.contains(v)
            ) {
                return refuse(&format!("{} of a descriptor that is not loaded", i.name()));
            }
            let index = self.bits_of(c.srcs[ns - 2][0]);
            Some(self.record_dword(index, 4))
        } else {
            None
        };
        if !load {
            if let Some(fmt) = runtime_fmt {
                // one predicated store per candidate format: the bytes a store covers depend
                // on the format, so each candidate writes its own range
                let tb = self.g.t_bool();
                let tu = self.g.t_u32();
                for &(dxgi, cbits, cnum) in KNOWN_FORMATS {
                    if cbits.len() < n {
                        continue;
                    }
                    let bytes: u32 = cbits[..n].iter().map(|b| b / 8).sum();
                    let dwords = bytes.div_ceil(4) as usize;
                    let mut words = vec![self.g.k(0); dwords];
                    let mut bit = 0u32;
                    for (j, &b) in cbits.iter().take(n).enumerate() {
                        let Some(&d) = c.srcs[0].get(j) else {
                            return bad("a buffer store with too few data dwords");
                        };
                        let d = self.bits_of(d);
                        let raw = self.pack_component(d, b, cnum);
                        let shift = bit % 32;
                        let at = (bit / 32) as usize;
                        let raw = if shift == 0 {
                            raw
                        } else {
                            let sh = self.g.k(shift);
                            self.g.op(Op::ShiftLeftLogical, tu, &[raw, sh])
                        };
                        words[at] = self.g.op(Op::BitwiseOr, tu, &[words[at], raw]);
                        bit += b;
                    }
                    let want = self.g.k(dxgi);
                    let is = self.g.op(Op::IEqual, tb, &[fmt, want]);
                    for (k, &w) in words.iter().enumerate() {
                        let kk = k as u32;
                        let a = self.add_k64(base, u64::from(4 * kk));
                        let end = self.add_k64(off, u64::from(4 * (kk + 1)));
                        let in_range = self.g.op(Op::ULessThanEqual, tb, &[end, buf.size]);
                        let ok = self.and(in_range, Some(is));
                        let ok = self.and(ok, c.active);
                        let whole = 4 * (kk + 1) <= bytes;
                        if whole {
                            self.guarded(Some(ok), |t| {
                                t.bda_store_u32(a, w);
                                Ok(())
                            })?;
                        } else {
                            let old = self.guarded_load(a, ok)?;
                            let kept = self.g.k(u32::MAX << (8 * (bytes - 4 * kk)));
                            let old = self.g.op(Op::BitwiseAnd, tu, &[old, kept]);
                            let merged = self.g.op(Op::BitwiseOr, tu, &[old, w]);
                            self.guarded(Some(ok), |t| {
                                t.bda_store_u32(a, merged);
                                Ok(())
                            })?;
                        }
                    }
                }
                return Ok(Some(Vec::new()));
            }
            // pack the components, then write whole dwords (the last one merges with memory
            // when the store does not cover it)
            let mut words = vec![self.g.k(0); dwords as usize];
            let mut bit = 0u32;
            for (j, &b) in bits.iter().take(n).enumerate() {
                let Some(&d) = c.srcs[0].get(j) else {
                    return bad("a buffer store with too few data dwords");
                };
                let d = self.bits_of(d);
                let raw = self.pack_component(d, b, num);
                let shift = bit % 32;
                let at = (bit / 32) as usize;
                let raw = if shift == 0 {
                    raw
                } else {
                    let sh = self.g.k(shift);
                    self.g.op(Op::ShiftLeftLogical, tu, &[raw, sh])
                };
                words[at] = self.g.op(Op::BitwiseOr, tu, &[words[at], raw]);
                bit += b;
            }
            let stored: u32 = bits[..n].iter().map(|b| b / 8).sum();
            for (k, &w) in words.iter().enumerate() {
                let kk = k as u32;
                let a = self.add_k64(base, u64::from(4 * kk));
                let ok = self.and(inside, c.active);
                let whole = 4 * (kk + 1) <= stored;
                if !whole {
                    // merge with the bytes the store does not cover
                    let old = self.guarded_load(a, ok)?;
                    let kept = self.g.k(if stored > 4 * kk {
                        u32::MAX << (8 * (stored - 4 * kk))
                    } else {
                        u32::MAX
                    });
                    let w = self.g.op(Op::BitwiseAnd, tu, &[old, kept]);
                    let w = self.g.op(Op::BitwiseOr, tu, &[w, words[k]]);
                    self.guarded(Some(ok), |t| {
                        t.bda_store_u32(a, w);
                        Ok(())
                    })?;
                } else {
                    self.guarded(Some(ok), |t| {
                        t.bda_store_u32(a, w);
                        Ok(())
                    })?;
                }
            }
            return Ok(Some(Vec::new()));
        }
        let mut words = Vec::new();
        for k in 0..dwords {
            let a = self.add_k64(base, u64::from(4 * k));
            let ok = self.and(inside, c.active);
            words.push(self.guarded_load(a, ok)?);
        }
        let mut out = Vec::new();
        if let Some(fmt) = runtime_fmt {
            // one conversion per candidate format, selected by the descriptor's format
            let tb = self.g.t_bool();
            for j in 0..n {
                let mut selected = self.g.k(0);
                for &(dxgi, cbits, cnum) in KNOWN_FORMATS {
                    if cbits.len() < n {
                        continue;
                    }
                    let bit: u32 = cbits[..j].iter().sum();
                    let v = if matches!(cnum, BufNum::Float) && cbits[j] == 16 {
                        self.unpack_half(&words, bit)
                    } else {
                        let raw = self.extract_bits(&words, bit, cbits[j]);
                        self.unpack_component(raw, cbits[j], cnum)
                    };
                    let want = self.g.k(dxgi);
                    let is = self.g.op(Op::IEqual, tb, &[fmt, want]);
                    selected = self.g.select(is, v, selected);
                }
                out.push(self.lane_result(c, j, selected));
            }
            return Ok(Some(out));
        }
        let mut bit = 0u32;
        for (j, &b) in bits.iter().take(n).enumerate() {
            let raw = self.extract_bits(&words, bit, b);
            let v = self.unpack_component(raw, b, num);
            out.push(self.lane_result(c, j, v));
            bit += b;
        }
        Ok(Some(out))
    }

    /// The dword a format load leaves for one component (float bits for the normalized and
    /// float formats, the integer itself otherwise).
    fn unpack_component(&mut self, raw: u32, bits: u32, num: BufNum) -> u32 {
        let tf = self.g.t_f32();
        match num {
            BufNum::Uint | BufNum::Float => raw,
            BufNum::Unorm => {
                let f = self.g.op(Op::ConvertUToF, tf, &[raw]);
                let scale = self.g.fconst(1.0 / ((1u64 << bits) - 1) as f32);
                let f = self.g.op(Op::FMul, tf, &[f, scale]);
                self.g.as_bits(f)
            }
            BufNum::Snorm => {
                let s = self.sign_extend(raw, bits);
                let f = self.g.op(Op::ConvertSToF, tf, &[s]);
                let scale = self.g.fconst(1.0 / ((1u64 << (bits - 1)) - 1) as f32);
                let f = self.g.op(Op::FMul, tf, &[f, scale]);
                let one = self.g.fconst(1.0);
                let mone = self.g.fconst(-1.0);
                let hi = self.g.ext(tf, GL::FMin, &[f, one]);
                let lo = self.g.ext(tf, GL::FMax, &[hi, mone]);
                self.g.as_bits(lo)
            }
            BufNum::Sint => self.sign_extend(raw, bits),
        }
    }

    /// The packed bits a format store takes for one component.
    fn pack_component(&mut self, d: u32, bits: u32, num: BufNum) -> u32 {
        let (tu, tf) = (self.g.t_u32(), self.g.t_f32());
        match num {
            BufNum::Uint | BufNum::Float | BufNum::Sint => d,
            BufNum::Unorm => {
                let zero = self.g.fconst(0.0);
                let one = self.g.fconst(1.0);
                let f = self.g.as_f32(d);
                let f = self.g.ext(tf, GL::FMax, &[f, zero]);
                let f = self.g.ext(tf, GL::FMin, &[f, one]);
                let scale = self.g.fconst(((1u64 << bits) - 1) as f32);
                let f = self.g.op(Op::FMul, tf, &[f, scale]);
                let f = self.g.ext(tf, GL::RoundEven, &[f]);
                self.g.op(Op::ConvertFToU, tu, &[f])
            }
            BufNum::Snorm => {
                let mone = self.g.fconst(-1.0);
                let one = self.g.fconst(1.0);
                let f = self.g.as_f32(d);
                let f = self.g.ext(tf, GL::FMax, &[f, mone]);
                let f = self.g.ext(tf, GL::FMin, &[f, one]);
                let scale = self.g.fconst(((1u64 << (bits - 1)) - 1) as f32);
                let f = self.g.op(Op::FMul, tf, &[f, scale]);
                let f = self.g.ext(tf, GL::RoundEven, &[f]);
                let s = self.g.op(Op::ConvertFToS, tu, &[f]);
                let mask = self.g.k(if bits >= 32 {
                    u32::MAX
                } else {
                    (1u32 << bits) - 1
                });
                self.g.op(Op::BitwiseAnd, tu, &[s, mask])
            }
        }
    }

    /// A `bits`-wide signed value as i32.
    fn sign_extend(&mut self, raw: u32, bits: u32) -> u32 {
        let sh = self.g.k(32 - bits);
        let t = self.g.t_u32();
        let l = self.g.op(Op::ShiftLeftLogical, t, &[raw, sh]);
        let ti = self.g.m.global(Op::TypeInt, None, &[32, 1]);
        self.g.op(Op::ShiftRightArithmetic, ti, &[l, sh])
    }

    // -----------------------------------------------------------------------------------
    // Images

    /// The `Dim` and arrayed flag of a MIMG dimension, and its coordinate count.
    fn image_dim(dim: u8) -> Option<(u32, u32, u32)> {
        // (SPIR-V Dim, arrayed, coordinates the address holds)
        match dim {
            0 => Some((0, 0, 1)),
            1 => Some((1, 0, 2)),
            2 => Some((2, 0, 3)),
            3 => Some((3, 0, 3)),
            5 => Some((1, 1, 3)),
            _ => None,
        }
    }

    /// The array of sampled images of this dimension (set 0, binding 0).
    fn texture_array(&mut self, dim: u8) -> (u32, u32) {
        if let Some(&t) = self.res().textures.get(&dim) {
            return t;
        }
        let (d, arrayed, _) = Self::image_dim(dim).expect("a dimension that was checked");
        self.g.m.capability(Capability::RuntimeDescriptorArray);
        self.g
            .m
            .capability(Capability::SampledImageArrayNonUniformIndexing);
        self.g.m.capability(Capability::ShaderNonUniform);
        if d == 0 {
            self.g.m.capability(Capability::Sampled1D);
        }
        let tf = self.g.t_f32();
        let img = self
            .g
            .m
            .global(Op::TypeImage, None, &[tf, d, 0, arrayed, 0, 1, 0]);
        let arr = self.g.m.global(Op::TypeRuntimeArray, None, &[img]);
        let var = self.global(StorageClass::UniformConstant, arr);
        self.g
            .m
            .decorate(var, Decoration::DescriptorSet, &[SET_BINDLESS]);
        self.g.m.decorate(var, Decoration::Binding, &[BIND_SAMPLED]);
        self.g.m.name(var, &format!("textures_dim{dim}"));
        self.res().textures.insert(dim, (var, img));
        (var, img)
    }

    fn t_sampler(&mut self) -> u32 {
        self.g.m.global(Op::TypeSampler, None, &[])
    }

    /// The array of samplers (set 0, binding 2).
    fn sampler_array(&mut self) -> u32 {
        if let Some(v) = self.res().sampler_array {
            return v;
        }
        self.g.m.capability(Capability::RuntimeDescriptorArray);
        self.g
            .m
            .capability(Capability::SampledImageArrayNonUniformIndexing);
        self.g.m.capability(Capability::ShaderNonUniform);
        let s = self.t_sampler();
        let arr = self.g.m.global(Op::TypeRuntimeArray, None, &[s]);
        let var = self.global(StorageClass::UniformConstant, arr);
        self.g
            .m
            .decorate(var, Decoration::DescriptorSet, &[SET_BINDLESS]);
        self.g
            .m
            .decorate(var, Decoration::Binding, &[BIND_SAMPLERS]);
        self.g.m.name(var, "samplers");
        self.res().sampler_array = Some(var);
        var
    }

    /// The variable of static sampler `k` (set 1, binding k).
    fn static_sampler(&mut self, desc: SamplerDesc) -> u32 {
        let k = match self.res().samplers.iter().position(|d| *d == desc) {
            Some(k) => k,
            None => {
                self.res().samplers.push(desc);
                self.res().samplers.len() - 1
            }
        };
        if let Some(&v) = self.res().static_vars.get(k) {
            return v;
        }
        let s = self.t_sampler();
        let var = self.global(StorageClass::UniformConstant, s);
        self.g
            .m
            .decorate(var, Decoration::DescriptorSet, &[SET_STATIC]);
        self.g.m.decorate(var, Decoration::Binding, &[k as u32]);
        self.g.m.name(var, &format!("static_sampler{k}"));
        self.res().static_vars.push(var);
        var
    }

    fn non_uniform(&mut self, id: u32) -> u32 {
        // a value can be the index of more than one access chain; one decoration is enough
        if self.res().non_uniform.insert(id) {
            self.g.m.decorate(id, Decoration::NonUniform, &[]);
        }
        id
    }

    /// Element `index` of a descriptor array, loaded.
    fn array_element(&mut self, var: u32, elem: u32, index: u32) -> u32 {
        let p = self.g.t_ptr(StorageClass::UniformConstant, elem);
        let a = self.g.op(Op::AccessChain, p, &[var, index]);
        self.non_uniform(a);
        let v = self.g.load(elem, a);
        self.non_uniform(v)
    }

    /// The direction a cube face coordinate triple `(s, t, face)` stands for: the hardware's
    /// face coordinates are `sc / |ma| / 2 + 1.5` and `tc / |ma| / 2 + 1.5`, so the face
    /// plane coordinates in [-1, 1] are `2 * (x - 1.5)`; the face order is +X, -X, +Y, -Y,
    /// +Z, -Z with the usual `sc`, `tc` and `ma` of each.
    fn cube_direction(&mut self, s: u32, t: u32, face: u32) -> u32 {
        let (tf, tb) = (self.g.t_f32(), self.g.t_bool());
        let (c15, c2) = (self.g.fconst(1.5), self.g.fconst(2.0));
        let one = self.g.fconst(1.0);
        let plane = |t: &mut Self, x: u32| {
            let d = t.g.op(Op::FSub, tf, &[x, c15]);
            t.g.op(Op::FMul, tf, &[d, c2])
        };
        let (a, b) = (plane(self, s), plane(self, t));
        let (na, nb) = (
            self.g.op(Op::FNegate, tf, &[a]),
            self.g.op(Op::FNegate, tf, &[b]),
        );
        let mone = self.g.op(Op::FNegate, tf, &[one]);
        let tu = self.g.t_u32();
        let fi = self.g.op(Op::ConvertFToU, tu, &[face]);
        let pick = |t: &mut Self, vals: [u32; 6]| {
            let mut r = vals[5];
            for f in (0..5).rev() {
                let k = t.g.k(f as u32);
                let is = t.g.op(Op::IEqual, tb, &[fi, k]);
                r = t.g.op(Op::Select, tf, &[is, vals[f], r]);
            }
            r
        };
        // (x, y, z) of each face with |major| = 1 (sc: +X -z, -X +z, +Y +x, -Y +x, +Z +x, -Z -x;
        // tc: +X -y, -X -y, +Y +z, -Y -z, +Z -y, -Z -y)
        let x = pick(self, [one, mone, a, a, a, na]);
        let y = pick(self, [nb, nb, one, mone, nb, nb]);
        let z = pick(self, [na, a, b, nb, one, mone]);
        let v3 = self.g.m.global(Op::TypeVector, None, &[tf, 3]);
        self.g.op(Op::CompositeConstruct, v3, &[x, y, z])
    }

    /// The sampled image a sample or gather instruction reads: the image of the resource row
    /// (a table slot) with the sampler of the next row (or four constant words).
    fn sampled_image(&mut self, c: &Ctx<Gen>) -> R<u32> {
        let name = c.inst.instr.name();
        let m = c.inst.instr.img;
        let ns = c.srcs.len();
        let rsrc = &c.inst.srcs[ns - 2];
        let loaded = |t: &mut Self, s: Option<&Src>| matches!(s, Some(Src::Val(v)) if t.res().analysis.loaded.contains(v));
        if !loaded(self, rsrc.first()) {
            return refuse("an image that is not loaded from a table");
        }
        if std::env::var_os("NATIV64_ORIGINS").is_some() {
            let origin = rsrc.first().and_then(|s| match s {
                Src::Val(v) => self.res().analysis.origin.get(v).copied(),
                _ => None,
            });
            eprintln!("origin: {name} image {origin:?}");
        }
        let (tex, img) = self.texture_array(m.dim);
        let index = self.bits_of(c.srcs[ns - 2][0]);
        self.non_uniform(index);
        let image = self.array_element(tex, img, index);
        // the sampler: loaded from a table, or four constant words
        let samp = &c.inst.srcs[ns - 1];
        if std::env::var_os("NATIV64_ORIGINS").is_some() {
            let static_words = matches!(
                &samp[..4],
                [Src::Const(_), Src::Const(_), Src::Const(_), Src::Const(_)]
            );
            if static_words {
                let w: Vec<u32> = samp[..4]
                    .iter()
                    .map(|s| match s {
                        Src::Const(c) => *c,
                        _ => u32::MAX,
                    })
                    .collect();
                eprintln!("origin: {name} sampler static words {w:08x?}");
            } else {
                let origin = samp.first().and_then(|s| match s {
                    Src::Val(v) => self.res().analysis.origin.get(v).copied(),
                    _ => None,
                });
                eprintln!("origin: {name} sampler table {origin:?}");
            }
        }
        let ts = self.t_sampler();
        let sampler = if loaded(self, samp.first()) {
            let arr = self.sampler_array();
            let si = self.bits_of(c.srcs[ns - 1][0]);
            self.non_uniform(si);
            self.array_element(arr, ts, si)
        } else {
            // the four words of a sampler state, when they are the program's constants; a
            // word the program computes keeps a default (the address modes and the compare
            // function are too visible to guess)
            let word = |k: usize, default: u32| -> R<u32> {
                match samp.get(k) {
                    Some(Src::Const(c)) => Ok(*c),
                    // W1: min/max LOD (0..max), W2: bilinear, mip linear, no bias,
                    // W3: transparent black
                    Some(_) => Ok(match k {
                        1 => 0xfff << 12,
                        2 => 1 << 20 | 1 << 22 | 2 << 26,
                        3 => 0,
                        _ => default,
                    }),
                    None => Ok(default),
                }
            };
            if !matches!(samp.first(), Some(Src::Const(_))) {
                return refuse("a sampler whose address modes are not constants");
            }
            let w = [word(0, 0)?, word(1, 0)?, word(2, 0)?, word(3, 0)?];
            let var = self.static_sampler(SamplerDesc::from_words(w));
            self.g.load(ts, var)
        };
        let tsi = self.g.m.global(Op::TypeSampledImage, None, &[img]);
        let si = self.g.op(Op::SampledImage, tsi, &[image, sampler]);
        self.non_uniform(si);
        Ok(si)
    }

    /// `image_get_resinfo`: width, height, depth or layers and the level count of the level
    /// the address names (a channel the dmask leaves out reads 1).
    fn image_resinfo(&mut self, c: &Ctx<Gen>) -> R<Option<Vec<Opd<Gen>>>> {
        let i = &c.inst.instr;
        let m = i.img;
        let Some((dim, _arrayed, _ncoord)) = Self::image_dim(m.dim) else {
            return refuse(&format!("image_get_resinfo on dimension {}", m.dim));
        };
        let ns = c.srcs.len();
        if ns < 2 {
            return bad("image_get_resinfo without its operands");
        }
        let addr = &c.srcs[ns - 2];
        if addr.is_empty() {
            return bad("image_get_resinfo without its level");
        }
        let rsrc = &c.inst.srcs[ns - 1];
        let loaded = |t: &mut Self, s: Option<&Src>| matches!(s, Some(Src::Val(v)) if t.res().analysis.loaded.contains(v));
        if !loaded(self, rsrc.first()) {
            return refuse("image_get_resinfo of an image that is not loaded from a table");
        }
        let (tex, img) = self.texture_array(m.dim);
        let index = self.bits_of(c.srcs[ns - 1][0]);
        self.non_uniform(index);
        let image = self.array_element(tex, img, index);
        let mip = self.bits_of(addr[0]);
        self.g.m.capability(Capability::ImageQuery);
        let tu = self.g.t_u32();
        let ncomp = if dim == 1 { 2 } else { 3 };
        let size_ty = self.g.m.global(Op::TypeVector, None, &[tu, ncomp]);
        let size = self.g.op(Op::ImageQuerySizeLod, size_ty, &[image, mip]);
        let levels = self.g.op(Op::ImageQueryLevels, tu, &[image]);
        let one = self.g.k(1);
        let chans: Vec<u32> = (0..4).filter(|ch| m.dmask >> ch & 1 != 0).collect();
        if chans.len() != c.inst.dsts.len() {
            return bad("image_get_resinfo whose results do not match its dmask");
        }
        let mut out = Vec::new();
        for (j, ch) in chans.into_iter().enumerate() {
            let v = match ch {
                0 | 1 => self.g.op(Op::CompositeExtract, tu, &[size, ch]),
                2 if ncomp == 3 => self.g.op(Op::CompositeExtract, tu, &[size, 2]),
                2 => one,
                _ => levels,
            };
            out.push(self.lane_result(c, j, v));
        }
        Ok(Some(out))
    }

    /// `image_gather4_lz`: four texels at LOD 0 of the channel a dmask bit selects (four dwords
    /// per channel) with `OpImageGather`.
    fn image_gather(&mut self, c: &Ctx<Gen>) -> R<Option<Vec<Opd<Gen>>>> {
        let i = &c.inst.instr;
        let name = i.name();
        let m = i.img;
        if name != "image_gather4_lz" {
            return refuse(name);
        }
        if m.a16 || m.d16 || m.tfe || m.lwe || m.unorm {
            return refuse(&format!("{name} with a16, d16, tfe, lwe or unorm"));
        }
        let Some((dim, _arrayed, ncoord)) = Self::image_dim(m.dim) else {
            return refuse(&format!("{name} on dimension {}", m.dim));
        };
        if dim != 1 {
            return refuse(&format!("{name} on a non-2D image"));
        }
        let pixel = matches!(self.io(), StageIo::Pixel(_));
        let ns = c.srcs.len();
        if ns < 3 || c.srcs[ns - 2].len() < 8 || c.srcs[ns - 1].len() < 4 {
            return bad("a gather whose operands do not match its form");
        }
        let addr = &c.srcs[ns - 3];
        if addr.len() != ncoord as usize {
            return refuse(&format!("{name} with an address of {} dwords", addr.len()));
        }
        let chans: Vec<u32> = (0..4).filter(|ch| m.dmask >> ch & 1 != 0).collect();
        if chans.is_empty() {
            return bad("a gather without a channel");
        }
        if c.inst.dsts.len() != 4 * chans.len() {
            return bad("a gather whose results do not match its dmask");
        }
        let si = self.sampled_image(c)?;
        // the coordinates are floats (the console keeps them as dwords)
        let tf = self.g.t_f32();
        let mut cs = Vec::new();
        for &a in addr {
            let bits = self.bits_of(a);
            cs.push(self.g.as_f32(bits));
        }
        let coord = if cs.len() == 1 {
            cs[0]
        } else {
            let tv = self
                .g
                .m
                .global(Op::TypeVector, None, &[tf, cs.len() as u32]);
            self.g.op(Op::CompositeConstruct, tv, &cs)
        };
        if !pixel {
            // A program without quad derivatives (a compute or vertex one) cannot gather at an
            // implicit level: the four texels of the 2x2 block at LOD 0 are fetched instead.
            return self.gather_fetched(c, chans, &coord, m.dim);
        }
        let v4 = self.vec4(Class::Float);
        let tf = self.g.t_f32();
        let mut out = Vec::new();
        for (j, ch) in chans.into_iter().enumerate() {
            let ch = self.g.k(ch);
            let res = self.g.op(Op::ImageGather, v4, &[si, coord, ch]);
            for k in 0..4 {
                let x = self.g.op(Op::CompositeExtract, tf, &[res, k]);
                let bits = self.g.as_bits(x);
                out.push(self.lane_result(c, j * 4 + k as usize, bits));
            }
        }
        Ok(Some(out))
    }

    /// `image_gather4_lz` without quad derivatives: the four texels of the 2x2 block at LOD 0,
    /// as `((x0, y0), (x1, y0), (x0, y1), (x1, y1))` in the gather's order.
    fn gather_fetched(
        &mut self,
        c: &Ctx<Gen>,
        chans: Vec<u32>,
        coord: &u32,
        dim: u8,
    ) -> R<Option<Vec<Opd<Gen>>>> {
        let ns = c.srcs.len();
        let (_, arrayed, _) = Self::image_dim(dim).expect("a dimension that was checked");
        let (tex, img) = self.texture_array(dim);
        let index = self.bits_of(c.srcs[ns - 1][0]);
        self.non_uniform(index);
        let image = self.array_element(tex, img, index);
        let tu = self.g.t_u32();
        let tf = self.g.t_f32();
        self.g.m.capability(Capability::ImageQuery);
        let ncomp = if arrayed != 0 { 3 } else { 2 };
        let tu2 = self.g.m.global(Op::TypeVector, None, &[tu, ncomp]);
        let zero = self.g.k(0);
        let size = self.g.op(Op::ImageQuerySizeLod, tu2, &[image, zero]);
        let sw = self.g.op(Op::CompositeExtract, tu, &[size, 0]);
        let sh = self.g.op(Op::CompositeExtract, tu, &[size, 1]);
        let cw = self.g.op(Op::CompositeExtract, tf, &[*coord, 0]);
        let chf = self.g.op(Op::CompositeExtract, tf, &[*coord, 1]);
        let half = self.g.fconst(0.5);
        let one = self.g.fconst(1.0);
        let scaled = |t: &mut Self, c: u32, size: u32| {
            let s = t.g.op(Op::ConvertUToF, tf, &[size]);
            let x = t.g.op(Op::FMul, tf, &[c, s]);
            t.g.op(Op::FSub, tf, &[x, half])
        };
        let fx = scaled(self, cw, sw);
        let fy = scaled(self, chf, sh);
        let ix = self.g.op(Op::ConvertFToS, tu, &[fx]);
        let iy = self.g.op(Op::ConvertFToS, tu, &[fy]);
        let layer = if arrayed != 0 {
            let lf = self.g.op(Op::CompositeExtract, tf, &[*coord, 2]);
            let l = self.g.op(Op::ConvertFToS, tu, &[lf]);
            let count = self.g.op(Op::CompositeExtract, tu, &[size, 2]);
            let one_i = self.g.k(1);
            let max_l = self.g.op(Op::ISub, tu, &[count, one_i]);
            Some((l, max_l))
        } else {
            None
        };
        let one_i = self.g.k(1);
        let max_w = self.g.op(Op::ISub, tu, &[sw, one_i]);
        let max_h = self.g.op(Op::ISub, tu, &[sh, one_i]);
        let clamp = |t: &mut Self, v: u32, max: u32| {
            let z = t.g.k(0);
            let tb = t.g.t_bool();
            let below = t.g.op(Op::SLessThan, tb, &[v, z]);
            let v = t.g.select(below, z, v);
            let above = t.g.op(Op::SGreaterThan, tb, &[v, max]);
            t.g.select(above, max, v)
        };
        let tb = self.g.t_bool();
        let _ = tb;
        let one_again = one;
        let _ = one_again;
        let v4 = self.vec4(Class::Float);
        let mut out = Vec::new();
        for (j, ch) in chans.into_iter().enumerate() {
            for d in 0..4u32 {
                let dx = self.g.k(d & 1);
                let dy = self.g.k(d >> 1);
                let x = self.g.op(Op::IAdd, tu, &[ix, dx]);
                let y = self.g.op(Op::IAdd, tu, &[iy, dy]);
                let x = clamp(self, x, max_w);
                let y = clamp(self, y, max_h);
                let tc = match layer {
                    Some((l, max_l)) => {
                        let l = clamp(self, l, max_l);
                        self.g.op(Op::CompositeConstruct, tu2, &[x, y, l])
                    }
                    None => self.g.op(Op::CompositeConstruct, tu2, &[x, y]),
                };
                // the fetch takes the level as an operand
                let texel = self.g.op(Op::ImageFetch, v4, &[image, tc, 2, zero]);
                let x = self.g.op(Op::CompositeExtract, tf, &[texel, ch]);
                let bits = self.g.as_bits(x);
                out.push(self.lane_result(c, j * 4 + d as usize, bits));
            }
        }
        Ok(Some(out))
    }

    fn image_sample(&mut self, c: &Ctx<Gen>) -> R<Option<Vec<Opd<Gen>>>> {
        let i = &c.inst.instr;
        let name = i.name();
        if name == "image_get_resinfo" {
            return self.image_resinfo(c);
        }
        if name.starts_with("image_gather4") {
            return self.image_gather(c);
        }
        let (explicit, zero_lod, dref) = match name {
            "image_sample" => (false, false, false),
            "image_sample_l" => (true, false, false),
            "image_sample_lz" | "image_get_lod" => (true, true, false),
            // a depth compare: the address carries the comparison value after the coordinates
            "image_sample_c_lz" | "image_sample_c_l" => (true, true, true),
            _ => return Ok(None),
        };
        let m = i.img;
        if m.a16 || m.d16 || m.tfe || m.lwe || m.unorm {
            return refuse(&format!("{name} with a16, d16, tfe, lwe or unorm"));
        }
        let Some((_, _, ncoord)) = Self::image_dim(m.dim) else {
            return refuse(&format!("{name} on dimension {}", m.dim));
        };
        let ns = c.srcs.len();
        if ns < 3 || c.srcs[ns - 2].len() < 8 || c.srcs[ns - 1].len() < 4 {
            return bad("an image sample whose operands do not match its form");
        }
        let nlod = usize::from(name == "image_sample_l");
        let addr = &c.srcs[ns - 3];
        if addr.len() != ncoord as usize + nlod + usize::from(dref) {
            return refuse(&format!("{name} with an address of {} dwords", addr.len()));
        }
        let is_vertex = matches!(self.io(), StageIo::Vertex(_));
        if is_vertex && !explicit {
            return refuse("image_sample in a vertex stage");
        }
        // A compute program has no quad derivatives: the sample takes LOD 0 instead (the
        // derivative-based mip choice is lost, which a compute sample of a known level does
        // not need).
        let compute_implicit = matches!(self.io(), StageIo::Compute(_)) && !explicit;
        let chans: Vec<u32> = (0..4).filter(|ch| m.dmask >> ch & 1 != 0).collect();
        if chans.len() != c.inst.dsts.len() {
            return bad("an image sample whose results do not match its dmask");
        }
        let si = self.sampled_image(c)?;
        // the coordinates
        let tf = self.g.t_f32();
        let mut cs = Vec::new();
        for &a in &addr[..ncoord as usize] {
            let bits = self.bits_of(a);
            cs.push(self.g.as_f32(bits));
        }
        let coord = if m.dim == 3 {
            self.cube_direction(cs[0], cs[1], cs[2])
        } else if cs.len() == 1 {
            cs[0]
        } else {
            let tv = self
                .g
                .m
                .global(Op::TypeVector, None, &[tf, cs.len() as u32]);
            self.g.op(Op::CompositeConstruct, tv, &cs)
        };
        if name == "image_get_lod" {
            // `OpImageQueryLod`: x is the mipmap array index, y the primary LOD. The console
            // instruction returns two dwords; the first is the LOD (y) and the second the
            // index (x) **[unverified]**.
            self.g.m.capability(Capability::ImageQuery);
            let f2 = self.g.t_v2();
            let tf = self.g.t_f32();
            let q = self.g.op(Op::ImageQueryLod, f2, &[si, coord]);
            let mut out = Vec::new();
            for (j, ch) in chans.into_iter().enumerate() {
                let comp = if ch == 0 { 1 } else { 0 };
                let x = self.g.op(Op::CompositeExtract, tf, &[q, comp]);
                let bits = self.g.as_bits(x);
                out.push(self.lane_result(c, j, bits));
            }
            return Ok(Some(out));
        }
        let v4 = self.vec4(Class::Float);
        let res = if dref {
            // the compare value follows the coordinates; the level is 0
            let bits = self.bits_of(addr[ncoord as usize]);
            let d = self.g.as_f32(bits);
            let lod = self.g.fconst(0.0);
            let tf = self.g.t_f32();
            let r = self
                .g
                .op(Op::ImageSampleDrefExplicitLod, tf, &[si, coord, d, 2, lod]);
            let mut out = Vec::new();
            let bits = self.g.as_bits(r);
            for j in 0..chans.len() {
                out.push(self.lane_result(c, j, bits));
            }
            return Ok(Some(out));
        } else if !explicit && !compute_implicit {
            self.g.op(Op::ImageSampleImplicitLod, v4, &[si, coord])
        } else if compute_implicit {
            let lod = self.g.fconst(0.0);
            self.g
                .op(Op::ImageSampleExplicitLod, v4, &[si, coord, 2, lod])
        } else {
            let lod = if zero_lod {
                self.g.fconst(0.0)
            } else {
                let bits = self.bits_of(addr[ncoord as usize]);
                self.g.as_f32(bits)
            };
            // ImageOperands Lod
            self.g
                .op(Op::ImageSampleExplicitLod, v4, &[si, coord, 2, lod])
        };
        let mut out = Vec::new();
        for (j, ch) in chans.into_iter().enumerate() {
            let x = self.g.op(Op::CompositeExtract, tf, &[res, ch]);
            let bits = self.g.as_bits(x);
            out.push(self.lane_result(c, j, bits));
        }
        Ok(Some(out))
    }

    // -----------------------------------------------------------------------------------
    // Image loads and stores (chunk 10c)

    /// The array of storage images of this dimension (set 0, binding 1): the sampled type
    /// `f32`, no format (`StorageImageReadWithoutFormat` / `WriteWithoutFormat`).
    fn storage_array(&mut self, dim: u8) -> (u32, u32) {
        if let Some(&t) = self.res().images.get(&dim) {
            return t;
        }
        let (d, arrayed, _) = Self::image_dim(dim).expect("a dimension that was checked");
        for cap in [
            Capability::RuntimeDescriptorArray,
            Capability::StorageImageArrayNonUniformIndexing,
            Capability::ShaderNonUniform,
            Capability::StorageImageReadWithoutFormat,
            Capability::StorageImageWriteWithoutFormat,
        ] {
            self.g.m.capability(cap);
        }
        if d == 0 {
            self.g.m.capability(Capability::Image1D);
        }
        let tf = self.g.t_f32();
        // sampled 2 (storage), format Unknown
        let img = self
            .g
            .m
            .global(Op::TypeImage, None, &[tf, d, 0, arrayed, 0, 2, 0]);
        let arr = self.g.m.global(Op::TypeRuntimeArray, None, &[img]);
        let var = self.global(StorageClass::UniformConstant, arr);
        self.g
            .m
            .decorate(var, Decoration::DescriptorSet, &[SET_BINDLESS]);
        self.g.m.decorate(var, Decoration::Binding, &[BIND_STORAGE]);
        self.g.m.name(var, &format!("images_dim{dim}"));
        self.res().images.insert(dim, (var, img));
        (var, img)
    }

    /// `image_load*` / `image_store*` on a table slot: an SRV is fetched (`OpImageFetch`) and a
    /// UAV read (`OpImageRead`) or written (`OpImageWrite`), by the registry index in the slot.
    fn image_ls(&mut self, c: &Ctx<Gen>, store: bool) -> R<Option<Vec<Opd<Gen>>>> {
        let i = &c.inst.instr;
        let name = i.name();
        let mip = name.ends_with("_mip");
        let base = if store { "image_store" } else { "image_load" };
        if name.strip_suffix("_mip").unwrap_or(name) != base {
            return Ok(None);
        }
        let m = i.img;
        if m.d16 || m.tfe || m.lwe {
            return refuse(&format!("{name} with d16, tfe or lwe"));
        }
        let dim = match m.dim {
            0 | 1 | 2 | 5 => m.dim,
            d => return refuse(&format!("{name} on dimension {d}")),
        };
        let (_, _, ncoord) = Self::image_dim(dim).expect("a dimension that was checked");
        if store && m.dmask != 0xf {
            return refuse(&format!("{name} with dmask {:#x}", m.dmask));
        }
        let ns = c.srcs.len();
        let need = if store { 3 } else { 2 };
        if ns != need || c.srcs[ns - 1].len() < 8 {
            return bad("an image load or store whose operands do not match its form");
        }
        let addr = &c.srcs[ns - 2];
        let ncomp = ncoord as usize + usize::from(mip);
        let words = if m.a16 { ncomp.div_ceil(2) } else { ncomp };
        if addr.len() != words {
            return refuse(&format!("{name} with an address of {} dwords", addr.len()));
        }
        // the slot's kind: the used-resource row at the offset the descriptor was loaded from
        let rsrc = &c.inst.srcs[ns - 1];
        let origin = match rsrc.first() {
            Some(Src::Val(v)) => self.res().analysis.origin.get(v).copied(),
            _ => None,
        };
        if let Some((sgpr, offset)) = origin {
            if std::env::var_os("NATIV64_ORIGINS").is_some() {
                eprintln!("origin: {name} user sgpr {sgpr} table offset {offset} bytes");
            }
        }
        // Only compute programs carry used-resource rows. A load in a vertex or pixel
        // program is taken as a read of an SRV (a typed texture fetch) [unverified: a UAV
        // load in a graphics stage would be read through the wrong binding]; a load whose
        // table offset is not a constant is taken as one too (a store must name a UAV).
        // a store can only reach a UAV (D3D12 has no SRV stores): when the used rows do not
        // name the slot, a load is read as an SRV and a store as a UAV
        let kind = origin
            .and_then(|(sgpr, offset)| self.io().descriptor_kind(sgpr, offset))
            .or(Some(if store { 3 } else { 0 }));
        let Some(kind) = kind else {
            return refuse("an image whose table slot no used-resource row names");
        };
        if (store && kind != 3) || !matches!(kind, 0 | 3) {
            return refuse(&format!("{name} on a resource of kind {kind}"));
        }
        if kind == 3 && mip {
            return refuse(&format!("{name} on a UAV"));
        }
        // the coordinates (and the mip level)
        let tu = self.g.t_u32();
        let mut comps = Vec::new();
        for k in 0..ncomp {
            let w = self.bits_of(addr[if m.a16 { k / 2 } else { k }]);
            comps.push(if m.a16 {
                let sh = self.g.k(16 * (k as u32 % 2));
                let x = self.g.op(Op::ShiftRightLogical, tu, &[w, sh]);
                let mask = self.g.k(0xffff);
                self.g.op(Op::BitwiseAnd, tu, &[x, mask])
            } else {
                w
            });
        }
        let coord = if ncoord == 1 {
            comps[0]
        } else {
            let tv = self.g.m.global(Op::TypeVector, None, &[tu, ncoord]);
            self.g
                .op(Op::CompositeConstruct, tv, &comps[..ncoord as usize])
        };
        let index = self.bits_of(c.srcs[ns - 1][0]);
        self.non_uniform(index);
        let tf = self.g.t_f32();
        let v4 = self.vec4(Class::Float);
        if store {
            let (var, img) = self.storage_array(dim);
            let image = self.array_element(var, img, index);
            if c.srcs[0].len() != 4 {
                return bad("an image store whose data is not four dwords");
            }
            let mut chans = Vec::new();
            for &d in &c.srcs[0] {
                let bits = self.bits_of(d);
                chans.push(self.g.as_f32(bits));
            }
            let texel = self.g.op(Op::CompositeConstruct, v4, &chans);
            self.guarded(c.active, |t| {
                t.g.op0(Op::ImageWrite, &[image, coord, texel]);
                Ok(())
            })?;
            return Ok(Some(Vec::new()));
        }
        let res = if kind == 0 {
            let (var, img) = self.texture_array(dim);
            let image = self.array_element(var, img, index);
            let lod = if mip {
                comps[ncoord as usize]
            } else {
                self.g.k(0)
            };
            // ImageOperands Lod
            self.g.op(Op::ImageFetch, v4, &[image, coord, 2, lod])
        } else {
            let (var, img) = self.storage_array(dim);
            let image = self.array_element(var, img, index);
            self.g.op(Op::ImageRead, v4, &[image, coord])
        };
        let chans: Vec<u32> = (0..4).filter(|ch| m.dmask >> ch & 1 != 0).collect();
        if chans.len() != c.inst.dsts.len() {
            return bad("an image load whose results do not match its dmask");
        }
        let mut out = Vec::new();
        for (j, ch) in chans.into_iter().enumerate() {
            let x = self.g.op(Op::CompositeExtract, tf, &[res, ch]);
            let bits = self.g.as_bits(x);
            out.push(self.lane_result(c, j, bits));
        }
        Ok(Some(out))
    }

    fn image_load(&mut self, c: &Ctx<Gen>) -> R<Option<Vec<Opd<Gen>>>> {
        self.image_ls(c, false)
    }

    fn image_store(&mut self, c: &Ctx<Gen>) -> R<Option<Vec<Opd<Gen>>>> {
        self.image_ls(c, true)
    }

    // -----------------------------------------------------------------------------------
    // Workgroup memory (chunk 10c)

    /// The workgroup memory: a `u32` array of the record's LDS size.
    fn lds(&mut self) -> R<(u32, u32)> {
        let StageIo::Compute(cs) = self.io() else {
            return refuse("workgroup memory outside a compute program");
        };
        // A program that uses LDS without asking for any (a lane exchange through
        // `ds_swizzle_b32`) gets a wave's worth of dwords.
        let n = cs.lds_bytes.div_ceil(4).max(MIN_LDS);
        if let Some(v) = self.res().lds {
            return Ok((v, n));
        }
        let tu = self.g.t_u32();
        let len = self.g.k(n);
        let arr = self.g.m.global(Op::TypeArray, None, &[tu, len]);
        let var = self.global(StorageClass::Workgroup, arr);
        self.g.m.name(var, "lds");
        self.res().lds = Some(var);
        Ok((var, n))
    }

    /// `ds_read*` / `ds_write*` (b32 to b128, the `2` and `2st64` pairs): byte address plus
    /// offset, divided by 4; a dword past the LDS reads 0 and its write is dropped.
    /// The import table (set 0, binding 3): a `u64` array of a count then
    /// `(start, end, device address)` per imported title range.
    fn imports_table(&mut self) -> (u32, u32) {
        if let Some(t) = self.res().imports {
            return t;
        }
        let t64 = self.t64();
        // a runtime array of u64, strided by 8
        let rarr = self.g.m.global(Op::TypeRuntimeArray, None, &[t64]);
        self.g.m.decorate(rarr, Decoration::ArrayStride, &[8]);
        let st = self.g.m.global(Op::TypeStruct, None, &[rarr]);
        self.g.m.decorate(st, Decoration::Block, &[]);
        self.g.m.member_decorate(st, 0, Decoration::Offset, &[0]);
        let var = self.global(StorageClass::StorageBuffer, st);
        self.g
            .m
            .decorate(var, Decoration::DescriptorSet, &[SET_BINDLESS]);
        self.g.m.decorate(var, Decoration::Binding, &[3]);
        self.g.m.name(var, "imports");
        self.res().imports = Some((var, st));
        (var, st)
    }

    /// A word of the import table.
    fn import_word(&mut self, at: u32) -> u32 {
        let t64 = self.t64();
        let p = self.g.t_ptr(StorageClass::StorageBuffer, t64);
        let zero = self.g.k(0);
        let var = self.imports_table().0;
        let ptr = self.g.op(Op::AccessChain, p, &[var, zero, at]);
        self.g.load(t64, ptr)
    }

    /// Translates a title address through the import table: `(device address, range end)` and
    /// whether it was found.
    fn va_to_device(&mut self, va: u32) -> (u32, u32, u32) {
        let (var, _st) = self.imports_table();
        let _ = var;
        let tu = self.g.t_u32();
        let t64 = self.t64();
        let tb = self.g.t_bool();
        let (zero, one, three) = (self.g.k(0), self.g.k(1), self.g.k(3));
        // the count is word 0
        let count = self.import_word(zero);
        let count = self.g.op(Op::UConvert, tu, &[count]);
        // unrolled binary search: the first entry whose start is above `va`
        let mut lo = zero;
        let mut hi = self.g.op(Op::IAdd, tu, &[count, zero]);
        for _ in 0..14 {
            let sum = self.g.op(Op::IAdd, tu, &[lo, hi]);
            let mid = self.g.op(Op::ShiftRightLogical, tu, &[sum, one]);
            let word = self.g.op(Op::IMul, tu, &[mid, three]);
            let word = self.g.op(Op::IAdd, tu, &[word, one]);
            let start = self.import_word(word);
            let le = self.g.op(Op::ULessThanEqual, tb, &[start, va]);
            let next = self.g.op(Op::IAdd, tu, &[mid, one]);
            lo = self.g.select(le, next, lo);
            hi = self.g.select(le, hi, mid);
        }
        // the candidate is the entry before `lo`
        let has = self.g.op(Op::UGreaterThan, tb, &[lo, zero]);
        let idx = self.g.op(Op::ISub, tu, &[lo, one]);
        let safe = self.g.select(has, idx, zero);
        let word = self.g.op(Op::IMul, tu, &[safe, three]);
        let word = self.g.op(Op::IAdd, tu, &[word, one]);
        let start = self.import_word(word);
        let next = self.g.op(Op::IAdd, tu, &[word, one]);
        let end = self.import_word(next);
        let next = self.g.op(Op::IAdd, tu, &[word, three]);
        let dev = self.import_word(next);
        let lt = self.g.op(Op::ULessThan, tb, &[va, end]);
        let ok = self.g.op(Op::LogicalAnd, tb, &[has, lt]);
        let delta = self.g.op(Op::ISub, t64, &[va, start]);
        let addr = self.g.op(Op::IAdd, t64, &[dev, delta]);
        (addr, end, ok)
    }

    /// The low `bits` of a dword, zero- or sign-extended to the dword.
    fn narrow(&mut self, v: u32, bits: u32, signed: bool) -> u32 {
        let tu = self.g.t_u32();
        let t = if bits == 8 {
            self.g.m.global(Op::TypeInt, None, &[8, 0])
        } else {
            self.g.m.global(Op::TypeInt, None, &[16, 0])
        };
        let narrow = self.g.op(Op::UConvert, t, &[v]);
        self.g.op(
            if signed { Op::SConvert } else { Op::UConvert },
            tu,
            &[narrow],
        )
    }

    /// `global_load_dword*` / `global_store_dword*` / the `flat_` forms: the address is a
    /// title address, translated through the import table to a device address.
    fn flat(&mut self, c: &Ctx<Gen>) -> R<Option<Vec<Opd<Gen>>>> {
        let i = &c.inst.instr;
        let m = i.mem;
        let name = i.name();
        let base = name
            .strip_prefix("global_")
            .or_else(|| name.strip_prefix("flat_"));
        let Some(rest) = base else {
            return Ok(None);
        };
        if i.mem.seg == 1 {
            return refuse(&format!("{name} on scratch"));
        }
        let (load, n) = match rest {
            "load_dword" => (true, 1u32),
            "load_dwordx2" => (true, 2),
            "load_dwordx3" => (true, 3),
            "load_dwordx4" => (true, 4),
            "store_dword" => (false, 1),
            "store_dwordx2" => (false, 2),
            "store_dwordx3" => (false, 3),
            "store_dwordx4" => (false, 4),
            // a byte or a word of the addressed memory, extended to the dword
            "load_ubyte" | "load_sbyte" | "load_ushort" | "load_sshort" => (true, 1),
            _ => return refuse(name),
        };
        let narrow = match rest {
            "load_ubyte" => Some((8, false)),
            "load_sbyte" => Some((8, true)),
            "load_ushort" => Some((16, false)),
            "load_sshort" => Some((16, true)),
            _ => None,
        };
        if c.srcs.is_empty() || c.srcs[0].len() < 2 {
            return bad("a flat access without its address");
        }
        let t64 = self.t64();
        let (lo, hi) = (self.bits_of(c.srcs[0][0]), self.bits_of(c.srcs[0][1]));
        let (lo, hi) = (self.zext(lo), self.zext(hi));
        let thirty_two = self.g.k(32);
        let hi = self.g.op(Op::ShiftLeftLogical, t64, &[hi, thirty_two]);
        let addr = self.g.op(Op::BitwiseOr, t64, &[lo, hi]);
        let off = self.k64(u64::from(m.offset as u32));
        let addr = self.add64(addr, off);
        let (dev, end, ok) = self.va_to_device(addr);
        let tb = self.g.t_bool();
        let mut out = Vec::new();
        let data = if load { 0 } else { 1 };
        for k in 0..n {
            let a = self.add_k64(dev, 4 * u64::from(k));
            let kk = self.k64(4 * u64::from(k));
            let at = self.add64(addr, kk);
            let inside = self.g.op(Op::ULessThanEqual, tb, &[at, end]);
            let ok = self.g.op(Op::LogicalAnd, tb, &[ok, inside]);
            let ok = self.and(ok, c.active);
            if load {
                let v = self.guarded_load(a, ok)?;
                let v = match narrow {
                    Some((bits, signed)) => self.narrow(v, bits, signed),
                    None => v,
                };
                out.push(self.lane_result(c, k as usize, v));
            } else {
                let Some(&d) = c.srcs.get(data).and_then(|s| s.get(k as usize)) else {
                    return bad("a flat store with too few data dwords");
                };
                let d = self.bits_of(d);
                self.guarded(Some(ok), |t| {
                    t.bda_store_u32(a, d);
                    Ok(())
                })?;
            }
        }
        if load {
            Ok(Some(out))
        } else {
            Ok(Some(Vec::new()))
        }
    }

    /// `ds_swizzle_b32`: the LDS read address comes from the lane the swizzle immediate picks
    /// (`((lane & and) | or) ^ xor`, or a quad permutation), a subgroup shuffle of the address.
    fn ds_swizzle(&mut self, c: &Ctx<Gen>) -> R<Option<Vec<Opd<Gen>>>> {
        let i = &c.inst.instr;
        if i.mem.gds {
            return refuse("ds_swizzle_b32 on GDS");
        }
        let (var, n) = self.lds()?;
        let tu = self.g.t_u32();
        let tb = self.g.t_bool();
        let Some(&addr) = c.srcs.first().and_then(|s| s.first()) else {
            return bad("ds_swizzle_b32 without its address");
        };
        let addr = self.bits_of(addr);
        let imm = (i.mem.offset as u32) & 0xffff;
        let lane = <Gen as super::super::alu::Backend>::lane_id(&mut self.g);
        let (and_mask, or_mask, xor_mask) = (imm & 0x1f, imm >> 5 & 0x1f, imm >> 10 & 0x1f);
        let sw = if imm & 0x8000 != 0 {
            // a per-quad permutation of the low two lane bits
            let (zero, one, three) = (self.g.k(0), self.g.k(1), self.g.k(3));
            let low = self.g.op(Op::BitwiseAnd, tu, &[lane, three]);
            let sh = self.g.op(Op::ShiftLeftLogical, tu, &[low, one]);
            let all = self.g.k(imm);
            let shifted = self.g.op(Op::ShiftRightLogical, tu, &[all, sh]);
            let picked = self.g.op(Op::BitwiseAnd, tu, &[shifted, three]);
            let not_three = self.g.k(!3u32);
            let base = self.g.op(Op::BitwiseAnd, tu, &[lane, not_three]);
            let _ = zero;
            self.g.op(Op::BitwiseOr, tu, &[base, picked])
        } else {
            let (and_k, or_k, xor_k) = (self.g.k(and_mask), self.g.k(or_mask), self.g.k(xor_mask));
            let a = self.g.op(Op::BitwiseAnd, tu, &[lane, and_k]);
            let a = self.g.op(Op::BitwiseOr, tu, &[a, or_k]);
            self.g.op(Op::BitwiseXor, tu, &[a, xor_k])
        };
        self.g.m.capability(Capability::GroupNonUniformShuffle);
        let scope = self.g.k(3);
        let addr = self
            .g
            .op(Op::GroupNonUniformShuffle, tu, &[scope, addr, sw]);
        let two = self.g.k(2);
        let word = self.g.op(Op::ShiftRightLogical, tu, &[addr, two]);
        let nn = self.g.k(n);
        let inside = self.g.op(Op::ULessThan, tb, &[word, nn]);
        let zero = self.g.k(0);
        let safe = self.g.select(inside, word, zero);
        let p = self.g.t_ptr(StorageClass::Workgroup, tu);
        let ptr = self.g.op(Op::AccessChain, p, &[var, safe]);
        let v = self.g.load(tu, ptr);
        let v = self.g.select(inside, v, zero);
        Ok(Some(vec![self.lane_result(c, 0, v)]))
    }

    /// `ds_add_u32`, `ds_max_u32`, ...: an atomic on a workgroup-memory dword.
    fn ds_atomic(&mut self, c: &Ctx<Gen>, rest: &str) -> R<Option<Vec<Opd<Gen>>>> {
        let i = &c.inst.instr;
        let name = i.name();
        // the `_rtn` forms return the old value (a destination), the plain ones write nothing
        let rest = rest.replace("_rtn", "");
        let (spirv_op, operand) = match rest.as_str() {
            "add_u32" => (Op::AtomicIAdd, true),
            "wrxchg_b32" => (Op::AtomicExchange, true),
            "wrxchg2_b32" => (Op::AtomicExchange, true),
            "sub_u32" => (Op::AtomicISub, true),
            "min_i32" => (Op::AtomicSMin, true),
            "min_u32" => (Op::AtomicUMin, true),
            "max_i32" => (Op::AtomicSMax, true),
            "max_u32" => (Op::AtomicUMax, true),
            "and_b32" => (Op::AtomicAnd, true),
            "or_b32" => (Op::AtomicOr, true),
            "xor_b32" => (Op::AtomicXor, true),
            "inc_u32" => (Op::AtomicIIncrement, false),
            "dec_u32" => (Op::AtomicIDecrement, false),
            _ => return refuse(name),
        };
        if i.mem.gds {
            return refuse(&format!("{name} on GDS"));
        }
        let (var, n) = self.lds()?;
        let tu = self.g.t_u32();
        let tb = self.g.t_bool();
        let Some(&addr) = c.srcs.first().and_then(|s| s.first()) else {
            return bad("a DS atomic without its address");
        };
        let addr = self.bits_of(addr);
        let off = self.g.k(i.mem.offset as u32 & 0xffff);
        let addr = self.g.op(Op::IAdd, tu, &[addr, off]);
        let two = self.g.k(2);
        let word = self.g.op(Op::ShiftRightLogical, tu, &[addr, two]);
        let nn = self.g.k(n);
        let inside = self.g.op(Op::ULessThan, tb, &[word, nn]);
        let zero = self.g.k(0);
        let safe = self.g.select(inside, word, zero);
        let p = self.g.t_ptr(StorageClass::Workgroup, tu);
        let ptr = self.g.op(Op::AccessChain, p, &[var, safe]);
        let scope = self.g.k(2);
        let relaxed = self.g.k(0);
        let old = if operand {
            let Some(&v) = c.srcs.get(1).and_then(|s| s.first()) else {
                return bad("a DS atomic without its value");
            };
            let v = self.bits_of(v);
            self.g.op(spirv_op, tu, &[ptr, scope, relaxed, v])
        } else {
            self.g.op(spirv_op, tu, &[ptr, scope, relaxed])
        };
        if c.inst.dsts.is_empty() {
            // like the buffer atomics, the old value is a result only with glc
            return Ok(Some(Vec::new()));
        }
        let old = self.g.select(inside, old, zero);
        Ok(Some(vec![self.lane_result(c, 0, old)]))
    }

    fn ds(&mut self, c: &Ctx<Gen>) -> R<Option<Vec<Opd<Gen>>>> {
        let i = &c.inst.instr;
        let name = i.name();
        let Some(rest) = name.strip_prefix("ds_") else {
            return Ok(None);
        };
        if rest == "swizzle_b32" {
            return self.ds_swizzle(c);
        }
        if [
            "add_", "sub_", "min_", "max_", "and_", "or_", "xor_", "inc_", "dec_", "cmpst_",
            "wrxchg",
        ]
        .iter()
        .any(|p| rest.starts_with(p))
        {
            return self.ds_atomic(c, rest);
        }
        let (read, rest) = match (rest.strip_prefix("read"), rest.strip_prefix("write")) {
            (Some(r), _) => (true, r),
            (_, Some(r)) => (false, r),
            _ => return Ok(None),
        };
        // (elements, scale of the paired offsets in bytes, dwords per element)
        let (elems, scale, w) = match rest {
            "_b32" => (1, 0, 1),
            "_b64" => (1, 0, 2),
            "_b96" => (1, 0, 3),
            "_b128" => (1, 0, 4),
            "2_b32" => (2, 4, 1),
            "2_b64" => (2, 8, 2),
            "2st64_b32" => (2, 256, 1),
            "2st64_b64" => (2, 512, 2),
            _ => return Ok(None),
        };
        if i.mem.gds {
            return refuse(&format!("{name} on GDS"));
        }
        let (var, n) = self.lds()?;
        let tu = self.g.t_u32();
        let tb = self.g.t_bool();
        let Some(&addr) = c.srcs.first().and_then(|s| s.first()) else {
            return bad("a DS access without its address");
        };
        let addr = self.bits_of(addr);
        let offs: Vec<u32> = if elems == 1 {
            vec![i.mem.offset as u32 & 0xffff]
        } else {
            vec![
                (i.mem.offset as u32 & 0xff) * scale,
                u32::from(i.mem.offset1) * scale,
            ]
        };
        let p = self.g.t_ptr(StorageClass::Workgroup, tu);
        let two = self.g.k(2);
        let nn = self.g.k(n);
        let mut out = Vec::new();
        for (e, off) in offs.into_iter().enumerate() {
            let k = self.g.k(off);
            let byte = self.g.op(Op::IAdd, tu, &[addr, k]);
            let word = self.g.op(Op::ShiftRightLogical, tu, &[byte, two]);
            for j in 0..w {
                let kj = self.g.k(j as u32);
                let idx = self.g.op(Op::IAdd, tu, &[word, kj]);
                let inside = self.g.op(Op::ULessThan, tb, &[idx, nn]);
                if read {
                    let zero = self.g.k(0);
                    let safe = self.g.select(inside, idx, zero);
                    let ptr = self.g.op(Op::AccessChain, p, &[var, safe]);
                    let v = self.g.load(tu, ptr);
                    let v = self.g.select(inside, v, zero);
                    out.push(self.lane_result(c, e * w + j, v));
                } else {
                    let Some(&d) = c.srcs.get(1 + e).and_then(|s| s.get(j)) else {
                        return bad("a DS write with too few data dwords");
                    };
                    let d = self.bits_of(d);
                    let ok = self.and(inside, c.active);
                    self.guarded(Some(ok), |t| {
                        let ptr = t.g.op(Op::AccessChain, p, &[var, idx]);
                        t.g.store(ptr, d);
                        Ok(())
                    })?;
                }
            }
        }
        if read && out.len() != c.inst.dsts.len() {
            return bad("a DS read whose results do not match its width");
        }
        Ok(Some(out))
    }

    /// `s_barrier`: every invocation of the group meets here, so the group's memory is
    /// consistent. Refused outside a compute program and under a per-lane branch.
    pub(super) fn barrier(&mut self) -> R<()> {
        if !matches!(self.io(), StageIo::Compute(_)) {
            return refuse("s_barrier outside a compute program");
        }
        if self.lane_ifs > 0 {
            return refuse("s_barrier under a per-lane branch");
        }
        // Execution Workgroup, Memory Workgroup, AcquireRelease | WorkgroupMemory
        let (wg, sem) = (self.g.k(2), self.g.k(0x108));
        self.g.op0(Op::ControlBarrier, &[wg, wg, sem]);
        Ok(())
    }
}
