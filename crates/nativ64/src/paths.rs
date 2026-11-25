//! The file path rule for the file APIs (see docs/library.md, "File paths").
//!
//! A title's path is a Windows path: `\` becomes `/`, a drive prefix (`C:`) is dropped and the
//! rest is relative to the package root, the directory of the main executable. A path that
//! starts with `/` as written is a host path and is kept (our tests and tools use it). When no
//! exact match exists, each component is looked up ignoring case, as the title expects from a
//! Windows file system. The package directory is never written: writes go under
//! [`write_dir`].
//!
//! Reads search a list of directories: the package root first, then the extra directories
//! set with [`set_extra_package_dirs`] (`nativ64-run --data`), then those of the environment
//! variable `NATIV64_PACKAGE_DIRS` (colon-separated; empty entries are ignored). The first
//! directory where the path exists wins; when none has it, the package-root mapping is returned.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

static EXTRA_DIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Sets the extra package directories (searched after the package root, before those of
/// `NATIV64_PACKAGE_DIRS`). Replaces any earlier list.
pub fn set_extra_package_dirs(dirs: Vec<PathBuf>) {
    *EXTRA_DIRS.lock().unwrap_or_else(|e| e.into_inner()) = dirs;
}

/// Splits a colon-separated directory list, dropping empty entries.
pub fn parse_dir_list(list: &str) -> Vec<PathBuf> {
    list.split(':')
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// The extra package directories: the ones set with [`set_extra_package_dirs`], then those of
/// `NATIV64_PACKAGE_DIRS`.
pub fn extra_package_dirs() -> Vec<PathBuf> {
    let mut v = EXTRA_DIRS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(env) = std::env::var_os("NATIV64_PACKAGE_DIRS") {
        v.extend(parse_dir_list(&env.to_string_lossy()));
    }
    v
}

/// The whole search list: the package root, then [`extra_package_dirs`].
pub fn package_dirs() -> Vec<PathBuf> {
    let mut v = vec![package_root()];
    v.extend(extra_package_dirs());
    v
}

/// The package root: the directory of the registered main module (the current directory if
/// none is registered).
pub fn package_root() -> PathBuf {
    crate::process::main_module()
        .and_then(|m| Path::new(&m.path).parent().map(Path::to_path_buf))
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `$XDG_DATA_HOME/nativ64/<exe stem>` (default `~/.local/share`), where the title may write.
pub fn write_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))
        .unwrap_or_else(|| PathBuf::from("."));
    let stem = crate::process::main_module()
        .and_then(|m| {
            Path::new(&m.path)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "title".to_string());
    base.join("nativ64").join(stem)
}

/// Maps a title path to the host path to read: `\` to `/`, searched in [`package_dirs`] (first
/// existing wins, else the package-root mapping), with a case-insensitive fallback per component.
pub fn map_read_path(win: &str) -> PathBuf {
    map_read_path_in(&package_dirs(), win)
}

/// [`map_read_path`] over an explicit list of directories (the first is the default).
pub fn map_read_path_in(dirs: &[PathBuf], win: &str) -> PathBuf {
    let unified = win.replace('\\', "/");
    if win.starts_with('/') {
        return resolve_in(Path::new("/"), &unified);
    }
    let mut first = None;
    for d in dirs {
        let p = resolve_in(d, &unified);
        if p.symlink_metadata().is_ok() {
            return p;
        }
        first.get_or_insert(p);
    }
    first.unwrap_or_else(|| resolve_in(&package_root(), &unified))
}

/// `rest` (separators already `/`, optional drive prefix) under `start`, ignoring case per
/// component when the exact name is missing.
fn resolve_in(start: &Path, rest: &str) -> PathBuf {
    let mut rest = rest;
    let bytes = rest.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        rest = &rest[2..];
    }
    let mut dir = start.to_path_buf();
    for part in rest.split('/').filter(|p| !p.is_empty() && *p != ".") {
        let exact = dir.join(part);
        if exact.symlink_metadata().is_ok() {
            dir = exact;
            continue;
        }
        let found = std::fs::read_dir(&dir).ok().and_then(|it| {
            it.flatten()
                .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(part))
                .map(|e| e.path())
        });
        dir = found.unwrap_or(exact);
    }
    dir
}

/// Maps a title path to the host path to write: the same relative path under [`write_dir`] (a
/// drive prefix and `..` components are dropped, so nothing escapes). `None` for an empty path.
pub fn map_write_path(win: &str) -> Option<PathBuf> {
    let unified = win.replace('\\', "/");
    let mut rest = unified.as_str();
    let bytes = rest.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        rest = &rest[2..];
    }
    let mut out = write_dir();
    let mut any = false;
    for part in rest
        .split('/')
        .filter(|p| !p.is_empty() && *p != "." && *p != "..")
    {
        out.push(part);
        any = true;
    }
    any.then_some(out)
}

/// Tests that change the main module (so the write directory) or depend on it take this.
#[cfg(test)]
pub(crate) static MAIN_MODULE_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_separators_drive_and_case() {
        let root = std::env::temp_dir().join(format!("nativ64-paths-{}", std::process::id()));
        std::fs::create_dir_all(root.join("Pack")).unwrap();
        std::fs::write(root.join("Pack/Data.RPF"), b"x").unwrap();
        let host = root.to_string_lossy().into_owned();
        assert_eq!(
            map_read_path(&format!("{host}/pack\\data.rpf")),
            root.join("Pack/Data.RPF")
        );
        // Relative paths hang off the package root (the current directory here).
        assert_eq!(
            map_read_path("C:\\a\\b.bin"),
            package_root().join("a").join("b.bin")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn search_list_first_existing_wins() {
        let root = std::env::temp_dir().join(format!("nativ64-paths-list-{}", std::process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(b.join("Sub")).unwrap();
        std::fs::write(a.join("one.rpf"), b"a").unwrap();
        std::fs::write(b.join("one.rpf"), b"b").unwrap();
        std::fs::write(b.join("Sub/Two.rpf"), b"b").unwrap();
        let dirs = [a.clone(), b.clone()];
        assert_eq!(map_read_path_in(&dirs, "G:\\ONE.rpf"), a.join("one.rpf"));
        assert_eq!(
            map_read_path_in(&dirs, "G:\\sub\\two.RPF"),
            b.join("Sub/Two.rpf")
        );
        // Missing everywhere: the first directory's mapping, as before.
        assert_eq!(map_read_path_in(&dirs, "G:\\none.rpf"), a.join("none.rpf"));
        // No directory at all falls back to the package root.
        assert_eq!(map_read_path_in(&[], "x.bin"), package_root().join("x.bin"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn dir_list_ignores_empty_entries() {
        assert_eq!(
            parse_dir_list(":/a::/b:"),
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
        assert!(parse_dir_list("").is_empty());
        assert!(parse_dir_list("::").is_empty());
    }

    #[test]
    fn extra_dirs_come_from_setter_then_env() {
        let root = std::env::temp_dir().join(format!("nativ64-paths-env-{}", std::process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(b.join("Only.Bin"), b"x").unwrap();
        set_extra_package_dirs(vec![a.clone()]);
        std::env::set_var("NATIV64_PACKAGE_DIRS", format!(":{}:", b.display()));
        assert_eq!(extra_package_dirs(), vec![a.clone(), b.clone()]);
        assert_eq!(package_dirs().len(), 3);
        assert_eq!(map_read_path("c:\\only.bin"), b.join("Only.Bin"));
        std::env::remove_var("NATIV64_PACKAGE_DIRS");
        set_extra_package_dirs(Vec::new());
        assert!(extra_package_dirs().is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
