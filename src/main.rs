mod act;
mod cli;
mod clipboard;
mod error;
mod input;
mod selftest;
mod shot;
mod store;
#[cfg(test)]
mod tests;
mod uia;
mod win;

use cli::Args;
use error::{DeskError, Result};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
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
    let fg0 = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };

    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    let pretty = argv.iter().any(|a| a == "--pretty");
    argv.retain(|a| a != "--pretty");
    // 提权子进程用这个把自己那份 JSON 写回文件（跨 UAC 拿不到管道）。
    let mut result_file: Option<std::path::PathBuf> = None;
    if argv.len() >= 2 && argv[0] == "--result-file" {
        result_file = Some(std::path::PathBuf::from(argv[1].clone()));
        argv.drain(0..2);
    }

    // help 直接输出纯文本，不套 JSON 信封；其他命令一律 JSON。
    if matches!(
        argv.first().map(|s| s.as_str()),
        Some("help") | Some("--help") | Some("-h")
    ) || (argv.len() == 1 && argv[0] == "help")
    {
        let topic = argv.get(1).map(|s| s.as_str()).unwrap_or("");
        println!("{}", cli::help_for(topic));
        return;
    }

    match run(&argv) {
        Ok((cmd, data)) => {
            let restored = settle_foreground(fg0, &cmd);
            let envelope = json!({
                "version": env!("CARGO_PKG_VERSION"),
                "ok": true,
                "command": cmd,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "side_effects": { "foreground_restored": restored },
                "data": data,
            });
            emit(&envelope, result_file.as_deref(), pretty);
        }
        Err(e) => {
            let cmd = argv.first().cloned().unwrap_or_default();
            let restored = settle_foreground(fg0, &cmd);
            let envelope = json!({
                "version": env!("CARGO_PKG_VERSION"),
                "ok": false,
                "command": cmd,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "side_effects": { "foreground_restored": restored },
                "error": e.to_json(),
            });
            emit(&envelope, result_file.as_deref(), pretty);
            std::process::exit(1);
        }
    }
}

/// 命令结束后把前台还原回去（guard 命令本身除外），返回是否真的还原了。
fn settle_foreground(fg0: HWND, cmd: &str) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, SetForegroundWindow};
    // guard 自己就是管前台的；wait 可能跑很久，期间用户切窗不该被拉回来。
    if matches!(cmd, "guard" | "wait") || fg0.0.is_null() {
        return false;
    }
    let fg = unsafe { GetForegroundWindow() };
    if fg == fg0 {
        return false;
    }
    // launch 之后新程序常常自己激活；这种情况也要还原。
    // 其他命令只在"当前前台是我们这次碰过的窗口"时还原，用户自己切走的不动。
    let started_by_us = cmd == "launch";
    if !started_by_us && !win::touched_windows().contains(&(fg.0 as usize)) {
        return false;
    }
    unsafe {
        let _ = SetForegroundWindow(fg0);
    }
    std::thread::sleep(Duration::from_millis(20));
    unsafe { GetForegroundWindow() == fg0 }
}

fn emit(envelope: &Value, file: Option<&std::path::Path>, pretty: bool) {
    let text = if pretty {
        serde_json::to_string_pretty(envelope).unwrap_or_default()
    } else {
        serde_json::to_string(envelope).unwrap_or_default()
    };
    match file {
        Some(p) => {
            let _ = std::fs::write(p, text);
        }
        None => println!("{text}"),
    }
}

/// 只读/诊断命令：非管理员也允许（用于排查环境）。
fn is_readonly(cmd: &str) -> bool {
    matches!(cmd, "help" | "--help" | "-h" | "status" | "elevate")
}

