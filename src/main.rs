mod act;
mod cli;
mod error;
mod input;
mod selftest;
mod shot;
mod store;
mod uia;
mod win;

use cli::Args;
use error::{DeskError, Result};
use serde_json::{Value, json};
use std::time::Instant;
use uia::{Uia, WalkOptions};
use windows::Win32::Foundation::HWND;

fn main() {
    // 进程级 DPI 感知：坐标全部按物理像素处理，和 UIA / SendInput 一致。
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    let started = Instant::now();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match run(&argv) {
        Ok((cmd, data)) => {
            let envelope = json!({
                "version": env!("CARGO_PKG_VERSION"),
                "ok": true,
                "command": cmd,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "data": data,
            });
            println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
        }
        Err(e) => {
            let cmd = argv.first().cloned().unwrap_or_default();
            let envelope = json!({
                "version": env!("CARGO_PKG_VERSION"),
                "ok": false,
                "command": cmd,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "error": e.to_json(),
            });
            println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
            std::process::exit(1);
        }
    }
}

fn run(argv: &[String]) -> Result<(String, Value)> {
    let Some(cmd) = argv.first().map(|s| s.as_str()) else {
        return Err(DeskError::new("BAD_ARGS", "缺少子命令").with_suggestion("desk help"));
    };
    let args = Args::new(&argv[1..]);
    if args.has("--help") && cmd != "help" {
        return Ok((cmd.to_string(), json!({ "help": cli::help_for(cmd) })));
    }
    match cmd {
        "help" | "--help" | "-h" => {
            let topic = argv.get(1).map(|s| s.as_str()).unwrap_or("");
            Ok(("help".into(), json!({ "help": cli::help_for(topic) })))
        }
        "status" => Ok(("status".into(), status()?)),
        "windows" => Ok(("windows".into(), windows_cmd(&args)?)),
        "snap" => Ok(("snap".into(), snap_cmd(&args)?)),
        "find" => Ok(("find".into(), find_cmd(&args)?)),
        "click" => {
            let r = args.positionals();
            let ref_id = r.first().ok_or_else(|| need("<@ref>"))?;
            let mode = match args.opt("--mode") {
                Some(m) => Some(act::parse_mode(&m)?),
                None => None,
            };
            let uia = Uia::new()?;
            Ok((cmd.into(), act::click(&uia, ref_id, mode)?))
        }
        "select" | "toggle" | "expand" | "collapse" => {
            let r = args.positionals();
            let ref_id = r.first().ok_or_else(|| need("<@ref>"))?;
            let uia = Uia::new()?;
            let v = match cmd {
                "select" => act::select(&uia, ref_id)?,
                "toggle" => act::toggle(&uia, ref_id)?,
                "expand" => act::expand(&uia, ref_id, true)?,
                _ => act::expand(&uia, ref_id, false)?,
            };
            Ok((cmd.into(), v))
        }
        "set" => {
            let r = args.positionals();
            let ref_id = r.first().ok_or_else(|| need("<@ref> <value>"))?;
            let value = r.get(1).ok_or_else(|| need("<@ref> <value>"))?;
            let uia = Uia::new()?;
            Ok((cmd.into(), act::set_value(&uia, ref_id, value)?))
        }
        "key" => {
            let r = args.positionals();
            let combo = r.first().ok_or_else(|| need("<组合键>"))?;
            let hwnd = target_window(&args)?;
            Ok((cmd.into(), act::key(hwnd, combo)?))
        }
        "type" => {
            let r = args.positionals();
            let text = r.first().ok_or_else(|| need("<文本>"))?;
            let hwnd = target_window(&args)?;
            Ok((cmd.into(), act::type_text(hwnd, text)?))
        }
        "wheel" | "scroll" => {
            let hwnd = target_window(&args)?;
            let delta = args
                .int("--delta")?
                .or(args.int("--amount")?)
                .unwrap_or(-120) as i32;
            let (x, y) = match args.opt("--xy") {
                Some(s) => parse_xy(&s)?,
                None => {
                    let info = unsafe { win::describe(hwnd, true) }.ok_or_else(|| {
                        DeskError::new("WINDOW_GONE", "目标窗口已消失")
                    })?;
                    info.rect.center()
                }
            };
            Ok((cmd.into(), act::wheel(hwnd, x, y, delta)?))
        }
        "drag" => {
            let r = args.positionals();
            let from = parse_xy(r.first().ok_or_else(|| need("<x1,y1> <x2,y2>"))?)?;
            let to = parse_xy(r.get(1).ok_or_else(|| need("<x1,y1> <x2,y2>"))?)?;
            let hwnd = target_window(&args)?;
            Ok((cmd.into(), act::drag(hwnd, from, to)?))
        }
        "shot" => Ok(("shot".into(), shot_cmd(&args)?)),
        "selftest" => Ok(("selftest".into(), selftest::run()?)),
        "focus" | "minimize" | "maximize" | "restore" | "close" => {
            Ok((cmd.into(), window_action(cmd, &args)?))
        }
        "move" | "resize" => Ok((cmd.into(), window_geometry(cmd, &args)?)),
        "launch" => Ok(("launch".into(), launch_cmd(&args)?)),
        other => Err(DeskError::new("BAD_ARGS", format!("未知子命令：{other}"))
            .with_suggestion("desk help")),
    }
}

