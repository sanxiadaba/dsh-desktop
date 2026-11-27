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
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetMessageW, MSG, PostMessageW, RegisterClassW, TranslateMessage, WM_CHAR, WM_CLOSE,
    WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEWHEEL, WM_QUIT, WNDCLASSW,
    WS_CHILD, WS_EX_NOACTIVATE, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

struct Counters {
    lbutton_dblclick: AtomicI32,
    wheel_child: AtomicI32,
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
static CHILD_DOWN: AtomicI32 = AtomicI32::new(0);

fn counters() -> &'static Counters {
    STATE.get_or_init(|| Counters {
        lbutton_dblclick: AtomicI32::new(0),
        wheel_child: AtomicI32::new(0),
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

unsafe extern "system" fn child_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    const WM_LBUTTONDBLCLK: u32 = 0x0203;
    if msg == WM_LBUTTONDOWN {
        CHILD_DOWN.fetch_add(1, Ordering::SeqCst);
        return LRESULT(0);
    }
    if msg == WM_LBUTTONDBLCLK {
        counters().lbutton_dblclick.fetch_add(1, Ordering::SeqCst);
        return LRESULT(0);
    }
    if msg == WM_MOUSEWHEEL {
        counters()
            .wheel_child
            .fetch_add((((wp.0 >> 16) & 0xffff) as i16) as i32, Ordering::SeqCst);
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
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

pub fn run() -> Result<Value> {
    let (tx, rx) = std::sync::mpsc::channel::<std::result::Result<(usize, usize), String>>();

    let join = std::thread::spawn(move || unsafe {
        let hinst = match GetModuleHandleW(None) {
            Ok(h) => windows::Win32::Foundation::HINSTANCE(h.0),
            Err(_) => {
                let _ = tx.send(Err("GetModuleHandleW 失败".to_string()));
                return;
            }
        };
        let class_name = windows::core::HSTRING::from("DeskSelftestWindow");
        let child_class = windows::core::HSTRING::from("DeskSelftestChild");
        let cwc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(child_wndproc),
            hInstance: hinst,
            lpszClassName: windows::core::PCWSTR(child_class.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassW(&cwc);
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
            -4000,
            -4000,
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
        let child = CreateWindowExW(
            Default::default(),
            windows::core::PCWSTR(child_class.as_ptr()),
            &windows::core::HSTRING::from("child"),
            WS_CHILD | WS_VISIBLE,
            20,
            20,
            160,
            40,
            Some(hwnd),
            None,
            Some(hinst),
            None,
        );
        let child_v = child.map(|c| c.0 as usize).unwrap_or(0);
        let _ = tx.send(Ok((hwnd.0 as usize, child_v)));
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

    let (hwnd, child_hwnd) = match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok((v, c))) => (HWND(v as *mut _), HWND(c as *mut _)),
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

    // 1b) 子窗口：经典 Win32 程序把控件做成子 HWND，消息必须投给最深的那个。
    let (ccx, ccy) = win::client_to_screen(child_hwnd, 60, 18);
    let child_click = input::click_screen(hwnd, ccx, ccy, input::Button::Left);
    std::thread::sleep(std::time::Duration::from_millis(120));
    let cd = CHILD_DOWN.load(Ordering::SeqCst);
    check(
        "bg_click_child",
        "child received 1",
        format!("child_down={cd} err={:?}", child_click.err()),
        cd == 1,
    );

    // 1c) 双击与滚轮也要投给子窗口（和点击同一条路由规则）。
    crate::input::double_click_screen(hwnd, ccx, ccy)?;
    std::thread::sleep(std::time::Duration::from_millis(120));
    let dbl = c.lbutton_dblclick.load(Ordering::SeqCst);
    check(
        "bg_dclick_child",
        "child dblclick=1",
        format!("dblclick={dbl}"),
        dbl == 1,
    );

    crate::input::wheel_screen(hwnd, ccx, ccy, -120)?;
    std::thread::sleep(std::time::Duration::from_millis(120));
    let wc = c.wheel_child.load(Ordering::SeqCst);
    check(
        "bg_wheel_child",
        "child wheel=-120",
        format!("wheel={wc}"),
        wc == -120,
    );

    // 1d) 拖拽：按下/抬起都要落在同一个（父）窗口上，且中间要有移动。
    let before_down = c.lbutton_down.load(Ordering::SeqCst);
    let before_up = c.lbutton_up.load(Ordering::SeqCst);
    let (dx0, dy0) = win::client_to_screen(hwnd, 40, 200);
    let (dx1, dy1) = win::client_to_screen(hwnd, 220, 260);
    crate::input::drag_screen(hwnd, (dx0, dy0), (dx1, dy1), 8)?;
    std::thread::sleep(std::time::Duration::from_millis(120));
    let d_down = c.lbutton_down.load(Ordering::SeqCst) - before_down;
    let d_up = c.lbutton_up.load(Ordering::SeqCst) - before_up;
    check(
        "bg_drag",
        "down+1 up+1",
        format!("down+{d_down} up+{d_up}"),
        d_down == 1 && d_up == 1,
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
    let found = win::enum_windows(false).into_iter().any(|w| w.id == id);
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
