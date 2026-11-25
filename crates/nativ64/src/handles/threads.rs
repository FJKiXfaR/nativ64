//! Thread objects of the handle table (`CreateThread`, `OpenThread`, ...).
//!
//! * A thread object is signaled once its thread has exited; its exit code is then final
//!   (`STILL_ACTIVE` before). It lives while a handle, or the running thread, still refers to
//!   it, so closing the handle of a running thread is fine.
//! * Thread ids are Linux tids. `Kernel::threads` maps them to the objects (weakly) for
//!   `OpenThread`. A thread not made by `CreateThread` (the main thread, ...) gets its object the
//!   first time it asks for its pseudo handle (`-2`); a thread-local guard signals it on exit.
//!   Such threads can only be opened by id from themselves.
//! * `CREATE_SUSPENDED`: the new thread parks before the start routine until its suspend count
//!   is 0. `SuspendThread` only works on a thread that has not started its routine yet: stopping
//!   a running thread would need signals, which is deliberately not done, so it fails with
//!   `ERROR_NOT_SUPPORTED`.
//! * Priorities are validated and stored, not applied to the Linux scheduler.

use super::*;
use std::cell::Cell;
use std::ffi::c_void;
use std::sync::Condvar;

/// `GetCurrentThread()`: the pseudo handle (-2).
pub const CURRENT_THREAD: Handle = usize::MAX - 1;

pub const STILL_ACTIVE: u32 = 259;
pub const CREATE_SUSPENDED: u32 = 4;
pub const ERROR_NOT_SUPPORTED: u32 = 50;
pub const THREAD_PRIORITY_ERROR_RETURN: i32 = i32::MAX;
const SUSPEND_ERROR: u32 = u32::MAX;
const PRIORITY_IDLE: i32 = -15;
const PRIORITY_TIME_CRITICAL: i32 = 15;

pub type ThreadStart = unsafe extern "win64" fn(*mut c_void) -> u32;

pub struct ThreadState {
    tid: u32,
    pub(super) exited: bool,
    exit_code: u32,
    priority: i32,
    suspend: u32,
    /// The start routine has been entered (or the thread was not made by `CreateThread`).
    started: bool,
}

pub struct ThreadObj {
    pub(super) state: Mutex<ThreadState>,
    pub(super) waiters: Waiters,
    resume: Condvar,
}

fn new_thread(tid: u32, suspend: u32, started: bool) -> Arc<Object> {
    Arc::new(Object::Thread(ThreadObj {
        state: Mutex::new(ThreadState {
            tid,
            exited: false,
            exit_code: STILL_ACTIVE,
            priority: 0,
            suspend,
            started,
        }),
        waiters: Waiters::default(),
        resume: Condvar::new(),
    }))
}

fn as_thread(o: &Object) -> &ThreadObj {
    match o {
        Object::Thread(t) => t,
        _ => unreachable!("not a thread object"),
    }
}

/// Signals the end of the thread and wakes its waiters. Kernel lock must not be held.
fn finish(o: &Arc<Object>, code: u32) {
    let _k = kernel();
    let t = as_thread(o);
    {
        let mut s = lock(&t.state);
        if s.exited {
            return;
        }
        s.exited = true;
        s.exit_code = code;
    }
    t.waiters.wake();
}

