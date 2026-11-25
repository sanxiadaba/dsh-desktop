//! 窗口层：枚举、身份、几何、DPI、窗口管理。

use crate::error::{DeskError, Result};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::System::Threading::{
    OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetClassNameW, GetForegroundWindow, GetWindow,
    GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, WS_EX_TOOLWINDOW,
};
use windows::core::BOOL;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub fn center(&self) -> (i32, i32) {
        (self.x + self.width / 2, self.y + self.height / 2)
    }
    pub fn is_empty(&self) -> bool {
        self.width <= 0 || self.height <= 0
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WindowInfo {
    /// 稳定 id：`w-<hwnd 十六进制>`，窗口存活期间跨调用不变。
    pub id: String,
    pub hwnd: u64,
    pub pid: u32,
    pub exe: String,
    pub class: String,
    pub title: String,
    pub rect: Rect,
    pub dpi: u32,
    pub minimized: bool,
    pub visible: bool,
    pub focused: bool,
    pub elevated: bool,
    /// 完整性级别：low / medium / high / system / protected。
    pub integrity: String,
}

pub fn window_id(hwnd: HWND) -> String {
    format!("w-{:x}", hwnd.0 as usize)
}

/// 本进程这次调用里碰到过的窗口（用于判断前台变化是不是我们造成的）。
static TOUCHED: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

pub fn touched_windows() -> Vec<usize> {
    TOUCHED.lock().map(|v| v.clone()).unwrap_or_default()
}

fn remember(hwnd: HWND) {
    if let Ok(mut list) = TOUCHED.lock() {
        let v = hwnd.0 as usize;
        if !list.contains(&v) {
            list.push(v);
        }
    }
}

pub fn parse_window_id(id: &str) -> Result<HWND> {
    let raw = id.strip_prefix("w-").ok_or_else(|| {
        DeskError::new("BAD_WINDOW_ID", format!("窗口 id 格式不对：{id}"))
            .with_suggestion("id 形如 w-1a2b3c，用 `desk windows` 查看")
    })?;
    let v = u64::from_str_radix(raw, 16)
        .map_err(|_| DeskError::new("BAD_WINDOW_ID", format!("窗口 id 不是十六进制：{id}")))?;
    let hwnd = HWND(v as usize as *mut _);
    if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
        return Err(DeskError::new("WINDOW_GONE", format!("窗口已不存在：{id}"))
            .with_suggestion("重新运行 `desk windows` 取最新 id"));
    }
    remember(hwnd);
    Ok(hwnd)
}

pub fn enum_windows() -> Vec<WindowInfo> {
    let mut out: Vec<WindowInfo> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(enum_proc),
            LPARAM(&mut out as *mut Vec<WindowInfo> as isize),
        );
    }
    out
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let out = unsafe { &mut *(lparam.0 as *mut Vec<WindowInfo>) };
    if let Some(info) = unsafe { describe(hwnd, false) } {
        out.push(info);
    }
    BOOL(1)
}

/// 读取一个窗口的完整描述。`include_invisible` 为 false 时跳过不可见、
/// 被 DWM 隐藏、以及只有 owner 的工具窗口。
pub unsafe fn describe(hwnd: HWND, include_invisible: bool) -> Option<WindowInfo> {
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return None;
        }
        let visible = IsWindowVisible(hwnd).as_bool();
        if !visible && !include_invisible {
            return None;
        }
        // DWM 隐藏的窗口（例如挂在另一虚拟桌面上的 UWP）不参与交互。
        let mut cloaked: u32 = 0;
        let hr = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut _ as *mut _,
            std::mem::size_of::<u32>() as u32,
        );
        if hr.is_ok() && cloaked != 0 && !include_invisible {
            return None;
        }
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if (ex_style & WS_EX_TOOLWINDOW.0) != 0 && !include_invisible {
            return None;
        }

        let title = window_title(hwnd);
        let class = window_class(hwnd);
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid as *mut u32));
        let owner = GetWindow(hwnd, GW_OWNER).ok();
        if !include_invisible && title.is_empty() && owner.is_some() {
            return None;
        }

        Some(WindowInfo {
            id: window_id(hwnd),
            hwnd: hwnd.0 as usize as u64,
            pid,
            exe: exe_name(pid),
            class,
            title,
            rect: window_rect(hwnd),
            dpi: GetDpiForWindow(hwnd),
            minimized: IsIconic(hwnd).as_bool(),
            visible,
            focused: GetForegroundWindow() == hwnd,
            elevated: is_elevated(pid),
            integrity: integrity_label(pid),
        })
    }
}

