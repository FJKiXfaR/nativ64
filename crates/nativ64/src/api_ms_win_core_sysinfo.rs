use crate::api_ms_win_core_profile::monotonic_ns;

win32! {
    module "api-ms-win-core-sysinfo" => "kernel32";
    Native fn GetTickCount() -> u32 {
        (monotonic_ns() / 1_000_000) as u32
    }
    Native fn GetTickCount64() -> u64 {
        monotonic_ns() / 1_000_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks() {
        unsafe {
            let a = GetTickCount64();
            std::thread::sleep(std::time::Duration::from_millis(30));
            let b = GetTickCount64();
            assert!(b - a >= 25);
            assert!(GetTickCount().abs_diff(b as u32) < 1000);
        }
    }
}