// Run-and-escape of a thread's start routine, without unwinding (as `ExitThread` on Windows,
// which does not unwind the caller's frames).
//
// `nativ64_run_start(start, param, slot)` (SysV) saves the callee-saved registers on the stack,
// stores that stack pointer in `*slot`, and calls `start(param)` (win64). It returns the routine's
// result, or, if `nativ64_exit_jump(slot, code)` was called meanwhile, `code`: the jump reloads
// the stack pointer from `*slot` and falls into the epilogue, which restores the registers.
// Everything between (the win64 frames, with or without unwind info) is simply abandoned, so no
// destructor of theirs runs.
std::arch::global_asm!(
    ".text",
    ".globl nativ64_run_start",
    ".type nativ64_run_start,@function",
    "nativ64_run_start:",
    "push rbp",
    "push rbx",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "sub rsp, 8",
    "mov [rdx], rsp",
    "mov rax, rdi",
    "mov rcx, rsi",
    "sub rsp, 32",
    "call rax",
    "add rsp, 32",
    "2:",
    "add rsp, 8",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
    ".size nativ64_run_start, .-nativ64_run_start",
    ".globl nativ64_exit_jump",
    ".type nativ64_exit_jump,@function",
    "nativ64_exit_jump:",
    "mov rsp, [rdi]",
    "mov eax, esi",
    "jmp 2b",
    ".size nativ64_exit_jump, .-nativ64_exit_jump",
);

unsafe extern "sysv64" {
    fn nativ64_run_start(start: ThreadStart, param: *mut c_void, slot: *mut usize) -> u32;
    fn nativ64_exit_jump(slot: *mut usize, code: u32) -> !;
}

thread_local! {
    /// Stack pointer saved by `nativ64_run_start` while this thread runs a start routine of
    /// `CreateThread`; 0 otherwise.
    static RESUME_SP: Cell<usize> = const { Cell::new(0) };
}

/// Ends the calling thread with `code` (`_endthreadex`, and later `ExitThread`).
///
/// A thread made by `CreateThread` jumps back into its trampoline, which goes on as if the start
/// routine had returned `code` (mutexes abandoned, thread object signaled, then a normal return
/// from the pthread, which runs the TLS/FLS cleanup). Nothing is unwound. Under ThreadSanitizer
/// the jump skips the instrumented function exits of the abandoned frames; the shadow stack of
/// this ending thread is simply left unbalanced, which is harmless since the thread ends.
///
/// The main thread ends the process with `code` (the last thread to leave decides on Windows).
/// Any other thread (not made by `CreateThread`) gets the exit bookkeeping, then `pthread_exit`
/// as a last resort: it unwinds, which aborts if a frame lacks unwind information.
pub fn end_current_thread(code: u32) -> ! {
    // Plain copy: no value with a destructor is alive when jumping.
    let sp = RESUME_SP.with(|c| c.get());
    if sp != 0 {
        let slot = RESUME_SP.with(|c| c.as_ptr());
        unsafe { nativ64_exit_jump(slot, code) }
    }
    abandon_owned();
    let o = current_thread();
    finish(&o, code);
    drop(o);
    if tid() == std::process::id() {
        unsafe { libc::exit(code as i32) }
    }
    unsafe { libc::pthread_exit(std::ptr::null_mut()) }
}

/// Signals a thread that was not made by `CreateThread` when it ends.
struct ExitGuard(Arc<Object>);

impl Drop for ExitGuard {
    fn drop(&mut self) {
        abandon_owned();
        finish(&self.0, 0);
    }
}

thread_local! {
    static GUARD: RefCell<Option<ExitGuard>> = const { RefCell::new(None) };
}

/// The object of the calling thread, created on first use for threads `CreateThread` did not make.
pub(super) fn current_thread() -> Arc<Object> {
    let me = tid();
    let mut k = kernel();
    if let Some(o) = k.threads.get(&me).and_then(Weak::upgrade) {
        if !lock(&as_thread(&o).state).exited {
            return o;
        }
    }
    let o = new_thread(me, 0, true);
    k.threads.insert(me, Arc::downgrade(&o));
    drop(k);
    GUARD.with(|c| *c.borrow_mut() = Some(ExitGuard(o.clone())));
    o
}

/// Resolves a thread handle (or the pseudo handle); sets `ERROR_INVALID_HANDLE` otherwise.
fn thread_arg(h: Handle) -> Option<Arc<Object>> {
    if h == CURRENT_THREAD {
        return Some(current_thread());
    }
    match kernel().get(h) {
        Some(o) if matches!(*o, Object::Thread(_)) => Some(o),
        _ => {
            set_last_error(ERROR_INVALID_HANDLE);
            None
        }
    }
}

