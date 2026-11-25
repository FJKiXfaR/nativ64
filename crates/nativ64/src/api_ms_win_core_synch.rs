//! `api-ms-win-core-synch`: lightweight synchronization primitives on Linux futexes.
//!
//! The game allocates these structures itself, so layouts match Windows x64 exactly and a
//! zero-initialised `SRWLOCK`, `CONDITION_VARIABLE` or `INIT_ONCE` is valid. Nothing allocates:
//! all state lives in the caller's memory (atomics over its address) plus one static table.
//! Kernel objects (events, mutexes, semaphores, `WaitFor*`) live in `handles.rs`; their exports
//! are declared below.
//!
//! * `SRWLOCK` (one pointer): bit 0 writer held, bit 1 sleepers present, bit 2 a writer is
//!   waiting, bits 3.. reader count. Futex on the low 32 bits. Writer policy: once a writer
//!   waits, new readers block until it has run, so writers cannot starve; releases wake all
//!   sleepers, who re-evaluate (simple, no queue, no FIFO guarantee). Not recursive.
//! * `CRITICAL_SECTION` (40 bytes): `DebugInfo` (0, unused), `LockCount` i32 (futex word:
//!   -1 free, 0 held, 1 held with sleepers; Windows also uses -1 for free), `RecursionCount`
//!   i32, `OwningThread` (thread id as a handle value), `LockSemaphore` (0, unused),
//!   `SpinCount`. Recursive; a release by a non-owner is a `debug_assert`.
//! * `CONDITION_VARIABLE` (one pointer): a sequence counter in the low 32 bits; a waiter reads
//!   it before unlocking, sleeps while unchanged; wakes bump it. Spurious wakeups are possible.
//! * `INIT_ONCE` (one pointer): 0 new, 1 running, 5 running with waiters, completed is
//!   `(context & !3) | 2` (Windows reserves the low 2 bits of the context too).
//! * `WaitOnAddress`: a global table of 256 futex sequence counters hashed by address. Waiters
//!   snapshot the counter, compare the memory (1/2/4/8 bytes) and sleep; wakers bump it and wake
//!   the whole bucket, so `WakeByAddressSingle` may wake more than one waiter (and waiters of an
//!   aliasing address); spurious wakeups are allowed by the API, callers re-check.
//!
//! Timeouts: `FALSE` with last error `ERROR_TIMEOUT` (1460); `INFINITE` is `0xFFFFFFFF`.

use crate::api_ms_win_core_errorhandling::set_last_error;
use crate::handles;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicUsize, Ordering::*};
use std::time::Duration;

const ERROR_TIMEOUT: u32 = 1460;
const ERROR_INVALID_PARAMETER: u32 = 87;
const INFINITE: u32 = 0xFFFF_FFFF;
const CONDITION_VARIABLE_LOCKMODE_SHARED: u32 = 1;

type Bool = i32;
type Boolean = u8;

#[derive(PartialEq)]
pub(crate) enum Wait {
    Woken,
    TimedOut,
}

/// Sleeps while `*addr == expected` (relative timeout). EAGAIN/EINTR count as woken.
pub(crate) fn futex_wait(addr: &AtomicU32, expected: u32, timeout: Option<Duration>) -> Wait {
    let ts = timeout.map(|d| libc::timespec {
        tv_sec: d.as_secs().min(i64::MAX as u64 / 2) as libc::time_t,
        tv_nsec: d.subsec_nanos() as _,
    });
    let tsp = ts.as_ref().map_or(std::ptr::null(), |t| t as *const _);
    let r = unsafe {
        libc::syscall(
            libc::SYS_futex,
            addr.as_ptr(),
            libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
            expected,
            tsp,
            std::ptr::null::<u32>(),
            0u32,
        )
    };
    if r == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ETIMEDOUT) {
        Wait::TimedOut
    } else {
        Wait::Woken
    }
}

pub(crate) fn futex_wake(addr: &AtomicU32, count: i32) {
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            addr.as_ptr(),
            libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
            count,
        );
    }
}

pub(crate) fn timeout_of(ms: u32) -> Option<Duration> {
    (ms != INFINITE).then(|| Duration::from_millis(ms as u64))
}

fn tid() -> usize {
    unsafe { libc::syscall(libc::SYS_gettid) as usize }
}

// ---------------------------------------------------------------- SRW locks

const W_HELD: usize = 1;
const SLEEPERS: usize = 2;
const W_WAIT: usize = 4;
const READER: usize = 8;

/// # Safety
/// `p` must point to a valid, aligned pointer-sized word.
unsafe fn srw<'a>(p: *mut usize) -> &'a AtomicUsize {
    AtomicUsize::from_ptr(p)
}

