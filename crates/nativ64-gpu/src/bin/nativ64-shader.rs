//! Prints the parsed container of shader blobs (the files `NATIV64_DUMP_SHADERS` writes):
//! `nativ64-shader [--disasm | --ir] <blob>...`. With `--disasm` each program's code is followed
//! from its entry and printed in LLVM syntax; with `--ir` it is printed as the SSA IR. `nativ64-shader --hex <file>` disassembles one
//! instruction per line of hex bytes (for diffing against `llvm-mc`). Exit status 1 if any
//! blob is refused or any code fails to decode.

use nativ64_gpu::shader::{self, gfx10};
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let disasm = args.first().is_some_and(|a| a == "--disasm");
    let ir = args.first().is_some_and(|a| a == "--ir");
    if disasm || ir {
        args.remove(0);
    }
    if args.first().is_some_and(|a| a == "--stats") {
        return stats(&args[1..]);
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

/// Prints the SSA IR of the program's VS or PS record.
fn print_ir(p: &shader::ParsedShader) -> bool {
    let Some(rec) = p.record() else {
        return true;
    };
    let code = p.native.code(rec);
    let regs = shader::ir::EntryRegs::from_record(rec);
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
