//! `api-ms-win-core-interlocked`: the lock-free singly linked list (`SLIST_HEADER`).
//!
//! The header is 16 bytes, 16-aligned, and is updated with a 128-bit compare-and-swap
//! (`lock cmpxchg16b`; every x86-64-v2 CPU has it, and the process aborts if it does not).
//! Entries (`SLIST_ENTRY`, whose first 8 bytes are the next pointer) must be 16-aligned.
//!
//! Layout, compatible with the Windows x64 `HeaderX64`, so that an all-zero header is an empty
//! list:
//! - low qword: bits 0-15 depth, bits 16-63 sequence number (incremented by every successful
//!   push, pop and flush, which makes the CAS immune to the ABA problem);
//! - high qword: address of the first entry (16-aligned, so the 4 reserved low bits are 0), or 0.
//!
//! The depth wraps at 65536, as on Windows. As on Windows, a pop reads the next pointer of the
//! first entry, which another thread may pop and free at the same time: the memory of entries
//! must stay readable while the list is in use. A NULL or misaligned header is not checked (a
//! misaligned one faults, as on Windows).
//!
//! Functions are registered under their API set and under `kernel32`.

use std::arch::asm;
use std::sync::atomic::{AtomicU64, Ordering};

#[repr(C, align(16))]
pub struct SListHeader {
    low: AtomicU64,
    high: AtomicU64,
}

fn require_cx16() {
    use std::sync::OnceLock;
    static OK: OnceLock<bool> = OnceLock::new();
    if !*OK.get_or_init(|| is_x86_feature_detected!("cmpxchg16b")) {
        eprintln!("nativ64: this CPU lacks cmpxchg16b, required by the SList functions");
        std::process::abort();
    }
}

/// Replaces `(low, high)` by `new` if the header still holds `old`.
unsafe fn cas(h: *mut SListHeader, old: (u64, u64), new: (u64, u64)) -> bool {
    let ok: u8;
    asm!(
        "xchg {nl}, rbx",
        "lock cmpxchg16b [{p}]",
        "xchg {nl}, rbx",
        "sete {ok}",
        p = in(reg) h,
        nl = inout(reg) new.0 => _,
        in("rcx") new.1,
        inout("rax") old.0 => _,
        inout("rdx") old.1 => _,
        ok = out(reg_byte) ok,
        options(nostack),
    );
    ok != 0
}

unsafe fn load(h: *mut SListHeader) -> (u64, u64) {
    // A torn read is harmless: the CAS that follows then fails and the caller retries.
    (
        (*h).low.load(Ordering::Acquire),
        (*h).high.load(Ordering::Acquire),
    )
}

fn next_low(low: u64, delta: i32) -> u64 {
    let depth = (low as u16).wrapping_add(delta as u16) as u64;
    ((low >> 16).wrapping_add(1) << 16) | depth
}

