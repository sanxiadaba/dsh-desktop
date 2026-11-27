//! 截图：窗口优先 PrintWindow（能拍被遮挡的窗口），失败退回 BitBlt。
//!
//! 两张图都会带回 `origin`（图像像素 (0,0) 对应的屏幕坐标）和 `blank`
//! （整幅只有一种颜色——多半是没渲染/DirectX/受保护，不一定是真的白屏）。

use crate::error::{DeskError, Result};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetSystemMetrics, GetWindowRect, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// 单张图的像素上限（BGRA 字节）。超过就拒绝，避免 8K/多屏把内存打满。
const MAX_IMAGE_BYTES: u64 = 256 * 1024 * 1024;

pub struct Image {
    pub width: u32,
    pub height: u32,
    /// 图像像素 (0,0) 对应的屏幕坐标。
    pub origin_x: i32,
    pub origin_y: i32,
    /// BGRA，自顶向下。
    pub bgra: Vec<u8>,
    /// 整幅只有一种颜色。
    pub blank: bool,
}

impl Image {
    pub fn origin(&self) -> (i32, i32) {
        (self.origin_x, self.origin_y)
    }
}

/// 截取窗口客户区（1:1 像素，和 UIA 的屏幕坐标一一对应）。
///
/// 做法：先 PrintWindow 拍整个窗口（含标题栏，被遮挡也能拍），
/// 再按客户区相对窗口的偏移裁出客户区。直接按客户区尺寸拍会把整窗内容压扁。
pub fn window(hwnd: HWND) -> Result<Image> {
    let info = unsafe { crate::win::describe(hwnd, true) }
        .ok_or_else(|| DeskError::new("WINDOW_GONE", "目标窗口已消失"))?;
    if info.minimized {
        return Err(DeskError::new(
            "SHOT_EMPTY",
            "窗口已最小化，客户区没有可拍的内容；先 restore 再截图",
        ));
    }
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

    let full = capture_at(hwnd, ww, wh, true, (off_x, off_y))?;
    let mut img = crop(&full, off_x, off_y, cw, ch);
    img.origin_x = origin.x;
    img.origin_y = origin.y;
    img.blank = all_same(&img.bgra);
    Ok(img)
}

fn crop(full: &Image, x: i32, y: i32, w: i32, h: i32) -> Image {
    let (uw, uh) = (full.width as i32, full.height as i32);
    let x = x.clamp(0, (uw - 1).max(0));
    let y = y.clamp(0, (uh - 1).max(0));
    let w = w.min(uw - x).max(1);
    let h = h.min(uh - y).max(1);
    let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
    for row in 0..h {
        let src = (((y + row) * uw + x) * 4) as usize;
        let dst = (row * w * 4) as usize;
        out[dst..dst + (w * 4) as usize].copy_from_slice(&full.bgra[src..src + (w * 4) as usize]);
    }
    Image {
        width: w as u32,
        height: h as u32,
        origin_x: 0,
        origin_y: 0,
        bgra: out,
        blank: false,
    }
}

pub fn screen() -> Result<Image> {
    let (x, y, w, h) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if w <= 0 || h <= 0 {
        return Err(DeskError::new("SHOT_FAILED", "虚拟桌面尺寸为 0"));
    }
    // 屏幕 DC 的 (0,0) 就是虚拟桌面左上角，所以 BitBlt 起点要减掉虚拟原点。
    let mut img = capture_at(HWND::default(), w, h, false, (-x, -y))?;
    img.origin_x = x;
    img.origin_y = y;
    img.blank = all_same(&img.bgra);
    Ok(img)
}

fn capture_at(
    hwnd: HWND,
    w: i32,
    h: i32,
    want_print_window: bool,
    blit_at: (i32, i32),
) -> Result<Image> {
    let bytes = (w as u64)
        .checked_mul(h as u64)
        .and_then(|v| v.checked_mul(4))
        .filter(|v| *v <= MAX_IMAGE_BYTES)
        .ok_or_else(|| {
            DeskError::new(
                "SHOT_FAILED",
                format!(
                    "窗口尺寸过大，拒绝截图（{}x{}，上限 {} MB）",
                    w,
                    h,
                    MAX_IMAGE_BYTES / 1024 / 1024
                ),
            )
        })?;
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
            // GetDC(hwnd) 给的是客户区 DC，客户区在整窗里有偏移，必须贴到对应位置。
            let _ = BitBlt(
                mem_dc,
                blit_at.0,
                blit_at.1,
                w,
                h,
                Some(src_dc),
                0,
                0,
                SRCCOPY,
            );
        }

        // 读回像素：BITMAPINFO 里给负高度，得到自顶向下的 BGRA。
        let mut bi = BITMAPINFO::default();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -h;
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB.0;
        let mut buf = vec![0u8; bytes as usize];
        // MSDN：GetDIBits 调用时位图不能还选在 DC 里。
        let _ = SelectObject(mem_dc, old);
        let got = GetDIBits(
            mem_dc,
            bmp,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut _),
            &mut bi,
            DIB_RGB_COLORS,
        );
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(Some(hwnd), src_dc);

        if got == 0 {
            return Err(DeskError::new("SHOT_FAILED", "读取截图像素失败"));
        }
        Ok(Image {
            width: w as u32,
            height: h as u32,
            origin_x: 0,
            origin_y: 0,
            bgra: buf,
            blank: false,
        })
    }
}

