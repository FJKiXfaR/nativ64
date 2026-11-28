//! The probe translation of a structured program (chunk 6): the SPIR-V `Backend` over the
//! module builder, and the walk of the `Node` tree that writes one `GLCompute` entry point.
//!
//! The probe runs one invocation = one lane (`LocalSize 1 1 1`, console lane 0). Every register
//! dword is a `Private` variable: a `u32`, or a `bool` for the dwords the mask typing found to
//! be lane masks (a wave64 pair holds the lane's bit in both dwords). Registers that are live
//! on entry are read from a storage buffer (`inputs`, one dword each, EXEC set instead), and
//! every variable is written to a second buffer (`outputs`) at each `s_endpgm`.

use std::collections::{BTreeMap, HashMap};

mod resource;
mod stage;

use spirv::{
    AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode, ExecutionModel,
    GlslStd450Op as GL, MemoryModel, Op, StorageClass,
};

use super::alu::{
    self, Backend, Ctx, Cvt, F64Bin, FBin, FCmp, FUn, IBin, ICmp, IUn, Opd, Unsupported,
};
use super::builder::{inst, Module};
use crate::shader::io::StageIo;
use crate::shader::ir::{Func, Reg, Src, Term, Val};
use crate::shader::resource::{SamplerDesc, Translated};
use crate::shader::structure::{BrKind, BranchKind, Guard, Node, Structured, Target};

/// Why a program does not translate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpirvError {
    Unsupported(Unsupported),
    /// The tree or the IR broke an invariant the emitter relies on.
    Malformed(String),
}

impl std::fmt::Display for SpirvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(u) => write!(f, "unsupported: {u}"),
            Self::Malformed(s) => write!(f, "malformed: {s}"),
        }
    }
}

impl std::error::Error for SpirvError {}

impl From<Unsupported> for SpirvError {
    fn from(u: Unsupported) -> Self {
        Self::Unsupported(u)
    }
}

type R<T> = Result<T, SpirvError>;

fn bad<T>(s: &str) -> R<T> {
    Err(SpirvError::Malformed(s.to_string()))
}

/// A translated probe module and the layout of its two buffers.
#[derive(Clone, Debug)]
pub struct Probe {
    pub words: Vec<u32>,
    /// Binding 0: one dword per entry register, in this order (EXEC's slot is unused).
    pub inputs: Vec<Reg>,
    /// Binding 1: one dword per variable, in this order; a mask is 0 or 1.
    pub outputs: Vec<(Reg, bool)>,
}

/// The code the `target` variable takes for a multi-level `Br` (0 means none).
fn target_code(t: Target) -> u32 {
    t.block as u32 * 2 + u32::from(t.kind == BrKind::Continue) + 1
}

// ---------------------------------------------------------------------------------------
// The backend

pub struct Gen {
    pub m: Module,
    body: Vec<u32>,
    locals: Vec<u32>,
    terminated: bool,
    glsl: u32,
    /// The type of every value id this builder made (for the operators that need it).
    types: std::collections::HashMap<u32, u32>,
    /// Builtin variables made here, by builtin: the entry point must list them.
    builtins: std::collections::HashMap<u32, u32>,
    /// Builtin variables to add to the entry point's interface.
    pub extra_iface: Vec<u32>,
}

impl Gen {
    fn new() -> Self {
        let mut m = Module::new();
        m.capability(Capability::Shader);
        m.capability(Capability::PhysicalStorageBufferAddresses);
        m.set_model(
            AddressingModel::PhysicalStorageBuffer64,
            MemoryModel::GLSL450,
        );
        let glsl = m.ext_import("GLSL.std.450");
        Self {
            m,
            body: Vec::new(),
            locals: Vec::new(),
            terminated: false,
            glsl,
            types: std::collections::HashMap::new(),
            builtins: std::collections::HashMap::new(),
            extra_iface: Vec::new(),
        }
    }

    fn t_void(&mut self) -> u32 {
        self.m.global(Op::TypeVoid, None, &[])
    }
    fn t_bool(&mut self) -> u32 {
        self.m.global(Op::TypeBool, None, &[])
    }
    fn t_u32(&mut self) -> u32 {
        self.m.global(Op::TypeInt, None, &[32, 0])
    }
    fn t_f32(&mut self) -> u32 {
        self.m.global(Op::TypeFloat, None, &[32])
    }
    fn t_v2(&mut self) -> u32 {
        let f = self.t_f32();
        self.m.global(Op::TypeVector, None, &[f, 2])
    }
    fn t_pair(&mut self) -> u32 {
        let u = self.t_u32();
        self.m.global(Op::TypeStruct, None, &[u, u])
    }
    fn t_ptr(&mut self, class: StorageClass, t: u32) -> u32 {
        self.m.global(Op::TypePointer, None, &[class as u32, t])
    }

    /// `OpGroupNonUniformAny` over the subgroup: true when any invocation has `a` set.
    pub(super) fn any(&mut self, a: u32) -> u32 {
        self.m.capability(Capability::GroupNonUniformVote);
        let t = self.t_bool();
        let scope = self.k(3); // Subgroup
        self.op(Op::GroupNonUniformAny, t, &[scope, a])
    }

    /// Starts a new (unreachable) block when the current one already ended.
    fn open(&mut self) {
        if self.terminated {
            let l = self.m.id();
            self.label(l);
        }
    }

    fn label(&mut self, id: u32) {
        self.body.extend(inst(Op::Label, &[id]));
        self.terminated = false;
    }

    fn op(&mut self, op: Op, ty: u32, operands: &[u32]) -> u32 {
        self.open();
        let id = self.m.id();
        let mut ops = vec![ty, id];
        ops.extend_from_slice(operands);
        self.body.extend(inst(op, &ops));
        // a result type is not a value; the operators that name it are the arithmetic ones
        if !matches!(
            op,
            Op::TypeInt
                | Op::TypeFloat
                | Op::TypeBool
                | Op::TypeVector
                | Op::TypeArray
                | Op::TypeRuntimeArray
                | Op::TypeStruct
                | Op::TypePointer
        ) {
            self.types.insert(id, ty);
        }
        id
    }

    fn op0(&mut self, op: Op, operands: &[u32]) {
        self.open();
        self.body.extend(inst(op, operands));
    }

    fn term(&mut self, op: Op, operands: &[u32]) {
        self.open();
        self.body.extend(inst(op, operands));
        self.terminated = true;
    }

    fn ext(&mut self, ty: u32, what: GL, operands: &[u32]) -> u32 {
        let mut ops = vec![self.glsl, what as u32];
        ops.extend_from_slice(operands);
        self.op(Op::ExtInst, ty, &ops)
    }

    fn as_f32(&mut self, a: u32) -> u32 {
        let t = self.t_f32();
        self.op(Op::Bitcast, t, &[a])
    }

    fn as_bits(&mut self, a: u32) -> u32 {
        let t = self.t_u32();
        self.op(Op::Bitcast, t, &[a])
    }