struct Boot {
    obj: Arc<Object>,
    start: ThreadStart,
    param: usize,
    tx: std::sync::mpsc::Sender<u32>,
}

extern "C" fn trampoline(arg: *mut c_void) -> *mut c_void {
    let Boot {
        obj,
        start,
        param,
        tx,
    } = *unsafe { Box::from_raw(arg as *mut Boot) };
    let me = tid();
    {
        let mut k = kernel();
        k.threads.insert(me, Arc::downgrade(&obj));
        lock(&as_thread(&obj).state).tid = me;
    }
    let _ = tx.send(me);
    drop(tx);
    {
        let t = as_thread(&obj);
        let mut s = lock(&t.state);
        while s.suspend > 0 {
            s = t.resume.wait(s).unwrap_or_else(|e| e.into_inner());
        }
        s.started = true;
    }
    let hooks = crate::process::thread_hooks();
    let guard = hooks.map_or(std::ptr::null_mut(), |h| (h.attach)());
    let slot = RESUME_SP.with(|c| c.as_ptr());
    let code = unsafe { nativ64_run_start(start, param as *mut c_void, slot) };
    RESUME_SP.with(|c| c.set(0));
    // Also reached after `_endthreadex`, whose jump lands back here.
    if let Some(h) = hooks {
        (h.detach)(guard);
    }
    abandon_owned();
    finish(&obj, code);
    std::ptr::null_mut()
}

/// `CreateThread`. Returns the handle and the thread id, or 0.
pub fn create_thread(
    stack: usize,
    start: Option<ThreadStart>,
    param: *mut c_void,
    flags: u32,
) -> Option<(Handle, u32)> {
    let Some(start) = start else {
        set_last_error(ERROR_INVALID_PARAMETER);
        return None;
    };
    let suspended = flags & CREATE_SUSPENDED != 0;
    let obj = new_thread(0, suspended as u32, false);
    let (tx, rx) = std::sync::mpsc::channel();
    let boot = Box::into_raw(Box::new(Boot {
        obj: obj.clone(),
        start,
        param: param as usize,
        tx,
    }));
    // A raw detached pthread (not `std::thread`), whose trampoline `end_current_thread` jumps back to.
    let spawned = unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        libc::pthread_attr_init(&mut attr);
        libc::pthread_attr_setdetachstate(&mut attr, libc::PTHREAD_CREATE_DETACHED);
        libc::pthread_attr_setstacksize(&mut attr, crate::process::thread_stack_size(stack, flags));
        let mut th: libc::pthread_t = 0;
        let r = libc::pthread_create(&mut th, &attr, trampoline, boot as *mut c_void);
        libc::pthread_attr_destroy(&mut attr);
        r == 0
    };
    if !spawned {
        drop(unsafe { Box::from_raw(boot) });
        set_last_error(8); // ERROR_NOT_ENOUGH_MEMORY
        return None;
    }
    let Ok(id) = rx.recv() else {
        set_last_error(8);
        return None;
    };
    let h = kernel().insert(obj);
    set_last_error(0);
    Some((h, id))
}

pub fn get_exit_code(h: Handle) -> Option<u32> {
    let o = thread_arg(h)?;
    let code = lock(&as_thread(&o).state).exit_code;
    Some(code)
}

pub fn open_thread(id: u32) -> Handle {
    if id != 0 && id == tid() {
        let o = current_thread();
        return kernel().insert(o);
    }
    let mut k = kernel();
    match k.threads.get(&id).and_then(Weak::upgrade) {
        Some(o) if id != 0 => k.insert(o),
        _ => {
            set_last_error(ERROR_INVALID_PARAMETER);
            0
        }
    }
}