/// 整幅是否只有一种颜色（采样式，遇到第一个不同就返回 false）。
fn all_same(bgra: &[u8]) -> bool {
    let Some(first) = bgra.get(0..4) else {
        return true;
    };
    bgra.chunks_exact(4).all(|px| px == first)
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
    for px in img.bgra.chunks_exact(4) {
        rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
    }
    writer
        .write_image_data(&rgba)
        .map_err(|e| DeskError::new("SHOT_FAILED", format!("写 PNG 数据失败：{e}")))?;
    Ok(())
}

/// 一个待标注的框：编号、图像坐标（客户区相对）。
pub struct Mark {
    pub n: usize,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// 在图上画编号框（Set-of-Marks）。
///
/// 只画边框和编号，不依赖字体/DPI：数字用 3x5 点阵，放大 3 倍。
pub fn draw_marks(img: &mut Image, marks: &[Mark]) {
    for m in marks {
        draw_rect(img, m.x, m.y, m.w, m.h, [220, 40, 40]);
        let label = m.n.to_string();
        let scale = 3i32;
        let pad = 3i32;
        let cw = 3 * scale + 2;
        let ch = 5 * scale + 2;
        let bx = (m.x + 2).max(0);
        let by = (m.y + 2).max(0);
        // 白底，保证在任意背景上都能读。
        fill_rect(
            img,
            bx - pad,
            by - pad,
            label.len() as i32 * cw + pad * 2,
            ch + pad * 2,
            [255, 255, 255],
        );
        let mut cx = bx;
        for digit in label.bytes() {
            draw_digit(img, cx, by, digit, scale, [20, 20, 20]);
            cx += cw;
        }
    }
}

fn draw_rect(img: &mut Image, x: i32, y: i32, w: i32, h: i32, color: [u8; 3]) {
    if w <= 0 || h <= 0 {
        return;
    }
    for t in 0..2 {
        for i in 0..w {
            put(img, x + i, y + t, color);
            put(img, x + i, y + h - 1 - t, color);
        }
        for j in 0..h {
            put(img, x + t, y + j, color);
            put(img, x + w - 1 - t, y + j, color);
        }
    }
}

fn fill_rect(img: &mut Image, x: i32, y: i32, w: i32, h: i32, color: [u8; 3]) {
    for j in 0..h {
        for i in 0..w {
            put(img, x + i, y + j, color);
        }
    }
}

fn put(img: &mut Image, x: i32, y: i32, color: [u8; 3]) {
    if x < 0 || y < 0 || x >= img.width as i32 || y >= img.height as i32 {
        return;
    }
    let idx = ((y as usize) * img.width as usize + x as usize) * 4;
    img.bgra[idx] = color[2];
    img.bgra[idx + 1] = color[1];
    img.bgra[idx + 2] = color[0];
    img.bgra[idx + 3] = 255;
}

/// 3x5 点阵数字。每个字节的低 5 位是一行，按行从上到下。
fn digit_rows(d: u8) -> [u8; 5] {
    match d {
        b'0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        b'1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        b'2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        b'3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        b'4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        b'5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        b'6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        b'7' => [0b111, 0b001, 0b010, 0b010, 0b010],
        b'8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        b'9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        _ => [0; 5],
    }
}

fn draw_digit(img: &mut Image, x: i32, y: i32, digit: u8, scale: i32, color: [u8; 3]) {
    let rows = digit_rows(digit);
    for (ry, bits) in rows.iter().enumerate() {
        for rx in 0..3 {
            if bits & (1 << (2 - rx)) != 0 {
                for sy in 0..scale {
                    for sx in 0..scale {
                        put(img, x + rx * scale + sx, y + ry as i32 * scale + sy, color);
                    }
                }
            }
        }
    }
}
