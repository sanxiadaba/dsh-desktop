//! 后台输入：把鼠标/键盘消息直接投递到目标窗口。
//!
//! 这一层的意义：不移动真实光标、不抢前台、不依赖屏幕坐标命中，
//! 所以用户可以在前台干别的，动作照样落到目标窗口上。
//!
//! 用 SendMessageTimeout 而不是 PostMessage：保证顺序，也不会因为目标卡住
//! 把调用方一起拖死（超时返回 SMTO_ABORTIFHUNG 错误）。

use crate::error::{DeskError, Result};
use crate::win::{client_to_screen, screen_to_client};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    PostMessageW, SMTO_ABORTIFHUNG, SMTO_BLOCK, SendMessageTimeoutW, WM_CHAR, WM_KEYDOWN,
    WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN,
    WM_RBUTTONUP,
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
    let (cx, cy) = screen_to_client(hwnd, sx, sy);
    click_client(hwnd, cx, cy, button)
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
        .with_suggestion("先用 `desk status` 确认窗口还在；必要时用 --mode physical")
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
    for m in &mods {
        let _ = send(hwnd, WM_KEYDOWN, m.0 as usize, LPARAM(0));
    }
    send(hwnd, WM_KEYDOWN, vk as usize, LPARAM(0x0001_0001));
    std::thread::sleep(std::time::Duration::from_millis(15));
    send(hwnd, WM_KEYUP, vk as usize, LPARAM(0xC001_0001u32 as isize));
    for m in mods.iter().rev() {
        let _ = send(hwnd, WM_KEYUP, m.0 as usize, LPARAM(0xC001_0001u32 as isize));
    }
    Ok(())
}

/// 逐字符投递 WM_CHAR。ASCII 和大部分 BMP 字符可用；中文输入法文本建议走粘贴。
pub fn type_text(hwnd: HWND, text: &str) -> Result<()> {
    for ch in text.encode_utf16() {
        if !send(hwnd, WM_CHAR, ch as usize, LPARAM(1)) {
            return Err(DeskError::new("BG_DELIVER_FAILED", "投递 WM_CHAR 失败").delivered());
        }
        std::thread::sleep(std::time::Duration::from_millis(3));
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

/// 窗口客户区中心对应的屏幕坐标。
pub fn client_center(hwnd: HWND, w: i32, h: i32) -> (i32, i32) {
    client_to_screen(hwnd, w / 2, h / 2)
}

