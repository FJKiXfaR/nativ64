//! `api-ms-win-core-psm-appnotify`: app state (suspend/resume) and constrained-mode change
//! notifications.
//!
//! A desktop process is never suspended by the process state manager, so a registration is only
//! recorded: the routine is never called. `RegisterAppStateChangeNotification` hands out an
//! opaque non-NULL token (the app-constrained pair behaves the same) and returns `ERROR_SUCCESS`; `UnregisterAppStateChangeNotification`
//! forgets it (an unknown token is ignored). No Wine oracle (Wine does not implement them).

use std::ffi::c_void;
use std::sync::Mutex;

const ERROR_SUCCESS: u32 = 0;
const ERROR_INVALID_PARAMETER: u32 = 87;

/// Live registrations (tokens start at 0x1000 and step by 0x10: never dereferenced).
static REGISTRATIONS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0x1000);

/// Number of live registrations.
pub fn registration_count() -> usize {
    REGISTRATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len()
}

fn register(routine: *const c_void, registration: *mut usize) -> u32 {
    if routine.is_null() || registration.is_null() {
        return ERROR_INVALID_PARAMETER;
    }
    let t = NEXT.fetch_add(0x10, std::sync::atomic::Ordering::SeqCst);
    REGISTRATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(t);
    unsafe { *registration = t };
    ERROR_SUCCESS
}

fn unregister(registration: usize) {
    REGISTRATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|&t| t != registration);
}

win32! {
    module "api-ms-win-core-psm-appnotify";
    Native fn RegisterAppStateChangeNotification(routine: *const c_void, _context: *const c_void,
                                                 registration: *mut usize) -> u32 {
        register(routine, registration)
    }
    Native fn UnregisterAppStateChangeNotification(registration: usize) {
        unregister(registration)
    }
    Native fn RegisterAppConstrainedChangeNotification(routine: *const c_void,
                                                       _context: *const c_void,
                                                       registration: *mut usize) -> u32 {
        register(routine, registration)
    }
    Native fn UnregisterAppConstrainedChangeNotification(registration: usize) {
        unregister(registration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_unregister() {
        unsafe {
            let f = std::ptr::dangling::<c_void>();
            let (mut a, mut b) = (0usize, 0usize);
            assert_eq!(
                RegisterAppStateChangeNotification(std::ptr::null(), f, &mut a),
                ERROR_INVALID_PARAMETER
            );
            assert_eq!(RegisterAppStateChangeNotification(f, f, &mut a), 0);
            assert_eq!(RegisterAppStateChangeNotification(f, f, &mut b), 0);
            assert!(a != 0 && b != 0 && a != b);
            UnregisterAppStateChangeNotification(a);
            UnregisterAppStateChangeNotification(a);
            assert!(REGISTRATIONS.lock().unwrap().contains(&b));
            assert!(!REGISTRATIONS.lock().unwrap().contains(&a));
            assert_eq!(RegisterAppConstrainedChangeNotification(f, f, &mut a), 0);
            UnregisterAppConstrainedChangeNotification(a);
            UnregisterAppStateChangeNotification(b);
        }
    }
}
