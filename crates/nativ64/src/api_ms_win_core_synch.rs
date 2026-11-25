win32! {
    module "api-ms-win-core-synch" => "kernel32";
    Native fn Sleep(milliseconds: u32) {
        if milliseconds == 0 {
            libc::sched_yield();
        } else {
            std::thread::sleep(std::time::Duration::from_millis(milliseconds as u64));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleeps() {
        let t = std::time::Instant::now();
        unsafe { Sleep(30) };
        assert!(t.elapsed().as_millis() >= 30);
        unsafe { Sleep(0) };
    }
}