pub fn window_title(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

pub fn window_class(hwnd: HWND) -> String {
    unsafe {
        let mut buf = [0u16; 256];
        let n = GetClassNameW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

/// 窗口真实可见矩形：优先 DWM 扩展边框（去掉 Win10 的隐形阴影边），
/// 失败时退回 GetWindowRect。
pub fn window_rect(hwnd: HWND) -> Rect {
    unsafe {
        let mut r = RECT::default();
        let hr = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut _ as *mut _,
            std::mem::size_of::<RECT>() as u32,
        );
        if hr.is_ok() {
            let rect = Rect {
                x: r.left,
                y: r.top,
                width: r.right - r.left,
                height: r.bottom - r.top,
            };
            if !rect.is_empty() {
                return rect;
            }
        }
        let mut r = RECT::default();
        if GetWindowRect(hwnd, &mut r).is_ok() {
            Rect {
                x: r.left,
                y: r.top,
                width: r.right - r.left,
                height: r.bottom - r.top,
            }
        } else {
            Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            }
        }
    }
}

/// 目标窗口所在线程里真正持有键盘焦点的子控件。
/// 后台窗口的 UIA 元素不会报 focus，但线程的 hwndFocus 一直都在，
/// 键盘消息必须投给它，投给顶层窗口 Win32 程序收不到。
pub fn focus_hwnd(hwnd: HWND) -> Option<HWND> {
    use windows::Win32::UI::WindowsAndMessaging::{GUITHREADINFO, GetGUIThreadInfo};
    let tid = unsafe { GetWindowThreadProcessId(hwnd, None) };
    if tid == 0 {
        return None;
    }
    let mut gi = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    let ok = unsafe { GetGUIThreadInfo(tid, &mut gi) };
    if ok.is_err() {
        return None;
    }
    let focus = gi.hwndFocus;
    if focus.0.is_null() { None } else { Some(focus) }
}

/// 屏幕坐标 → 窗口客户区坐标。
pub fn screen_to_client(hwnd: HWND, x: i32, y: i32) -> (i32, i32) {
    let mut p = windows::Win32::Foundation::POINT { x, y };
    unsafe {
        let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut p);
    }
    (p.x, p.y)
}

pub fn client_to_screen(hwnd: HWND, x: i32, y: i32) -> (i32, i32) {
    let mut p = windows::Win32::Foundation::POINT { x, y };
    unsafe {
        let _ = windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut p);
    }
    (p.x, p.y)
}

pub fn exe_name(pid: u32) -> String {
    if pid == 0 {
        return String::new();
    }
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            h,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(h);
        if ok.is_ok() {
            let full = String::from_utf16_lossy(&buf[..size as usize]);
            return full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string();
        }
        String::new()
    }
}

/// 进程的完整性级别 RID：0x1000 低、0x2000 中、0x3000 高、0x4000 系统。
pub fn integrity_rid(pid: u32) -> Option<u32> {
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
        TOKEN_QUERY, TokenIntegrityLevel,
    };
    if pid == 0 {
        return None;
    }
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut token = HANDLE::default();
        let opened = OpenProcessToken(h, TOKEN_QUERY, &mut token);
        let _ = CloseHandle(h);
        opened.ok()?;

        // 先问需要多大，再按实际大小取：TOKEN_MANDATORY_LABEL 后面还跟着一个 SID。
        let mut need = 0u32;
        let _ = GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut need);
        if need == 0 {
            let _ = CloseHandle(token);
            return None;
        }
        let mut buf = vec![0u8; need as usize];
        let ok = GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buf.as_mut_ptr() as *mut _),
            need,
            &mut need,
        );
        let _ = CloseHandle(token);
        ok.ok()?;

        let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let sid = label.Label.Sid;
        if sid.is_invalid() {
            return None;
        }
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0 {
            return None;
        }
        let rid = *GetSidSubAuthority(sid, (count - 1) as u32);
        Some(rid)
    }
}
pub fn integrity_name(rid: u32) -> &'static str {
    match rid {
        0x0000 => "untrusted",
        0x1000..=0x1fff => "low",
        0x2000..=0x2fff => "medium",
        0x3000..=0x3fff => "high",
        0x4000..=0x4fff => "system",
        _ => "protected",
    }
}

pub fn integrity_label(pid: u32) -> String {
    match integrity_rid(pid) {
        Some(rid) => format!("{} (0x{rid:x})", integrity_name(rid)),
        None => "unknown".to_string(),
    }
}

/// 我们能不能把消息/输入投给这个进程：调用方完整性必须 >= 目标。
pub fn can_reach(target_pid: u32) -> std::result::Result<(), (Option<u32>, Option<u32>)> {
    let me = integrity_rid(std::process::id());
    let them = integrity_rid(target_pid);
    match (me, them) {
        (Some(a), Some(b)) if a < b => Err((Some(a), Some(b))),
        // 读不到目标进程的令牌：多半是受保护进程（PPL）。动作一律拒绝，
        // 消息反正会被内核丢弃，放行只会"看起来成功"。
        (None, _) | (_, None) => Err((me, them)),
        _ => Ok(()),
    }
}

pub fn is_elevated(pid: u32) -> bool {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut token = HANDLE::default();
        let ok = OpenProcessToken(h, windows::Win32::Security::TOKEN_QUERY, &mut token);
        let _ = CloseHandle(h);
        if ok.is_err() {
            return false;
        }
        let mut info = windows::Win32::Security::TOKEN_ELEVATION::default();
        let mut ret = 0u32;
        let ok = windows::Win32::Security::GetTokenInformation(
            token,
            windows::Win32::Security::TokenElevation,
            Some(&mut info as *mut _ as *mut _),
            std::mem::size_of::<windows::Win32::Security::TOKEN_ELEVATION>() as u32,
            &mut ret,
        );
        let _ = CloseHandle(token);
        ok.is_ok() && info.TokenIsElevated != 0
    }
}
