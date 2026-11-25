//! 后台输入：把鼠标/键盘消息直接投递到目标窗口。
//!
//! 这一层的意义：不移动真实光标、不抢前台、不依赖屏幕坐标命中，
//! 所以用户可以在前台干别的，动作照样落到目标窗口上。
//!
//! 用 SendMessageTimeout 而不是 PostMessage：保证顺序，也不会因为目标卡住
//! 把调用方一起拖死（超时返回 SMTO_ABORTIFHUNG 错误）。

use crate::error::{DeskError, Result};
use crate::win::screen_to_client;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    PostMessageW, SMTO_ABORTIFHUNG, SMTO_BLOCK, SendMessageTimeoutW, WM_CHAR, WM_KEYDOWN, WM_KEYUP,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP,
};

const TIMEOUT_MS: u32 = 2000;
const MK_LBUTTON: usize = 0x0001;
const MK_RBUTTON: usize = 0x0002;

fn lparam_xy(x: i32, y: i32) -> LPARAM {
    let lo = (x as u32 & 0xffff) as isize;
    let hi = ((y as u32 & 0xffff) as isize) << 16;
    LPARAM(lo | hi)
}

fn send(hwnd: HWND, msg: u32, wparam: usize, lparam: LPARAM) -> bool {
    let mut out: usize = 0;
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
    ok.0 != 0
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
pub fn with_foreground_guard<T>(f: impl FnOnce() -> T) -> (T, bool) {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, SetForegroundWindow};
    let before = unsafe { GetForegroundWindow() };
    let out = f();
    let after = unsafe { GetForegroundWindow() };
    if before != after && !before.0.is_null() {
        unsafe {
            let _ = SetForegroundWindow(before);
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
        let now = unsafe { GetForegroundWindow() };
        if now == before {
            return (out, true);
        }
    }
    (out, false)
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
    let mut ok = true;
    for m in &mods {
        ok &= send(hwnd, WM_KEYDOWN, m.0 as usize, LPARAM(0));
    }
    ok &= send(hwnd, WM_KEYDOWN, vk as usize, LPARAM(0x0001_0001));
    std::thread::sleep(std::time::Duration::from_millis(15));
    ok &= send(hwnd, WM_KEYUP, vk as usize, LPARAM(0xC001_0001u32 as isize));
    for m in mods.iter().rev() {
        ok &= send(
            hwnd,
            WM_KEYUP,
            m.0 as usize,
            LPARAM(0xC001_0001u32 as isize),
        );
    }
    if !ok {
        return Err(
            DeskError::new("BG_DELIVER_FAILED", "组合键投递失败：目标窗口没有处理消息")
                .with_suggestion("先用 `desk status` 确认窗口还在、进程没挂起")
                .delivered(),
        );
    }
    Ok(())
}

/// 屏幕坐标双击：投给该点下最深的子窗口（与点击一致）。
pub fn double_click_screen(hwnd: HWND, sx: i32, sy: i32) -> Result<()> {
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
pub fn double_click_client(hwnd: HWND, cx: i32, cy: i32) -> Result<()> {
    const WM_LBUTTONDBLCLK: u32 = 0x0203;
    let lp = lparam_xy(cx, cy);
    let _ = send(hwnd, WM_MOUSEMOVE, 0, lp);
    if !send(hwnd, WM_LBUTTONDOWN, MK_LBUTTON, lp) {
        return Err(DeskError::new("BG_DELIVER_FAILED", "双击：第一次按下失败").delivered());
    }
    let _ = send(hwnd, WM_LBUTTONUP, 0, lp);
    std::thread::sleep(std::time::Duration::from_millis(40));
    if !send(hwnd, WM_LBUTTONDBLCLK, MK_LBUTTON, lp) {
        return Err(DeskError::new("BG_DELIVER_FAILED", "双击：DBLCLK 消息投递失败").delivered());
    }
    let _ = send(hwnd, WM_LBUTTONUP, 0, lp);
    Ok(())
}

/// 把剪贴板内容粘贴到目标窗口：优先 WM_PASTE，不行再发 Ctrl+V。
/// 中文优先用 WM_CHAR（`type` 默认路径），它绕过输入法；这条只在目标不认 WM_CHAR 时用。
pub fn paste(hwnd: HWND) -> Result<()> {
    const WM_PASTE: u32 = 0x0302;
    if send(hwnd, WM_PASTE, 0, LPARAM(0)) {
        return Ok(());
    }
    key_combo(hwnd, "ctrl+v")
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
                        return Err(DeskError::new("BAD_ARGS", format!("不认识的按键：{name}")));
                    }
                }
                _ => return Err(DeskError::new("BAD_ARGS", format!("不认识的按键：{name}"))),
            }
        }
    };
    Ok(vk)
}
