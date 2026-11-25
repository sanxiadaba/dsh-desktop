//! 物理输入（SendInput）：会移动真实光标、打进当前前台窗口。
//!
//! 只在语义和后台注入都进不去的程序上才用（DirectX 独占、UIPI 隔离等），
//! 调用方必须显式选择，工具不会自己切过来。

use crate::error::{DeskError, Result};
use crate::input;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEINPUT,
    SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;

fn send_inputs(inputs: &[INPUT]) -> Result<u32> {
    let n = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if n == 0 {
        return Err(DeskError::new(
            "PHYSICAL_FAILED",
            "SendInput 被系统拒绝（可能被 UIPI 拦截或当前会话不是交互桌面）",
        )
        .delivered());
    }
    Ok(n)
}

fn key_input(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn unicode_input(unit: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: if up {
                    KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
                } else {
                    KEYEVENTF_UNICODE
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse_input(flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

pub fn click(sx: i32, sy: i32) -> Result<()> {
    unsafe {
        SetCursorPos(sx, sy)
            .map_err(|e| DeskError::new("PHYSICAL_FAILED", format!("移动光标失败：{e}")))?;
    }
    std::thread::sleep(std::time::Duration::from_millis(30));
    send_inputs(&[mouse_input(MOUSEEVENTF_LEFTDOWN)])?;
    std::thread::sleep(std::time::Duration::from_millis(30));
    send_inputs(&[mouse_input(MOUSEEVENTF_LEFTUP)])?;
    Ok(())
}

pub fn type_text(text: &str) -> Result<()> {
    let mut batch: Vec<INPUT> = Vec::with_capacity(text.len() * 2);
    for unit in text.encode_utf16() {
        batch.push(unicode_input(unit, false));
        batch.push(unicode_input(unit, true));
    }
    for chunk in batch.chunks(64) {
        send_inputs(chunk)?;
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    Ok(())
}

pub fn key_combo(combo: &str) -> Result<()> {
    let (mods, vk) = input::parse_combo(combo)?;
    let mut down: Vec<INPUT> = mods.iter().map(|m| key_input(m.0, false)).collect();
    down.push(key_input(vk, false));
    send_inputs(&down)?;
    std::thread::sleep(std::time::Duration::from_millis(20));
    let mut up: Vec<INPUT> = vec![key_input(vk, true)];
    up.extend(mods.iter().rev().map(|m| key_input(m.0, true)));
    send_inputs(&up)?;
    Ok(())
}