fn need(what: &str) -> DeskError {
    DeskError::new("BAD_ARGS", format!("缺少参数：{what}")).with_suggestion("desk help")
}

pub fn state_dir() -> std::path::PathBuf {
    let base = std::env::var_os("USERPROFILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(".dsh").join("desk")
}

fn status() -> Result<Value> {
    let all = win::enum_windows();
    let foreground = all.iter().find(|w| w.focused);
    Ok(json!({
        "exe": std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default(),
        "pid": std::process::id(),
        "elevated": win::is_elevated(std::process::id()),
        "windows": all.len(),
        "foreground": foreground.map(|w| json!({"id": w.id, "title": w.title, "exe": w.exe})),
        "state_dir": state_dir().display().to_string(),
    }))
}

fn windows_cmd(args: &Args) -> Result<Value> {
    args.reject_unknown(&["--all", "--json"])?;
    let mut all = win::enum_windows();
    if !args.has("--all") {
        all.retain(|w| !w.title.is_empty() && w.pid != std::process::id());
    }
    all.sort_by(|a, b| a.pid.cmp(&b.pid).then(a.title.cmp(&b.title)));
    Ok(json!(all))
}

fn walk_options(args: &Args) -> Result<WalkOptions> {
    Ok(WalkOptions {
        max_depth: args.int("--depth")?.unwrap_or(16) as usize,
        limit: args.int("--limit")?.unwrap_or(1200) as usize,
        raw_view: args.has("--raw"),
    })
}

fn snap_cmd(args: &Args) -> Result<Value> {
    args.reject_unknown(&[
        "--window", "--depth", "--limit", "--raw", "--actionable", "--name", "--text",
    ])?;
    let hwnd = target_window(args)?;
    let opts = walk_options(args)?;
    let uia = Uia::new()?;
    let mut out = act::snap(&uia, hwnd, &opts)?;
    if args.has("--actionable") {
        filter_elements(&mut out, |e| {
            e.get("actions")
                .and_then(|a| a.as_array())
                .is_some_and(|a| !a.is_empty())
        });
    }
    if let Some(name) = args.opt("--name").or(args.opt("--text")) {
        let needle = name.to_lowercase();
        filter_elements(&mut out, |e| {
            e.get("name")
                .and_then(|n| n.as_str())
                .map(|n| n.to_lowercase().contains(&needle))
                .unwrap_or(false)
        });
    }
    Ok(out)
}

fn filter_elements(out: &mut Value, keep: impl Fn(&Value) -> bool) {
    if let Some(arr) = out.get_mut("elements").and_then(|e| e.as_array_mut()) {
        arr.retain(|e| keep(e));
        let n = arr.len();
        out["count"] = json!(n);
    }
}

fn find_cmd(args: &Args) -> Result<Value> {
    let uia = Uia::new()?;
    let mut out = snap_cmd(args)?;
    let selector = args.positionals().first().cloned().unwrap_or_default();
    if selector.is_empty() {
        return Ok(out);
    }
    let (role_part, name_part) = match selector.split_once(':') {
        Some((r, n)) if is_role(r) => (Some(r.to_string()), n.to_string()),
        _ => (None, selector.clone()),
    };
    filter_elements(&mut out, |e| {
        let role_ok = role_part
            .as_ref()
            .map(|r| e.get("role").and_then(|v| v.as_str()) == Some(r.as_str()))
            .unwrap_or(true);
        let name_ok = e
            .get("name")
            .and_then(|v| v.as_str())
            .map(|n| n.contains(&name_part))
            .unwrap_or(false);
        role_ok && name_ok
    });
    Ok(out)
}

fn is_role(s: &str) -> bool {
    matches!(
        s,
        "button"
            | "checkbox"
            | "combobox"
            | "edit"
            | "link"
            | "image"
            | "listitem"
            | "list"
            | "menu"
            | "menuitem"
            | "radio"
            | "tab"
            | "tabitem"
            | "text"
            | "toolbar"
            | "tree"
            | "treeitem"
            | "custom"
            | "group"
            | "document"
            | "window"
            | "pane"
            | "datagrid"
            | "dataitem"
            | "slider"
            | "spinner"
            | "progressbar"
            | "statusbar"
            | "tooltip"
            | "titlebar"
            | "splitbutton"
            | "header"
            | "headeritem"
            | "table"
            | "menubar"
            | "scrollbar"
    )
}

fn target_window(args: &Args) -> Result<HWND> {
    if let Some(id) = args.opt("--window") {
        return win::parse_window_id(&id);
    }
    if let Some(r) = args.opt("--ref") {
        let body = r.trim_start_matches('@');
        if let Some((sid, _)) = body.split_once(':') {
            let snap = store::load(sid)?;
            return win::parse_window_id(&snap.window_id);
        }
    }
    {
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.0.is_null() {
            return Err(DeskError::new("NO_FOREGROUND", "当前没有前台窗口"));
        }
        Ok(hwnd)
    }
}

fn parse_xy(s: &str) -> Result<(i32, i32)> {
    let (a, b) = s
        .split_once(',')
        .ok_or_else(|| DeskError::new("BAD_ARGS", format!("坐标格式应为 x,y：{s}")))?;
    let x = a
        .trim()
        .parse::<i32>()
        .map_err(|_| DeskError::new("BAD_ARGS", format!("x 不是整数：{a}")))?;
    let y = b
        .trim()
        .parse::<i32>()
        .map_err(|_| DeskError::new("BAD_ARGS", format!("y 不是整数：{b}")))?;
    Ok((x, y))
}

fn shot_cmd(args: &Args) -> Result<Value> {
    args.reject_unknown(&["--window", "--out", "--screen"])?;
    let img = if args.has("--screen") {
        shot::screen()?
    } else {
        let hwnd = target_window(args)?;
        shot::window(hwnd)?
    };
    let path = match args.opt("--out") {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let dir = state_dir().join("shots");
            let _ = std::fs::create_dir_all(&dir);
            dir.join(format!("shot-{}.png", now_ms()))
        }
    };
    shot::save_png(&img, &path)?;
    Ok(json!({
        "path": path.display().to_string(),
        "width": img.width,
        "height": img.height,
    }))
}

