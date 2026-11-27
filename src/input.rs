//! 后台输入：把鼠标/键盘消息直接投递到目标窗口。
//!
//! 这一层的意义：不移动真实光标、不抢前台、不依赖屏幕坐标命中，
//! 所以用户可以在前台干别的，动作照样落到目标窗口上。
//!
//! 用 SendMessageTimeout 而不是 PostMessage：保证顺序，也不会因为目标卡住
//! 把调用方一起拖死（超时返回 SMTO_ABORTIFHUNG 错误）。

use crate::error::{DeskError, Result};
use crate::win::screen_to_client;
use windows::Win32::Foundation::{ERROR_SUCCESS, GetLastError, HWND, LPARAM, SetLastError, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MAPVK_VK_TO_VSC, MapVirtualKeyW, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_DBLCLKS, GCL_STYLE, GetClassLongPtrW, PostMessageW, SMTO_ABORTIFHUNG, SMTO_BLOCK,
    SendMessageTimeoutW, WM_CHAR, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

const TIMEOUT_MS: u32 = 2000;
const MK_LBUTTON: usize = 0x0001;
const MK_RBUTTON: usize = 0x0002;

fn lparam_xy(x: i32, y: i32) -> LPARAM {
    // 消息里的坐标是 16 位有符号；超出范围说明调用方算错了，截断只会点错地方。
    let lo = (x as i16 as u16) as isize;
    let hi = ((y as i16 as u16) as isize) << 16;
    LPARAM(lo | hi)
}

fn check_xy(x: i32, y: i32) -> Result<()> {
    if !(-32768..=32767).contains(&x) || !(-32768..=32767).contains(&y) {
        return Err(DeskError::new(
            "BAD_ARGS",
            format!("坐标超出消息范围（{x},{y}），屏幕坐标必须在 -32768..32767 之间"),
        ));
    }
    Ok(())
}

fn send(hwnd: HWND, msg: u32, wparam: usize, lparam: LPARAM) -> bool {
    let mut out: usize = 0;
    // SendMessageTimeout 的返回值是窗口过程的 LRESULT，0 是合法结果；
    // 真正的失败/超时看 GetLastError。先把 last error 清零，避免读到上一次的残留。
    unsafe { SetLastError(ERROR_SUCCESS) };
    let ok = unsafe {
        SendMessageTimeoutW(
            hwnd,
            msg,
            WPARAM(wparam),
            lparam,
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            TIMEOUT_MS,
            Some(&mut out as *mut usize),
        )
    };
    if ok.0 != 0 {
        return true;
    }
    unsafe { GetLastError() == ERROR_SUCCESS }
}

pub fn post(hwnd: HWND, msg: u32, wparam: usize, lparam: LPARAM) -> bool {
    unsafe { PostMessageW(Some(hwnd), msg, WPARAM(wparam), lparam).is_ok() }
}

/// 屏幕坐标点击（左键）。窗口可以先不在前台。
pub fn click_screen(hwnd: HWND, sx: i32, sy: i32, button: Button) -> Result<()> {
    let target = deepest_child(hwnd, sx, sy);
    let (cx, cy) = screen_to_client(target, sx, sy);
    click_client(target, cx, cy, button)
}

/// 执行一个后台动作，并保证它不改变前台窗口。
///
/// 有些程序（Qt 尤其）收到点击会自己 SetActiveWindow，把窗口顶到最前。
/// 这里在动作前后对比前台窗口，一旦被换走就立刻还回原来的前台。
/// 返回 (动作结果, 是否发生过抢占)。
pub fn with_foreground_guard<T>(hwnd: HWND, f: impl FnOnce() -> T) -> (T, bool) {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, SetForegroundWindow};
    let before = unsafe { GetForegroundWindow() };
    let out = f();
    let after = unsafe { GetForegroundWindow() };
    if before == after || before.0.is_null() {
        return (out, false);
    }
    // 只有"新前台属于目标进程"才判定为被目标抢走。用户在动作期间自己切到别的
    // 程序（前台 pid 与目标无关）时不能把他拽回去。
    let target_pid = unsafe {
        let mut pid = 0u32;
        windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(hwnd, Some(&mut pid));
        pid
    };
    let after_pid = unsafe {
        let mut pid = 0u32;
        windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(after, Some(&mut pid));
        pid
    };
    if after_pid == 0 || after_pid != target_pid {
        return (out, false);
    }
    unsafe {
        let _ = SetForegroundWindow(before);
    }
    std::thread::sleep(std::time::Duration::from_millis(30));
    (out, unsafe { GetForegroundWindow() == before })
}

