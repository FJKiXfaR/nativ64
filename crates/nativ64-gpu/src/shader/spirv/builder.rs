//! Our own SPIR-V module builder over the `spirv` crate's enums: the logical sections in
//! order, types and constants deduplicated by key, ids, and `finish()` to words.

use std::collections::HashMap;

use spirv::{AddressingModel, Capability, MemoryModel, Op, StorageClass};

/// The version word of SPIR-V 1.5 (Vulkan 1.2).
const VERSION_1_5: u32 = 0x0001_0500;

/// One instruction's words: `(word count << 16) | opcode`, then the operands.
pub fn inst(op: Op, operands: &[u32]) -> Vec<u32> {
    let mut v = Vec::with_capacity(operands.len() + 1);
    v.push(((operands.len() as u32 + 1) << 16) | op as u32);
    v.extend_from_slice(operands);
    v
}

/// A string literal as operand words (UTF-8, NUL-terminated, padded).
pub fn string_words(s: &str) -> Vec<u32> {
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0);
    while !bytes.len().is_multiple_of(4) {
        bytes.push(0);
    }
    bytes
        .chunks(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// A module under construction.
pub struct Module {
    next_id: u32,
    caps: Vec<u32>,
    exts: Vec<String>,
    imports: Vec<Vec<u32>>,
    addressing: AddressingModel,
    memory: MemoryModel,
    entry_points: Vec<Vec<u32>>,
    exec_modes: Vec<Vec<u32>>,
    names: Vec<Vec<u32>>,
    annotations: Vec<Vec<u32>>,
    /// Types, constants and global variables, in dependency order.
    globals: Vec<Vec<u32>>,
    functions: Vec<u32>,
    keys: HashMap<Vec<u32>, u32>,
}

impl Default for Module {
    fn default() -> Self {
        Self::new()
    }
}

impl Module {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            caps: Vec::new(),
            exts: Vec::new(),
            imports: Vec::new(),
            addressing: AddressingModel::Logical,
            memory: MemoryModel::GLSL450,
            entry_points: Vec::new(),
            exec_modes: Vec::new(),
            names: Vec::new(),
            annotations: Vec::new(),
            globals: Vec::new(),
            functions: Vec::new(),
            keys: HashMap::new(),
        }
    }

    pub fn id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn capability(&mut self, c: Capability) {
        if !self.caps.contains(&(c as u32)) {
            self.caps.push(c as u32);
        }
    }

    /// `OpExtension`, once per name.
    pub fn extension(&mut self, name: &str) {
        if !self.exts.iter().any(|e| e == name) {
            self.exts.push(name.to_string());
        }
    }

    pub fn set_model(&mut self, a: AddressingModel, m: MemoryModel) {
        self.addressing = a;
        self.memory = m;
    }

    /// `OpExtInstImport`, once per set name.
    pub fn ext_import(&mut self, name: &str) -> u32 {
        let mut key = vec![u32::MAX - 1];
        key.extend(string_words(name));
        if let Some(&id) = self.keys.get(&key) {
            return id;
        }
        let id = self.id();
        let mut ops = vec![id];
        ops.extend(string_words(name));
        self.imports.push(inst(Op::ExtInstImport, &ops));
        self.keys.insert(key, id);
        id
    }

    pub fn entry_point(&mut self, model: spirv::ExecutionModel, f: u32, name: &str, iface: &[u32]) {
        let mut ops = vec![model as u32, f];
        ops.extend(string_words(name));
        ops.extend_from_slice(iface);
        self.entry_points.push(inst(Op::EntryPoint, &ops));
    }

    pub fn exec_mode(&mut self, f: u32, mode: spirv::ExecutionMode, args: &[u32]) {
        let mut ops = vec![f, mode as u32];
        ops.extend_from_slice(args);
        self.exec_modes.push(inst(Op::ExecutionMode, &ops));
    }

    pub fn name(&mut self, id: u32, name: &str) {
        let mut ops = vec![id];
        ops.extend(string_words(name));
        self.names.push(inst(Op::Name, &ops));
    }

    pub fn decorate(&mut self, target: u32, d: spirv::Decoration, args: &[u32]) {
        let mut ops = vec![target, d as u32];
        ops.extend_from_slice(args);
        self.annotations.push(inst(Op::Decorate, &ops));
    }

    pub fn member_decorate(&mut self, st: u32, member: u32, d: spirv::Decoration, args: &[u32]) {
        let mut ops = vec![st, member, d as u32];
        ops.extend_from_slice(args);
        self.annotations.push(inst(Op::MemberDecorate, &ops));
    }

    /// A type or constant with a result id (and optional result type), deduplicated by
    /// `(op, type, operands)`.
    pub fn global(&mut self, op: Op, ty: Option<u32>, operands: &[u32]) -> u32 {
        let mut key = vec![op as u32, ty.unwrap_or(0)];
        key.extend_from_slice(operands);
        if let Some(&id) = self.keys.get(&key) {
            return id;
        }
        let id = self.id();
        let mut ops: Vec<u32> = ty.into_iter().collect();
        ops.push(id);
        ops.extend_from_slice(operands);
        self.globals.push(inst(op, &ops));
        self.keys.insert(key, id);
        id
    }

    /// A global `OpVariable` (never deduplicated).
    pub fn global_var(&mut self, ptr_ty: u32, class: StorageClass, init: Option<u32>) -> u32 {
        let id = self.id();
        let mut ops = vec![ptr_ty, id, class as u32];
        ops.extend(init);
        self.globals.push(inst(Op::Variable, &ops));
        id
    }

    /// Appends a finished function (`OpFunction` .. `OpFunctionEnd`).
    pub fn add_function(&mut self, words: &[u32]) {
        self.functions.extend_from_slice(words);
    }

    pub fn finish(self) -> Vec<u32> {
        let mut out = vec![spirv::MAGIC_NUMBER, VERSION_1_5, 0, self.next_id, 0];
        for c in &self.caps {
            out.extend(inst(Op::Capability, &[*c]));
        }
        for e in &self.exts {
            out.extend(inst(Op::Extension, &string_words(e)));
        }
        for i in &self.imports {
            out.extend(i);
        }
        out.extend(inst(
            Op::MemoryModel,
            &[self.addressing as u32, self.memory as u32],
        ));
        for sec in [
            &self.entry_points,
            &self.exec_modes,
            &self.names,
            &self.annotations,
            &self.globals,
        ] {
            for i in sec {
                out.extend(i);
            }
        }
        out.extend(self.functions);
        out
    }
}