    fn fconst(&mut self, x: f32) -> u32 {
        let t = self.t_f32();
        let id = self.m.global(Op::Constant, Some(t), &[x.to_bits()]);
        self.types.insert(id, t);
        id
    }

    fn load(&mut self, ty: u32, ptr: u32) -> u32 {
        self.op(Op::Load, ty, &[ptr])
    }

    fn store(&mut self, ptr: u32, v: u32) {
        self.op0(Op::Store, &[ptr, v]);
    }

    fn branch(&mut self, to: u32) {
        self.term(Op::Branch, &[to]);
    }
}

impl Gen {
    /// `a` from the lane `index` (`OpGroupNonUniformShuffle`).
    pub(super) fn shuffle(&mut self, a: u32, index: u32) -> u32 {
        self.m.capability(Capability::GroupNonUniformShuffle);
        let scope = self.k(3);
        let t = self.t_u32();
        self.op(Op::GroupNonUniformShuffle, t, &[scope, a, index])
    }

    /// A builtin input variable (`gl_SubgroupInvocationID`, ...), made once.
    fn builtin(&mut self, b: BuiltIn, ty: u32) -> u32 {
        if let Some(&v) = self.builtins.get(&(b as u32)) {
            return v;
        }
        let p = self.t_ptr(StorageClass::Input, ty);
        let v = self.m.global_var(p, StorageClass::Input, None);
        self.m.decorate(v, Decoration::BuiltIn, &[b as u32]);
        self.m.name(v, "builtin");
        self.builtins.insert(b as u32, v);
        self.extra_iface.push(v);
        v
    }
}

impl Gen {
    /// The `f64` type and a dword pair as its bits.
    fn f64_type(&mut self) -> u32 {
        self.m.capability(Capability::Float64);
        self.m.global(Op::TypeFloat, None, &[64])
    }

    /// A dword pair (low, high) as the u64's bits.
    fn u64_of(&mut self, (lo, hi): (u32, u32)) -> u32 {
        let t64 = self.m.global(Op::TypeInt, None, &[64, 0]);
        let (lo, hi) = (self.zext32(lo), self.zext32(hi));
        let sh = self.k(32);
        let hi = self.op(Op::ShiftLeftLogical, t64, &[hi, sh]);
        self.op(Op::BitwiseOr, t64, &[lo, hi])
    }

    fn zext32(&mut self, v: u32) -> u32 {
        let t64 = self.m.global(Op::TypeInt, None, &[64, 0]);
        self.op(Op::UConvert, t64, &[v])
    }

    /// The low and high dwords of a u64.
    fn words64(&mut self, v: u32) -> (u32, u32) {
        let (t64, tu) = (self.m.global(Op::TypeInt, None, &[64, 0]), self.t_u32());
        let sh = self.k(32);
        let hi = self.op(Op::ShiftRightLogical, t64, &[v, sh]);
        (
            self.op(Op::UConvert, tu, &[v]),
            self.op(Op::UConvert, tu, &[hi]),
        )
    }
}

impl Backend for Gen {
    fn active_count(&mut self) -> u32 {
        // `s_bcnt1_i32_b64` of the exec mask: the number of active lanes
        self.m.capability(Capability::GroupNonUniformBallot);
        let t = self.t_u32();
        let scope = self.k(3);
        let tv4 = self.m.global(Op::TypeVector, None, &[t, 4]);
        let one = self.kb(true);
        let ballot = self.op(Op::GroupNonUniformBallot, tv4, &[scope, one]);
        // the group operation (Reduce) is an immediate operand after the scope
        self.op(Op::GroupNonUniformBallotBitCount, t, &[scope, 0, ballot])
    }

    fn f64bin(
        &mut self,
        op: F64Bin,
        a: (u32, u32),
        b: (u32, u32),
        c: Option<(u32, u32)>,
    ) -> Option<(u32, u32)> {
        let f64t = self.f64_type();
        let t64 = self.m.global(Op::TypeInt, None, &[64, 0]);
        let bit = |s: &mut Self, u: u32| s.op(Op::Bitcast, f64t, &[u]);
        let ua = self.u64_of(a);
        let fa = bit(self, ua);
        let ub = self.u64_of(b);
        let fb = bit(self, ub);
        let r = match op {
            F64Bin::Add => self.op(Op::FAdd, f64t, &[fa, fb]),
            F64Bin::Sub => self.op(Op::FSub, f64t, &[fa, fb]),
            F64Bin::Mul => self.op(Op::FMul, f64t, &[fa, fb]),
            F64Bin::Fma => {
                let uc = self.u64_of(c?);
                let fc = bit(self, uc);
                let t = self.op(Op::FMul, f64t, &[fa, fb]);
                self.op(Op::FAdd, f64t, &[t, fc])
            }
        };
        let w = self.op(Op::Bitcast, t64, &[r]);
        Some(self.words64(w))
    }

    fn f64_to_f32(&mut self, a: (u32, u32)) -> Option<u32> {
        let f64t = self.f64_type();
        let tf = self.t_f32();
        let u = self.u64_of(a);
        let f = self.op(Op::Bitcast, f64t, &[u]);
        let f32v = self.op(Op::FConvert, tf, &[f]);
        // the model carries float bits as dwords
        Some(self.as_bits(f32v))
    }

    fn f32_to_f64(&mut self, a: u32) -> Option<(u32, u32)> {
        let f64t = self.f64_type();
        let t64 = self.m.global(Op::TypeInt, None, &[64, 0]);
        let af = self.as_f32(a);
        let f = self.op(Op::FConvert, f64t, &[af]);
        let u = self.op(Op::Bitcast, t64, &[f]);
        Some(self.words64(u))
    }

    fn ballot_words(&mut self, bit: u32) -> (u32, u32) {
        self.m.capability(Capability::GroupNonUniformBallot);
        let t = self.t_u32();
        let scope = self.k(3);
        let tv4 = self.m.global(Op::TypeVector, None, &[t, 4]);
        let ballot = self.op(Op::GroupNonUniformBallot, tv4, &[scope, bit]);
        let lo = self.op(Op::CompositeExtract, t, &[ballot, 0]);
        let hi = self.op(Op::CompositeExtract, t, &[ballot, 1]);
        (lo, hi)
    }

    fn active_first(&mut self) -> u32 {
        // `s_ff1_i32_b64` of the exec mask: the lowest active lane
        self.m.capability(Capability::GroupNonUniformBallot);
        let t = self.t_u32();
        let scope = self.k(3);
        let tv4 = self.m.global(Op::TypeVector, None, &[t, 4]);
        let one = self.kb(true);
        let ballot = self.op(Op::GroupNonUniformBallot, tv4, &[scope, one]);
        self.op(Op::GroupNonUniformBallotFindLSB, t, &[scope, ballot])
    }

    fn lane_id(&mut self) -> u32 {
        self.m.capability(Capability::GroupNonUniform);
        let t = self.t_u32();
        let v = self.builtin(BuiltIn::SubgroupLocalInvocationId, t);
        self.load(t, v)
    }

