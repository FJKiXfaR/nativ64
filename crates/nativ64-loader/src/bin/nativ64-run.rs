use nativ64_loader::{Image, LoadOptions};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let check = args.iter().any(|a| a == "--check");
    let Some(path) = args.iter().find(|a| !a.starts_with("--")) else {
        eprintln!("usage: nativ64-run [--check] <exe>");
        return ExitCode::from(64);
    };
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("nativ64-run: {path}: {e}");
            return ExitCode::from(1);
        }
    };
    let img = match Image::load(&data, &LoadOptions::default()) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("nativ64-run: {path}: {e}");
            return ExitCode::from(1);
        }
    };
    drop(data);
    if !check {
        eprintln!("execution not implemented yet");
        return ExitCode::from(2);
    }
    let unresolved: Vec<_> = img.unresolved().collect();
    println!(
        "image base:   {:#x} (preferred {:#x})",
        img.base(),
        img.preferred_base
    );
    println!("entry point:  {:#x}", img.entry_point());
    println!("sections:     {}", img.section_count);
    println!(
        "imports:      {} total, {} resolved, {} unresolved",
        img.imports.len(),
        img.imports.len() - unresolved.len(),
        unresolved.len()
    );
    for i in unresolved.iter().take(20) {
        println!("  unresolved: {}", i.display());
    }
    ExitCode::SUCCESS
}
