//! `api-ms-win-core-handle`: closing and duplicating handles of the table in `handles.rs`.

use crate::api_ms_win_core_errorhandling::set_last_error;
use crate::handles::{self, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER};

/// `GetCurrentProcess()`: the only process a handle can be duplicated within or to.
const CURRENT_PROCESS: usize = usize::MAX;

win32! {
    module "api-ms-win-core-handle" => "kernel32";
    Native fn CloseHandle(h: usize) -> i32 { handles::close_handle(h) }

    // Same-process only; access rights and inheritance are not modelled (every duplicate has
    // full access), so `DUPLICATE_SAME_ACCESS` and the access mask make no difference.
    Native fn DuplicateHandle(src_process: usize, src: usize, dst_process: usize, dst: *mut usize,
                              _access: u32, _inherit: i32, options: u32) -> i32 {
        if src_process != CURRENT_PROCESS || dst_process != CURRENT_PROCESS {
            set_last_error(ERROR_INVALID_HANDLE);
            return 0;
        }
        let close = options & handles::DUPLICATE_CLOSE_SOURCE != 0;
        if dst.is_null() && !close {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        match handles::duplicate_handle(src, close) {
            Some(h) => {
                if !dst.is_null() {
                    *dst = h;
                }
                1
            }
            None => 0,
        }
    }
}