/// Increments the suspend count; only possible before the start routine runs.
pub fn suspend_thread(h: Handle) -> u32 {
    let Some(o) = thread_arg(h) else {
        return SUSPEND_ERROR;
    };
    let mut s = lock(&as_thread(&o).state);
    if s.started || s.exited {
        set_last_error(ERROR_NOT_SUPPORTED);
        return SUSPEND_ERROR;
    }
    s.suspend += 1;
    s.suspend - 1
}

pub fn resume_thread(h: Handle) -> u32 {
    let Some(o) = thread_arg(h) else {
        return SUSPEND_ERROR;
    };
    let t = as_thread(&o);
    let mut s = lock(&t.state);
    let prev = s.suspend;
    if prev > 0 {
        s.suspend -= 1;
        if s.suspend == 0 {
            t.resume.notify_all();
        }
    }
    prev
}

fn valid_priority(p: i32) -> bool {
    (-2..=2).contains(&p) || p == PRIORITY_IDLE || p == PRIORITY_TIME_CRITICAL
}

pub fn set_priority(h: Handle, p: i32) -> i32 {
    let Some(o) = thread_arg(h) else { return 0 };
    if !valid_priority(p) {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    lock(&as_thread(&o).state).priority = p;
    1
}

pub fn get_priority(h: Handle) -> i32 {
    match thread_arg(h) {
        Some(o) => lock(&as_thread(&o).state).priority,
        None => THREAD_PRIORITY_ERROR_RETURN,
    }
}

fn get_affinity(tid: u32) -> Option<u64> {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(tid as i32, std::mem::size_of_val(&set), &mut set) != 0 {
            return None;
        }
        Some(
            (0..64usize)
                .filter(|&i| libc::CPU_ISSET(i, &set))
                .fold(0u64, |m, i| m | 1 << i),
        )
    }
}

/// Sets the affinity of the thread; returns the previous mask or 0 with the last error set.
/// `!0` means "all processors of the process".
pub fn set_affinity(h: Handle, mask: usize) -> usize {
    let Some(o) = thread_arg(h) else { return 0 };
    let id = lock(&as_thread(&o).state).tid;
    let process = get_affinity(std::process::id()).unwrap_or(0);
    let mask = if mask == usize::MAX {
        process
    } else {
        mask as u64
    };
    let prev = get_affinity(id);
    if mask == 0 || mask & !process != 0 || prev.is_none() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        for i in (0..64usize).filter(|i| mask >> i & 1 == 1) {
            libc::CPU_SET(i, &mut set);
        }
        if libc::sched_setaffinity(id as i32, std::mem::size_of_val(&set), &set) != 0 {
            set_last_error(ERROR_INVALID_PARAMETER);
            return 0;
        }
    }
    prev.unwrap_or(0) as usize
}