fn window_action(cmd: &str, args: &Args) -> Result<Value> {
    use windows::Win32::UI::WindowsAndMessaging::{
        SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SetForegroundWindow, ShowWindow, WM_CLOSE,
    };
    let r = args.positionals();
    let id = r.first().ok_or_else(|| need("<窗口 id>"))?;
    let hwnd = win::parse_window_id(id)?;
    match cmd {
        "focus" => {
            let ok = unsafe { SetForegroundWindow(hwnd) }.as_bool();
            Ok(json!({"id": id, "focused": ok, "note": "focus 会抢前台，只在必要时用"}))
        }
        "minimize" => {
            unsafe { let _ = ShowWindow(hwnd, SW_MINIMIZE); }
            Ok(json!({"id": id, "minimized": true}))
        }
        "maximize" => {
            unsafe { let _ = ShowWindow(hwnd, SW_MAXIMIZE); }
            Ok(json!({"id": id, "maximized": true}))
        }
        "restore" => {
            unsafe { let _ = ShowWindow(hwnd, SW_RESTORE); }
            Ok(json!({"id": id, "restored": true}))
        }
        _ => {
            let ok = unsafe {
                windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(hwnd),
                    WM_CLOSE,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(0),
                )
            }
            .is_ok();
            Ok(json!({"id": id, "close_sent": ok}))
        }
    }
}

fn window_geometry(cmd: &str, args: &Args) -> Result<Value> {
    use windows::Win32::UI::WindowsAndMessaging::{
        SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos,
    };
    let r = args.positionals();
    let id = r.first().ok_or_else(|| need("<窗口 id> <x> <y>|<w> <h>"))?;
    let a = r
        .get(1)
        .and_then(|s| s.parse::<i32>().ok())
        .ok_or_else(|| need("<x> <y> 或 <w> <h>"))?;
    let b = r
        .get(2)
        .and_then(|s| s.parse::<i32>().ok())
        .ok_or_else(|| need("<x> <y> 或 <w> <h>"))?;
    let hwnd = win::parse_window_id(id)?;
    let info = unsafe { win::describe(hwnd, true) }
        .ok_or_else(|| DeskError::new("WINDOW_GONE", "目标窗口已消失"))?;
    let (x, y, w, h) = if cmd == "move" {
        (a, b, info.rect.width, info.rect.height)
    } else {
        (info.rect.x, info.rect.y, a, b)
    };
    unsafe {
        SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE)
    }
    .map_err(|e| DeskError::new("WINDOW_MOVE_FAILED", format!("移动/缩放窗口失败：{e}")))?;
    Ok(json!({"id": id, "rect": {"x": x, "y": y, "w": w, "h": h}}))
}

fn launch_cmd(args: &Args) -> Result<Value> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let r = args.positionals();
    let target = r.first().ok_or_else(|| need("<exe 或路径>"))?;
    let dir = args.opt("--dir");
    let dir_h = dir.as_deref().map(windows::core::HSTRING::from);
    let hinst = unsafe {
        ShellExecuteW(
            None,
            &windows::core::HSTRING::from("open"),
            &windows::core::HSTRING::from(target.as_str()),
            None,
            dir_h
                .as_ref()
                .map(|h| windows::core::PCWSTR(h.as_ptr()))
                .unwrap_or(windows::core::PCWSTR::null()),
            SW_SHOWNORMAL,
        )
    };
    let code = hinst.0 as isize;
    if code <= 32 {
        return Err(DeskError::new(
            "LAUNCH_FAILED",
            format!("启动失败（ShellExecute 返回 {code}）：{target}"),
        )
        .with_suggestion("确认路径存在，或直接给完整 exe 路径"));
    }
    Ok(json!({"launched": target}))
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