/// 本技能以管理员权限为运行前提：桌面自动化要投消息、读 UIA、操作提权窗口，
/// 低完整性跑起来只会"看起来成功、实际没动"，所以直接拒绝，而不是打补丁。
fn ensure_admin() -> Result<()> {
    match win::integrity_rid(std::process::id()) {
        Some(rid) if rid < 0x3000 => Err(DeskError::new(
            "NEED_ADMIN",
            format!(
                "本技能需要管理员权限运行（当前完整性：{}）",
                win::integrity_name(rid)
            ),
        )
        .with_suggestion("用管理员身份启动 DSH / 终端；单条命令可用 `desk elevate -- <命令>`")
        .with_details(json!({
            "tool_integrity": win::integrity_name(rid),
            "required": "high",
        }))),
        _ => Ok(()),
    }
}

fn run(argv: &[String]) -> Result<(String, Value)> {
    let Some(cmd) = argv.first().map(|s| s.as_str()) else {
        return Err(DeskError::new("BAD_ARGS", "缺少子命令").with_suggestion("desk help"));
    };
    let args = Args::new(&argv[1..]);
    if args.has("--help") {
        return Ok((cmd.to_string(), json!({ "help": cli::help_for(cmd) })));
    }
    args.validate()?;
    if !is_readonly(cmd) {
        ensure_admin()?;
    }
    match cmd {
        "help" => Ok(("help".into(), json!({ "help": cli::help_for("") }))),
        "status" => Ok(("status".into(), status()?)),
        "windows" => Ok(("windows".into(), windows_cmd(&args)?)),
        "snap" => Ok(("snap".into(), snap_cmd(&args)?)),
        "find" => Ok(("find".into(), find_cmd(&args)?)),
        "click" => click_cmd(&args),
        "rclick" | "dclick" => {
            let r = args.positionals();
            let ref_id = r.first().ok_or_else(|| need("<@ref>"))?;
            let uia = Uia::new()?;
            let v = if cmd == "rclick" {
                act::right_click(&uia, ref_id)?
            } else {
                act::double_click(&uia, ref_id)?
            };
            Ok((cmd.into(), v))
        }
        "info" => {
            let r = args.positionals();
            let ref_id = r.first().ok_or_else(|| need("<@ref>"))?;
            let uia = Uia::new()?;
            Ok(("info".into(), act::info(&uia, ref_id)?))
        }
        "scrollitem" | "scrollinto" => {
            let r = args.positionals();
            let ref_id = r.first().ok_or_else(|| need("<@ref>"))?;
            let uia = Uia::new()?;
            let v = act::scroll_into_view(&uia, ref_id)?;
            Ok((cmd.into(), v))
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
        "key" => key_cmd(&args),
        "type" => type_cmd(&args),
        "wheel" => {
            let hwnd = target_window(&args)?;
            let delta = args
                .int("--delta")?
                .or(args.int("--amount")?)
                .unwrap_or(-120) as i32;
            let (x, y) = match args.opt("--xy") {
                Some(s) => parse_xy(&s)?,
                None => window_center(hwnd)?,
            };
            Ok((cmd.into(), act::wheel(hwnd, x, y, delta)?))
        }
        "scroll" => {
            // scroll <@ref> [增量]  或  scroll --window <id> [--xy x,y] [--delta N]
            let r = args.positionals();
            let delta = match r.get(1).and_then(|s| s.parse::<i32>().ok()) {
                Some(v) => v,
                None => args
                    .int("--delta")?
                    .or(args.int("--amount")?)
                    .unwrap_or(-120) as i32,
            };
            match r.first() {
                Some(ref_id) if ref_id.starts_with('@') => {
                    let uia = Uia::new()?;
                    Ok(("scroll".into(), act::scroll_ref(&uia, ref_id, delta)?))
                }
                _ => {
                    let hwnd = target_window(&args)?;
                    let (x, y) = match args.opt("--xy") {
                        Some(s) => parse_xy(&s)?,
                        None => window_center(hwnd)?,
                    };
                    Ok(("scroll".into(), act::wheel(hwnd, x, y, delta)?))
                }
            }
        }
        "drag" => {
            let r = args.positionals();
            let from = parse_xy(r.first().ok_or_else(|| need("<x1,y1> <x2,y2>"))?)?;
            let to = parse_xy(r.get(1).ok_or_else(|| need("<x1,y1> <x2,y2>"))?)?;
            let hwnd = target_window(&args)?;
            Ok((cmd.into(), act::drag(hwnd, from, to)?))
        }
        "shot" => Ok(("shot".into(), shot_cmd(&args)?)),
        "clipboard" => clipboard_cmd(&args),
        "wait" => Ok(("wait".into(), wait_cmd(&args)?)),
        "guard" => Ok(("guard".into(), guard_cmd(&args)?)),
        "elevate" => Ok(("elevate".into(), elevate_cmd(&argv[1..])?)),
        "run" => Ok(("run".into(), run_cmd(&args)?)),
        "selftest" => Ok(("selftest".into(), selftest::run()?)),
        "focus" => Err(
            DeskError::new("DISABLED", "focus 已禁用：本工具不会把窗口切到前台")
                .with_suggestion("用 snap/click 在后台操作"),
        ),
        "minimize" | "maximize" | "restore" | "close" => {
            Ok((cmd.into(), window_action(cmd, &args)?))
        }
        "move" | "resize" => Ok((cmd.into(), window_geometry(cmd, &args)?)),
        "launch" => Ok(("launch".into(), launch_cmd(&args)?)),
        other => {
            Err(DeskError::new("BAD_ARGS", format!("未知子命令：{other}"))
                .with_suggestion("desk help"))
        }
    }
}

/// 键盘该发给谁：线程焦点子控件 → 可聚焦的 edit/document → 顶层窗口。
fn typing_hwnd(hwnd: HWND) -> HWND {
    if let Some(h) = win::focus_hwnd(hwnd) {
        return h;
    }
    if let Ok(uia) = Uia::new()
        && let Some(h) = uia.typing_target(hwnd)
    {
        return h;
    }
    hwnd
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
        "integrity": win::integrity_label(std::process::id()),
        "needs_admin": win::integrity_rid(std::process::id()).map(|r| r < 0x3000).unwrap_or(false),
        "windows": all.len(),
        "foreground": foreground.map(|w| json!({"id": w.id, "title": w.title, "exe": w.exe})),
        "state_dir": state_dir().display().to_string(),
    }))
}

