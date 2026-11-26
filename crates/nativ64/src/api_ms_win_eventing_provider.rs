//! `api-ms-win-eventing-provider` (also `advapi32`): event tracing for Windows providers. There
//! is no tracing session on the host, so registering succeeds with a handle, nothing is enabled
//! and every event written is dropped.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::com::Guid;

const ERROR_SUCCESS: u32 = 0;
const ERROR_INVALID_PARAMETER: u32 = 87;

/// Registration handles: non-zero, never reused.
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

win32! {
    module "api-ms-win-eventing-provider" => "advapi32";
    // (provider id, enable callback, callback context, registration handle out)
    Native fn EventRegister(provider: *const Guid, _callback: *const std::ffi::c_void,
                            _context: *const std::ffi::c_void, handle: *mut u64) -> u32 {
        if provider.is_null() || handle.is_null() {
            return ERROR_INVALID_PARAMETER;
        }
        *handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        ERROR_SUCCESS
    }
    Native fn EventUnregister(_handle: u64) -> u32 {
        ERROR_SUCCESS
    }
    // (handle, descriptor, activity id, related activity id, data count, data)
    Native fn EventWriteTransfer(_handle: u64, _descriptor: *const std::ffi::c_void,
                                 _activity: *const Guid, _related: *const Guid, _count: u32,
                                 _data: *const std::ffi::c_void) -> u32 {
        ERROR_SUCCESS
    }
    // (handle, information class, information, size)
    Native fn EventSetInformation(_handle: u64, _class: u32, _info: *const std::ffi::c_void,
                                  _size: u32) -> u32 {
        ERROR_SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookup;

    #[test]
    fn registering_gives_distinct_handles() {
        unsafe {
            let g = Guid::new(1, 2, 3, [4; 8]);
            let np = std::ptr::null();
            let (mut a, mut b) = (0, 0);
            assert_eq!(EventRegister(&g, np, np, &mut a), 0);
            assert_eq!(EventRegister(&g, np, np, &mut b), 0);
            assert!(a != 0 && a != b);
            assert_eq!(
                EventRegister(np.cast(), np, np, &mut a),
                ERROR_INVALID_PARAMETER
            );
            assert_eq!(
                EventRegister(&g, np, np, std::ptr::null_mut()),
                ERROR_INVALID_PARAMETER
            );
            assert_eq!(EventWriteTransfer(a, np, np.cast(), np.cast(), 0, np), 0);
            assert_eq!(EventSetInformation(a, 1, np, 0), 0);
            assert_eq!(EventUnregister(a), 0);
        }
    }

    #[test]
    fn exports_resolve_under_both_names() {
        for n in [
            "EventRegister",
            "EventUnregister",
            "EventWriteTransfer",
            "EventSetInformation",
        ] {
            assert!(lookup("ADVAPI32.dll", n).is_some(), "{n}");
            assert!(
                lookup("api-ms-win-eventing-provider-l1-1-0.dll", n).is_some(),
                "{n}"
            );
        }
    }
}