/// Fills a `GROUP_AFFINITY` (mask, group 0, reserved) with the thread's affinity: the process
/// has one processor group of up to 64 processors.
///
/// # Safety
/// `out` must be NULL or point to 16 writable bytes.
pub unsafe fn get_group_affinity(h: Handle, out: *mut u8) -> i32 {
    if out.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let Some(o) = thread_arg(h) else { return 0 };
    let id = lock(&as_thread(&o).state).tid;
    let Some(mask) = get_affinity(id) else {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    std::ptr::write_bytes(out, 0, 16);
    std::ptr::write_unaligned(out as *mut u64, mask);
    1
}

/// Sets the affinity from a `GROUP_AFFINITY`; only group 0 exists. `old`, when not NULL,
/// receives the previous affinity first (as Wine does).
///
/// # Safety
/// `new` must be NULL or point to 16 readable bytes, `old` NULL or 16 writable bytes.
pub unsafe fn set_group_affinity(h: Handle, new: *const u8, old: *mut u8) -> i32 {
    if new.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let mask = std::ptr::read_unaligned(new as *const u64);
    let group = std::ptr::read_unaligned(new.add(8) as *const u16);
    if group != 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if !old.is_null() && get_group_affinity(h, old) == 0 {
        return 0;
    }
    (set_affinity(h, mask as usize) != 0) as i32
}

pub fn switch_to_thread() -> i32 {
    unsafe { libc::sched_yield() };
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_ms_win_core_errorhandling::{GetLastError, SetLastError};
    use crate::api_ms_win_core_handle::*;
    use crate::api_ms_win_core_processthreads::*;
    use std::ptr::{null, null_mut};
    use std::sync::atomic::AtomicUsize;

    const CUR: usize = usize::MAX;

    unsafe extern "win64" fn ret_param(p: *mut c_void) -> u32 {
        p as usize as u32
    }

    unsafe extern "win64" fn sleep_then_ret(p: *mut c_void) -> u32 {
        std::thread::sleep(std::time::Duration::from_millis(p as usize as u64));
        42
    }

    unsafe extern "win64" fn dup_self(p: *mut c_void) -> u32 {
        let out = &*(p as *const AtomicUsize);
        let mut d = 0usize;
        assert_eq!(
            DuplicateHandle(CUR, GetCurrentThread(), CUR, &mut d, 0, 0, 2),
            1
        );
        out.store(d, SeqCst);
        7
    }

    unsafe fn spawn(f: ThreadStart, p: usize, flags: u32) -> (usize, u32) {
        let mut id = 0u32;
        let h = CreateThread(null(), 0, Some(f), p as *mut c_void, flags, &mut id);
        assert_ne!(h, 0);
        assert_ne!(id, 0);
        (h, id)
    }

    #[test]
    fn create_wait_exit_code() {
        unsafe {
            let (h, _) = spawn(ret_param, 1234, 0);
            assert_eq!(wait(&[h], false, 5000), WAIT_OBJECT_0);
            let mut c = 0;
            assert_eq!(GetExitCodeThread(h, &mut c), 1);
            assert_eq!(c, 1234);
            // Stays signaled.
            assert_eq!(wait(&[h], false, 0), WAIT_OBJECT_0);
            assert_eq!(CloseHandle(h), 1);
            // Optional id, errors.
            let h = CreateThread(null(), 0, Some(ret_param), null_mut(), 0, null_mut());
            assert_ne!(h, 0);
            wait(&[h], false, 5000);
            SetLastError(0);
            assert_eq!(CreateThread(null(), 0, None, null_mut(), 0, null_mut()), 0);
            assert_eq!(GetLastError(), 87);
            SetLastError(0);
            assert_eq!(GetExitCodeThread(h, null_mut()), 0);
            assert_eq!(GetLastError(), 87);
            assert_eq!(CloseHandle(h), 1);
            SetLastError(0);
            let mut c = 0;
            assert_eq!(GetExitCodeThread(h, &mut c), 0);
            assert_eq!(GetLastError(), 6);
            let ev = create_event(None, true, false);
            assert_eq!(GetExitCodeThread(ev, &mut c), 0);
            assert_eq!(GetLastError(), 6);
            assert_eq!(GetExitCodeThread(CUR, &mut c), 0);
        }
    }

    #[test]
    fn still_active_while_running() {
        unsafe {
            let (h, _) = spawn(sleep_then_ret, 300, 0);
            let mut c = 0;
            assert_eq!(GetExitCodeThread(h, &mut c), 1);
            assert_eq!(c, STILL_ACTIVE);
            assert_eq!(wait(&[h], false, 0), WAIT_TIMEOUT);
            assert_eq!(wait(&[h], false, 5000), WAIT_OBJECT_0);
            assert_eq!(GetExitCodeThread(h, &mut c), 1);
            assert_eq!(c, 42);
            CloseHandle(h);
            // The current thread is running.
            assert_eq!(GetExitCodeThread(GetCurrentThread(), &mut c), 1);
            assert_eq!(c, STILL_ACTIVE);
        }
    }

    #[test]
    fn suspended_start_then_resume() {
        unsafe {
            let (h, _) = spawn(ret_param, 5, CREATE_SUSPENDED);
            assert_eq!(wait(&[h], false, 100), WAIT_TIMEOUT);
            let mut c = 0;
            GetExitCodeThread(h, &mut c);
            assert_eq!(c, STILL_ACTIVE);
            // Count bookkeeping before the routine starts.
            assert_eq!(SuspendThread(h), 1);
            assert_eq!(ResumeThread(h), 2);
            assert_eq!(wait(&[h], false, 100), WAIT_TIMEOUT);
            assert_eq!(ResumeThread(h), 1);
            assert_eq!(wait(&[h], false, 5000), WAIT_OBJECT_0);
            GetExitCodeThread(h, &mut c);
            assert_eq!(c, 5);
            // Not suspended any more.
            assert_eq!(ResumeThread(h), 0);
            // A started or finished thread cannot be suspended.
            SetLastError(0);
            assert_eq!(SuspendThread(h), u32::MAX);
            assert_eq!(GetLastError(), ERROR_NOT_SUPPORTED);
            SetLastError(0);
            assert_eq!(SuspendThread(GetCurrentThread()), u32::MAX);
            assert_eq!(GetLastError(), ERROR_NOT_SUPPORTED);
            CloseHandle(h);
            SetLastError(0);
            assert_eq!(ResumeThread(h), u32::MAX);
            assert_eq!(GetLastError(), 6);
            SetLastError(0);
            assert_eq!(SuspendThread(0x1234), u32::MAX);
            assert_eq!(GetLastError(), 6);
        }
    }

    #[test]
    fn open_thread_by_id() {
        unsafe {
            let (h, id) = spawn(ret_param, 9, CREATE_SUSPENDED);
            let o = OpenThread(0x1fffff, 0, id);
            assert!(o != 0 && o != h);
            ResumeThread(o);
            assert_eq!(wait(&[o], false, 5000), WAIT_OBJECT_0);
            assert_eq!(wait(&[h], false, 0), WAIT_OBJECT_0);
            // Still openable while an object exists (handle h).
            let o2 = OpenThread(0x1fffff, 0, id);
            assert_ne!(o2, 0);
            for x in [h, o, o2] {
                CloseHandle(x);
            }
            // Unknown ids.
            SetLastError(0);
            assert_eq!(OpenThread(0x1fffff, 0, 0), 0);
            assert_eq!(GetLastError(), 87);
            SetLastError(0);
            assert_eq!(OpenThread(0x1fffff, 0, 0x7fff_fff0), 0);
            assert_eq!(GetLastError(), 87);
            // The current thread, never created by CreateThread.
            let me = OpenThread(0x1fffff, 0, GetCurrentThreadId());
            assert_ne!(me, 0);
            CloseHandle(me);
        }
    }

    #[test]
    fn priority_round_trip() {
        unsafe {
            let t = GetCurrentThread();
            assert_eq!(t, usize::MAX - 1);
            assert_eq!(GetThreadPriority(t), 0);
            for p in [-15, -2, -1, 0, 1, 2, 15] {
                assert_eq!(SetThreadPriority(t, p), 1);
                assert_eq!(GetThreadPriority(t), p);
            }
            SetThreadPriority(t, 1);
            for bad in [-3, 3, 14, -16, 16, i32::MAX] {
                SetLastError(0);
                assert_eq!(SetThreadPriority(t, bad), 0, "{bad}");
                assert_eq!(GetLastError(), 87);
                assert_eq!(GetThreadPriority(t), 1);
            }
            SetThreadPriority(t, 0);
            // A real handle, per thread.
            let (h, _) = spawn(ret_param, 0, CREATE_SUSPENDED);
            assert_eq!(GetThreadPriority(h), 0);
            assert_eq!(SetThreadPriority(h, -2), 1);
            assert_eq!(GetThreadPriority(h), -2);
            assert_eq!(GetThreadPriority(t), 0);
            ResumeThread(h);
            wait(&[h], false, 5000);
            CloseHandle(h);
            SetLastError(0);
            assert_eq!(GetThreadPriority(h), i32::MAX);
            assert_eq!(GetLastError(), 6);
            SetLastError(0);
            assert_eq!(SetThreadPriority(h, 0), 0);
            assert_eq!(GetLastError(), 6);
        }
    }

    #[test]
    fn affinity() {
        unsafe {
            let t = GetCurrentThread();
            let process = get_affinity(std::process::id()).unwrap() as usize;
            let cur = get_affinity(tid()).unwrap() as usize;
            let lowest = 1usize << process.trailing_zeros();
            assert_eq!(SetThreadAffinityMask(t, lowest), cur);
            assert_eq!(SetThreadAffinityMask(t, process), lowest);
            // "All processors".
            assert_eq!(SetThreadAffinityMask(t, usize::MAX), process);
            let mut ga = [0xFFu8; 16];
            assert_eq!(GetThreadGroupAffinity(t, ga.as_mut_ptr()), 1);
            assert_eq!(
                u64::from_le_bytes(ga[..8].try_into().unwrap()),
                process as u64
            );
            assert_eq!(&ga[8..], &[0u8; 8]);
            assert_eq!(GetThreadGroupAffinity(t, null_mut()), 0);
            assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER);
            assert_eq!(GetThreadGroupAffinity(0x1234, ga.as_mut_ptr()), 0);
            let mut old = [0xFFu8; 16];
            let mut new = [0u8; 16];
            new[..8].copy_from_slice(&lowest.to_le_bytes());
            assert_eq!(SetThreadGroupAffinity(t, new.as_ptr(), old.as_mut_ptr()), 1);
            assert_eq!(
                u64::from_le_bytes(old[..8].try_into().unwrap()),
                process as u64
            );
            assert_eq!(SetThreadAffinityMask(t, usize::MAX), lowest);
            new[8] = 1; // group 1 does not exist
            assert_eq!(SetThreadGroupAffinity(t, new.as_ptr(), null_mut()), 0);
            assert_eq!(SetThreadGroupAffinity(t, null(), null_mut()), 0);
            // Errors: empty, outside the process mask, bad handle.
            SetLastError(0);
            assert_eq!(SetThreadAffinityMask(t, 0), 0);
            assert_eq!(GetLastError(), 87);
            SetLastError(0);
            assert_eq!(
                SetThreadAffinityMask(t, !process & (usize::MAX >> 1) | 1 << 62),
                0
            );
            assert_eq!(GetLastError(), 87);
            SetLastError(0);
            assert_eq!(SetThreadAffinityMask(0x1234, 1), 0);
            assert_eq!(GetLastError(), 6);
            // Another thread by handle.
            let (h, _) = spawn(ret_param, 0, CREATE_SUSPENDED);
            assert_eq!(
                SetThreadAffinityMask(h, lowest) as u64 & process as u64,
                process as u64
            );
            ResumeThread(h);
            wait(&[h], false, 5000);
            CloseHandle(h);
            assert_eq!(SwitchToThread(), 1);
        }
    }

    #[test]
    fn duplicate_pseudo_handle() {
        unsafe {
            let mut d = 0usize;
            assert_eq!(
                DuplicateHandle(CUR, GetCurrentThread(), CUR, &mut d, 0, 0, 2),
                1
            );
            assert!(d != 0 && d != GetCurrentThread() && d.is_multiple_of(4));
            let mut c = 0;
            assert_eq!(GetExitCodeThread(d, &mut c), 1);
            assert_eq!(c, STILL_ACTIVE);
            // Same object as the pseudo handle: priorities are shared.
            SetThreadPriority(d, 2);
            assert_eq!(GetThreadPriority(GetCurrentThread()), 2);
            SetThreadPriority(d, 0);
            assert_eq!(wait(&[d], false, 0), WAIT_TIMEOUT);
            CloseHandle(d);
            // From a created thread: the duplicate outlives it and shows its exit code.
            let out = AtomicUsize::new(0);
            let (h, _) = spawn(dup_self, &out as *const _ as usize, 0);
            assert_eq!(wait(&[h], false, 5000), WAIT_OBJECT_0);
            let d = out.load(SeqCst);
            assert_ne!(d, 0);
            assert_ne!(d, h);
            assert_eq!(wait(&[d], false, 0), WAIT_OBJECT_0);
            assert_eq!(GetExitCodeThread(d, &mut c), 1);
            assert_eq!(c, 7);
            CloseHandle(h);
            CloseHandle(d);
            // The pseudo handle itself cannot be closed.
            SetLastError(0);
            assert_eq!(CloseHandle(GetCurrentThread()), 0);
            assert_eq!(GetLastError(), 6);
        }
    }

    #[test]
    fn foreign_thread_object_is_signaled_on_exit() {
        let h = std::thread::spawn(|| unsafe {
            let mut d = 0usize;
            assert_eq!(
                DuplicateHandle(CUR, GetCurrentThread(), CUR, &mut d, 0, 0, 2),
                1
            );
            d
        })
        .join()
        .unwrap();
        unsafe {
            assert_eq!(wait(&[h], false, 5000), WAIT_OBJECT_0);
            let mut c = 1;
            assert_eq!(GetExitCodeThread(h, &mut c), 1);
            assert_eq!(c, 0);
            CloseHandle(h);
        }
    }

    #[test]
    fn wait_on_several_threads() {
        unsafe {
            let a = spawn(sleep_then_ret, 400, 0).0;
            let b = spawn(sleep_then_ret, 20, 0).0;
            let c = spawn(sleep_then_ret, 400, CREATE_SUSPENDED).0;
            assert_eq!(wait(&[a, b, c], false, 5000), WAIT_OBJECT_0 + 1);
            assert_eq!(wait(&[a, b, c], true, 100), WAIT_TIMEOUT);
            ResumeThread(c);
            assert_eq!(wait(&[a, b, c], true, 5000), WAIT_OBJECT_0);
            for h in [a, b, c] {
                CloseHandle(h);
            }
        }
    }

    #[test]
    fn exiting_thread_abandons_its_mutex_before_signaling() {
        unsafe extern "win64" fn own(p: *mut c_void) -> u32 {
            assert_eq!(wait(&[p as usize], false, 0), WAIT_OBJECT_0);
            0
        }
        unsafe {
            let m = create_mutex(None, false);
            let (h, _) = spawn(own, m, 0);
            assert_eq!(wait(&[h], false, 5000), WAIT_OBJECT_0);
            assert_eq!(wait(&[m], false, 0), WAIT_ABANDONED_0);
            CloseHandle(m);
            CloseHandle(h);
        }
    }

    #[test]
    fn closing_the_handle_of_a_running_thread_does_not_leak() {
        unsafe {
            let (h, id) = spawn(sleep_then_ret, 100, 0);
            assert_eq!(CloseHandle(h), 1);
            let gone = || kernel().threads.get(&id).and_then(Weak::upgrade).is_none();
            assert!(!gone(), "alive while running");
            for _ in 0..200 {
                if gone() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("thread object leaked");
        }
    }

    #[test]
    fn exported_through_lookup() {
        for f in [
            "CreateThread",
            "GetCurrentThread",
            "GetExitCodeThread",
            "OpenThread",
            "ResumeThread",
            "SuspendThread",
            "GetThreadPriority",
            "SetThreadPriority",
            "SetThreadAffinityMask",
            "SwitchToThread",
        ] {
            for m in ["api-ms-win-core-processthreads-l1-1-0.dll", "KERNEL32.dll"] {
                assert!(crate::registry::lookup(m, f).is_some(), "{m}!{f}");
            }
        }
    }
}
