//! 自测：自己开一个窗口，往里注入消息，断言真的收到了。
//!
//! 不依赖任何外部程序，也不依赖屏幕内容，所以可以被反复跑。

use crate::error::{DeskError, Result};
use crate::input;
use crate::win;
use serde_json::{Value, json};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicIsize, AtomicU32, Ordering};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, WS_EX_NOACTIVATE, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetMessageW, MSG, PostMessageW, RegisterClassW, TranslateMessage, WM_CHAR, WM_CLOSE,
    WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEWHEEL, WM_QUIT, WNDCLASSW,
    WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

struct Counters {
    lbutton_down: AtomicI32,
    lbutton_up: AtomicI32,
    key_down: AtomicU32,
    key_up: AtomicU32,
    wheel: AtomicI32,
    click_x: AtomicI32,
    click_y: AtomicI32,
    last_vk: AtomicIsize,
    chars: Mutex<String>,
}

static STATE: std::sync::OnceLock<Counters> = std::sync::OnceLock::new();

fn counters() -> &'static Counters {
    STATE.get_or_init(|| Counters {
        lbutton_down: AtomicI32::new(0),
        lbutton_up: AtomicI32::new(0),
        key_down: AtomicU32::new(0),
        key_up: AtomicU32::new(0),
        wheel: AtomicI32::new(0),
        click_x: AtomicI32::new(-1),
        click_y: AtomicI32::new(-1),
        last_vk: AtomicIsize::new(0),
        chars: Mutex::new(String::new()),
    })
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let c = counters();
    match msg {
        WM_LBUTTONDOWN => {
            c.lbutton_down.fetch_add(1, Ordering::SeqCst);
            c.click_x
                .store((lp.0 & 0xffff) as i16 as i32, Ordering::SeqCst);
            c.click_y
                .store(((lp.0 >> 16) & 0xffff) as i16 as i32, Ordering::SeqCst);
        }
        WM_LBUTTONUP => {
            c.lbutton_up.fetch_add(1, Ordering::SeqCst);
        }
        WM_KEYDOWN => {
            c.key_down.fetch_add(1, Ordering::SeqCst);
            c.last_vk.store(wp.0 as isize, Ordering::SeqCst);
        }
        WM_KEYUP => {
            c.key_up.fetch_add(1, Ordering::SeqCst);
        }
        WM_CHAR => {
            if let Some(ch) = char::from_u32(wp.0 as u32) {
                c.chars.lock().unwrap().push(ch);
            }
        }
        WM_MOUSEWHEEL => {
            c.wheel
                .fetch_add(((wp.0 >> 16) & 0xffff) as i16 as i32, Ordering::SeqCst);
        }
        WM_CLOSE => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return LRESULT(0);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

pub fn run(physical_check: bool) -> Result<Value> {
    let (tx, rx) = std::sync::mpsc::channel::<std::result::Result<usize, String>>();

    let join = std::thread::spawn(move || unsafe {
        let hinst = match GetModuleHandleW(None) {
            Ok(h) => windows::Win32::Foundation::HINSTANCE(h.0),
            Err(_) => {
                let _ = tx.send(Err("GetModuleHandleW 失败".to_string()));
                return;
            }
        };
        let class_name = windows::core::HSTRING::from("DeskSelftestWindow");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            let _ = tx.send(Err("RegisterClassW 失败".to_string()));
            return;
        }
        let title = windows::core::HSTRING::from("desk selftest");
        let hwnd = CreateWindowExW(
            WS_EX_NOACTIVATE,
            windows::core::PCWSTR(class_name.as_ptr()),
            &title,
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            120,
            120,
            480,
            320,
            None,
            None,
            Some(hinst),
            None,
        );
        let Ok(hwnd) = hwnd else {
            let _ = tx.send(Err("CreateWindowExW 失败".to_string()));
            return;
        };
        let _ = tx.send(Ok(hwnd.0 as usize));
        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 == 0 || r.0 == -1 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });

    let hwnd = match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(v)) => HWND(v as *mut _),
        Ok(Err(e)) => return Err(DeskError::new("SELFTEST_FAILED", e)),
        Err(_) => return Err(DeskError::new("SELFTEST_FAILED", "窗口创建超时")),
    };
    std::thread::sleep(std::time::Duration::from_millis(200));

    let mut results: Vec<Value> = Vec::new();
    let mut check = |name: &str, want: &str, got: String, pass: bool| {
        results.push(json!({"check": name, "want": want, "got": got, "pass": pass}));
    };

    // 1) 后台点击：屏幕坐标 → 客户区坐标，按下/抬起都要到。
    let (sx, sy) = win::client_to_screen(hwnd, 60, 80);
    let click = input::click_screen(hwnd, sx, sy, input::Button::Left);
    std::thread::sleep(std::time::Duration::from_millis(120));
    let c = counters();
    let down = c.lbutton_down.load(Ordering::SeqCst);
    let up = c.lbutton_up.load(Ordering::SeqCst);
    let cx = c.click_x.load(Ordering::SeqCst);
    let cy = c.click_y.load(Ordering::SeqCst);
    check(
        "bg_click",
        "down=1 up=1 at 60,80",
        format!("down={down} up={up} at {cx},{cy} err={:?}", click.err()),
        down == 1 && up == 1 && cx == 60 && cy == 80,
    );

    // 2) 后台键盘：组合键的按下次数、最后一次的 VK。
    let key = input::key_combo(hwnd, "ctrl+a");
    std::thread::sleep(std::time::Duration::from_millis(120));
    let kd = c.key_down.load(Ordering::SeqCst);
    let last = c.last_vk.load(Ordering::SeqCst);
    check(
        "bg_key",
        "keydown>=2 last=0x41",
        format!("keydown={kd} last=0x{last:x} err={:?}", key.err()),
        kd >= 2 && last == 0x41,
    );

    // 3) 后台文本：WM_CHAR 拼回原串。先清空，避免把用户此刻的按键算进来。
    c.chars.lock().unwrap().clear();
    let text = input::type_text(hwnd, "hi 42");
    std::thread::sleep(std::time::Duration::from_millis(150));
    let got = c.chars.lock().unwrap().clone();
    check(
        "bg_type",
        "hi 42",
        format!("{got:?} err={:?}", text.err()),
        got == "hi 42",
    );

    // 4) 滚轮。
    let wheel = input::wheel_screen(hwnd, sx, sy, -120);
    std::thread::sleep(std::time::Duration::from_millis(120));
    let w = c.wheel.load(Ordering::SeqCst);
    check(
        "bg_wheel",
        "-120",
        format!("{w} err={:?}", wheel.err()),
        w == -120,
    );

    // 5) 窗口枚举能看到它。
    let id = win::window_id(hwnd);
    let found = win::enum_windows().into_iter().any(|w| w.id == id);
    check("windows_enum", "found", format!("id={id}"), found);

    // 6) 截图非空。
    let shot = crate::shot::window(hwnd);
    let ok_shot = shot.as_ref().map(|i| !i.bgra.is_empty()).unwrap_or(false);
    check(
        "screenshot",
        "non-empty",
        format!("{:?}", shot.map(|i| (i.width, i.height))),
        ok_shot,
    );

    // 7) 物理输入（可选）：会移动真实光标，只在 --physical 时跑。
    if physical_check {
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd);
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
        let (px, py) = win::client_to_screen(hwnd, 120, 140);
        let ok_phys;
        if let Err(e) = crate::physical::click(px, py) {
            ok_phys = false;
            check("physical_click", "ok", format!("err={e}"), false);
        } else {
            std::thread::sleep(std::time::Duration::from_millis(120));
            ok_phys = c.lbutton_down.load(Ordering::SeqCst) >= 2;
            check(
                "physical_click",
                "second click counted",
                format!("down={}", c.lbutton_down.load(Ordering::SeqCst)),
                ok_phys,
            );
        }
        if ok_phys {
            let before = c.chars.lock().unwrap().len();
            let r = crate::physical::type_text("xy");
            std::thread::sleep(std::time::Duration::from_millis(120));
            let after = c.chars.lock().unwrap().len();
            check(
                "physical_type",
                "+2 chars",
                format!("before={before} after={after} err={:?}", r.err()),
                after >= before + 2,
            );
        }
    }

    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_QUIT, WPARAM(0), LPARAM(0));
    }
    let _ = join.join();

    let passed = results.iter().filter(|r| r["pass"] == true).count();
    let total = results.len();
    let value = json!({
        "passed": passed,
        "total": total,
        "checks": results,
    });
    if passed == total {
        Ok(value)
    } else {
        Err(DeskError::new(
            "SELFTEST_FAILED",
            format!("自测有 {}/{} 项没通过", total - passed, total),
        )
        .with_details(value))
    }
}