/// Futex view of the low 32 bits (little-endian x86-64).
fn low32(a: &AtomicUsize) -> &AtomicU32 {
    unsafe { &*(a.as_ptr() as *const AtomicU32) }
}

fn srw_lock_exclusive(a: &AtomicUsize) {
    loop {
        let s = a.load(Relaxed);
        if s & !(SLEEPERS | W_WAIT) == 0 {
            // Acquire; clear the waiting-writer mark (other writers re-set it when they retry).
            if a.compare_exchange_weak(s, W_HELD | (s & SLEEPERS), Acquire, Relaxed)
                .is_ok()
            {
                return;
            }
            continue;
        }
        let n = s | SLEEPERS | W_WAIT;
        if n == s || a.compare_exchange(s, n, Relaxed, Relaxed).is_ok() {
            futex_wait(low32(a), n as u32, None);
        }
    }
}

fn srw_lock_shared(a: &AtomicUsize) {
    loop {
        let s = a.load(Relaxed);
        if s & (W_HELD | W_WAIT) == 0 {
            if a.compare_exchange_weak(s, s + READER, Acquire, Relaxed)
                .is_ok()
            {
                return;
            }
            continue;
        }
        let n = s | SLEEPERS;
        if n == s || a.compare_exchange(s, n, Relaxed, Relaxed).is_ok() {
            futex_wait(low32(a), n as u32, None);
        }
    }
}

fn srw_unlock_exclusive(a: &AtomicUsize) {
    let old = a.fetch_and(!(W_HELD | W_WAIT | SLEEPERS), Release);
    debug_assert!(old & W_HELD != 0, "ReleaseSRWLockExclusive on a free lock");
    if old & SLEEPERS != 0 {
        futex_wake(low32(a), i32::MAX);
    }
}

fn srw_unlock_shared(a: &AtomicUsize) {
    loop {
        let s = a.load(Relaxed);
        debug_assert!(s >= READER, "ReleaseSRWLockShared without a reader");
        let mut n = s - READER;
        let last = n < READER;
        if last {
            n &= !(SLEEPERS | W_WAIT);
        }
        if a.compare_exchange_weak(s, n, Release, Relaxed).is_ok() {
            if last && s & SLEEPERS != 0 {
                futex_wake(low32(a), i32::MAX);
            }
            return;
        }
    }
}

fn srw_try_exclusive(a: &AtomicUsize) -> bool {
    let mut s = a.load(Relaxed);
    while s & !(SLEEPERS | W_WAIT) == 0 {
        match a.compare_exchange_weak(s, W_HELD | (s & SLEEPERS), Acquire, Relaxed) {
            Ok(_) => return true,
            Err(x) => s = x,
        }
    }
    false
}

fn srw_try_shared(a: &AtomicUsize) -> bool {
    let mut s = a.load(Relaxed);
    while s & (W_HELD | W_WAIT) == 0 {
        match a.compare_exchange_weak(s, s + READER, Acquire, Relaxed) {
            Ok(_) => return true,
            Err(x) => s = x,
        }
    }
    false
}

// ---------------------------------------------------------------- critical sections

#[repr(C)]
pub struct CriticalSection {
    debug_info: usize,
    lock_count: AtomicI32,
    recursion_count: AtomicI32,
    owning_thread: AtomicUsize,
    lock_semaphore: usize,
    spin_count: AtomicUsize,
}

const _: () = assert!(std::mem::size_of::<CriticalSection>() == 40);

const CS_FREE: i32 = -1;

impl CriticalSection {
    fn futex(&self) -> &AtomicU32 {
        unsafe { &*(self.lock_count.as_ptr() as *const AtomicU32) }
    }

    fn init(&self, spin: u32) {
        self.lock_count.store(CS_FREE, Relaxed);
        self.recursion_count.store(0, Relaxed);
        self.owning_thread.store(0, Relaxed);
        self.spin_count
            .store((spin & 0x00FF_FFFF) as usize, Relaxed);
    }

    fn try_lock(&self) -> bool {
        self.lock_count
            .compare_exchange(CS_FREE, 0, Acquire, Relaxed)
            .is_ok()
    }

    fn enter(&self) {
        let me = tid();
        if self.owning_thread.load(Relaxed) == me {
            self.recursion_count.fetch_add(1, Relaxed);
            return;
        }
        let mut spins = self.spin_count.load(Relaxed);
        while spins > 0 && !self.try_lock() {
            std::hint::spin_loop();
            spins -= 1;
        }
        if spins == 0 && !self.try_lock() {
            // Mark "held with sleepers" (1); we own it when the swap saw it free.
            while self.lock_count.swap(1, Acquire) != CS_FREE {
                futex_wait(self.futex(), 1, None);
            }
        }
        self.owning_thread.store(me, Relaxed);
        self.recursion_count.store(1, Relaxed);
    }

