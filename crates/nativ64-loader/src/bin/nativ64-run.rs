use nativ64_loader::{Image, LoadOptions};
use std::process::ExitCode;

/// Real windows over Wayland when a compositor is set and `NATIV64_WINDOW` is not "headless";
/// otherwise (or when the connection fails) the headless default stays.
fn install_window_backend() {
    if std::env::var_os("WAYLAND_DISPLAY").is_none()
        || std::env::var("NATIV64_WINDOW").as_deref() == Ok("headless")
    {
        return;
    }
    match nativ64_gpu::wsi::wayland::WaylandBackend::new() {
        Ok(b) => nativ64::windowing::set_backend(Box::new(b)),
        Err(e) => eprintln!("nativ64-run: {e}; windows stay headless"),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let check = args.iter().any(|a| a == "--check");
    let all = args.iter().any(|a| a == "--all");
    let trace_imports = args.iter().any(|a| a == "--trace-imports");
    let mut timeout = 20u32;
    let mut path = None;
    let mut data_dirs: Vec<std::path::PathBuf> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--data" {
            match it.next() {
                Some(v) => data_dirs.push(v.into()),
                None => {
                    eprintln!("nativ64-run: --data needs a directory");
                    return ExitCode::from(64);
                }
            }
            continue;
        }
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
        eprintln!(
            "usage: nativ64-run [--check [--all]] [--timeout SECONDS] [--trace-imports] [--data DIR]... <exe>"
        );
        return ExitCode::from(64);
    };
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("nativ64-run: {path}: {e}");
            return ExitCode::from(1);
        }
    };
    nativ64::paths::set_extra_package_dirs(data_dirs);
    // DLL search list: the exe's directory first, then the extra package directories.
    let mut search_dirs = vec![std::fs::canonicalize(path)
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| ".".into())];
    search_dirs.extend(nativ64::paths::extra_package_dirs());
    let native_modules = std::env::var("NATIV64_NATIVE_MODULES")
        .map(|v| {
            v.split(',')
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let img = match Image::load(
        &data,
        &LoadOptions {
            trace_imports,
            search_dirs,
            native_modules,
            ..LoadOptions::default()
        },
    ) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("nativ64-run: {path}: {e}");
            return ExitCode::from(1);
        }
    };
    drop(data);
    if !check {
        install_window_backend();
        let full = std::fs::canonicalize(path)
            .map_or_else(|_| path.clone(), |p| p.to_string_lossy().into_owned());
        nativ64::process::set_main_module(img.base() as usize, &full);
        nativ64_loader::fault::install(img.base(), img.size());
        for d in img.dlls() {
            nativ64_loader::fault::register_dll(d.name(), d.base(), d.size());
        }
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
    for i in unresolved.iter().take(if all { usize::MAX } else { 20 }) {
        println!("  unresolved: {}", i.display());
    }
    println!("package DLLs: {}", img.dlls().len());
    for d in img.dlls() {
        println!(
            "  {} at {:#x} ({}), {} imports, {} unresolved",
            d.name(),
            d.base(),
            d.path(),
            d.imports.len(),
            d.unresolved().count()
        );
    }
    // Unresolved imports per module, for the main image and each DLL.
    for (owner, im) in
        std::iter::once(("exe", &img)).chain(img.dlls().iter().map(|d| (d.name(), d)))
    {
        let mut per: std::collections::BTreeMap<&str, usize> = Default::default();
        for i in im.unresolved() {
            *per.entry(i.module.as_str()).or_default() += 1;
        }
        for (m, n) in per {
            println!("  unresolved in {owner}: {m}: {n}");
        }
    }
    ExitCode::SUCCESS
}
