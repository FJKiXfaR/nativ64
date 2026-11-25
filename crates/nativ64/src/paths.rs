//! The file path rule for the file APIs (see docs/library.md, "File paths").
//!
//! A title's path is a Windows path: `\` becomes `/`, a drive prefix (`C:`) is dropped and the
//! rest is relative to the package root, the directory of the main executable. A path that
//! starts with `/` as written is a host path and is kept (our tests and tools use it). When no
//! exact match exists, each component is looked up ignoring case, as the title expects from a
//! Windows file system. The package directory is never written: writes go under
//! [`write_dir`].

use std::path::{Path, PathBuf};

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

/// Maps a title path to the host path to read: `\` to `/`, relative to the package root, with a
/// case-insensitive fallback per component.
pub fn map_read_path(win: &str) -> PathBuf {
    let host_absolute = win.starts_with('/');
    let unified = win.replace('\\', "/");
    let mut rest = unified.as_str();
    let bytes = rest.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        rest = &rest[2..];
    }
    let mut dir = if host_absolute {
        PathBuf::from("/")
    } else {
        package_root()
    };
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
}
