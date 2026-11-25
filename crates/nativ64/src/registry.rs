use std::ffi::{c_char, c_void, CStr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Native,
    Stub,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Native => "native",
            Status::Stub => "stub",
        }
    }
}

/// A function exported under `module!name`; `addr` is a `extern "win64"` function pointer.
#[derive(Debug)]
pub struct Export {
    pub module: &'static str,
    pub name: &'static str,
    pub addr: *const (),
    pub status: Status,
}

// The address of a function is immutable and valid for the whole process.
unsafe impl Sync for Export {}

/// Declares win64 functions and registers each under `$module` and, with the `=> $alias` form,
/// its forwarder `$alias`. Exported so sibling crates (e.g. nativ64-gpu) share one registry type.
///
/// ```ignore
/// win32! {
///     module "api-ms-win-core-foo" => "kernel32";
///     Native fn Foo(a: u32) -> u32 { a }
/// }
/// ```
#[macro_export]
macro_rules! win32 {
    (module $m:literal;
     $( $status:ident fn $name:ident ( $($arg:ident : $ty:ty),* ) $(-> $ret:ty)? $body:block )* ) => {
        $(
            #[allow(non_snake_case, clippy::missing_safety_doc)]
            pub unsafe extern "win64" fn $name($($arg: $ty),*) $(-> $ret)? $body
        )*
        pub static EXPORTS: &[$crate::Export] = &[
            $(
                $crate::Export { module: $m, name: stringify!($name),
                    addr: $name as *const (), status: $crate::Status::$status },
            )*
        ];
    };
    (module $m:literal => $k:literal;
     $( $status:ident fn $name:ident ( $($arg:ident : $ty:ty),* ) $(-> $ret:ty)? $body:block )* ) => {
        $(
            #[allow(non_snake_case, clippy::missing_safety_doc)]
            pub unsafe extern "win64" fn $name($($arg: $ty),*) $(-> $ret)? $body
        )*
        pub static EXPORTS: &[$crate::Export] = &[
            $(
                $crate::Export { module: $m, name: stringify!($name),
                    addr: $name as *const (), status: $crate::Status::$status },
                $crate::Export { module: $k, name: stringify!($name),
                    addr: $name as *const (), status: $crate::Status::$status },
            )*
        ];
    };
}

/// 'API-MS-WIN-CORE-FILE-L1-2-0.dll' -> 'api-ms-win-core-file' (same rules as tools/import_map.py).
pub fn normalize_module(name: &str) -> String {
    let mut n = name.to_ascii_lowercase();
    if let Some(s) = n.strip_suffix(".dll") {
        n = s.to_string();
    }
    if n.starts_with("api-ms-") || n.starts_with("ext-ms-") {
        let parts: Vec<&str> = n.rsplitn(4, '-').collect();
        let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if parts.len() == 4
            && numeric(parts[0])
            && numeric(parts[1])
            && parts[2].len() > 1
            && parts[2].starts_with('l')
            && numeric(&parts[2][1..])
        {
            n = parts[3].to_string();
        }
    }
    n
}

pub fn lookup(module: &str, name: &str) -> Option<&'static Export> {
    let m = normalize_module(module);
    crate::exports().find(|e| e.module == m && e.name == name)
}

/// Resolves an import by ordinal (only `ws2_32` has an ordinal table) to the named export.
pub fn lookup_ordinal(module: &str, ordinal: u16) -> Option<&'static Export> {
    let m = normalize_module(module);
    let name = match m.as_str() {
        "ws2_32" => crate::ws2_32::ORDINALS.iter().find(|o| o.0 == ordinal)?.1,
        _ => return None,
    };
    lookup(&m, name)
}

/// C ABI for a C/C++ base: returns the function address, or NULL when unknown.
///
/// # Safety
/// `module` and `name` must be NULL or valid NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn nativ64_lookup(module: *const c_char, name: *const c_char) -> *mut c_void {
    if module.is_null() || name.is_null() {
        return std::ptr::null_mut();
    }
    let (Ok(m), Ok(n)) = (
        CStr::from_ptr(module).to_str(),
        CStr::from_ptr(name).to_str(),
    ) else {
        return std::ptr::null_mut();
    };
    lookup(m, n).map_or(std::ptr::null_mut(), |e| e.addr as *mut c_void)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        assert_eq!(
            normalize_module("API-MS-WIN-CORE-FILE-L1-2-0.dll"),
            "api-ms-win-core-file"
        );
        assert_eq!(normalize_module("KERNEL32.DLL"), "kernel32");
        assert_eq!(
            normalize_module("api-ms-win-core-synch"),
            "api-ms-win-core-synch"
        );
    }

    #[test]
    fn lookup_works() {
        let e = lookup("KERNEL32.dll", "GetLastError").unwrap();
        assert_eq!(e.status, Status::Native);
        assert!(lookup("kernel32", "Nope").is_none());
        let p = unsafe {
            nativ64_lookup(
                c"api-ms-win-core-synch-l1-2-0.dll".as_ptr(),
                c"Sleep".as_ptr(),
            )
        };
        assert!(!p.is_null());
        assert!(unsafe { nativ64_lookup(std::ptr::null(), c"x".as_ptr()) }.is_null());
    }
}
