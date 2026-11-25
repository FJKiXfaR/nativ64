//! `api-ms-win-core-rtlsupport`: `RtlPcToFileHeader`, the image containing an address, and the
//! unwinding support (`RtlLookupFunctionEntry`, `RtlVirtualUnwind`, `RtlCaptureContext`,
//! `RtlRestoreContext`, `RtlUnwindEx`, `RtlUnwind`, `RtlAddFunctionTable`, `RtlDeleteFunctionTable`); the logic is in
//! [`crate::seh`].
//!
//! Images are the ranges registered with [`crate::process::register_image_range`] (the main
//! image and the guest DLLs). An address outside every registered image gives `NULL`, as on
//! Windows.

use crate::process::image_base_of;
use crate::seh;
use crate::{Export, Status};

/// The assembly functions, which cannot go through `win32!` (a Rust wrapper would change
/// the frame they capture). Also known under `kernel32`.
pub static ASM_EXPORTS: &[Export] = &[
    Export {
        module: "api-ms-win-core-rtlsupport",
        name: "RtlCaptureContext",
        addr: seh::nativ64_rtl_capture_context as *const (),
        status: Status::Native,
    },
    Export {
        module: "kernel32",
        name: "RtlCaptureContext",
        addr: seh::nativ64_rtl_capture_context as *const (),
        status: Status::Native,
    },
    Export {
        module: "api-ms-win-core-rtlsupport",
        name: "RtlRestoreContext",
        addr: seh::nativ64_rtl_restore_context as *const (),
        status: Status::Native,
    },
    Export {
        module: "kernel32",
        name: "RtlRestoreContext",
        addr: seh::nativ64_rtl_restore_context as *const (),
        status: Status::Native,
    },
    Export {
        module: "api-ms-win-core-rtlsupport",
        name: "RtlUnwindEx",
        addr: seh::nativ64_rtl_unwind_ex as *const (),
        status: Status::Native,
    },
    Export {
        module: "kernel32",
        name: "RtlUnwindEx",
        addr: seh::nativ64_rtl_unwind_ex as *const (),
        status: Status::Native,
    },
    Export {
        module: "api-ms-win-core-rtlsupport",
        name: "RtlUnwind",
        addr: seh::nativ64_rtl_unwind as *const (),
        status: Status::Native,
    },
    Export {
        module: "kernel32",
        name: "RtlUnwind",
        addr: seh::nativ64_rtl_unwind as *const (),
        status: Status::Native,
    },
];

win32! {
    module "api-ms-win-core-rtlsupport" => "kernel32";
    // Returns the base of the image holding `pc` (and stores it in `*base` when `base` is not
    // NULL), or NULL (also stored) when `pc` is in no image.
    Native fn RtlPcToFileHeader(pc: usize, base: *mut usize) -> usize {
        let b = image_base_of(pc).unwrap_or(0);
        if !base.is_null() {
            *base = b;
        }
        b
    }
    // The function table entry covering `pc` (NULL for a leaf function or an unknown address);
    // `*base` receives the image base. The history table is ignored.
    Native fn RtlLookupFunctionEntry(pc: usize, base: *mut usize, _history: usize) -> usize {
        let (b, entry) = seh::lookup_function_entry(pc);
        if !base.is_null() {
            *base = b;
        }
        entry
    }
    // Moves `ctx` to the caller's frame; returns the language handler of the frame when it has
    // one of `handler_type` (1 = exception, 2 = unwind). A malformed unwind info returns NULL
    // and leaves `ctx` unchanged.
    Native fn RtlVirtualUnwind(
        handler_type: u32,
        base: usize,
        pc: usize,
        entry: *const seh::RuntimeFunction,
        ctx: *mut u8,
        handler_data: *mut usize,
        establisher: *mut usize,
        ptrs: *mut usize
    ) -> usize {
        match seh::virtual_unwind(handler_type, base, pc, entry, ctx, ptrs) {
            Ok(u) => {
                if !handler_data.is_null() && u.handler != 0 {
                    *handler_data = u.handler_data;
                }
                if !establisher.is_null() {
                    *establisher = u.establisher;
                }
                u.handler
            }
            Err(_) => 0,
        }
    }
    Native fn RtlAddFunctionTable(table: usize, count: u32, base: usize) -> u8 {
        seh::add_function_table(table, count, base) as u8
    }
    Native fn RtlDeleteFunctionTable(table: usize) -> u8 {
        seh::delete_function_table(table) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::register_image_range;

    #[test]
    fn registered() {
        assert!(
            crate::lookup("api-ms-win-core-rtlsupport-l1-1-0.dll", "RtlPcToFileHeader").is_some()
        );
    }

    #[test]
    fn finds_the_image_of_an_address() {
        register_image_range(0x7000_0000_0000, 0x1000);
        register_image_range(0x7000_0001_0000, 0x2000);
        unsafe {
            let mut b = 1usize;
            assert_eq!(
                RtlPcToFileHeader(0x7000_0000_0000, &mut b),
                0x7000_0000_0000
            );
            assert_eq!(b, 0x7000_0000_0000);
            assert_eq!(
                RtlPcToFileHeader(0x7000_0001_1fff, &mut b),
                0x7000_0001_0000
            );
            assert_eq!(b, 0x7000_0001_0000);
        }
    }

    #[test]
    fn address_outside_every_image() {
        register_image_range(0x7000_0000_0000, 0x1000);
        unsafe {
            let mut b = 1usize;
            assert_eq!(RtlPcToFileHeader(0x7000_0000_1000, &mut b), 0);
            assert_eq!(b, 0);
            assert_eq!(RtlPcToFileHeader(0x10, &mut b), 0);
            // NULL out pointer is allowed
            assert_eq!(
                RtlPcToFileHeader(0x7000_0000_0800, std::ptr::null_mut()),
                0x7000_0000_0000
            );
        }
    }
}