    fn try_enter(&self) -> bool {
        let me = tid();
        if self.owning_thread.load(Relaxed) == me {
            self.recursion_count.fetch_add(1, Relaxed);
            return true;
        }
        if self.try_lock() {
            self.owning_thread.store(me, Relaxed);
            self.recursion_count.store(1, Relaxed);
            return true;
        }
        false
    }

    fn leave(&self) {
        debug_assert_eq!(
            self.owning_thread.load(Relaxed),
            tid(),
            "LeaveCriticalSection by a thread that does not own it"
        );
        if self.recursion_count.fetch_sub(1, Relaxed) > 1 {
            return;
        }
        self.owning_thread.store(0, Relaxed);
        if self.lock_count.swap(CS_FREE, Release) == 1 {
            futex_wake(self.futex(), 1);
        }
    }
}

// ---------------------------------------------------------------- condition variables

/// # Safety
/// `p` must point to a valid, aligned pointer-sized word.
unsafe fn cond<'a>(p: *mut usize) -> &'a AtomicU32 {
    &*(p as *const AtomicU32)
}

/// Sleeps on the sequence number read before `unlock` ran; `relock` always runs afterwards.
fn cond_sleep(c: &AtomicU32, ms: u32, unlock: impl FnOnce(), relock: impl FnOnce()) -> Bool {
    let seq = c.load(SeqCst);
    unlock();
    let r = futex_wait(c, seq, timeout_of(ms));
    relock();
    if r == Wait::TimedOut {
        set_last_error(ERROR_TIMEOUT);
        0
    } else {
        1
    }
}

// ---------------------------------------------------------------- INIT_ONCE

const ONCE_RUNNING: usize = 1;
const ONCE_WAITERS: usize = 5;
const ONCE_DONE: usize = 2;

pub type InitOnceFn = unsafe extern "win64" fn(*mut usize, *mut (), *mut *mut ()) -> Bool;

// ---------------------------------------------------------------- WaitOnAddress

const TABLE: usize = 256;
static ADDR_TABLE: [AtomicU32; TABLE] = [const { AtomicU32::new(0) }; TABLE];

fn bucket(addr: *const ()) -> &'static AtomicU32 {
    let h = (addr as usize >> 2).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56;
    &ADDR_TABLE[h % TABLE]
}

unsafe fn load_sized(p: *const u8, size: usize) -> u64 {
    match size {
        1 => std::ptr::read_volatile(p) as u64,
        2 => std::ptr::read_volatile(p as *const u16) as u64,
        4 => std::ptr::read_volatile(p as *const u32) as u64,
        _ => std::ptr::read_volatile(p as *const u64),
    }
}

