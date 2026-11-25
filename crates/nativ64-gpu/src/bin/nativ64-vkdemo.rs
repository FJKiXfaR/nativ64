//! Opens a window the way a title does (class, `CreateWindowEx`, message loop of nativ64's
//! windowing layer) over the Wayland backend and presents a slowly cycling clear colour with
//! the Vulkan swapchain. Exits on `WM_CLOSE` or after `--frames N` presented frames.

use nativ64::windowing::{self, Msg, WndClassExA, PM_REMOVE, WM_NCDESTROY, WM_SIZE, WS_VISIBLE};
use nativ64_gpu::backend::vk::Gpu;
use nativ64_gpu::wsi::wayland::WaylandBackend;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static GONE: AtomicBool = AtomicBool::new(false);

unsafe extern "win64" fn wnd_proc(hwnd: usize, msg: u32, w: usize, l: isize) -> isize {
    match msg {
        WM_SIZE => {} // the swapchain reads the host size each frame
        WM_NCDESTROY => GONE.store(true, Ordering::SeqCst),
        _ => {}
    }
    windowing::def_window_proc(hwnd, msg, w, l)
}

/// A hue (turns, 0..1) as a fully saturated RGB colour.
fn hue(t: f32) -> [f32; 4] {
    let h = t.rem_euclid(1.0) * 6.0;
    let x = 1.0 - (h % 2.0 - 1.0).abs();
    let (r, g, b) = match h as u32 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    };
    [r, g, b, 1.0]
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut frames: Option<u64> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--frames" => match it.next().and_then(|v| v.parse().ok()) {
                Some(n) => frames = Some(n),
                None => {
                    eprintln!("nativ64-vkdemo: --frames needs a number");
                    return ExitCode::from(64);
                }
            },
            _ => {
                eprintln!("usage: nativ64-vkdemo [--frames N]");
                return ExitCode::from(64);
            }
        }
    }

    let backend = match WaylandBackend::new() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("nativ64-vkdemo: {e}");
            return ExitCode::from(2);
        }
    };
    windowing::set_backend(Box::new(backend));

    let wc = WndClassExA {
        cb_size: std::mem::size_of::<WndClassExA>() as u32,
        style: 0,
        wnd_proc: wnd_proc as *const () as usize,
        cb_cls_extra: 0,
        cb_wnd_extra: 0,
        h_instance: 0,
        h_icon: 0,
        h_cursor: 0,
        hbr_background: 0,
        menu_name: std::ptr::null(),
        class_name: c"vkdemo".as_ptr().cast(),
        h_icon_sm: 0,
    };
    // SAFETY: `wc` is a complete WNDCLASSEXA with a valid class name and procedure.
    if unsafe { windowing::register_class(&wc) } == 0 {
        eprintln!("nativ64-vkdemo: RegisterClassEx failed");
        return ExitCode::from(1);
    }
    // SAFETY: class and title are NUL-terminated strings.
    let hwnd = unsafe {
        windowing::create_window(
            0,
            c"vkdemo".as_ptr().cast(),
            c"nativ64 vkdemo".as_ptr().cast(),
            WS_VISIBLE,
            0,
            0,
            640,
            360,
            0,
            0,
            0,
            0,
        )
    };
    if hwnd == 0 {
        eprintln!("nativ64-vkdemo: CreateWindowEx failed");
        return ExitCode::from(1);
    }
    let Some(surface) = windowing::window_surface(hwnd) else {
        eprintln!("nativ64-vkdemo: the window has no native surface");
        return ExitCode::from(1);
    };

    let gpu = match Gpu::open() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("nativ64-vkdemo: {e}");
            return ExitCode::from(2);
        }
    };
    println!("device: {}", gpu.name());
    let size = |h| {
        windowing::window_client_size(h)
            .map(|(w, h)| (w.max(1) as u32, h.max(1) as u32))
            .unwrap_or((640, 360))
    };
    let mut sc = match gpu.create_swapchain(surface, size(hwnd)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("nativ64-vkdemo: {e}");
            return ExitCode::from(2);
        }
    };

    let mut presented = 0u64;
    let mut tick = 0u64;
    let mut msg = Msg::default();
    'run: loop {
        // SAFETY: `msg` is a valid MSG.
        while unsafe { windowing::peek_message(&mut msg, 0, 0, 0, PM_REMOVE) } {
            // SAFETY: as above.
            unsafe { windowing::dispatch_message(&msg) };
            if GONE.load(Ordering::SeqCst) {
                break 'run;
            }
        }
        if frames.is_some_and(|n| presented >= n) {
            break;
        }
        let (w, h) = size(hwnd);
        sc.resize(w, h);
        match sc.present_clear(hue(tick as f32 / 360.0)) {
            Ok(true) => presented += 1,
            Ok(false) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => {
                eprintln!("nativ64-vkdemo: {e}");
                return ExitCode::from(1);
            }
        }
        tick += 1;
    }
    println!(
        "presented {presented} frames, last size {:?}, format {:?}",
        sc.extent(),
        sc.format()
    );
    // The swapchain goes before the window it draws into.
    drop(sc);
    if !GONE.load(Ordering::SeqCst) {
        windowing::destroy_window(hwnd);
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::hue;

    #[test]
    fn hue_is_a_valid_colour() {
        for i in 0..360 {
            let c = hue(i as f32 / 360.0);
            assert!(c.iter().all(|v| (0.0..=1.0).contains(v)), "{c:?}");
        }
        assert_eq!(hue(0.0), [1.0, 0.0, 0.0, 1.0]);
    }
}