fn windows_cmd(args: &Args) -> Result<Value> {
    let mut all = win::enum_windows();
    if !args.has("--all") {
        all.retain(|w| !w.title.is_empty() && w.pid != std::process::id());
    }
    all.sort_by(|a, b| a.pid.cmp(&b.pid).then(a.title.cmp(&b.title)));
    Ok(json!(all))
}

fn walk_options(args: &Args) -> Result<WalkOptions> {
    Ok(WalkOptions {
        max_depth: args.int("--depth")?.unwrap_or(16).max(1) as usize,
        limit: args.int("--limit")?.unwrap_or(300).max(1) as usize,
        raw_view: args.has("--raw"),
    })
}

fn snap_cmd(args: &Args) -> Result<Value> {
    let hwnd = target_window(args)?;
    let opts = walk_options(args)?;
    let uia = Uia::new()?;
    let mut out = act::snap(&uia, hwnd, &opts, true)?;
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
    let _uia = Uia::new()?;
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

fn click_cmd(args: &Args) -> Result<(String, Value)> {
    let r = args.positionals();
    let ref_id = r.first().ok_or_else(|| need("<@ref>"))?;
    let mode = match args.opt("--mode") {
        Some(m) => Some(act::parse_mode(&m)?),
        None => None,
    };
    let uia = Uia::new()?;
    let mut out = act::click(&uia, ref_id, mode)?;

    if args.has("--verify") {
        // 验证必须针对被点的那个窗口：优先 --window，其次 ref 所在快照的窗口。
        let hwnd = match args.opt("--window") {
            Some(id) => win::parse_window_id(&id)?,
            None => {
                let sid = ref_id
                    .trim_start_matches('@')
                    .split(':')
                    .next()
                    .unwrap_or("");
                let snap = store::load(sid)?;
                win::parse_window_id(&snap.window_id)?
            }
        };
        // 定向验证：直接用被点的那个 ref 重新解析，读几个状态属性即可，
        // 不再整棵树重拍一遍（那会慢 300ms 还会挤掉快照）。
        std::thread::sleep(Duration::from_millis(120));
        let _ = hwnd;
        out["verify"] = match act::resolve(&uia, ref_id) {
            Ok(again) => json!({
                "still_present": true,
                "checked": uia.is_toggled(&again.element),
                "selected": uia.is_selected(&again.element),
                "value": uia.value_of(&again.element),
            }),
            Err(e) => json!({ "still_present": false, "error": e.code }),
        };
    }
    Ok(("click".into(), out))
}

fn key_cmd(args: &Args) -> Result<(String, Value)> {
    let r = args.positionals();
    let combo = r.first().ok_or_else(|| need("<组合键>"))?;

    let hwnd = target_window(args)?;
    let key_hwnd = typing_hwnd(hwnd);
    Ok(("key".into(), act::key(key_hwnd, combo)?))
}

fn type_cmd(args: &Args) -> Result<(String, Value)> {
    let r = args.positionals();
    let text = r.first().ok_or_else(|| need("<文本>"))?;
    if r.len() > 1 {
        return Err(DeskError::new(
            "BAD_ARGS",
            "type 只接受一个文本参数；含空格请用引号包成一个参数",
        )
        .with_suggestion("例：desk type \"hello world\" --window w-1"));
    }
    let method = args.opt("--method").unwrap_or_else(|| "auto".to_string());
    let hwnd = target_window(args)?;
    match method.as_str() {
        "paste" | "clipboard" => {
            // 保存用户原来的剪贴板，粘完恢复；失败也不留下敏感文本。
            let previous = clipboard::get_text().ok();
            clipboard::set_text(text)?;
            let key_hwnd = typing_hwnd(hwnd);
            let result = input::paste(key_hwnd);
            match previous {
                Some(prev) => {
                    let _ = clipboard::set_text(&prev);
                }
                None => {
                    let _ = clipboard::clear();
                }
            }
            result?;
            Ok((
                "type".into(),
                json!({"mode": "paste", "chars": text.chars().count()}),
            ))
        }
        "value" => {
            let ref_id = args
                .opt("--ref")
                .ok_or_else(|| need("--ref（method=value 需要元素引用）"))?;
            let uia = Uia::new()?;
            let mut out = act::set_value(&uia, &ref_id, text)?;
            out["mode"] = json!("semantic:set");
            Ok(("type".into(), out))
        }
        "keys" | "auto" => {
            let key_hwnd = typing_hwnd(hwnd);
            let mut out = act::type_text(key_hwnd, text)?;
            out["target"] = json!({"hwnd": format!("{:x}", key_hwnd.0 as usize), "class": win::window_class(key_hwnd)});
            Ok(("type".into(), out))
        }
        other => Err(DeskError::new(
            "BAD_ARGS",
            format!("--method 只能是 auto / value / keys / paste，收到：{other}"),
        )),
    }
}

fn clipboard_cmd(args: &Args) -> Result<(String, Value)> {
    let r = args.positionals();
    let action = r
        .first()
        .map(|s| s.as_str())
        .ok_or_else(|| need("get|set <文本>"))?;
    match action {
        "get" => Ok(("clipboard".into(), json!({"text": clipboard::get_text()?}))),
        "set" => {
            let text = r.get(1).ok_or_else(|| need("set <文本>"))?;
            clipboard::set_text(text)?;
            Ok((
                "clipboard".into(),
                json!({"text": clipboard::get_text()?, "chars": text.chars().count()}),
            ))
        }
        other => Err(DeskError::new(
            "BAD_ARGS",
            format!("clipboard 只支持 get / set，收到：{other}"),
        )),
    }
}

/// 临时钉住前台：在 N 秒内，只要目标程序把自己顶到前台，就把它压回去。
/// 有些上位机会定时自我激活，用这个压住。
/// 以管理员身份重新执行同一条命令，并把结果带回来。
/// 只在目标进程完整性更高（UIPI 会拦）时才需要。
/// 按 CommandLineToArgvW 的规则给一个参数加引号。
/// 简单加双引号会把 `C:\Program Files\` 这种以反斜杠结尾的路径改坏。
fn quote_arg(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"']) {
        return a.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0usize;
    for ch in a.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                out.push(ch);
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

fn known_command(cmd: &str) -> bool {
    matches!(
        cmd,
        "status"
            | "windows"
            | "snap"
            | "find"
            | "info"
            | "click"
            | "rclick"
            | "dclick"
            | "select"
            | "toggle"
            | "set"
            | "expand"
            | "collapse"
            | "scrollitem"
            | "scrollinto"
            | "scroll"
            | "wheel"
            | "drag"
            | "key"
            | "type"
            | "shot"
            | "clipboard"
            | "wait"
            | "guard"
            | "minimize"
            | "maximize"
            | "restore"
            | "close"
            | "move"
            | "resize"
            | "launch"
            | "selftest"
    )
}

fn elevate_cmd(rest: &[String]) -> Result<Value> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};
    use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

    // 只剥掉第一个 `--`，其余原样传（`--` 本身也可能是合法参数值）。
    let mut args: Vec<String> = rest.to_vec();
    if args.first().map(|a| a == "--").unwrap_or(false) {
        args.remove(0);
    }
    if args.is_empty() {
        return Err(need("<命令>").with_suggestion("例：desk elevate -- click @s1:e3"));
    }
    // 先在提权之前校验命令名，免得弹了 UAC 才报错。
    if !known_command(&args[0]) {
        return Err(
            DeskError::new("BAD_ARGS", format!("elevate 不支持这个子命令：{}", args[0]))
                .with_suggestion("desk help 查看可用命令"),
        );
    }
    if args.iter().any(|a| a.contains(['\t', '\r', '\n'])) {
        return Err(DeskError::new("BAD_ARGS", "参数里不允许制表符或换行符"));
    }

    let exe = std::env::current_exe()
        .map_err(|e| DeskError::new("ELEVATE_FAILED", format!("取自身路径失败：{e}")))?;
    let dir = state_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| DeskError::new("ELEVATE_FAILED", format!("创建状态目录失败：{e}")))?;
    let nonce = format!("{:x}{:x}", now_ms(), std::process::id());
    let out = dir.join(format!("elevated-{nonce}.json"));

    let mut params = format!("--result-file {}", quote_arg(&out.display().to_string()));
    // child 用 --result-file 传回结果，再带上 --pretty 之外的原始参数
    for a in &args {
        params.push(' ');
        params.push_str(&quote_arg(a));
    }

    let verb = windows::core::HSTRING::from("runas");
    let file = windows::core::HSTRING::from(exe.to_string_lossy().to_string());
    let params_h = windows::core::HSTRING::from(params);
    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: windows::core::PCWSTR(verb.as_ptr()),
        lpFile: windows::core::PCWSTR(file.as_ptr()),
        lpParameters: windows::core::PCWSTR(params_h.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut sei) }.map_err(|e| {
        DeskError::new(
            "ELEVATE_FAILED",
            format!("提权启动失败（UAC 被取消或策略禁止）：{e}"),
        )
    })?;

    let h = sei.hProcess;
    let timed_out = if !h.is_invalid() {
        let wait = unsafe { WaitForSingleObject(h, 60_000) };
        // WAIT_TIMEOUT = 0x102
        if wait.0 == 0x102 {
            let _ = unsafe { TerminateProcess(h, 1) };
            true
        } else {
            false
        }
    } else {
        false
    };
    if !h.is_invalid() {
        let _ = unsafe { CloseHandle(h) };
    }

    let read = std::fs::read_to_string(&out);
    let _ = std::fs::remove_file(&out);
    if timed_out {
        return Err(
            DeskError::new("ELEVATE_TIMEOUT", "提权子进程 60 秒没有结束，已终止").delivered(),
        );
    }
    let text = read.map_err(|e| {
        // 子进程可能已经执行了动作，只是没写回结果：不能当成"安全可重试"。
        DeskError::new(
            "ELEVATE_FAILED",
            format!("提权进程没有留下结果（可能被取消）：{e}"),
        )
        .delivered()
    })?;
    if text.len() > 16 * 1024 * 1024 {
        return Err(DeskError::new("ELEVATE_FAILED", "提权结果过大，已拒绝").delivered());
    }
    let child: Value = serde_json::from_str(&text).map_err(|e| {
        DeskError::new("ELEVATE_FAILED", format!("提权结果解析失败：{e}")).delivered()
    })?;
    if child.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        Ok(json!({ "elevated": true, "result": child }))
    } else {
        Err(
            DeskError::new("ELEVATED_COMMAND_FAILED", "提权执行的命令返回失败")
                .with_details(json!({ "result": child }))
                .delivered(),
        )
    }
}

