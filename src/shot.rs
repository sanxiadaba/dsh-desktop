//! 截图：窗口优先 PrintWindow（能拍被遮挡的窗口），失败退回 BitBlt。

use crate::error::{DeskError, Result};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, GetWindowRect};

pub struct Image {
    pub width: u32,
    pub height: u32,
    /// BGRA，自顶向下。
    pub bgra: Vec<u8>,
}

/// 截取窗口客户区（1:1 像素，和 UIA 的屏幕坐标一一对应）。
///
/// 做法：先 PrintWindow 拍整个窗口（含标题栏，被遮挡也能拍），
/// 再按客户区相对窗口的偏移裁出客户区。直接按客户区尺寸拍会把整窗内容压扁。
pub fn window(hwnd: HWND) -> Result<Image> {
    let mut client = RECT::default();
    unsafe { GetClientRect(hwnd, &mut client) }
        .map_err(|e| DeskError::new("SHOT_FAILED", format!("读取窗口客户区失败：{e}")))?;
    let (cw, ch) = (client.right - client.left, client.bottom - client.top);
    if cw <= 0 || ch <= 0 {
        return Err(DeskError::new(
            "SHOT_EMPTY",
            "窗口客户区是空的（可能已最小化），先 restore 再截图",
        ));
    }

    let mut win_rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut win_rect) }
        .map_err(|e| DeskError::new("SHOT_FAILED", format!("读取窗口矩形失败：{e}")))?;
    let (ww, wh) = (
        win_rect.right - win_rect.left,
        win_rect.bottom - win_rect.top,
    );
    let mut origin = POINT { x: 0, y: 0 };
    unsafe {
        let _ = ClientToScreen(hwnd, &mut origin);
    }
    let off_x = (origin.x - win_rect.left).max(0);
    let off_y = (origin.y - win_rect.top).max(0);

    let full = capture(hwnd, ww, wh, true)?;
    Ok(crop(&full, off_x, off_y, cw, ch))
}

fn crop(full: &Image, x: i32, y: i32, w: i32, h: i32) -> Image {
    let (uw, uh) = (full.width as i32, full.height as i32);
    let x = x.clamp(0, uw.saturating_sub(1));
    let y = y.clamp(0, uh.saturating_sub(1));
    let w = w.min(uw - x);
    let h = h.min(uh - y);
    let mut out = vec![0u8; (w * h * 4) as usize];
    for row in 0..h {
        let src = (((y + row) * uw + x) * 4) as usize;
        let dst = (row * w * 4) as usize;
        out[dst..dst + (w * 4) as usize].copy_from_slice(&full.bgra[src..src + (w * 4) as usize]);
    }
    Image {
        width: w as u32,
        height: h as u32,
        bgra: out,
    }
}
pub fn screen() -> Result<Image> {
    let (w, h) = unsafe {
        (
            windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                windows::Win32::UI::WindowsAndMessaging::SM_CXSCREEN,
            ),
            windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                windows::Win32::UI::WindowsAndMessaging::SM_CYSCREEN,
            ),
        )
    };
    capture(HWND::default(), w, h, false)
}

fn capture(hwnd: HWND, w: i32, h: i32, want_print_window: bool) -> Result<Image> {
    unsafe {
        let src_dc = if hwnd.is_invalid() {
            GetDC(None)
        } else {
            GetDC(Some(hwnd))
        };
        if src_dc.is_invalid() {
            return Err(DeskError::new("SHOT_FAILED", "取窗口 DC 失败"));
        }
        let mem_dc = CreateCompatibleDC(Some(src_dc));
        let bmp = CreateCompatibleBitmap(src_dc, w, h);
        let old = SelectObject(mem_dc, bmp.into());

        let mut printed = false;
        if want_print_window {
            // PW_RENDERFULLCONTENT = 2：让 Chromium/Qt 这类自绘窗口也能取到内容。
            printed = PrintWindow(hwnd, mem_dc, PRINT_WINDOW_FLAGS(2)).as_bool();
        }
        if !printed {
            let _ = BitBlt(mem_dc, 0, 0, w, h, Some(src_dc), 0, 0, SRCCOPY);
        }

        // 读回像素：BITMAPINFO 里给负高度，得到自顶向下的 BGRA。
        let mut bi = BITMAPINFO::default();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -h;
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB.0;
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let got = GetDIBits(
            mem_dc,
            bmp,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut _),
            &mut bi,
            DIB_RGB_COLORS,
        );
        let _ = SelectObject(mem_dc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(Some(hwnd), src_dc);

        if got == 0 {
            return Err(DeskError::new("SHOT_FAILED", "读取截图像素失败"));
        }
        Ok(Image {
            width: w as u32,
            height: h as u32,
            bgra: buf,
        })
    }
}

/// 写 PNG。BGRA 在这里换成 RGBA。
pub fn save_png(img: &Image, path: &std::path::Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::File::create(path)
        .map_err(|e| DeskError::new("SHOT_FAILED", format!("创建文件失败：{e}")))?;
    let w = std::io::BufWriter::new(file);
    let mut enc = png::Encoder::new(w, img.width, img.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc
        .write_header()
        .map_err(|e| DeskError::new("SHOT_FAILED", format!("写 PNG 头失败：{e}")))?;
    let mut rgba = Vec::with_capacity(img.bgra.len());
    for px in img.bgra.as_chunks::<4>().0 {
        rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
    }
    writer
        .write_image_data(&rgba)
        .map_err(|e| DeskError::new("SHOT_FAILED", format!("写 PNG 数据失败：{e}")))?;
    Ok(())
}
