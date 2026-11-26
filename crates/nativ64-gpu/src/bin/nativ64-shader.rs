//! Prints the parsed container of shader blobs (the files `NATIV64_DUMP_SHADERS` writes):
//! `nativ64-shader [--disasm | --ir] <blob>...`. With `--disasm` each program's code is followed
//! from its entry and printed in LLVM syntax; with `--ir` it is printed as the SSA IR. `nativ64-shader --hex <file>` disassembles one
//! instruction per line of hex bytes (for diffing against `llvm-mc`). `--stats` counts the
//! programs that decode and why the others do not; `--ir --stats` counts the programs whose IR
//! builds, per stage, and the errors by kind; `--structure --stats` does the same for the
//! structuring (cross-lane uses by instruction name included). Exit status 1 if any blob is refused or any code
//! fails to decode.

use nativ64_gpu::shader::{self, gfx10};
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let disasm = args.first().is_some_and(|a| a == "--disasm");
    let ir = args.first().is_some_and(|a| a == "--ir");
    let structure = args.first().is_some_and(|a| a == "--structure");
    if disasm || ir {
        args.remove(0);
    }
    if structure {
        args.remove(0);
        if args.first().is_some_and(|a| a == "--stats") {
            return structure_stats(&args[1..]);
        }
        eprintln!("usage: nativ64-shader --structure --stats <blob.dxbc>...");
        return ExitCode::from(2);
    }
    if args.first().is_some_and(|a| a == "--stats") {
        return if ir {
            ir_stats(&args[1..])
        } else {
            stats(&args[1..])
        };
    }
    if args.first().is_some_and(|a| a == "--hex") {
        return match args.get(1) {
            Some(f) => hex_lines(f),
            None => ExitCode::from(2),
        };
    }
    if args.is_empty() {
        eprintln!("usage: nativ64-shader [--disasm | --ir] <blob.dxbc>... | --hex <file>");
        return ExitCode::from(2);
    }
    let mut failed = false;
    for f in &args {
        let bytes = match std::fs::read(f) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("{f}: {e}");
                failed = true;
                continue;
            }
        };
        println!(
            "== {f} ({} bytes, hash {:016x})",
            bytes.len(),
            shader::fnv1a64(&bytes)
        );
        match shader::parse(&bytes) {
            Ok(p) => {
                print!("{p}");
                if ir {
                    failed |= !print_ir(&p);
                } else if disasm {
                    failed |= !print_code(&p);
                }
            }
            Err(e) => {
                println!("refused: {e}");
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Prints the SSA IR of the program's own stage record.
fn print_ir(p: &shader::ParsedShader) -> bool {
    let Some(rec) = p.record() else {
        return true;
    };
    let code = p.native.code(rec);
    let regs = shader::ir::EntryRegs::from_stage(rec);
    match shader::ir::build(code, gfx10::ENTRY, rec.wave32(), &regs) {
        Ok(f) => {
            print!("{f}");
            true
        }
        Err(e) => {
            println!("  IR error: {e}");
            false
        }
    }
}

/// Prints the flow-followed code of the program's VS or PS record. Stages without a decoded
/// record (compute, mesh) print each slot's code, read as wave64 like `--stats` does.
fn print_code(p: &shader::ParsedShader) -> bool {
    let Some(rec) = p.record() else {
        let mut ok = true;
        for (_, code) in p.native.slot_codes() {
            ok &= print_follow(code, false);
        }
        return ok;
    };
    print_follow(p.native.code(rec), rec.wave32())
}

fn print_follow(code: &[u8], wave32: bool) -> bool {
    match gfx10::follow(code, gfx10::ENTRY, wave32) {
        Ok(prog) => {
            for i in &prog.instrs {
                println!("  +{:#05x}: {}", i.at, gfx10::disasm_instr(i));
            }
            for at in &prog.indirect {
                println!("  indirect jump at +{at:#x}");
            }
            true
        }
        Err(e) => {
            println!("  decode error: {e}");
            false
        }
    }
}

/// Decodes every program of every blob (all stages) and reports how many decode entirely and
/// which opcodes / encodings stop the rest, by count.
fn stats(files: &[String]) -> ExitCode {
    use std::collections::BTreeMap;
    let mut by_stage: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    let mut errs: BTreeMap<String, u32> = BTreeMap::new();
    let mut refused = 0;
    let mut instrs = 0u64;
    for f in files {
        let Ok(bytes) = std::fs::read(f) else {
            continue;
        };
        let Ok(p) = shader::parse(&bytes) else {
            refused += 1;
            continue;
        };
        let stage = format!("{:?}", p.kind);
        for (_, code) in p.native.slot_codes() {
            let e = by_stage.entry(stage.clone()).or_default();
            e.0 += 1;
            match gfx10::follow(code, gfx10::ENTRY, false) {
                Ok(prog) => {
                    instrs += prog.instrs.len() as u64;
                    e.1 += 1;
                }
                Err(err) => {
                    let key = match err.kind {
                        gfx10::ErrorKind::UnknownOpcode { enc, opcode } => {
                            format!("{} opcode {opcode:#x}", enc.name())
                        }
                        gfx10::ErrorKind::UnknownEncoding { name, .. } => {
                            format!("encoding {name}")
                        }
                        k => format!("{k:?}"),
                    };
                    *errs.entry(key).or_default() += 1;
                }
            }
        }
    }
    println!("blobs refused by the container parser: {refused}; instructions decoded: {instrs}");
    for (s, (n, ok)) in &by_stage {
        println!("{s}: {ok}/{n} programs decode entirely");
    }
    let mut v: Vec<_> = errs.into_iter().collect();
    v.sort_by_key(|a| std::cmp::Reverse(a.1));
    for (k, n) in v.iter().take(40) {
        println!("{n:6}  first failure: {k}");
    }
    ExitCode::SUCCESS
}

/// Builds the IR of every program with a decoded record (vertex, pixel, mesh, compute) and
/// reports, per stage, how many build and the errors by kind. A read of a register the entry
/// table does not explain is listed with the first reading instruction and a sample blob.
fn ir_stats(files: &[String]) -> ExitCode {
    use shader::ir::{self, IrErrorKind};
    use std::collections::BTreeMap;
    let mut stage: BTreeMap<&str, (u32, u32)> = BTreeMap::new();
    let mut errs: BTreeMap<(String, String), u32> = BTreeMap::new();
    let mut live_ins: BTreeMap<(String, String), (u64, String)> = BTreeMap::new();
    // vertex programs: (has attributes, opens with the fetch call) -> count
    let mut fetch: BTreeMap<(bool, bool), u32> = BTreeMap::new();
    let (mut refused, mut blocks, mut insts, mut phis) = (0, 0usize, 0usize, 0usize);
    for f in files {
        let Ok(bytes) = std::fs::read(f) else {
            continue;
        };
        let Ok(p) = shader::parse(&bytes) else {
            refused += 1;
            continue;
        };
        for (name, rec) in p.native.records() {
            let code = p.native.code(rec);
            let regs = ir::EntryRegs::from_stage(rec);
            let e = stage.entry(name).or_default();
            e.0 += 1;
            if name == "vs" {
                let call = gfx10::decode_wave(code, gfx10::ENTRY, rec.wave32())
                    .is_ok_and(|i| i.name() == "s_swappc_b64");
                *fetch.entry((regs.fetch.is_some(), call)).or_default() += 1;
            }
            match ir::build(code, gfx10::ENTRY, rec.wave32(), &regs) {
                Ok(func) => {
                    e.1 += 1;
                    blocks += func.blocks.len();
                    insts += func.blocks.iter().map(|b| b.insts.len()).sum::<usize>();
                    phis += func.blocks.iter().map(|b| b.phis.len()).sum::<usize>();
                }
                Err(err) => {
                    let kind = match &err.kind {
                        IrErrorKind::Decode(d) => match d.kind {
                            gfx10::ErrorKind::UnknownOpcode { enc, opcode } => {
                                format!("decode: {} opcode {opcode:#x}", enc.name())
                            }
                            gfx10::ErrorKind::UnknownEncoding { name, .. } => {
                                format!("decode: encoding {name}")
                            }
                            ref k => format!("decode: {k:?}"),
                        },
                        IrErrorKind::Indirect => "register jump".to_string(),
                        IrErrorKind::UnsupportedEncoding(n) => format!("encoding {n}"),
                        IrErrorKind::Unsupported(s) => format!("unsupported: {s}"),
                        IrErrorKind::UndefinedRead(r) => {
                            let at = gfx10::decode_wave(code, err.at, rec.wave32())
                                .map(|i| gfx10::disasm_instr(&i))
                                .unwrap_or_default();
                            live_ins
                                .entry((name.to_string(), r.to_string()))
                                .or_insert((
                                    shader::fnv1a64(&bytes),
                                    format!("+{:#x}: {at}", err.at),
                                ));
                            format!("live-in {r} not explained")
                        }
                        IrErrorKind::BadOperand(s) => format!("bad operand: {s}"),
                    };
                    *errs.entry((name.to_string(), kind)).or_default() += 1;
                }
            }
        }
    }
    println!("blobs refused by the container parser: {refused}");
    let (programs, built) = stage.values().fold((0, 0), |a, v| (a.0 + v.0, a.1 + v.1));
    println!("IR built: {built}/{programs} programs ({blocks} blocks, {insts} instructions, {phis} phis)");
    for (s, (n, ok)) in &stage {
        println!("  {s}: {ok}/{n} build");
    }
    let mut v: Vec<_> = errs.into_iter().collect();
    v.sort_by_key(|a| std::cmp::Reverse(a.1));
    for ((s, k), n) in &v {
        println!("{n:6}  {s}: {k}");
    }
    for ((s, r), (hash, at)) in &live_ins {
        println!("live-in {s} {r}: first read {at} (blob hash {hash:016x})");
    }
    for ((attrs, call), n) in &fetch {
        println!(
            "vs {n}: {} attributes in the stage data, {} fetch call",
            if *attrs { "with" } else { "no" },
            if *call { "opens with a" } else { "no" }
        );
    }
    ExitCode::SUCCESS
}

/// Structures the IR of every program that builds and reports, per stage, how many structure
/// and the errors by kind; then the same ignoring cross-lane uses (what else stops them), and
/// the cross-lane instructions by name with the number of programs that have each.
fn structure_stats(files: &[String]) -> ExitCode {
    use shader::ir;
    use shader::structure::{self, StructError};
    use std::collections::{BTreeMap, BTreeSet};
    let kind = |e: &StructError| match e {
        StructError::CrossLane(_) => "cross-lane use".to_string(),
        StructError::DivergentScalar { .. } => "divergent scalar".to_string(),
        StructError::Irreducible { .. } => "irreducible".to_string(),
        StructError::Malformed(s) => format!("malformed: {s}"),
    };
    let mut stage: BTreeMap<String, (u32, u32, u32)> = BTreeMap::new();
    let mut errs: BTreeMap<(String, String), u32> = BTreeMap::new();
    let mut errs_free: BTreeMap<(String, String), u32> = BTreeMap::new();
    let mut names: BTreeMap<(String, String), u32> = BTreeMap::new();
    let mut samples: BTreeMap<(String, String), (u64, String)> = BTreeMap::new();
    let (mut no_ir, mut demotes, mut lane_br, mut uni_br) = (0u32, 0u32, 0usize, 0usize);
    let (mut proved, mut active) = (0usize, 0usize);
    for f in files {
        let Ok(bytes) = std::fs::read(f) else {
            continue;
        };
        let Ok(p) = shader::parse(&bytes) else {
            continue;
        };
        for (name, rec) in p.native.records() {
            let code = p.native.code(rec);
            let regs = ir::EntryRegs::from_stage(rec);
            let Ok(func) = ir::build(code, gfx10::ENTRY, rec.wave32(), &regs) else {
                no_ir += 1;
                continue;
            };
            let e = stage.entry(name.to_string()).or_default();
            e.0 += 1;
            let masks = structure::type_masks(&func);
            let distinct: BTreeSet<&str> =
                masks.cross_lane.iter().map(|c| c.name.as_str()).collect();
            for n in distinct {
                *names.entry((name.to_string(), n.to_string())).or_default() += 1;
            }
            match structure::structure(&func) {
                Ok(s) => {
                    e.1 += 1;
                    demotes += u32::from(!s.demotes.is_empty());
                    for b in s.branches.iter().flatten() {
                        match b {
                            structure::BranchKind::Lane(_) => lane_br += 1,
                            structure::BranchKind::Uniform => uni_br += 1,
                        }
                    }
                    for g in s.guards.iter().flatten() {
                        match g {
                            structure::Guard::Proved => proved += 1,
                            structure::Guard::Active => active += 1,
                            structure::Guard::Scalar => {}
                        }
                    }
                }
                Err(err) => {
                    *errs.entry((name.to_string(), kind(&err))).or_default() += 1;
                }
            }
            match structure::structure_with(&func, true) {
                Ok(_) => e.2 += 1,
                Err(err) => {
                    let key = (name.to_string(), kind(&err));
                    samples
                        .entry(key.clone())
                        .or_insert((shader::fnv1a64(&bytes), err.to_string()));
                    *errs_free.entry(key).or_default() += 1;
                }
            }
        }
    }
    println!("programs without IR (not counted): {no_ir}");
    let total = stage
        .values()
        .fold((0, 0, 0), |a, v| (a.0 + v.0, a.1 + v.1, a.2 + v.2));
    println!(
        "structured: {}/{} programs; ignoring cross-lane uses: {}/{}",
        total.1, total.0, total.2, total.0
    );
    for (s, (n, ok, free)) in &stage {
        println!("  {s}: {ok}/{n} structure ({free} ignoring cross-lane uses)");
    }
    println!("branches in structured programs: {lane_br} per-lane, {uni_br} uniform");
    println!("vector instructions in structured programs: {proved} guards proved, {active} kept");
    println!("programs with a pixel discard (demote): {demotes}");
    let sorted = |m: BTreeMap<(String, String), u32>| {
        let mut v: Vec<_> = m.into_iter().collect();
        v.sort_by_key(|a| std::cmp::Reverse(a.1));
        v
    };
    println!("errors:");
    for ((s, k), n) in sorted(errs) {
        println!("{n:6}  {s}: {k}");
    }
    println!("errors ignoring cross-lane uses:");
    for ((s, k), n) in sorted(errs_free) {
        println!("{n:6}  {s}: {k}");
    }
    for ((s, k), (hash, text)) in &samples {
        println!("sample {s}: {k}: {text} (blob hash {hash:016x})");
    }
    println!("cross-lane instructions (programs that have one):");
    for ((s, k), n) in sorted(names) {
        println!("{n:6}  {s}: {k}");
    }
    ExitCode::SUCCESS
}

/// One instruction per input line (hex bytes, any separators); prints `text` or `ERR message`.
fn hex_lines(file: &str) -> ExitCode {
    let Ok(text) = std::fs::read_to_string(file) else {
        eprintln!("{file}: unreadable");
        return ExitCode::from(2);
    };
    for line in text.lines() {
        let hex: String = line.chars().filter(char::is_ascii_hexdigit).collect();
        let bytes: Vec<u8> = (0..hex.len() / 2)
            .map(|k| u8::from_str_radix(&hex[2 * k..2 * k + 2], 16).unwrap_or(0))
            .collect();
        match gfx10::decode_wave(&bytes, 0, false) {
            Ok(i) if i.len == bytes.len() => println!("{}", gfx10::disasm_instr(&i)),
            Ok(i) => println!("ERR length {} != {}", i.len, bytes.len()),
            Err(e) => println!("ERR {e}"),
        }
    }
    ExitCode::SUCCESS
}