fn guard_cmd(args: &Args) -> Result<Value> {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, SetForegroundWindow};
    let secs = args.int("--seconds")?.unwrap_or(30).max(1) as u64;
    let mut exe = args.opt("--exe");
    if exe.is_none()
        && let Some(w) = args.opt("--window")
    {
        let hwnd = win::parse_window_id(&w)?;
        exe = unsafe { win::describe(hwnd, true) }.map(|i| i.exe);
    }
    let keep = unsafe { GetForegroundWindow() };
    if keep.0.is_null() {
        return Err(DeskError::new("NO_FOREGROUND", "当前没有前台窗口"));
    }
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut restores = 0u32;
    while Instant::now() < deadline {
        let fg = unsafe { GetForegroundWindow() };
        if fg != keep && !fg.0.is_null() {
            let fg_exe = unsafe { win::describe(fg, true) }
                .map(|i| i.exe)
                .unwrap_or_default();
            let hit = match &exe {
                Some(t) => fg_exe.eq_ignore_ascii_case(t),
                None => true,
            };
            if hit {
                unsafe {
                    let _ = SetForegroundWindow(keep);
                }
                restores += 1;
            } else {
                // 用户自己切到别的程序了，跟着走，不再钉住。
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(80));
    }
    Ok(json!({
        "kept": win::window_id(keep),
        "seconds": secs,
        "restores": restores,
        "note": "期间目标程序抢前台会被压回去；用户切到别的程序就自动停止",
    }))
}