    fn broadcast_first(&mut self, a: u32) -> u32 {
        // `v_readfirstlane_b32`: the value of the lowest active lane of the wave
        self.m.capability(Capability::GroupNonUniformBallot);
        let scope = self.k(3);
        let t = self.t_u32();
        self.op(Op::GroupNonUniformBroadcastFirst, t, &[scope, a])
    }

    type V = u32;
    type B = u32;

    fn k(&mut self, x: u32) -> u32 {
        let t = self.t_u32();
        let id = self.m.global(Op::Constant, Some(t), &[x]);
        self.types.insert(id, t);
        id
    }

    fn kb(&mut self, x: bool) -> u32 {
        let t = self.t_bool();
        let id = self.m.global(
            if x {
                Op::ConstantTrue
            } else {
                Op::ConstantFalse
            },
            Some(t),
            &[],
        );
        self.types.insert(id, t);
        id
    }

    fn ibin(&mut self, op: IBin, a: u32, b: u32) -> u32 {
        let t = self.t_u32();
        let simple = |o| Some(o);
        let o = match op {
            IBin::Add => simple(Op::IAdd),
            IBin::Sub => simple(Op::ISub),
            IBin::Mul => simple(Op::IMul),
            IBin::And => simple(Op::BitwiseAnd),
            IBin::Or => simple(Op::BitwiseOr),
            IBin::Xor => simple(Op::BitwiseXor),
            IBin::Shl => simple(Op::ShiftLeftLogical),
            IBin::Shr => simple(Op::ShiftRightLogical),
            IBin::Sar => simple(Op::ShiftRightArithmetic),
            _ => None,
        };
        if let Some(o) = o {
            return self.op(o, t, &[a, b]);
        }
        match op {
            IBin::UMin => self.ext(t, GL::UMin, &[a, b]),
            IBin::UMax => self.ext(t, GL::UMax, &[a, b]),
            IBin::SMin => self.ext(t, GL::SMin, &[a, b]),
            IBin::SMax => self.ext(t, GL::SMax, &[a, b]),
            _ => {
                let pair = self.t_pair();
                let o = if op == IBin::UMulHi {
                    Op::UMulExtended
                } else {
                    Op::SMulExtended
                };
                let r = self.op(o, pair, &[a, b]);
                self.op(Op::CompositeExtract, t, &[r, 1])
            }
        }
    }

    fn iun(&mut self, op: IUn, a: u32) -> u32 {
        let t = self.t_u32();
        match op {
            IUn::Not => self.op(Op::Not, t, &[a]),
            IUn::BitCount => self.op(Op::BitCount, t, &[a]),
            IUn::BitRev => self.op(Op::BitReverse, t, &[a]),
            IUn::FindLsb => self.ext(t, GL::FindILsb, &[a]),
            IUn::FindUMsb => self.ext(t, GL::FindUMsb, &[a]),
            IUn::FindSMsb => self.ext(t, GL::FindSMsb, &[a]),
        }
    }

    fn icmp(&mut self, op: ICmp, a: u32, b: u32) -> u32 {
        let t = self.t_bool();
        let o = match op {
            ICmp::Eq => Op::IEqual,
            ICmp::Ne => Op::INotEqual,
            ICmp::ULt => Op::ULessThan,
            ICmp::ULe => Op::ULessThanEqual,
            ICmp::UGt => Op::UGreaterThan,
            ICmp::UGe => Op::UGreaterThanEqual,
            ICmp::SLt => Op::SLessThan,
            ICmp::SLe => Op::SLessThanEqual,
            ICmp::SGt => Op::SGreaterThan,
            ICmp::SGe => Op::SGreaterThanEqual,
        };
        self.op(o, t, &[a, b])
    }

    fn fbin(&mut self, op: FBin, a: u32, b: u32) -> u32 {
        let (fa, fb) = (self.as_f32(a), self.as_f32(b));
        let t = self.t_f32();
        let o = match op {
            FBin::Add => Op::FAdd,
            FBin::Sub => Op::FSub,
            FBin::Mul => Op::FMul,
            // a min or a max is a GLSL ext instruction
            FBin::Min | FBin::Max => {
                let e = if op == FBin::Min { GL::FMin } else { GL::FMax };
                return self.ext(t, e, &[fa, fb]);
            }
        };
        let r = self.op(o, t, &[fa, fb]);
        self.as_bits(r)
    }

    fn fma(&mut self, a: u32, b: u32, c: u32) -> u32 {
        let (fa, fb, fc) = (self.as_f32(a), self.as_f32(b), self.as_f32(c));
        let t = self.t_f32();
        let r = self.ext(t, GL::Fma, &[fa, fb, fc]);
        self.as_bits(r)
    }

    fn fun(&mut self, op: FUn, a: u32) -> u32 {
        let fa = self.as_f32(a);
        let t = self.t_f32();
        let r = match op {
            FUn::Sqrt => self.ext(t, GL::Sqrt, &[fa]),
            FUn::Rsq => self.ext(t, GL::InverseSqrt, &[fa]),
            FUn::Rcp => {
                let one = self.fconst(1.0);
                self.op(Op::FDiv, t, &[one, fa])
            }
            FUn::Exp2 => self.ext(t, GL::Exp2, &[fa]),
            FUn::Log2 => self.ext(t, GL::Log2, &[fa]),
            FUn::Sin => self.ext(t, GL::Sin, &[fa]),
            FUn::Cos => self.ext(t, GL::Cos, &[fa]),
            FUn::Floor => self.ext(t, GL::Floor, &[fa]),
            FUn::Ceil => self.ext(t, GL::Ceil, &[fa]),
            FUn::Trunc => self.ext(t, GL::Trunc, &[fa]),
            FUn::RoundEven => self.ext(t, GL::RoundEven, &[fa]),
        };
        self.as_bits(r)
    }

    fn fcmp(&mut self, op: FCmp, a: u32, b: u32) -> u32 {
        let (fa, fb) = (self.as_f32(a), self.as_f32(b));
        let t = self.t_bool();
        let o = match op {
            FCmp::OLt => Op::FOrdLessThan,
            FCmp::OEq => Op::FOrdEqual,
            FCmp::OLe => Op::FOrdLessThanEqual,
            FCmp::OGt => Op::FOrdGreaterThan,
            FCmp::ONe => Op::FOrdNotEqual,
            FCmp::OGe => Op::FOrdGreaterThanEqual,
            FCmp::Ord | FCmp::Uno => {
                // `OpOrdered` / `OpUnordered` need the Kernel capability
                let na = self.op(Op::IsNan, t, &[fa]);
                let nb = self.op(Op::IsNan, t, &[fb]);
                let un = self.op(Op::LogicalOr, t, &[na, nb]);
                return if op == FCmp::Uno {
                    un
                } else {
                    self.op(Op::LogicalNot, t, &[un])
                };
            }
            FCmp::ULt => Op::FUnordLessThan,
            FCmp::UEq => Op::FUnordEqual,
            FCmp::ULe => Op::FUnordLessThanEqual,
            FCmp::UGt => Op::FUnordGreaterThan,
            FCmp::UNe => Op::FUnordNotEqual,
            FCmp::UGe => Op::FUnordGreaterThanEqual,
        };
        self.op(o, t, &[fa, fb])
    }

