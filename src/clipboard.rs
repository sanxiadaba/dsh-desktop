//! 剪贴板读写（Unicode 文本）。

use crate::error::{DeskError, Result};
use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;

fn to_wide(text: &str) -> Vec<u16> {
    let mut v: Vec<u16> = text.encode_utf16().collect();
    v.push(0);
    v
}

pub fn set_text(text: &str) -> Result<()> {
    let wide = to_wide(text);
    unsafe {
        OpenClipboard(None).map_err(|e| {
            DeskError::new("CLIPBOARD_FAILED", format!("打开剪贴板失败：{e}"))
                .with_suggestion("剪贴板可能被其他程序占用，稍后重试")
        })?;
        let _ = EmptyClipboard();
        let bytes = wide.len() * 2;
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes)
            .map_err(|e| DeskError::new("CLIPBOARD_FAILED", format!("分配内存失败：{e}")))?;
        let p = GlobalLock(h);
        if p.is_null() {
            let _ = CloseClipboard();
            return Err(DeskError::new("CLIPBOARD_FAILED", "锁定内存失败"));
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), p as *mut u16, wide.len());
        let _ = GlobalUnlock(h);
        if SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(h.0))).is_err() {
            let _ = CloseClipboard();
            return Err(DeskError::new("CLIPBOARD_FAILED", "写入剪贴板失败"));
        }
        let _ = CloseClipboard();
    }
    Ok(())
}

pub fn get_text() -> Result<String> {
    unsafe {
        OpenClipboard(None)
            .map_err(|e| DeskError::new("CLIPBOARD_FAILED", format!("打开剪贴板失败：{e}")))?;
        let h = GetClipboardData(CF_UNICODETEXT.0 as u32);
        let out = match h {
            Ok(h) => {
                let hg = HGLOBAL(h.0);
                let p = GlobalLock(hg) as *const u16;
                if p.is_null() {
                    String::new()
                } else {
                    let mut len = 0usize;
                    while *p.add(len) != 0 {
                        len += 1;
                    }
                    let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
                    let _ = GlobalUnlock(hg);
                    s
                }
            }
            Err(_) => String::new(),
        };
        let _ = CloseClipboard();
        Ok(out)
    }
}
