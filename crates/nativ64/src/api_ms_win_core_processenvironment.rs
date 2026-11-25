use crate::api_ms_win_crt_runtime::full_command_line;
use std::ffi::c_char;

win32! {
    module "api-ms-win-core-processenvironment" => "kernel32";
    // The whole command line, program name included, as the CRT startup sees it.
    Native fn GetCommandLineA() -> *const c_char {
        full_command_line().0
    }
    Native fn GetCommandLineW() -> *const u16 {
        full_command_line().1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_and_wide_agree() {
        unsafe {
            let a = std::ffi::CStr::from_ptr(GetCommandLineA())
                .to_str()
                .unwrap();
            let w = GetCommandLineW();
            let mut v = Vec::new();
            let mut i = 0;
            while *w.add(i) != 0 {
                v.push(*w.add(i));
                i += 1;
            }
            assert_eq!(String::from_utf16(&v).unwrap(), a);
            assert_eq!(GetCommandLineA(), GetCommandLineA());
        }
    }
}