fn wait_cmd(args: &Args) -> Result<Value> {
    let timeout = args.int("--timeout-ms")?.unwrap_or(10000).max(100) as u64;
    if let Some(title) = args.opt("--title") {
        let exe = args.opt("--exe");
        let deadline = Instant::now() + Duration::from_millis(timeout);
        loop {
            let hit = win::enum_windows().into_iter().find(|w| {
                w.title.contains(title.as_str())
                    && exe
                        .as_ref()
                        .map(|e| w.exe.eq_ignore_ascii_case(e))
                        .unwrap_or(true)
            });
            if let Some(w) = hit {
                return Ok(
                    json!({"matched": "window", "id": w.id, "title": w.title, "exe": w.exe}),
                );
            }
            if Instant::now() >= deadline {
                return Err(DeskError::new(
                    "WAIT_TIMEOUT",
                    format!("等待窗口超时（{timeout} ms）：{title}"),
                )
                .with_suggestion("确认程序已启动，或加大 --timeout-ms"));
            }
            std::thread::sleep(Duration::from_millis(120));
        }
    }
    let hwnd = target_window(args)?;
    let text = args.opt("--text");
    let element = args.opt("--element").or(args.opt("--sel"));
    if text.is_none() && element.is_none() {
        return Err(need("--text 或 --element")
            .with_suggestion("desk wait --window w-1 --text 完成 --timeout-ms 5000"));
    }
    let deadline = Instant::now() + Duration::from_millis(timeout);
    let mut last_seen = Value::Null;
    loop {
        if let Some(t) = &text {
            // 先看标题，再看界面文本（很多"完成/成功"只出现在界面里）。
            let title = win::window_title(hwnd);
            if title.contains(t.as_str()) {
                return Ok(json!({ "matched": "title", "value": title }));
            }
            let uia = Uia::new()?;
            let snap = act::snap(
                &uia,
                hwnd,
                &WalkOptions {
                    max_depth: 12,
                    limit: 600,
                    raw_view: false,
                },
                false,
            )?;
            let hit = snap
                .get("elements")
                .and_then(|e| e.as_array())
                .and_then(|arr| {
                    arr.iter().find(|e| {
                        let name = e.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let value = e.get("value").and_then(|v| v.as_str()).unwrap_or("");
                        name.contains(t.as_str()) || value.contains(t.as_str())
                    })
                })
                .cloned();
            if let Some(el) = hit {
                return Ok(json!({ "matched": "element", "element": el }));
            }
            last_seen = json!(title);
        }
        if let Some(sel) = &element {
            let uia = Uia::new()?;
            let opts = WalkOptions {
                max_depth: 14,
                limit: 900,
                raw_view: false,
            };
            let snap = act::snap(&uia, hwnd, &opts, false)?;
            let (role, name) = match sel.split_once(':') {
                Some((r, n)) if is_role(r) => (Some(r), n),
                _ => (None, sel.as_str()),
            };
            let found = snap
                .get("elements")
                .and_then(|e| e.as_array())
                .map(|arr| {
                    arr.iter().any(|e| {
                        let role_ok = role
                            .map(|r| e.get("role").and_then(|v| v.as_str()) == Some(r))
                            .unwrap_or(true);
                        let name_ok = e
                            .get("name")
                            .and_then(|v| v.as_str())
                            .map(|n| n.contains(name))
                            .unwrap_or(false);
                        role_ok && name_ok
                    })
                })
                .unwrap_or(false);
            if found {
                return Ok(json!({"matched": "element", "selector": sel}));
            }
        }
        if Instant::now() >= deadline {
            return Err(
                DeskError::new("WAIT_TIMEOUT", format!("等待超时（{timeout} ms）"))
                    .with_details(json!({"last_seen": last_seen}))
                    .with_suggestion("加大 --timeout-ms，或先确认窗口在执行"),
            );
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn run_cmd(args: &Args) -> Result<Value> {
    let _ = args;
    let mut buf = String::new();
    use std::io::Read;
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| DeskError::new("BAD_ARGS", format!("读取 stdin 失败：{e}")))?;
    let doc: Value = serde_json::from_str(&buf)
        .map_err(|e| DeskError::new("BAD_ARGS", format!("步骤 JSON 解析失败：{e}")))?;
    let steps = doc
        .get("steps")
        .and_then(|s| s.as_array())
        .ok_or_else(|| DeskError::new("BAD_ARGS", "步骤 JSON 需要一个 steps 数组"))?;
    let mut results = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let argv: Vec<String> = match step {
            Value::Array(a) => a
                .iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect(),
            Value::String(line) => line.split_whitespace().map(|s| s.to_string()).collect(),
            _ => {
                results.push(json!({"i": i, "ok": false, "error": "步骤必须是数组或字符串"}));
                continue;
            }
        };
        let started = Instant::now();
        match run(&argv) {
            Ok((cmd, data)) => results.push(json!({
                "i": i, "ok": true, "command": cmd,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "data": data,
            })),
            Err(e) => results.push(json!({
                "i": i, "ok": false, "error": e.to_json(),
                "elapsed_ms": started.elapsed().as_millis() as u64,
            })),
        }
    }
    let failed = results.iter().filter(|r| r["ok"] == false).count();
    Ok(json!({"count": results.len(), "failed": failed, "results": results}))
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
    Err(DeskError::new(
        "BAD_ARGS",
        "这个命令必须明确指定目标窗口：加 --window <id> 或 --ref <@ref>",
    )
    .with_suggestion("先 `desk windows` 拿 id，或 `desk snap` 拿 ref；工具不会默认操作前台窗口"))
}

fn window_center(hwnd: HWND) -> Result<(i32, i32)> {
    let info = unsafe { win::describe(hwnd, true) }
        .ok_or_else(|| DeskError::new("WINDOW_GONE", "目标窗口已消失"))?;
    Ok(info.rect.center())
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
        IsIconic, IsZoomed, SW_MAXIMIZE, SW_MINIMIZE, SW_SHOWNOACTIVATE, ShowWindow, WM_CLOSE,
    };
    let r = args.positionals();
    let id = r.first().ok_or_else(|| need("<窗口 id>"))?;
    let hwnd = win::parse_window_id(id)?;
    match cmd {
        "minimize" => {
            unsafe {
                let _ = ShowWindow(hwnd, SW_MINIMIZE);
            }
            Ok(json!({"id": id, "minimized": unsafe { IsIconic(hwnd).as_bool() }}))
        }
        "maximize" => {
            unsafe {
                let _ = ShowWindow(hwnd, SW_MAXIMIZE);
            }
            Ok(json!({"id": id, "maximized": unsafe { IsZoomed(hwnd).as_bool() }}))
        }
        "restore" => {
            unsafe {
                // SW_RESTORE 会抢前台；SW_SHOWNOACTIVATE 只恢复不激活。
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            let minimized = unsafe { IsIconic(hwnd).as_bool() };
            Ok(json!({"id": id, "restored": !minimized, "minimized": minimized}))
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
    use windows::Win32::UI::WindowsAndMessaging::{SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos};
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
    if info.minimized {
        return Err(DeskError::new(
            "WINDOW_MINIMIZED",
            "窗口已最小化，先 restore 再 move/resize",
        ));
    }
    // 用真正的窗口矩形：DWM 的可视矩形比 GetWindowRect 小一圈，
    // 拿它当坐标会让窗口每 resize 一次就往右下漂、越缩越小。
    let mut wr = windows::Win32::Foundation::RECT::default();
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut wr) }
        .map_err(|e| DeskError::new("WINDOW_MOVE_FAILED", format!("读取窗口矩形失败：{e}")))?;
    let (x, y, w, h) = if cmd == "move" {
        (a, b, wr.right - wr.left, wr.bottom - wr.top)
    } else {
        (wr.left, wr.top, a, b)
    };
    unsafe { SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE) }
        .map_err(|e| DeskError::new("WINDOW_MOVE_FAILED", format!("移动/缩放窗口失败：{e}")))?;
    Ok(json!({"id": id, "rect": {"x": x, "y": y, "w": w, "h": h}}))
}

fn launch_cmd(args: &Args) -> Result<Value> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE;
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
            SW_SHOWNOACTIVATE,
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

fn is_role(s: &str) -> bool {
    // 直接以 uia::role_name 为准，避免两处表漂移。
    if (50000..=50040).any(|t| uia::role_name(t) == s) {
        return true;
    }
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

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
