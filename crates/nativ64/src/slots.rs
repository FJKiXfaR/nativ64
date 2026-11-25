//! Indexed per-thread storage shared by TLS (`TlsAlloc`...) and FLS (`FlsAlloc`...).
//!
//! A [`SlotTable`] is the process-wide allocator of indices. Each thread keeps its own values in a
//! [`Values`] (one `thread_local!` per table). Reads are lock-free; allocation and release take a
//! mutex. Every slot has a state `generation << 1 | in_use`; a value remembers the state it was
//! stored under, so freeing a slot makes the values of all threads read as NULL, and a reallocated
//! index starts at NULL everywhere (as on Windows). Callbacks (FLS) run when a slot is freed, for
//! the current thread's value only, and when a thread exits, for its remaining non-NULL values.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread::LocalKey;

/// Capacity of the arrays (the largest limit of the two tables, FLS).
pub const CAPACITY: usize = 4080;

pub type Local = LocalKey<RefCell<Values>>;

pub struct SlotTable {
    limit: usize,
    lock: Mutex<()>,
    state: [AtomicU64; CAPACITY],
    callback: [AtomicUsize; CAPACITY],
}

impl SlotTable {
    pub const fn new(limit: usize) -> Self {
        assert!(limit <= CAPACITY);
        SlotTable {
            limit,
            lock: Mutex::new(()),
            state: [const { AtomicU64::new(0) }; CAPACITY],
            callback: [const { AtomicUsize::new(0) }; CAPACITY],
        }
    }

    /// Allocates the lowest free index, or `None` when the table is full.
    pub fn alloc(&self, callback: usize) -> Option<u32> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let i = (0..self.limit).find(|&i| self.state[i].load(Ordering::Relaxed) & 1 == 0)?;
        self.callback[i].store(callback, Ordering::Relaxed);
        self.state[i].fetch_or(1, Ordering::Release);
        Some(i as u32)
    }

    fn current(&self, idx: u32) -> Option<u64> {
        let i = idx as usize;
        let s = (i < self.limit).then(|| self.state[i].load(Ordering::Acquire))?;
        (s & 1 == 1).then_some(s)
    }

    /// Releases `idx`; runs the callback on the calling thread's value. False if not allocated.
    pub fn free(&'static self, idx: u32, local: &'static Local) -> bool {
        let i = idx as usize;
        let cb;
        let old;
        {
            let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
            let Some(s) = self.current(idx) else {
                return false;
            };
            old = s;
            cb = self.callback[i].swap(0, Ordering::Relaxed);
            self.state[i].store(((s >> 1) + 1) << 1, Ordering::Release);
        }
        let val = local.try_with(|v| v.borrow_mut().take(i, old)).unwrap_or(0);
        if cb != 0 && val != 0 {
            // SAFETY: the callback was supplied as an `extern "win64" fn(*mut c_void)`.
            let f: unsafe extern "win64" fn(*mut std::ffi::c_void) =
                unsafe { std::mem::transmute(cb) };
            unsafe { f(val as *mut _) };
        }
        true
    }

    /// The calling thread's value, or `None` when `idx` is not allocated.
    pub fn get(&self, idx: u32, local: &'static Local) -> Option<usize> {
        let s = self.current(idx)?;
        Some(
            local
                .try_with(|v| v.borrow().get(idx as usize, s))
                .unwrap_or(0),
        )
    }

    /// Sets the calling thread's value. False when `idx` is not allocated.
    pub fn set(&self, idx: u32, val: usize, local: &'static Local) -> bool {
        let Some(s) = self.current(idx) else {
            return false;
        };
        local
            .try_with(|v| v.borrow_mut().put(idx as usize, s, val))
            .is_ok()
    }
}

/// One thread's values for one table.
pub struct Values {
    table: &'static SlotTable,
    v: Vec<(u64, usize)>,
}

impl Values {
    pub const fn new(table: &'static SlotTable) -> Self {
        Values {
            table,
            v: Vec::new(),
        }
    }
    fn get(&self, i: usize, state: u64) -> usize {
        match self.v.get(i) {
            Some(&(s, val)) if s == state => val,
            _ => 0,
        }
    }
    fn take(&mut self, i: usize, state: u64) -> usize {
        match self.v.get_mut(i) {
            Some(e) if e.0 == state => std::mem::take(&mut e.1),
            _ => 0,
        }
    }
    fn put(&mut self, i: usize, state: u64, val: usize) {
        if self.v.len() <= i {
            self.v.resize(i + 1, (0, 0));
        }
        self.v[i] = (state, val);
    }
}

impl Drop for Values {
    fn drop(&mut self) {
        // Thread exit: callbacks for the values still set in slots still allocated.
        for (i, (s, val)) in std::mem::take(&mut self.v).into_iter().enumerate() {
            let cb = self.table.callback[i].load(Ordering::Relaxed);
            if val != 0 && cb != 0 && self.table.state[i].load(Ordering::Acquire) == s {
                let f: unsafe extern "win64" fn(*mut std::ffi::c_void) =
                    unsafe { std::mem::transmute(cb) };
                unsafe { f(val as *mut _) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static T: SlotTable = SlotTable::new(3);
    thread_local! {
        static V: RefCell<Values> = const { RefCell::new(Values::new(&T)) };
    }

    #[test]
    fn full_table_and_reuse() {
        let a = T.alloc(0).unwrap();
        let b = T.alloc(0).unwrap();
        let c = T.alloc(0).unwrap();
        assert_eq!((a, b, c), (0, 1, 2));
        assert_eq!(T.alloc(0), None);
        assert!(T.set(b, 42, &V));
        assert_eq!(T.get(b, &V), Some(42));
        assert!(T.free(b, &V));
        assert!(!T.free(b, &V));
        assert_eq!(T.get(b, &V), None);
        assert!(!T.set(b, 1, &V));
        assert_eq!(T.alloc(0), Some(b));
        assert_eq!(T.get(b, &V), Some(0));
        assert_eq!(T.get(3, &V), None);
    }
}
