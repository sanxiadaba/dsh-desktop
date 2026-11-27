//! 剪贴板读写（Unicode 文本）。
//!
//! 顺序很重要：先把内存准备好，再清空剪贴板。反过来的话，任何一步失败
//! 都会把用户原有的剪贴板内容清掉且不恢复。

use crate::error::{DeskError, Result};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;

fn to_wide(text: &str) -> Vec<u16> {
    let mut v: Vec<u16> = text.encode_utf16().collect();
    v.push(0);
    v
}

pub fn set_text(text: &str) -> Result<()> {
    let wide = to_wide(text);
    unsafe {
        // 1) 先把数据准备好（失败就还没动用户剪贴板）
        let bytes = wide.len() * 2;
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes)
            .map_err(|e| DeskError::new("CLIPBOARD_FAILED", format!("分配内存失败：{e}")))?;
        let p = GlobalLock(h);
        if p.is_null() {
            let _ = GlobalFree(Some(h));
            return Err(DeskError::new("CLIPBOARD_FAILED", "锁定内存失败"));
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), p as *mut u16, wide.len());
        let _ = GlobalUnlock(h);

        // 2) 再打开并替换
        OpenClipboard(None).map_err(|e| {
            let _ = GlobalFree(Some(h));
            DeskError::new("CLIPBOARD_FAILED", format!("打开剪贴板失败：{e}"))
                .with_suggestion("剪贴板可能被其他程序占用，稍后重试")
        })?;
        let _ = EmptyClipboard();
        if SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(h.0))).is_err() {
            let _ = GlobalFree(Some(h));
            let _ = CloseClipboard();
            return Err(DeskError::new("CLIPBOARD_FAILED", "写入剪贴板失败"));
        }
        let _ = CloseClipboard();
    }
    Ok(())
}

pub fn clear() -> Result<()> {
    unsafe {
        OpenClipboard(None)
            .map_err(|e| DeskError::new("CLIPBOARD_FAILED", format!("打开剪贴板失败：{e}")))?;
        let _ = EmptyClipboard();
        let _ = CloseClipboard();
    }
    Ok(())
}

/// 剪贴板里当前是否有文本（用来判断"粘贴后能不能还原"）。
pub fn has_text() -> bool {
    unsafe {
        windows::Win32::System::DataExchange::IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32)
            .is_ok()
    }
}

pub fn get_text() -> Result<String> {
    // 上限 4 MB 字符（8 MB UTF-16）：剪贴板是别的进程能写的，不能按"扫到 NUL"无限读。
    const MAX_CHARS: usize = 4 * 1024 * 1024;
    unsafe {
        OpenClipboard(None)
            .map_err(|e| DeskError::new("CLIPBOARD_FAILED", format!("打开剪贴板失败：{e}")))?;
        let h = GetClipboardData(CF_UNICODETEXT.0 as u32);
        let out = match h {
            Ok(h) => {
                let hg = HGLOBAL(h.0);
                let p = GlobalLock(hg) as *const u16;
                if p.is_null() {
                    let _ = CloseClipboard();
                    return Err(DeskError::new("CLIPBOARD_FAILED", "锁定剪贴板内存失败"));
                }
                let size = GlobalSize(hg);
                let max_chars = (size as usize / 2).min(MAX_CHARS);
                let mut len = 0usize;
                while len < max_chars && *p.add(len) != 0 {
                    len += 1;
                }
                let terminated = len < max_chars || (max_chars > 0 && *p.add(max_chars - 1) == 0);
                let _ = GlobalUnlock(hg);
                if !terminated {
                    let _ = CloseClipboard();
                    return Err(DeskError::new(
                        "CLIPBOARD_TOO_LARGE",
                        format!(
                            "剪贴板文本没有在 {} 字符内结束，拒绝继续读（可能是异常 owner）",
                            MAX_CHARS
                        ),
                    ));
                }
                String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
            }
            Err(_) => {
                let _ = CloseClipboard();
                return Err(DeskError::new(
                    "NO_TEXT",
                    "剪贴板里没有文本（可能是图片/文件，或刚被别的程序换掉）",
                ));
            }
        };
        let _ = CloseClipboard();
        Ok(out)
    }
}