    fn cvt(&mut self, op: Cvt, a: u32) -> u32 {
        let (u, f) = (self.t_u32(), self.t_f32());
        match op {
            Cvt::FToU | Cvt::FToI => {
                let fa = self.as_f32(a);
                let o = if op == Cvt::FToU {
                    Op::ConvertFToU
                } else {
                    Op::ConvertFToS
                };
                self.op(o, u, &[fa])
            }
            Cvt::UToF | Cvt::IToF => {
                let o = if op == Cvt::UToF {
                    Op::ConvertUToF
                } else {
                    Op::ConvertSToF
                };
                let r = self.op(o, f, &[a]);
                self.as_bits(r)
            }
        }
    }

    fn ldexp(&mut self, a: u32, e: u32) -> u32 {
        let fa = self.as_f32(a);
        let t = self.t_f32();
        // the exponent is a signed 32-bit integer
        let r = self.ext(t, GL::Ldexp, &[fa, e]);
        self.as_bits(r)
    }

    fn unpack_half2(&mut self, a: u32) -> (u32, u32) {
        let v2 = self.t_v2();
        let v = self.ext(v2, GL::UnpackHalf2x16, &[a]);
        let f = self.t_f32();
        let x = self.op(Op::CompositeExtract, f, &[v, 0]);
        let y = self.op(Op::CompositeExtract, f, &[v, 1]);
        (self.as_bits(x), self.as_bits(y))
    }

    fn pack_half2(&mut self, a: u32, b: u32) -> u32 {
        // the values are f32 bits
        let (fa, fb) = (self.as_f32(a), self.as_f32(b));
        let v2 = self.t_v2();
        let v = self.op(Op::CompositeConstruct, v2, &[fa, fb]);
        let u = self.t_u32();
        self.ext(u, GL::PackHalf2x16, &[v])
    }

    fn pack_half(&mut self, a: u32) -> u32 {
        let fa = self.as_f32(a);
        let v2 = self.t_v2();
        let zero = self.fconst(0.0);
        let v = self.op(Op::CompositeConstruct, v2, &[fa, zero]);
        let u = self.t_u32();
        self.ext(u, GL::PackHalf2x16, &[v])
    }

    fn unpack_half(&mut self, a: u32) -> u32 {
        let v2 = self.t_v2();
        let v = self.ext(v2, GL::UnpackHalf2x16, &[a]);
        let f = self.t_f32();
        let x = self.op(Op::CompositeExtract, f, &[v, 0]);
        self.as_bits(x)
    }

    fn select(&mut self, c: u32, a: u32, b: u32) -> u32 {
        let ta = self.types.get(&a).copied();
        let tb = self.types.get(&b).copied();
        let t = ta.unwrap_or_else(|| self.t_u32());
        // the two values can be the same dwords under a different (signed / unsigned, integer
        // / float) type: a bit cast makes them agree with the result type
        let b = match tb {
            Some(tb) if tb != t => self.op(Op::Bitcast, t, &[b]),
            _ => b,
        };
        let a = match ta {
            Some(ta) if ta != t => self.op(Op::Bitcast, t, &[a]),
            _ => a,
        };
        self.op(Op::Select, t, &[c, a, b])
    }

    fn bnot(&mut self, a: u32) -> u32 {
        let t = self.t_bool();
        self.op(Op::LogicalNot, t, &[a])
    }

    fn band(&mut self, a: u32, b: u32) -> u32 {
        let t = self.t_bool();
        self.op(Op::LogicalAnd, t, &[a, b])
    }

    fn bor(&mut self, a: u32, b: u32) -> u32 {
        let t = self.t_bool();
        self.op(Op::LogicalOr, t, &[a, b])
    }

    fn bxor(&mut self, a: u32, b: u32) -> u32 {
        let t = self.t_bool();
        self.op(Op::LogicalNotEqual, t, &[a, b])
    }
}

// ---------------------------------------------------------------------------------------
// The translation

/// One enclosing `Block` / `Loop` of the tree being emitted.
struct Frame {
    target: Target,
    merge: u32,
    cont: u32,
}

/// Whether `NATIV64_CLAMP_COUNT` asks for the loop clamp's counter (it costs an atomic per
/// loop iteration).
fn clamp_count_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("NATIV64_CLAMP_COUNT").is_some())
}

/// The loop iteration cap of `NATIV64_LOOP_CLAMP` (0 or unset: none).
fn loop_clamp() -> Option<u32> {
    static CLAMP: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    let n = *CLAMP.get_or_init(|| {
        std::env::var("NATIV64_LOOP_CLAMP")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    });
    (n > 0).then_some(n)
}

impl Tr<'_> {
    /// Adds one to the loop clamp's counter (set 0, binding 4) so the backend can say which
    /// submission stopped a loop.
    fn clamp_count(&mut self, inc: u32) {
        let tu = self.g.t_u32();
        if self.stage.is_none() {
            return;
        }
        let var = match self.res().clamp_log {
            Some(v) => v,
            None => {
                let rarr = self.g.m.global(Op::TypeRuntimeArray, None, &[tu]);
                self.g.m.decorate(rarr, Decoration::ArrayStride, &[4]);
                let st = self.g.m.global(Op::TypeStruct, None, &[rarr]);
                self.g.m.decorate(st, Decoration::Block, &[]);
                self.g.m.member_decorate(st, 0, Decoration::Offset, &[0]);
                let var = self.global(StorageClass::StorageBuffer, st);
                self.g.m.decorate(var, Decoration::DescriptorSet, &[0]);
                self.g.m.decorate(var, Decoration::Binding, &[4]);
                self.g.m.name(var, "clamp_log");
                self.res().clamp_log = Some(var);
                var
            }
        };
        let p = self.g.t_ptr(StorageClass::StorageBuffer, tu);
        let zero = self.g.k(0);
        let ptr = self.g.op(Op::AccessChain, p, &[var, zero, zero]);
        // Device scope, relaxed
        let (scope, relaxed) = (self.g.k(1), self.g.k(0));
        self.g.op(Op::AtomicIAdd, tu, &[ptr, scope, relaxed, inc]);
    }
}

/// How many words of a program's code a module embeds for its literal tables.
const CODE_WINDOW_WORDS: usize = 8192;

struct Tr<'a> {
    f: &'a Func,
    s: &'a Structured,
    g: Gen,
    /// The variable of each register dword and kind (true: a lane mask).
    vars: BTreeMap<(Reg, bool), u32>,
    target_var: u32,
    in_buf: u32,
    out_buf: u32,
    /// The condition of each block ending in a branch, computed at the end of its code.
    conds: HashMap<usize, u32>,
    frames: Vec<Frame>,
    /// The stage entry (`translate_io`); `None` for the probe.
    stage: Option<stage::Stage<'a>>,
    /// The `If` arms being emitted whose branch is a per-lane test (a barrier must not be in
    /// one).
    lane_ifs: u32,
    /// A barrier met inside a per-lane branch: it is emitted once the branch has merged, where
    /// every invocation of the group reaches it.
    pending_barrier: bool,
    /// The program's native code as words, for the literal tables a program indexes with its
    /// own address.
    code: Vec<u32>,
    /// The byte offset of the code window embedded in the module, and its size in words.
    code_base: u32,
    code_window_words: usize,
    /// The private array holding `code`, made by [`Tr::code_prologue`] when a load reads it.
    code_arr: Option<u32>,
}

