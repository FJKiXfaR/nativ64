win32! {
    module "api-ms-win-core-processthreads" => "kernel32";
    Native fn GetCurrentProcessId() -> u32 {
        libc::getpid() as u32
    }
    Native fn GetCurrentThreadId() -> u32 {
        libc::syscall(libc::SYS_gettid) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        unsafe {
            assert_eq!(GetCurrentProcessId(), std::process::id());
            let main = GetCurrentThreadId();
            assert_ne!(main, 0);
            let other = std::thread::spawn(|| GetCurrentThreadId()).join().unwrap();
            assert_ne!(main, other);
        }
    }
}
