//! Prints the parsed container of shader blobs (the files `NATIV64_DUMP_SHADERS` writes):
//! `nativ64-shader <blob>...`. Exit status 1 if any blob is refused.

use nativ64_gpu::shader;
use std::process::ExitCode;

fn main() -> ExitCode {
    let files: Vec<String> = std::env::args().skip(1).collect();
    if files.is_empty() {
        eprintln!("usage: nativ64-shader <blob.dxbc>...");
        return ExitCode::from(2);
    }
    let mut failed = false;
    for f in &files {
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
            Ok(p) => print!("{p}"),
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