win32! {
    module "api-ms-win-core-synch" => "kernel32";
    Native fn Sleep(milliseconds: u32) {
        if milliseconds == 0 {
            libc::sched_yield();
        } else {
            std::thread::sleep(std::time::Duration::from_millis(milliseconds as u64));
        }
    }

    Native fn InitializeSRWLock(lock: *mut usize) { srw(lock).store(0, Relaxed) }
    Native fn AcquireSRWLockExclusive(lock: *mut usize) { srw_lock_exclusive(srw(lock)) }
    Native fn AcquireSRWLockShared(lock: *mut usize) { srw_lock_shared(srw(lock)) }
    Native fn ReleaseSRWLockExclusive(lock: *mut usize) { srw_unlock_exclusive(srw(lock)) }
    Native fn ReleaseSRWLockShared(lock: *mut usize) { srw_unlock_shared(srw(lock)) }
    Native fn TryAcquireSRWLockExclusive(lock: *mut usize) -> Boolean {
        srw_try_exclusive(srw(lock)) as Boolean
    }
    Native fn TryAcquireSRWLockShared(lock: *mut usize) -> Boolean {
        srw_try_shared(srw(lock)) as Boolean
    }

    Native fn InitializeCriticalSection(cs: *mut CriticalSection) { (*cs).init(0) }
    Native fn InitializeCriticalSectionAndSpinCount(cs: *mut CriticalSection, spin: u32) -> Bool {
        (*cs).init(spin);
        1
    }
    Native fn InitializeCriticalSectionEx(cs: *mut CriticalSection, spin: u32, _flags: u32) -> Bool {
        (*cs).init(spin);
        1
    }
    Native fn EnterCriticalSection(cs: *mut CriticalSection) { (*cs).enter() }
    Native fn TryEnterCriticalSection(cs: *mut CriticalSection) -> Bool { (*cs).try_enter() as Bool }
    Native fn LeaveCriticalSection(cs: *mut CriticalSection) { (*cs).leave() }
    Native fn DeleteCriticalSection(cs: *mut CriticalSection) {
        debug_assert_eq!((*cs).owning_thread.load(Relaxed), 0, "deleting a held critical section");
        (*cs).lock_count.store(CS_FREE, Relaxed);
    }

    Native fn InitializeConditionVariable(c: *mut usize) { *c = 0 }
    Native fn SleepConditionVariableCS(c: *mut usize, cs: *mut CriticalSection, ms: u32) -> Bool {
        let cs = &*cs;
        let depth = cs.recursion_count.load(Relaxed).max(1);
        cond_sleep(cond(c), ms,
            || { cs.recursion_count.store(1, Relaxed); cs.leave() },
            || { cs.enter(); cs.recursion_count.store(depth, Relaxed) })
    }
    Native fn SleepConditionVariableSRW(c: *mut usize, lock: *mut usize, ms: u32, flags: u32) -> Bool {
        let a = srw(lock);
        let shared = flags & CONDITION_VARIABLE_LOCKMODE_SHARED != 0;
        cond_sleep(cond(c), ms,
            || if shared { srw_unlock_shared(a) } else { srw_unlock_exclusive(a) },
            || if shared { srw_lock_shared(a) } else { srw_lock_exclusive(a) })
    }
    Native fn WakeConditionVariable(c: *mut usize) {
        let c = cond(c);
        c.fetch_add(1, SeqCst);
        futex_wake(c, 1);
    }
    Native fn WakeAllConditionVariable(c: *mut usize) {
        let c = cond(c);
        c.fetch_add(1, SeqCst);
        futex_wake(c, i32::MAX);
    }

    Native fn InitOnceExecuteOnce(once: *mut usize, f: Option<InitOnceFn>, param: *mut (), context: *mut *mut ()) -> Bool {
        let Some(f) = f else {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        };
        let a = srw(once);
        loop {
            let s = a.load(Acquire);
            if s & ONCE_DONE != 0 {
                if !context.is_null() {
                    *context = (s & !3) as *mut ();
                }
                return 1;
            }
            if s == 0 {
                if a.compare_exchange(0, ONCE_RUNNING, Acquire, Relaxed).is_err() {
                    continue;
                }
                let mut ctx: *mut () = std::ptr::null_mut();
                let ok = f(once, param, &mut ctx) != 0 && (ctx as usize) & 3 == 0;
                let new = if ok { (ctx as usize) | ONCE_DONE } else { 0 };
                let old = a.swap(new, Release);
                if old == ONCE_WAITERS {
                    futex_wake(low32(a), i32::MAX);
                }
                if ok {
                    if !context.is_null() {
                        *context = ctx;
                    }
                    return 1;
                }
                if (ctx as usize) & 3 != 0 {
                    set_last_error(ERROR_INVALID_PARAMETER);
                }
                return 0;
            }
            // Running: mark waiters and sleep until the runner finishes.
            if s == ONCE_WAITERS || a.compare_exchange(s, ONCE_WAITERS, Relaxed, Relaxed).is_ok() {
                futex_wait(low32(a), ONCE_WAITERS as u32, None);
            }
        }
    }

    Native fn WaitOnAddress(address: *const u8, compare: *const u8, size: usize, ms: u32) -> Bool {
        if !matches!(size, 1 | 2 | 4 | 8) || address.is_null() || compare.is_null() {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
        let b = bucket(address as *const ());
        let seq = b.load(SeqCst);
        if load_sized(address, size) != load_sized(compare, size) {
            return 1;
        }
        match futex_wait(b, seq, timeout_of(ms)) {
            Wait::TimedOut => {
                set_last_error(ERROR_TIMEOUT);
                0
            }
            Wait::Woken => 1,
        }
    }
    Native fn WakeByAddressSingle(address: *const u8) {
        let b = bucket(address as *const ());
        b.fetch_add(1, SeqCst);
        futex_wake(b, i32::MAX);
    }
    Native fn WakeByAddressAll(address: *const u8) {
        let b = bucket(address as *const ());
        b.fetch_add(1, SeqCst);
        futex_wake(b, i32::MAX);
    }

    Native fn CreateEventW(_attrs: *const (), manual: Bool, initial: Bool, name: *const u16) -> usize {
        handles::create_event(handles::name_w(name), manual != 0, initial != 0)
    }
    Native fn CreateEventA(_attrs: *const (), manual: Bool, initial: Bool, name: *const u8) -> usize {
        handles::create_event(handles::name_a(name), manual != 0, initial != 0)
    }
    Native fn CreateEventExW(_attrs: *const (), name: *const u16, flags: u32, _access: u32) -> usize {
        handles::create_event(handles::name_w(name), flags & 1 != 0, flags & 2 != 0)
    }
    Native fn SetEvent(h: usize) -> Bool { handles::set_event(h) }
    Native fn ResetEvent(h: usize) -> Bool { handles::reset_event(h) }

    Native fn CreateMutexW(_attrs: *const (), owner: Bool, name: *const u16) -> usize {
        handles::create_mutex(handles::name_w(name), owner != 0)
    }
    Native fn CreateMutexA(_attrs: *const (), owner: Bool, name: *const u8) -> usize {
        handles::create_mutex(handles::name_a(name), owner != 0)
    }
    Native fn ReleaseMutex(h: usize) -> Bool { handles::release_mutex(h) }

    Native fn CreateSemaphoreW(_attrs: *const (), initial: i32, max: i32, name: *const u16) -> usize {
        handles::create_semaphore(handles::name_w(name), initial, max)
    }
    Native fn CreateSemaphoreExW(_attrs: *const (), initial: i32, max: i32, name: *const u16, _flags: u32, _access: u32) -> usize {
        handles::create_semaphore(handles::name_w(name), initial, max)
    }
    Native fn ReleaseSemaphore(h: usize, n: i32, previous: *mut i32) -> Bool {
        let (ok, prev) = handles::release_semaphore(h, n);
        if let (false, Some(p)) = (previous.is_null(), prev) {
            *previous = p;
        }
        ok
    }

    Native fn WaitForSingleObject(h: usize, ms: u32) -> u32 { handles::wait(&[h], false, ms) }
    Native fn WaitForSingleObjectEx(h: usize, ms: u32, _alertable: Bool) -> u32 { handles::wait(&[h], false, ms) }
    Native fn WaitForMultipleObjects(count: u32, handles: *const usize, all: Bool, ms: u32) -> u32 {
        handles::wait_multiple(count, handles, all != 0, ms)
    }
    Native fn WaitForMultipleObjectsEx(count: u32, handles: *const usize, all: Bool, ms: u32, _alertable: Bool) -> u32 {
        handles::wait_multiple(count, handles, all != 0, ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_ms_win_core_errorhandling::GetLastError;
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;
    use std::time::Instant;

    /// Test cell handing raw addresses of caller-owned memory to threads.
    struct Mem<T>(std::cell::UnsafeCell<T>);
    unsafe impl<T> Sync for Mem<T> {}
    impl<T> Mem<T> {
        fn new(v: T) -> Arc<Self> {
            Arc::new(Mem(std::cell::UnsafeCell::new(v)))
        }
        fn p(&self) -> *mut T {
            self.0.get()
        }
    }

    /// Non-atomic-style increment: loses updates unless the lock really excludes.
    fn bump(c: &AtomicU64) {
        let v = c.load(Relaxed);
        std::hint::spin_loop();
        c.store(v + 1, Relaxed);
    }

    fn cs_new() -> Arc<Mem<CriticalSection>> {
        let m = Mem::new(unsafe { std::mem::zeroed::<CriticalSection>() });
        unsafe { InitializeCriticalSection(m.p()) };
        m
    }

    #[test]
    fn registry_exports() {
        for n in [
            "InitializeSRWLock",
            "TryAcquireSRWLockShared",
            "EnterCriticalSection",
            "SleepConditionVariableSRW",
            "InitOnceExecuteOnce",
            "WaitOnAddress",
            "WakeByAddressAll",
        ] {
            assert!(crate::lookup("api-ms-win-core-synch-l1-2-0.dll", n).is_some());
            assert!(crate::lookup("KERNEL32.dll", n).is_some());
        }
        unsafe {
            let f: unsafe extern "win64" fn(*mut usize) -> u8 = std::mem::transmute(
                crate::lookup("kernel32", "TryAcquireSRWLockExclusive")
                    .unwrap()
                    .addr,
            );
            let mut l = 0usize;
            assert_eq!(f(&mut l), 1);
            assert_eq!(f(&mut l), 0);
        }
    }

    #[test]
    fn sleeps() {
        let t = std::time::Instant::now();
        unsafe { Sleep(30) };
        assert!(t.elapsed().as_millis() >= 30);
        unsafe { Sleep(0) };
    }

    #[test]
    fn srw_exclusive_contention() {
        let lock = Mem::new(0usize);
        let count = Arc::new(AtomicU64::new(0));
        let hs: Vec<_> = (0..8)
            .map(|_| {
                let (l, c) = (lock.clone(), count.clone());
                std::thread::spawn(move || {
                    for _ in 0..5000 {
                        unsafe {
                            AcquireSRWLockExclusive(l.p());
                            bump(&c);
                            ReleaseSRWLockExclusive(l.p());
                        }
                    }
                })
            })
            .collect();
        hs.into_iter().for_each(|h| h.join().unwrap());
        assert_eq!(count.load(Relaxed), 40000);
        assert_eq!(unsafe { *lock.p() }, 0);
    }

    #[test]
    fn srw_shared_and_try() {
        let lock = Mem::new(0usize);
        unsafe {
            InitializeSRWLock(lock.p());
            AcquireSRWLockShared(lock.p());
            assert_eq!(TryAcquireSRWLockShared(lock.p()), 1);
            assert_eq!(TryAcquireSRWLockExclusive(lock.p()), 0);
            ReleaseSRWLockShared(lock.p());
            ReleaseSRWLockShared(lock.p());
            assert_eq!(TryAcquireSRWLockExclusive(lock.p()), 1);
            assert_eq!(TryAcquireSRWLockShared(lock.p()), 0);
            assert_eq!(TryAcquireSRWLockExclusive(lock.p()), 0);
            ReleaseSRWLockExclusive(lock.p());
            assert_eq!(*lock.p(), 0);
        }
        // Several readers are inside at once; a writer waits for all of them.
        let inside = Arc::new(AtomicU64::new(0));
        let hs: Vec<_> = (0..4)
            .map(|_| {
                let (l, i) = (lock.clone(), inside.clone());
                std::thread::spawn(move || unsafe {
                    AcquireSRWLockShared(l.p());
                    i.fetch_add(1, SeqCst);
                    while i.load(SeqCst) < 4 {
                        std::thread::yield_now();
                    }
                    ReleaseSRWLockShared(l.p());
                })
            })
            .collect();
        hs.into_iter().for_each(|h| h.join().unwrap());
        assert_eq!(inside.load(SeqCst), 4);
        // A waiting writer is not starved by a stream of readers.
        let stop = Arc::new(AtomicU32::new(0));
        let rs: Vec<_> = (0..3)
            .map(|_| {
                let (l, s) = (lock.clone(), stop.clone());
                std::thread::spawn(move || {
                    while s.load(Relaxed) == 0 {
                        unsafe {
                            AcquireSRWLockShared(l.p());
                            std::hint::spin_loop();
                            ReleaseSRWLockShared(l.p());
                        }
                    }
                })
            })
            .collect();
        std::thread::sleep(Duration::from_millis(20));
        unsafe {
            AcquireSRWLockExclusive(lock.p());
            ReleaseSRWLockExclusive(lock.p());
        }
        stop.store(1, Relaxed);
        rs.into_iter().for_each(|h| h.join().unwrap());
    }

    #[test]
    fn critical_section_layout_and_recursion() {
        let cs = cs_new();
        unsafe {
            let base = cs.p() as *const u8;
            let c = &*cs.p();
            assert_eq!(c.lock_count.as_ptr() as *const u8, base.add(8));
            assert_eq!(c.recursion_count.as_ptr() as *const u8, base.add(12));
            assert_eq!(c.owning_thread.as_ptr() as *const u8, base.add(16));
            assert_eq!(c.spin_count.as_ptr() as *const u8, base.add(32));
            assert_eq!(c.lock_count.load(Relaxed), -1);
            EnterCriticalSection(cs.p());
            EnterCriticalSection(cs.p());
            assert_eq!(c.recursion_count.load(Relaxed), 2);
            assert_eq!(c.owning_thread.load(Relaxed), tid());
            let c2 = cs.clone();
            let other = std::thread::spawn(move || TryEnterCriticalSection(c2.p()))
                .join()
                .unwrap();
            assert_eq!(other, 0);
            LeaveCriticalSection(cs.p());
            assert_eq!(c.recursion_count.load(Relaxed), 1);
            LeaveCriticalSection(cs.p());
            assert_eq!(c.owning_thread.load(Relaxed), 0);
            assert_eq!(c.lock_count.load(Relaxed), -1);
            assert_eq!(TryEnterCriticalSection(cs.p()), 1);
            LeaveCriticalSection(cs.p());
            assert_eq!(InitializeCriticalSectionAndSpinCount(cs.p(), 100), 1);
            assert_eq!(InitializeCriticalSectionEx(cs.p(), 0, 0), 1);
            DeleteCriticalSection(cs.p());
        }
    }

    #[test]
    fn critical_section_contention() {
        let cs = cs_new();
        unsafe { InitializeCriticalSectionAndSpinCount(cs.p(), 50) };
        let count = Arc::new(AtomicU64::new(0));
        let hs: Vec<_> = (0..8)
            .map(|_| {
                let (m, c) = (cs.clone(), count.clone());
                std::thread::spawn(move || {
                    for _ in 0..5000 {
                        unsafe {
                            EnterCriticalSection(m.p());
                            EnterCriticalSection(m.p());
                            bump(&c);
                            LeaveCriticalSection(m.p());
                            LeaveCriticalSection(m.p());
                        }
                    }
                })
            })
            .collect();
        hs.into_iter().for_each(|h| h.join().unwrap());
        assert_eq!(count.load(Relaxed), 40000);
    }

    struct Queue {
        cs: CriticalSection,
        cv: usize,
        items: std::collections::VecDeque<u32>,
    }

    #[test]
    fn condition_variable_cs_producer_consumer() {
        let q = Mem::new(Queue {
            cs: unsafe { std::mem::zeroed() },
            cv: 0,
            items: Default::default(),
        });
        unsafe {
            InitializeCriticalSection(&mut (*q.p()).cs);
            InitializeConditionVariable(&mut (*q.p()).cv);
        }
        let qc = q.clone();
        let consumer = std::thread::spawn(move || unsafe {
            let q = qc.p();
            let mut got = Vec::new();
            EnterCriticalSection(&mut (*q).cs);
            while got.len() < 100 {
                while (*q).items.is_empty() {
                    assert_eq!(
                        SleepConditionVariableCS(&mut (*q).cv, &mut (*q).cs, INFINITE),
                        1
                    );
                }
                got.push((*q).items.pop_front().unwrap());
            }
            LeaveCriticalSection(&mut (*q).cs);
            got
        });
        for i in 0..100 {
            unsafe {
                let q = q.p();
                EnterCriticalSection(&mut (*q).cs);
                (*q).items.push_back(i);
                LeaveCriticalSection(&mut (*q).cs);
                if i % 2 == 0 {
                    WakeConditionVariable(&mut (*q).cv);
                } else {
                    WakeAllConditionVariable(&mut (*q).cv);
                }
            }
        }
        assert_eq!(consumer.join().unwrap(), (0..100).collect::<Vec<_>>());
    }

    #[test]
    fn condition_variable_srw_modes_and_timeout() {
        let m = Mem::new((0usize, 0usize, 0u32)); // lock, cv, flag
        let mc = m.clone();
        let waiter = std::thread::spawn(move || unsafe {
            let p = mc.p();
            AcquireSRWLockShared(&mut (*p).0);
            while std::ptr::read_volatile(&(*p).2) == 0 {
                assert_eq!(
                    SleepConditionVariableSRW(&mut (*p).1, &mut (*p).0, INFINITE, 1),
                    1
                );
            }
            ReleaseSRWLockShared(&mut (*p).0);
        });
        std::thread::sleep(Duration::from_millis(20));
        unsafe {
            let p = m.p();
            AcquireSRWLockExclusive(&mut (*p).0);
            std::ptr::write_volatile(&mut (*p).2, 1);
            ReleaseSRWLockExclusive(&mut (*p).0);
            WakeAllConditionVariable(&mut (*p).1);
        }
        waiter.join().unwrap();
        unsafe {
            let p = m.p();
            AcquireSRWLockExclusive(&mut (*p).0);
            let t = Instant::now();
            assert_eq!(
                SleepConditionVariableSRW(&mut (*p).1, &mut (*p).0, 40, 0),
                0
            );
            assert_eq!(GetLastError(), ERROR_TIMEOUT);
            assert!(t.elapsed() >= Duration::from_millis(40));
            assert!(t.elapsed() < Duration::from_secs(2));
            // The lock is held again after the timeout.
            assert_eq!(TryAcquireSRWLockShared(&mut (*p).0), 0);
            ReleaseSRWLockExclusive(&mut (*p).0);
            let cs = cs_new();
            EnterCriticalSection(cs.p());
            EnterCriticalSection(cs.p());
            assert_eq!(SleepConditionVariableCS(&mut (*p).1, cs.p(), 20), 0);
            assert_eq!(GetLastError(), ERROR_TIMEOUT);
            assert_eq!((*cs.p()).recursion_count.load(Relaxed), 2);
            LeaveCriticalSection(cs.p());
            LeaveCriticalSection(cs.p());
        }
    }

    #[test]
    fn wait_on_address() {
        let v = Mem::new(0u64);
        unsafe {
            let same = 0u64;
            let diff = 1u64;
            let t = Instant::now();
            // Value differs from the compare value: returns at once.
            assert_eq!(
                WaitOnAddress(v.p() as _, &diff as *const u64 as _, 8, INFINITE),
                1
            );
            assert!(t.elapsed() < Duration::from_millis(500));
            // Equal: times out.
            assert_eq!(
                WaitOnAddress(v.p() as _, &same as *const u64 as _, 8, 30),
                0
            );
            assert_eq!(GetLastError(), ERROR_TIMEOUT);
            assert_eq!(
                WaitOnAddress(v.p() as _, &same as *const u64 as _, 3, 30),
                0
            );
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            let one = 0u8;
            assert_eq!(WaitOnAddress(v.p() as _, &one, 1, 10), 0);
        }
        // WakeByAddressAll wakes every waiter.
        let woke = Arc::new(AtomicU64::new(0));
        let hs: Vec<_> = (0..4)
            .map(|_| {
                let (v, w) = (v.clone(), woke.clone());
                std::thread::spawn(move || unsafe {
                    let zero = 0u32;
                    while std::ptr::read_volatile(v.p() as *const u32) == 0 {
                        WaitOnAddress(v.p() as _, &zero as *const u32 as _, 4, INFINITE);
                    }
                    w.fetch_add(1, SeqCst);
                })
            })
            .collect();
        std::thread::sleep(Duration::from_millis(30));
        unsafe {
            std::ptr::write_volatile(v.p() as *mut u32, 1);
            WakeByAddressAll(v.p() as _);
        }
        hs.into_iter().for_each(|h| h.join().unwrap());
        assert_eq!(woke.load(SeqCst), 4);
        // WakeByAddressSingle wakes a waiter.
        let v = Mem::new(0u32);
        let vc = v.clone();
        let h = std::thread::spawn(move || unsafe {
            let zero = 0u32;
            while std::ptr::read_volatile(vc.p()) == 0 {
                WaitOnAddress(vc.p() as _, &zero as *const u32 as _, 4, INFINITE);
            }
        });
        std::thread::sleep(Duration::from_millis(20));
        unsafe {
            std::ptr::write_volatile(v.p(), 1);
            WakeByAddressSingle(v.p() as _);
        }
        h.join().unwrap();
    }

    static RUNS: AtomicU32 = AtomicU32::new(0);

    unsafe extern "win64" fn once_ok(_o: *mut usize, p: *mut (), ctx: *mut *mut ()) -> Bool {
        RUNS.fetch_add(1, SeqCst);
        std::thread::sleep(Duration::from_millis(20));
        *ctx = p;
        1
    }
    unsafe extern "win64" fn once_fail(_o: *mut usize, _p: *mut (), _c: *mut *mut ()) -> Bool {
        RUNS.fetch_add(1, SeqCst);
        0
    }

    #[test]
    fn init_once() {
        let once = Mem::new(0usize);
        let hs: Vec<_> = (0..6)
            .map(|_| {
                let o = once.clone();
                std::thread::spawn(move || unsafe {
                    let mut ctx: *mut () = std::ptr::null_mut();
                    let r = InitOnceExecuteOnce(o.p(), Some(once_ok), 0x1000 as *mut (), &mut ctx);
                    (r, ctx as usize)
                })
            })
            .collect();
        for h in hs {
            assert_eq!(h.join().unwrap(), (1, 0x1000));
        }
        assert_eq!(RUNS.load(SeqCst), 1);
        unsafe {
            // Already done: callback not run again; null context is accepted.
            assert_eq!(
                InitOnceExecuteOnce(
                    once.p(),
                    Some(once_fail),
                    std::ptr::null_mut(),
                    std::ptr::null_mut()
                ),
                1
            );
        }
        assert_eq!(RUNS.load(SeqCst), 1);
        // Failure leaves it uninitialised, so the next call runs the callback again.
        let once = Mem::new(0usize);
        unsafe {
            let mut ctx = std::ptr::null_mut();
            assert_eq!(
                InitOnceExecuteOnce(once.p(), Some(once_fail), std::ptr::null_mut(), &mut ctx),
                0
            );
            assert_eq!(*once.p(), 0);
            assert_eq!(
                InitOnceExecuteOnce(once.p(), Some(once_ok), 0x2000 as *mut (), &mut ctx),
                1
            );
            assert_eq!(ctx as usize, 0x2000);
        }
        assert_eq!(RUNS.load(SeqCst), 3);
    }
}
