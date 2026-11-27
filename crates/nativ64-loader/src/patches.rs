//! Development patches: `NATIV64_PATCH=0x<image offset>=<hex bytes>,...` rewrites image bytes
//! before the entry point runs.
//!
//! This exists for experiments against the title (for example turning a watchdog thread into a
//! `ret`), not for shipping behaviour: every patch is logged, and without the variable nothing
//! is touched.

/// One patch: an image-relative offset and the bytes to write there.
#[derive(Debug, PartialEq, Eq)]
pub struct Patch {
    pub offset: usize,
    pub bytes: Vec<u8>,
}

/// Parses `0x2215a60=31c0c3,0x1234=90`.
pub fn parse(spec: &str) -> Result<Vec<Patch>, String> {
    let mut out = Vec::new();
    for item in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (at, bytes) = item
            .split_once('=')
            .ok_or_else(|| format!("patch {item:?} is not <offset>=<bytes>"))?;
        let at = at.trim();
        let offset = usize::from_str_radix(at.trim_start_matches("0x"), 16)
            .map_err(|e| format!("patch {item:?}: offset: {e}"))?;
        let hex = bytes.trim();
        if hex.len() % 2 != 0 {
            return Err(format!("patch {item:?}: an odd number of hex digits"));
        }
        let mut raw = Vec::with_capacity(hex.len() / 2);
        for pair in hex.as_bytes().chunks(2) {
            let s = std::str::from_utf8(pair).map_err(|e| format!("patch {item:?}: {e}"))?;
            raw.push(u8::from_str_radix(s, 16).map_err(|e| format!("patch {item:?}: {e}"))?);
        }
        out.push(Patch { offset, bytes: raw });
    }
    Ok(out)
}

/// Applies `NATIV64_PATCH` (when set) to the image mapped at `base`.
pub fn apply(base: usize) {
    let Ok(spec) = std::env::var("NATIV64_PATCH") else {
        return;
    };
    let patches = match parse(&spec) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("nativ64-loader: NATIV64_PATCH: {e}");
            return;
        }
    };
    for p in patches {
        let at = base + p.offset;
        let end = at + p.bytes.len();
        let page = at & !(4095);
        let len = (end.next_multiple_of(4096)) - page;
        // SAFETY: the patch lies inside the mapped image; the page protection is restored below.
        unsafe {
            if libc::mprotect(
                page as *mut libc::c_void,
                len,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            ) != 0
            {
                eprintln!(
                    "nativ64-loader: cannot patch image+{:#x}: {}",
                    p.offset,
                    std::io::Error::last_os_error()
                );
                continue;
            }
            std::ptr::copy_nonoverlapping(p.bytes.as_ptr(), at as *mut u8, p.bytes.len());
            if libc::mprotect(
                page as *mut libc::c_void,
                len,
                libc::PROT_READ | libc::PROT_EXEC,
            ) != 0
            {
                eprintln!(
                    "nativ64-loader: patched image+{:#x} but cannot restore protection: {}",
                    p.offset,
                    std::io::Error::last_os_error()
                );
            }
        }
        eprintln!(
            "nativ64-loader: patched image+{:#x} with {} bytes",
            p.offset,
            p.bytes.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_offsets_and_bytes() {
        let p = parse("0x2215a60=31c0c3").unwrap();
        assert_eq!(
            p,
            [Patch {
                offset: 0x0221_5a60,
                bytes: vec![0x31, 0xC0, 0xC3]
            }]
        );
        let p = parse("10=90,0x20=b8 00").unwrap_err();
        assert!(p.contains("odd"), "{p}");
        assert!(parse("nope").is_err());
    }
}
