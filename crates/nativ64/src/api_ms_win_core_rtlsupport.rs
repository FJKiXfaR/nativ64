//! `api-ms-win-core-rtlsupport`: `RtlPcToFileHeader`, the image containing an address.
//!
//! Images are the ranges registered with [`crate::process::register_image_range`] (the main
//! image and the guest DLLs). An address outside every registered image gives `NULL`, as on
//! Windows. The unwinding functions of this API set (`RtlLookupFunctionEntry`,
//! `RtlVirtualUnwind`, ...) are not implemented here.

use crate::process::image_base_of;

win32! {
    module "api-ms-win-core-rtlsupport";
    // Returns the base of the image holding `pc` (and stores it in `*base` when `base` is not
    // NULL), or NULL (also stored) when `pc` is in no image.
    Native fn RtlPcToFileHeader(pc: usize, base: *mut usize) -> usize {
        let b = image_base_of(pc).unwrap_or(0);
        if !base.is_null() {
            *base = b;
        }
        b
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