win32! {
    module "api-ms-win-core-interlocked" => "kernel32";
    Native fn InitializeSListHead(h: *mut SListHeader) {
        (*h).low.store(0, Ordering::Release);
        (*h).high.store(0, Ordering::Release);
    }
    Native fn InterlockedPushEntrySList(h: *mut SListHeader, entry: *mut usize) -> *mut usize {
        require_cx16();
        loop {
            let (low, high) = load(h);
            *entry = high as usize;
            if cas(h, (low, high), (next_low(low, 1), entry as u64)) {
                return high as *mut usize;
            }
        }
    }
    Native fn InterlockedPopEntrySList(h: *mut SListHeader) -> *mut usize {
        require_cx16();
        loop {
            let (low, high) = load(h);
            if high == 0 {
                return std::ptr::null_mut();
            }
            let next = *(high as *const u64);
            if cas(h, (low, high), (next_low(low, -1), next)) {
                return high as *mut usize;
            }
        }
    }
    Native fn InterlockedFlushSList(h: *mut SListHeader) -> *mut usize {
        require_cx16();
        loop {
            let (low, high) = load(h);
            if high == 0 {
                return std::ptr::null_mut();
            }
            if cas(h, (low, high), (next_low(low & !0xFFFF, 0), 0)) {
                return high as *mut usize;
            }
        }
    }
    Native fn QueryDepthSList(h: *mut SListHeader) -> u16 {
        (*h).low.load(Ordering::Acquire) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[repr(C, align(16))]
    struct Entry {
        next: usize,
        value: usize,
    }

    fn new_entries(n: usize) -> Vec<Entry> {
        (0..n).map(|i| Entry { next: 0, value: i }).collect()
    }

    #[test]
    fn registered() {
        for n in [
            "InitializeSListHead",
            "InterlockedPushEntrySList",
            "InterlockedPopEntrySList",
            "InterlockedFlushSList",
            "QueryDepthSList",
        ] {
            assert!(crate::lookup("api-ms-win-core-interlocked-l1-1-0.dll", n).is_some());
            assert!(crate::lookup("KERNEL32.dll", n).is_some());
        }
    }

    #[test]
    fn lifo_order_depth_and_flush() {
        unsafe {
            let mut head = SListHeader {
                low: AtomicU64::new(0xdead),
                high: AtomicU64::new(0xbeef),
            };
            InitializeSListHead(&mut head);
            assert_eq!(QueryDepthSList(&mut head), 0);
            assert!(InterlockedPopEntrySList(&mut head).is_null());
            assert!(InterlockedFlushSList(&mut head).is_null());
            let mut es = new_entries(3);
            let p: Vec<*mut usize> = es
                .iter_mut()
                .map(|e| e as *mut Entry as *mut usize)
                .collect();
            assert!(InterlockedPushEntrySList(&mut head, p[0]).is_null());
            assert_eq!(InterlockedPushEntrySList(&mut head, p[1]), p[0]);
            assert_eq!(InterlockedPushEntrySList(&mut head, p[2]), p[1]);
            assert_eq!(QueryDepthSList(&mut head), 3);
            assert_eq!(InterlockedPopEntrySList(&mut head), p[2]);
            assert_eq!(QueryDepthSList(&mut head), 2);
            // Flush returns the chain, first entry = last pushed.
            assert_eq!(InterlockedFlushSList(&mut head), p[1]);
            assert_eq!(es[1].next, p[0] as usize);
            assert_eq!(es[0].next, 0);
            assert_eq!(QueryDepthSList(&mut head), 0);
            assert!(InterlockedPopEntrySList(&mut head).is_null());
            // A zeroed header is an empty list.
            let mut z: SListHeader = std::mem::zeroed();
            assert!(InterlockedPopEntrySList(&mut z).is_null());
            assert!(InterlockedPushEntrySList(&mut z, p[0]).is_null());
            assert_eq!(QueryDepthSList(&mut z), 1);
        }
    }

    #[test]
    fn stress_push_pop() {
        const THREADS: usize = 6;
        const PER: usize = 2000;
        let head = Arc::new(unsafe {
            let mut h: Box<SListHeader> = Box::new(std::mem::zeroed());
            InitializeSListHead(&mut *h);
            h
        });
        let mut all = new_entries(THREADS * PER);
        let ptrs: Vec<usize> = all.iter_mut().map(|e| e as *mut Entry as usize).collect();
        let hp = &**head as *const SListHeader as usize;
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let mine: Vec<usize> = ptrs[t * PER..(t + 1) * PER].to_vec();
                std::thread::spawn(move || unsafe {
                    let h = hp as *mut SListHeader;
                    let mut popped = Vec::new();
                    for (i, p) in mine.iter().enumerate() {
                        InterlockedPushEntrySList(h, *p as *mut usize);
                        if i % 2 == 1 {
                            let e = InterlockedPopEntrySList(h);
                            assert!(!e.is_null());
                            popped.push(e as usize);
                        }
                    }
                    popped
                })
            })
            .collect();
        let mut seen: Vec<usize> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        unsafe {
            let mut e = InterlockedFlushSList(hp as *mut SListHeader) as *const usize;
            while !e.is_null() {
                seen.push(e as usize);
                e = *e as *const usize;
            }
            assert_eq!(QueryDepthSList(hp as *mut SListHeader), 0);
        }
        // Every entry came out exactly once.
        seen.sort();
        let mut want = ptrs.clone();
        want.sort();
        assert_eq!(seen, want);
        drop(all);
    }
}