/// Translates a structured program to the probe module.
pub fn translate(f: &Func, s: &Structured) -> Result<Probe, SpirvError> {
    let (words, inputs, outputs, _, _) = build(f, s, None, &[])?;
    Ok(Probe {
        words,
        inputs,
        outputs,
    })
}

/// Translates a structured vertex or pixel program to a module with its real entry point:
/// inputs from the root block and the stage's built-ins, outputs to the stage's variables.
pub fn translate_io(
    f: &Func,
    s: &Structured,
    io: &StageIo,
    code: &[u8],
) -> Result<Translated, SpirvError> {
    let (words, _, _, samplers, flags) = build(f, s, Some(io), code)?;
    Ok(Translated {
        words,
        samplers,
        images: flags.0,
        storage_images: flags.2,
        memory_addresses: flags.1,
    })
}

type Built = (
    Vec<u32>,
    Vec<Reg>,
    Vec<(Reg, bool)>,
    Vec<SamplerDesc>,
    (bool, bool, bool),
);

fn build<'a>(
    f: &'a Func,
    s: &'a Structured,
    io: Option<&'a StageIo>,
    code: &[u8],
) -> Result<Built, SpirvError> {
    let mut g = Gen::new();
    let (tu, tb) = (g.t_u32(), g.t_bool());
    let pu = g.t_ptr(StorageClass::Private, tu);
    let pb = g.t_ptr(StorageClass::Private, tb);
    let (zero, fls) = (g.k(0), g.kb(false));
    let mut vars = BTreeMap::new();
    // every (register, kind) a value of the program has
    let mut kinds: Vec<(Reg, bool)> = Vec::new();
    for (n, info) in f.vals.iter().enumerate() {
        if info.reg != Reg::Null {
            kinds.push((info.reg, s.masks.is_mask(Val(n as u32))));
        }
    }
    kinds.sort();
    kinds.dedup();
    let mut iface = Vec::new();
    for &(reg, mask) in &kinds {
        let v = if mask {
            g.m.global_var(pb, StorageClass::Private, Some(fls))
        } else {
            g.m.global_var(pu, StorageClass::Private, Some(zero))
        };
        g.m.name(v, &format!("{reg}{}", if mask { "_m" } else { "" }));
        vars.insert((reg, mask), v);
        iface.push(v);
    }
    // the two buffers of the probe: struct { uint data[]; } at set 0, bindings 0 and 1
    let (mut in_buf, mut out_buf) = (0, 0);
    if io.is_none() {
        let arr = g.m.global(Op::TypeRuntimeArray, None, &[tu]);
        g.m.decorate(arr, Decoration::ArrayStride, &[4]);
        let st = g.m.global(Op::TypeStruct, None, &[arr]);
        g.m.decorate(st, Decoration::Block, &[]);
        g.m.member_decorate(st, 0, Decoration::Offset, &[0]);
        let pst = g.t_ptr(StorageClass::StorageBuffer, st);
        in_buf = g.m.global_var(pst, StorageClass::StorageBuffer, None);
        out_buf = g.m.global_var(pst, StorageClass::StorageBuffer, None);
        for (v, b) in [(in_buf, 0), (out_buf, 1)] {
            g.m.decorate(v, Decoration::DescriptorSet, &[0]);
            g.m.decorate(v, Decoration::Binding, &[b]);
            iface.push(v);
        }
        g.m.name(in_buf, "inputs");
        g.m.name(out_buf, "outputs");
    }
    let pf = g.t_ptr(StorageClass::Function, tu);
    let target_var = g.m.id();
    g.locals.extend(inst(
        Op::Variable,
        &[pf, target_var, StorageClass::Function as u32, zero],
    ));

    let mut inputs: Vec<Reg> = f.args.iter().map(|(r, _)| *r).collect();
    inputs.sort();
    let stage = match io {
        Some(io) => Some(stage::Stage::new(io, resource::analyze(f, io.tables())?)),
        None => None,
    };
    let mut t = Tr {
        f,
        s,
        g,
        vars,
        target_var,
        in_buf,
        out_buf,
        conds: HashMap::new(),
        frames: Vec::new(),
        stage,
        lane_ifs: 0,
        pending_barrier: false,
        code: code
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect(),
        code_arr: None,
        code_base: 0,
        code_window_words: 0,
    };
    let entry = t.g.m.id();
    t.g.label(entry);
    if io.is_some() {
        t.begin_stage()?;
        t.code_prologue()?;
    } else {
        t.load_inputs(&inputs)?;
    }
    t.nodes(&s.tree)?;
    if !t.g.terminated {
        t.g.term(Op::Unreachable, &[]);
    }
    let outputs: Vec<(Reg, bool)> = t.vars.keys().copied().collect();
    let tv = t.g.t_void();
    let fnty = t.g.m.global(Op::TypeFunction, None, &[tv]);
    let main = t.g.m.id();
    let mut words = inst(Op::Function, &[tv, main, 0, fnty]);
    words.extend(std::mem::take(&mut t.g.body));
    // the local variables go right after the entry label
    let lab_len = 2;
    let locals = std::mem::take(&mut t.g.locals);
    words.splice(5 + lab_len..5 + lab_len, locals);
    words.extend(inst(Op::FunctionEnd, &[]));
    t.g.m.add_function(&words);
    let mut samplers = Vec::new();
    let mut flags = (false, false, false);
    match t.stage.take() {
        Some(st) => {
            iface.extend(st.interface());
            samplers = st.res.samplers.clone();
            flags = st.res.flags();
            let model = st.model();
            iface.extend(t.g.extra_iface.iter().copied());
            t.g.m.entry_point(model, main, "main", &iface);
            st.exec_modes(&mut t.g.m, main);
        }
        None => {
            iface.extend(t.g.extra_iface.iter().copied());
            t.g.m
                .entry_point(ExecutionModel::GLCompute, main, "main", &iface);
            t.g.m.exec_mode(main, ExecutionMode::LocalSize, &[1, 1, 1]);
        }
    }
    t.g.m.name(main, "main");
    Ok((t.g.m.finish(), inputs, outputs, samplers, flags))
}