/// 坐标下面真正该收消息的那个 HWND。
///
/// 很多程序把界面拆成子窗口（比如 Win32 的 SysTabControl32 页签控件）：
/// 把消息投给顶层窗口，对话框过程不会转发给子控件，点了等于没点。
/// 这里取该坐标下最深的、且属于目标窗口的 HWND；被别的程序挡住时退回顶层窗口。
fn deepest_child(root: HWND, sx: i32, sy: i32) -> HWND {
    use windows::Win32::UI::WindowsAndMessaging::{CWP_SKIPINVISIBLE, ChildWindowFromPointEx};
    // 逐层往下找包含该点的子窗口。用 ChildWindowFromPointEx 而不是 WindowFromPoint：
    // 目标窗口被别的程序挡住时，WindowFromPoint 会给出挡住它的窗口，导致消息投错地方。
    let mut cur = root;
    for _ in 0..8 {
        let (cx, cy) = screen_to_client(cur, sx, sy);
        let p = windows::Win32::Foundation::POINT { x: cx, y: cy };
        let child = unsafe { ChildWindowFromPointEx(cur, p, CWP_SKIPINVISIBLE) };
        if child.0.is_null() || child == cur {
            break;
        }
        cur = child;
    }
    cur
}

pub fn click_client(hwnd: HWND, cx: i32, cy: i32, button: Button) -> Result<()> {
    check_xy(cx, cy)?;
    let (down, up, mk) = match button {
        Button::Left => (WM_LBUTTONDOWN, WM_LBUTTONUP, MK_LBUTTON),
        Button::Right => (WM_RBUTTONDOWN, WM_RBUTTONUP, MK_RBUTTON),
    };
    let lp = lparam_xy(cx, cy);
    // 先发一次移动：让目标更新悬停/命中位置（Qt、Win32 都靠它）。
    let _ = send(hwnd, WM_MOUSEMOVE, 0, lp);
    if !send(hwnd, down, mk, lp) {
        return Err(DeskError::new(
            "BG_DELIVER_FAILED",
            "投递鼠标按下消息失败：目标窗口可能已挂起或拒绝消息",
        )
        .with_suggestion("先用 `desk status` 确认窗口还在、进程没挂起")
        .delivered());
    }
    std::thread::sleep(std::time::Duration::from_millis(20));
    if !send(hwnd, up, 0, lp) {
        return Err(DeskError::new(
            "BG_DELIVER_FAILED",
            "投递鼠标松开消息失败（按下已送达，目标可能处于按下状态）",
        )
        .delivered());
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub enum Button {
    Left,
    Right,
}

/// 滚轮：`delta` 正数向上，负数向下（一格 120）。
pub fn wheel_screen(hwnd: HWND, sx: i32, sy: i32, delta: i32) -> Result<()> {
    check_xy(sx, sy)?;
    // 滚轮必须投给坐标下最深的子窗口：WM_MOUSEWHEEL 不会被子控件收到，
    // 投给顶层窗口时列表/编辑框会"滚不动"。
    let hwnd = deepest_child(hwnd, sx, sy);
    // WM_MOUSEWHEEL 的坐标是屏幕坐标，不是客户区坐标。
    let lo = (sx as u32 & 0xffff) as isize;
    let hi = ((sy as u32 & 0xffff) as isize) << 16;
    let lp = LPARAM(lo | hi);
    let wp = ((delta as u32) << 16) as usize;
    if !post(hwnd, WM_MOUSEWHEEL, wp, lp) {
        return Err(DeskError::new("BG_DELIVER_FAILED", "投递滚轮消息失败").delivered());
    }
    Ok(())
}

/// 拖拽：按下 → 若干移动 → 松开，全部走客户区坐标。
pub fn drag_client(hwnd: HWND, from: (i32, i32), to: (i32, i32), steps: u32) -> Result<()> {
    check_xy(from.0, from.1)?;
    check_xy(to.0, to.1)?;
    let steps = steps.max(4);
    let lp0 = lparam_xy(from.0, from.1);
    let _ = send(hwnd, WM_MOUSEMOVE, 0, lp0);
    if !send(hwnd, WM_LBUTTONDOWN, MK_LBUTTON, lp0) {
        return Err(DeskError::new("BG_DELIVER_FAILED", "拖拽：按下失败").delivered());
    }
    for i in 1..=steps {
        let x = from.0 + (to.0 - from.0) * i as i32 / steps as i32;
        let y = from.1 + (to.1 - from.1) * i as i32 / steps as i32;
        let _ = send(hwnd, WM_MOUSEMOVE, MK_LBUTTON, lparam_xy(x, y));
        std::thread::sleep(std::time::Duration::from_millis(12));
    }
    if !send(hwnd, WM_LBUTTONUP, 0, lparam_xy(to.0, to.1)) {
        return Err(DeskError::new("BG_DELIVER_FAILED", "拖拽：松开失败").delivered());
    }
    Ok(())
}

pub fn key_combo(hwnd: HWND, combo: &str) -> Result<()> {
    let (mods, vk) = parse_combo(combo)?;
    let alt = mods.iter().any(|m| m.0 == VK_MENU.0);
    let (down_msg, up_msg) = if alt {
        (WM_SYSKEYDOWN, WM_SYSKEYUP)
    } else {
        (WM_KEYDOWN, WM_KEYUP)
    };
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as usize;
    // 方向键/编辑键的真实输入带扩展位，缺了它有些控件会按小键盘解释。
    let ext = if is_extended(vk) { 0x0100_0000usize } else { 0 };

    // 用 Post 而不是 Send：消息进目标线程队列，让它的消息循环还有机会跑
    // TranslateAccelerator / IsDialogMessage；Send 会直接调用窗口过程，绕开这些。
    // 注意：修饰键的"按下状态"不在目标线程里，所以带修饰键的快捷键只保证投递，
    // 不保证目标按真实键盘状态解释——这一点在返回的 effective 里如实标注。
    let mut ok = true;
    for m in &mods {
        // 修饰键本身永远走普通 KEYDOWN，WM_SYSKEYDOWN 只给带 Alt 的主键。
        ok &= post(hwnd, WM_KEYDOWN, m.0 as usize, LPARAM(0x0001_0001));
    }
    ok &= post(
        hwnd,
        down_msg,
        vk as usize,
        LPARAM((0x0001 | (scan << 16) | ext) as isize),
    );
    std::thread::sleep(std::time::Duration::from_millis(20));
    ok &= post(
        hwnd,
        up_msg,
        vk as usize,
        LPARAM((0x0001 | (scan << 16) | ext | 0xC000_0000u32 as usize) as isize),
    );
    for m in mods.iter().rev() {
        ok &= post(
            hwnd,
            WM_KEYUP,
            m.0 as usize,
            LPARAM((0x0001_0001 | 0xC000_0000u32 as usize) as isize),
        );
    }
    if !ok {
        return Err(
            DeskError::new("BG_DELIVER_FAILED", "组合键投递失败：目标窗口没有接收消息")
                .with_suggestion("先用 `desk status` 确认窗口还在、进程没挂起")
                .delivered(),
        );
    }
    Ok(())
}

fn is_extended(vk: u16) -> bool {
    matches!(
        vk,
        0x21 | 0x22 | 0x23 | 0x24 | 0x25 | 0x26 | 0x27 | 0x28 | 0x2D | 0x2E | 0x6F | 0x90
    )
}

/// 屏幕坐标双击：投给该点下最深的子窗口（与点击一致）。
pub fn double_click_screen(hwnd: HWND, sx: i32, sy: i32) -> Result<bool> {
    let target = deepest_child(hwnd, sx, sy);
    let (cx, cy) = screen_to_client(target, sx, sy);
    double_click_client(target, cx, cy)
}

/// 屏幕坐标拖拽：按下点决定投给哪个子窗口。
pub fn drag_screen(hwnd: HWND, from: (i32, i32), to: (i32, i32), steps: u32) -> Result<()> {
    let target = deepest_child(hwnd, from.0, from.1);
    let (x0, y0) = screen_to_client(target, from.0, from.1);
    let (x1, y1) = screen_to_client(target, to.0, to.1);
    drag_client(target, (x0, y0), (x1, y1), steps)
}

/// 双击：系统平时靠真实输入生成 WM_LBUTTONDBLCLK，投递时得自己补上。
///
/// 目标窗口类没注册 CS_DBLCLKS 时，系统不会把两条按下当双击；这时补第二次
/// down/up 作为退路，并返回 true 表示"是模拟双击"。
pub fn double_click_client(hwnd: HWND, cx: i32, cy: i32) -> Result<bool> {
    const WM_LBUTTONDBLCLK: u32 = 0x0203;
    let lp = lparam_xy(cx, cy);
    let _ = send(hwnd, WM_MOUSEMOVE, 0, lp);
    if !send(hwnd, WM_LBUTTONDOWN, MK_LBUTTON, lp) {
        return Err(DeskError::new("BG_DELIVER_FAILED", "双击：第一次按下失败").delivered());
    }
    let _ = send(hwnd, WM_LBUTTONUP, 0, lp);
    std::thread::sleep(std::time::Duration::from_millis(40));
    // 窗口类没注册 CS_DBLCLKS 时，系统不会把两条按下当双击；DBLCLK 消息也会被
    // DefWindowProc 直接吞掉。这时补第二次完整点击作为退路，并返回 emulated=true。
    if has_dblclks(hwnd) {
        let _ = send(hwnd, WM_LBUTTONDBLCLK, MK_LBUTTON, lp);
        let _ = send(hwnd, WM_LBUTTONUP, 0, lp);
        return Ok(false);
    }
    let _ = send(hwnd, WM_LBUTTONDOWN, MK_LBUTTON, lp);
    let _ = send(hwnd, WM_LBUTTONUP, 0, lp);
    Ok(true)
}

fn has_dblclks(hwnd: HWND) -> bool {
    let style = unsafe { GetClassLongPtrW(hwnd, GCL_STYLE) } as u32;
    style & CS_DBLCLKS.0 != 0
}

/// 把剪贴板内容粘贴到目标窗口。
///
/// 只发 WM_PASTE：以前失败后回退 ctrl+v，但 Ctrl 状态投递不到目标线程，
/// 实际效果是往焦点控件里插入一个 "v"，比失败更糟。
pub fn paste(hwnd: HWND) -> Result<()> {
    const WM_PASTE: u32 = 0x0302;
    if send(hwnd, WM_PASTE, 0, LPARAM(0)) {
        Ok(())
    } else {
        Err(
            DeskError::new("PASTE_FAILED", "WM_PASTE 投递失败，目标窗口没有接收消息")
                .with_suggestion("改用 `type --method value` 直接写值，或先确认目标窗口没挂起")
                .delivered(),
        )
    }
}
/// 逐字符投递 WM_CHAR。ASCII 和大部分 BMP 字符可用；中文输入法文本建议走粘贴。
pub fn type_text(hwnd: HWND, text: &str) -> Result<()> {
    for ch in text.encode_utf16() {
        if !send(hwnd, WM_CHAR, ch as usize, LPARAM(1)) {
            return Err(DeskError::new("BG_DELIVER_FAILED", "投递 WM_CHAR 失败").delivered());
        }
    }
    Ok(())
}

pub fn parse_combo(combo: &str) -> Result<(Vec<VIRTUAL_KEY>, u16)> {
    let mut mods = Vec::new();
    let mut key: Option<u16> = None;
    for part in combo.split('+').map(|s| s.trim().to_ascii_lowercase()) {
        match part.as_str() {
            "ctrl" | "control" => mods.push(VK_CONTROL),
            "shift" => mods.push(VK_SHIFT),
            "alt" => mods.push(VK_MENU),
            "win" | "meta" => mods.push(VK_LWIN),
            _ => {
                if key.is_some() {
                    return Err(DeskError::new(
                        "BAD_ARGS",
                        format!("组合键里出现多个主键：{combo}"),
                    ));
                }
                key = Some(parse_key(&part)?);
            }
        }
    }
    let vk = key.ok_or_else(|| DeskError::new("BAD_ARGS", format!("组合键缺少主键：{combo}")))?;
    Ok((mods, vk))
}

pub fn parse_key(name: &str) -> Result<u16> {
    let vk = match name {
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "space" => 0x20,
        "backspace" => 0x08,
        "delete" | "del" => 0x2E,
        "insert" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "f1" => 0x70,
        "f2" => 0x71,
        "f3" => 0x72,
        "f4" => 0x73,
        "f5" => 0x74,
        "f6" => 0x75,
        "f7" => 0x76,
        "f8" => 0x77,
        "f9" => 0x78,
        "f10" => 0x79,
        "f11" => 0x7A,
        "f12" => 0x7B,
        _ => {
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => {
                    let up = c.to_ascii_uppercase();
                    if up.is_ascii_alphanumeric() {
                        up as u16
                    } else {
                        // 标点/符号键：交给 VkKeyScanW（低字节是 VK，高位是 shift 状态）。
                        let scan = unsafe {
                            windows::Win32::UI::Input::KeyboardAndMouse::VkKeyScanW(c as u16)
                        };
                        if scan == -1 {
                            return Err(DeskError::new(
                                "BAD_ARGS",
                                format!("不认识的按键：{name}"),
                            ));
                        }
                        (scan as u16) & 0xff
                    }
                }
                _ => return Err(DeskError::new("BAD_ARGS", format!("不认识的按键：{name}"))),
            }
        }
    };
    Ok(vk)
}
