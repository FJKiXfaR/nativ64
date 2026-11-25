use nativ64_loader::{Image, LoadOptions};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let check = args.iter().any(|a| a == "--check");
    let mut timeout = 20u32;
    let mut path = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--timeout" {
            match it.next().and_then(|v| v.parse().ok()) {
                Some(v) => timeout = v,
                None => {
                    eprintln!("nativ64-run: --timeout needs a number of seconds");
                    return ExitCode::from(64);
                }
            }
        } else if !a.starts_with("--") {
            path = Some(a);
        }
    }
    let Some(path) = path else {
        eprintln!("usage: nativ64-run [--check] [--timeout SECONDS] <exe>");
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
        nativ64_loader::fault::install(img.base(), img.size());
        nativ64_loader::fault::set_timeout(timeout);
        return match img.run_main() {
            Ok(code) => ExitCode::from(code as u8),
            Err(e) => {
                eprintln!("nativ64-run: {path}: {e}");
                ExitCode::from(1)
            }
        };
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