impl Tr<'_> {
    fn var(&self, reg: Reg, mask: bool) -> R<u32> {
        match self.vars.get(&(reg, mask)) {
            Some(v) => Ok(*v),
            None => bad("a register has no variable"),
        }
    }

    fn buf_ptr(&mut self, buf: u32, index: u32) -> u32 {
        let tu = self.g.t_u32();
        let p = self.g.t_ptr(StorageClass::StorageBuffer, tu);
        let zero = self.g.k(0);
        let i = self.g.k(index);
        self.g.op(Op::AccessChain, p, &[buf, zero, i])
    }

    fn load_inputs(&mut self, inputs: &[Reg]) -> R<()> {
        let args: HashMap<Reg, Val> = self.f.args.iter().copied().collect();
        let (tu, tb) = (self.g.t_u32(), self.g.t_bool());
        for (k, reg) in inputs.iter().enumerate() {
            let val = args[reg];
            if self.s.masks.is_mask(val) {
                let t = self.g.kb(true);
                let v = self.var(*reg, true)?;
                self.g.store(v, t);
            } else {
                let p = self.buf_ptr(self.in_buf, k as u32);
                let x = self.g.load(tu, p);
                let v = self.var(*reg, false)?;
                self.g.store(v, x);
            }
        }
        let _ = tb;
        Ok(())
    }

    fn write_outputs(&mut self) -> R<()> {
        let (tu, tb) = (self.g.t_u32(), self.g.t_bool());
        let keys: Vec<(Reg, bool)> = self.vars.keys().copied().collect();
        for (k, (reg, mask)) in keys.into_iter().enumerate() {
            let v = self.var(reg, mask)?;
            let x = if mask {
                let b = self.g.load(tb, v);
                let (one, zero) = (self.g.k(1), self.g.k(0));
                self.g.select(b, one, zero)
            } else {
                self.g.load(tu, v)
            };
            let p = self.buf_ptr(self.out_buf, k as u32);
            self.g.store(p, x);
        }
        Ok(())
    }

    /// Makes the code array when the program reads its own code: a private array of the code
    /// words, initialised once before the body (a store in the entry block).
    fn code_prologue(&mut self) -> R<()> {
        let Some(st) = &self.stage else {
            return Ok(());
        };
        if st.res.analysis.code_loads.is_empty() || self.code.is_empty() {
            return Ok(());
        }
        // The literal tables sit around the code offsets the addresses name: a window from
        // there is embedded (one instruction holds at most 65535 operands, and a program's
        // code is larger).
        let base = (st.res.analysis.code_window & !3) as usize / 4;
        let base = base.min(self.code.len());
        let words = (self.code.len() - base).min(CODE_WINDOW_WORDS);
        let window = &self.code[base..base + words];
        let tu = self.g.t_u32();
        let n = self.g.k(words as u32);
        let arr = self.g.m.global(Op::TypeArray, None, &[tu, n]);
        let ids: Vec<u32> = window.iter().map(|&w| self.g.k(w)).collect();
        let init = self.g.m.global(Op::ConstantComposite, Some(arr), &ids);
        let var = self.global(StorageClass::Private, arr);
        self.g.m.name(var, "code");
        self.g.store(var, init);
        self.code_arr = Some(var);
        self.code_base = (base * 4) as u32;
        self.code_window_words = words;
        Ok(())
    }

    /// A dword of the program's code at byte offset `off` (0 outside the embedded window).
    fn code_word(&mut self, off: u32) -> u32 {
        let tu = self.g.t_u32();
        let arr = self.code_arr.expect("the code array");
        let base = self.code_base;
        let tb = self.g.t_bool();
        let base_k = self.g.k(base);
        let below = self.g.op(Op::UGreaterThanEqual, tb, &[off, base_k]);
        let rel = self.g.op(Op::ISub, tu, &[off, base_k]);
        let none = self.g.k(u32::MAX);
        let rel = self.g.select(below, rel, none);
        let two = self.g.k(2);
        let idx = self.g.op(Op::ShiftRightLogical, tu, &[rel, two]);
        let n = self.g.k(self.code_window_words as u32);
        let tb = self.g.t_bool();
        let inside = self.g.op(Op::ULessThan, tb, &[idx, n]);
        let zero = self.g.k(0);
        let safe = self.g.select(inside, idx, zero);
        let p = self.g.t_ptr(StorageClass::Private, tu);
        let ptr = self.g.op(Op::AccessChain, p, &[arr, safe]);
        let v = self.g.load(tu, ptr);
        self.g.select(inside, v, zero)
    }

    /// A mask read as data: the ballot of the lane's bit, and the dword the value's register
    /// names (a wave64 mask pair's low or high half).
    fn as_data(&mut self, s: Src, o: Opd<Gen>) -> R<Opd<Gen>> {
        let Opd::B(bit) = o else {
            return Ok(o);
        };
        let dword = match s {
            Src::Val(v) => match self.f.vals[v.0 as usize].reg {
                Reg::ExecHi | Reg::VccHi => 1,
                Reg::S(n) if n % 2 == 1 => 1,
                _ => 0,
            },
            _ => 0,
        };
        let (lo, hi) = self.g.ballot_words(bit);
        Ok(Opd::V(if dword == 0 { lo } else { hi }))
    }

    /// The value of a source operand.
    fn read(&mut self, s: Src) -> R<Opd<Gen>> {
        Ok(match s {
            Src::Val(v) => {
                let reg = self.f.vals[v.0 as usize].reg;
                let mask = self.s.masks.is_mask(v);
                let var = self.var(reg, mask)?;
                if mask {
                    let t = self.g.t_bool();
                    Opd::B(self.g.load(t, var))
                } else {
                    let t = self.g.t_u32();
                    Opd::V(self.g.load(t, var))
                }
            }
            Src::Const(c) => Opd::V(self.g.k(c)),
            Src::Undef => Opd::V(self.g.k(0)),
            // the program's own address: a constant code offset (the module embeds the code
            // words for the loads that read it)
            Src::CodeLo(o) => Opd::V(self.g.k(o)),
            Src::CodeHi => Opd::V(self.g.k(0)),
        })
    }

    fn write(&mut self, reg: Reg, val: Val, o: Opd<Gen>) -> R<()> {
        let mask = self.s.masks.is_mask(val);
        let var = self.var(reg, mask)?;
        let x = match (o, mask) {
            (Opd::B(b), true) | (Opd::V(b), false) => b,
            (Opd::V(v), true) => {
                let one = self.g.k(1);
                let t = self.g.t_u32();
                let a = self.g.op(Op::BitwiseAnd, t, &[v, one]);
                let z = self.g.k(0);
                let tb = self.g.t_bool();
                self.g.op(Op::INotEqual, tb, &[a, z])
            }
            (Opd::B(b), false) => {
                let (one, zero) = (self.g.k(1), self.g.k(0));
                self.g.select(b, one, zero)
            }
        };
        self.g.store(var, x);
        Ok(())
    }

    /// The lane a DPP control fetches its source from (`lane & 15` rows and the quad and dpp8
    /// groups), or a refusal for a pattern not translated.
    fn dpp_lane(&mut self, dpp: &crate::shader::gfx10::Dpp) -> R<u32> {
        use crate::shader::gfx10::DppKind;
        let g = &mut self.g;
        let lane = <Gen as Backend>::lane_id(g);
        let (three, not3, seven, not7, fifteen, not15) =
            (g.k(3), g.k(!3u32), g.k(7), g.k(!7u32), g.k(15), g.k(!15u32));
        let pick = |g: &mut Gen, src: u32, shift: u32, mask: u32| {
            let t = g.t_u32();
            let sh = g.op(Op::ShiftRightLogical, t, &[src, shift]);
            g.op(Op::BitwiseAnd, t, &[sh, mask])
        };
        match dpp.kind {
            DppKind::Dpp8 | DppKind::Dpp8Fi => {
                let mut sel = 0u32;
                for (k, s) in dpp.sel.iter().enumerate() {
                    sel |= u32::from(*s) << (3 * k);
                }
                let t = g.t_u32();
                let p = g.op(Op::BitwiseAnd, t, &[lane, seven]);
                let sh = g.op(Op::IMul, t, &[p, three]);
                let all = g.k(sel);
                let pickv = pick(g, all, sh, seven);
                let base = g.op(Op::BitwiseAnd, t, &[lane, not7]);
                Ok(g.op(Op::BitwiseOr, t, &[base, pickv]))
            }
            DppKind::Dpp16 => {
                let ctrl = u32::from(dpp.ctrl);
                let n = ctrl & 15;
                let t = g.t_u32();
                match ctrl {
                    0..=0xff => {
                        let p = g.op(Op::BitwiseAnd, t, &[lane, three]);
                        let one = g.k(1);
                        let sh = g.op(Op::ShiftLeftLogical, t, &[p, one]);
                        let perm = g.k(ctrl);
                        let pickv = pick(g, perm, sh, three);
                        let base = g.op(Op::BitwiseAnd, t, &[lane, not3]);
                        Ok(g.op(Op::BitwiseOr, t, &[base, pickv]))
                    }
                    0x101..=0x10f | 0x111..=0x11f => {
                        let nn = g.k(n);
                        let r = if ctrl < 0x110 {
                            g.op(Op::IAdd, t, &[lane, nn])
                        } else {
                            g.op(Op::ISub, t, &[lane, nn])
                        };
                        let row = g.op(Op::BitwiseAnd, t, &[lane, not15]);
                        let low = g.op(Op::BitwiseAnd, t, &[r, fifteen]);
                        Ok(g.op(Op::BitwiseOr, t, &[row, low]))
                    }
                    0x121..=0x12f => {
                        let nn = g.k(n);
                        let r = g.op(Op::IAdd, t, &[lane, nn]);
                        let row = g.op(Op::BitwiseAnd, t, &[lane, not15]);
                        let low = g.op(Op::BitwiseAnd, t, &[r, fifteen]);
                        Ok(g.op(Op::BitwiseOr, t, &[row, low]))
                    }
                    0x140 | 0x141 => {
                        let m = g.k(if ctrl == 0x140 { 15 } else { 8 });
                        Ok(g.op(Op::BitwiseXor, t, &[lane, m]))
                    }
                    0x150..=0x15f => {
                        let row = g.op(Op::BitwiseAnd, t, &[lane, not15]);
                        let nn = g.k(n);
                        Ok(g.op(Op::BitwiseOr, t, &[row, nn]))
                    }
                    0x160..=0x16f => {
                        let m = g.k(n);
                        Ok(g.op(Op::BitwiseXor, t, &[lane, m]))
                    }
                    _ => Err(SpirvError::Unsupported(Unsupported {
                        chunk: 7,
                        op: "a DPP lane pattern".to_string(),
                    })),
                }
            }
        }
    }

    fn code(&mut self, b: usize) -> R<()> {
        let f = self.f;
        let blk = &f.blocks[b];
        for (k, i) in blk.insts.iter().enumerate() {
            // An instruction that is not a mask operation (and does not take a lane mask)
            // reads a mask-valued source as data: the console's mask dword, which the ballot of
            // the lane's bit gives back.
            let data_read = !crate::shader::structure::is_mask_op(&i.instr)
                && !crate::shader::structure::takes_lane_mask(i.instr.name());
            let mut srcs = Vec::new();
            for s in &i.srcs {
                let mut v = Vec::new();
                for &d in s {
                    let o = self.read(d)?;
                    v.push(if data_read { self.as_data(d, o)? } else { o });
                }
                srcs.push(v);
            }
            let mut imps = Vec::new();
            for &(r, s) in &i.imps {
                imps.push((r, self.read(s)?));
            }
            let dst_mask: Vec<bool> = i
                .dsts
                .iter()
                .map(|(_, v)| self.s.masks.is_mask(*v))
                .collect();
            let active = if self.s.guards[b][k] == Guard::Active {
                let e = i.imps.iter().find(|(r, _)| *r == Reg::ExecLo);
                match e {
                    Some(&(_, s)) => {
                        let o = self.read(s)?;
                        Some(match o {
                            Opd::B(c) => c,
                            Opd::V(v) => {
                                let z = self.g.k(0);
                                let tb = self.g.t_bool();
                                self.g.op(Op::INotEqual, tb, &[v, z])
                            }
                        })
                    }
                    None => None,
                }
            } else {
                None
            };
            // a DPP instruction takes its first source from another lane
            if let Some(dpp) = &i.instr.dpp {
                let sw = self.dpp_lane(dpp)?;
                if let Some(first) = srcs.first_mut().and_then(|v| v.first_mut()) {
                    if let Opd::V(v) = *first {
                        *first = Opd::V(self.g.shuffle(v, sw));
                    }
                }
            }
            let ctx = Ctx {
                inst: i,
                srcs,
                imps,
                dst_mask,
                active,
            };
            let res = match self.stage_exec(&ctx, b, k)? {
                Some(r) => r,
                None => alu::exec(&mut self.g, &ctx)?,
            };
            if res.len() != i.dsts.len() {
                return bad("an instruction returned the wrong number of results");
            }
            for ((reg, val), o) in i.dsts.iter().zip(res) {
                self.write(*reg, *val, o)?;
            }
        }
        if let Term::Branch { op, cond, .. } = &blk.term {
            let c = match self.demote_cond(b) {
                Some(c) => c,
                None => self.branch_cond(b, op, cond)?,
            };
            self.conds.insert(b, c);
        }
        // phi inputs that are constants are not in any variable: store them on the edge
        for succ in blk.succs() {
            for phi in &f.blocks[succ].phis {
                for (p, s) in &phi.args {
                    if *p == b && matches!(s, Src::Const(_)) {
                        let o = self.read(*s)?;
                        self.write(phi.reg, phi.dst, o)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// True when the branch of block `b` is taken.
    fn branch_cond(&mut self, b: usize, op: &str, cond: &[(Reg, Src)]) -> R<u32> {
        let tb = self.g.t_bool();
        if let Some(Some(k @ (BranchKind::Lane(t) | BranchKind::Wave(t)))) = self.s.branches.get(b)
        {
            let o = self.read(Src::Val(t.vals[0]))?;
            let mut bit = match o {
                Opd::B(c) => c,
                Opd::V(v) => {
                    let z = self.g.k(0);
                    self.g.op(Op::INotEqual, tb, &[v, z])
                }
            };
            if matches!(k, BranchKind::Wave(_)) {
                // the wave goes in when any lane does
                bit = self.g.any(bit);
            }
            return Ok(if t.taken_when_set {
                bit
            } else {
                self.g.bnot(bit)
            });
        }
        let mut nz: Option<u32> = None;
        for (_, s) in cond {
            let o = self.read(*s)?;
            let c = match o {
                Opd::B(c) => c,
                Opd::V(v) => {
                    let z = self.g.k(0);
                    self.g.op(Op::INotEqual, tb, &[v, z])
                }
            };
            nz = Some(match nz {
                Some(a) => self.g.bor(a, c),
                None => c,
            });
        }
        let Some(nz) = nz else {
            return Ok(self.g.kb(false));
        };
        let set = matches!(
            op,
            "s_cbranch_scc1" | "s_cbranch_vccnz" | "s_cbranch_execnz"
        );
        Ok(if set { nz } else { self.g.bnot(nz) })
    }

    fn nodes(&mut self, ns: &[Node]) -> R<()> {
        for n in ns {
            self.node(n)?;
        }
        Ok(())
    }

    /// A one-trip (`Block`) or looping (`Loop`) construct: `OpLoopMerge` with a continue block
    /// that only goes back to the header.
    fn construct(
        &mut self,
        target: Target,
        body: &[Node],
        forwards: &[Target],
        looping: bool,
    ) -> R<()> {
        let (hdr, body_l, cont, merge) =
            (self.g.m.id(), self.g.m.id(), self.g.m.id(), self.g.m.id());
        // `NATIV64_LOOP_CLAMP=<n>` stops a loop after n iterations: a safety net for a
        // translated bound that never terminates (a GPU hang loses the device).
        let clamp = loop_clamp();
        let counter = match clamp {
            Some(_) => {
                let tu = self.g.t_u32();
                let var = self.global(StorageClass::Private, tu);
                let zero = self.g.k(0);
                self.g.store(var, zero);
                Some(var)
            }
            None => None,
        };
        self.g.branch(hdr);
        self.g.label(hdr);
        self.g.op0(Op::LoopMerge, &[merge, cont, 0]);
        self.g.term(Op::Branch, &[body_l]);
        self.g.label(body_l);
        self.frames.push(Frame {
            target,
            merge,
            cont,
        });
        let r = self.nodes(body);
        self.frames.pop();
        r?;
        if !self.g.terminated {
            // a Block falls to its merge; a Loop never ends without a `Br`
            self.g.branch(if looping { cont } else { merge });
        }
        self.g.label(cont);
        match (counter, clamp) {
            (Some(var), Some(limit)) => {
                // the clamp exits from the continue block: the back-edge block is the continue
                // construct itself, so the structure stays valid
                let tu = self.g.t_u32();
                let tb = self.g.t_bool();
                let n = self.g.load(tu, var);
                let one = self.g.k(1);
                let n = self.g.op(Op::IAdd, tu, &[n, one]);
                self.g.store(var, n);
                let k = self.g.k(limit);
                let under = self.g.op(Op::ULessThan, tb, &[n, k]);
                let over = self.g.op(Op::LogicalNot, tb, &[under]);
                // count the stop so a run says which submission clamps (the atomic adds zero
                // when the loop goes on); off unless asked, an atomic per iteration costs
                if clamp_count_on() {
                    let (one, zero) = (self.g.k(1), self.g.k(0));
                    let inc = self.g.select(over, one, zero);
                    self.clamp_count(inc);
                }
                self.g.term(Op::BranchConditional, &[under, hdr, merge]);
            }
            _ => self.g.branch(hdr),
        }
        self.g.label(merge);
        for &t in forwards {
            self.forward(t)?;
        }
        Ok(())
    }

    /// After a construct's merge: a multi-level `Br` that left through it goes on.
    fn forward(&mut self, t: Target) -> R<()> {
        let Some(parent) = self.frames.last() else {
            return bad("a forwarded branch has no enclosing construct");
        };
        let (pmerge, pcont, ptarget) = (parent.merge, parent.cont, parent.target);
        let (tu, tb) = (self.g.t_u32(), self.g.t_bool());
        let cur = self.g.load(tu, self.target_var);
        let code = self.g.k(target_code(t));
        let c = self.g.op(Op::IEqual, tb, &[cur, code]);
        let (then, next) = (self.g.m.id(), self.g.m.id());
        self.g.op0(Op::SelectionMerge, &[next, 0]);
        self.g.term(Op::BranchConditional, &[c, then, next]);
        self.g.label(then);
        if t == ptarget {
            let z = self.g.k(0);
            self.g.store(self.target_var, z);
            self.g.branch(if t.kind == BrKind::Continue {
                pcont
            } else {
                pmerge
            });
        } else {
            self.g.branch(pmerge);
        }
        self.g.label(next);
        Ok(())
    }

    fn node(&mut self, n: &Node) -> R<()> {
        match n {
            Node::Code(b) => self.code(*b),
            Node::Block {
                label,
                body,
                forwards,
            } => self.construct(
                Target {
                    block: *label,
                    kind: BrKind::Exit,
                },
                body,
                forwards,
                false,
            ),
            Node::Loop {
                label,
                body,
                forwards,
            } => self.construct(
                Target {
                    block: *label,
                    kind: BrKind::Continue,
                },
                body,
                forwards,
                true,
            ),
            Node::If { head, taken, fall } => {
                let Some(&c) = self.conds.get(head) else {
                    return bad("an If without the condition of its block");
                };
                let (t, e, merge) = (self.g.m.id(), self.g.m.id(), self.g.m.id());
                self.g.op0(Op::SelectionMerge, &[merge, 0]);
                self.g.term(Op::BranchConditional, &[c, t, e]);
                let lane = matches!(self.s.branches.get(*head), Some(Some(BranchKind::Lane(_))));
                self.lane_ifs += u32::from(lane);
                for (l, arm) in [(t, taken), (e, fall)] {
                    self.g.label(l);
                    self.nodes(arm)?;
                    if !self.g.terminated {
                        self.g.branch(merge);
                    }
                }
                self.lane_ifs -= u32::from(lane);
                self.g.label(merge);
                if self.pending_barrier && self.lane_ifs == 0 {
                    // a barrier that was under a per-lane branch: this merge is where the whole
                    // group is together again (a nested per-lane branch keeps it pending until
                    // its outer merge, so the barrier never runs diverged)
                    self.pending_barrier = false;
                    self.workgroup_barrier();
                }
                Ok(())
            }
            Node::Br { to, depth } => {
                let Some(inner) = self.frames.last() else {
                    return bad("a Br outside every construct");
                };
                let (imerge, icont) = (inner.merge, inner.cont);
                if *depth == 0 {
                    if inner.target != *to {
                        return bad("a Br does not match its innermost construct");
                    }
                    self.g.branch(if to.kind == BrKind::Continue {
                        icont
                    } else {
                        imerge
                    });
                } else {
                    let code = self.g.k(target_code(*to));
                    self.g.store(self.target_var, code);
                    self.g.branch(imerge);
                }
                Ok(())
            }
            Node::Return => {
                if self.stage.is_none() {
                    self.write_outputs()?;
                }
                self.g.term(Op::Return, &[]);
                Ok(())
            }
        }
    }
}
