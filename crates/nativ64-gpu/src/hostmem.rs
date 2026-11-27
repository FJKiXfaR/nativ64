//! Reads and writes of the title's memory that cannot fault the process.
//!
//! A texture upload can race the title's own thread that frees the memory: by the time the
//! upload reads it, a page may be gone. A slice over it would crash the process, so the
//! copies here go through `process_vm_readv`/`writev`, which report the fault instead.

/// The bytes of `[addr, addr + len)`; `None` when any of them is not readable.
pub fn peek(addr: u64, len: usize) -> Option<Vec<u8>> {
    if len == 0 {
        return Some(Vec::new());
    }
    let mut out = vec![0u8; len];
    let mut done = 0;
    while done < len {
        let local = libc::iovec {
            iov_base: out[done..].as_mut_ptr().cast(),
            iov_len: len - done,
        };
        let remote = libc::iovec {
            iov_base: (addr as usize + done) as *mut libc::c_void,
            iov_len: len - done,
        };
        // SAFETY: both iovecs describe valid buffers for the driver to copy into/from.
        let n = unsafe { libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0) };
        if n <= 0 {
            return None;
        }
        done += n as usize;
    }
    Some(out)
}

/// Writes `bytes` at `addr`; false when any page is not writable.
pub fn poke(addr: u64, bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    let mut done = 0;
    while done < bytes.len() {
        let local = libc::iovec {
            iov_base: bytes[done..].as_ptr() as *mut libc::c_void,
            iov_len: bytes.len() - done,
        };
        let remote = libc::iovec {
            iov_base: (addr as usize + done) as *mut libc::c_void,
            iov_len: bytes.len() - done,
        };
        // SAFETY: the local buffer is valid; the remote range is checked by the kernel.
        let n = unsafe { libc::process_vm_writev(libc::getpid(), &local, 1, &remote, 1, 0) };
        if n <= 0 {
            return false;
        }
        done += n as usize;
    }
    true
}
