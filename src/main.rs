mod act;
mod cli;
mod clipboard;
mod commands;
mod error;
mod input;
mod observe;
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
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use uia::{Uia, WalkOptions};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, IsIconic, IsWindow, IsZoomed, PostMessageW, SW_MAXIMIZE,
    SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOZORDER, SetForegroundWindow,
    SetWindowPos, ShowWindow, WM_CLOSE,
};

/// 提权子进程的结果文件：看门狗超时也要把信封写进去，父进程才拿得到原因。
static RESULT_FILE: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);

/// 看门狗的截止时间（unix ms）。`run` 解析出各步声明的超时后可以把它往后推，
/// 这样"run 里包一个 90 秒 wait"不会被入口的 60 秒预算误杀。
static WATCHDOG_DEADLINE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static WATCHDOG_STARTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 信封只有一个出口：主线程和看门狗谁先拿到锁谁写，另一个什么都不写。
static EMIT_LOCK: Mutex<()> = Mutex::new(());
static EMITTED: AtomicBool = AtomicBool::new(false);

fn main() {
    // 进程级 DPI 感知：坐标全部按物理像素处理，和 UIA 一致。
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    let started = Instant::now();
    let fg0 = unsafe { GetForegroundWindow() };

    // 用 args_os + lossy：std::env::args() 遇到非法 UTF-16 会直接 panic（EXIT 101 无信封）。
    let mut argv: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|s| s.to_string_lossy().into_owned())
        .collect();

    // --pretty 只认 `--` 之前的那个；`--` 之后是要传给目标程序的字面量。
    let mut pretty = false;
    let cut = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    if let Some(i) = argv[..cut].iter().position(|a| a == "--pretty") {
        pretty = true;
        argv.remove(i);
    }

    // 提权子进程用这个把自己那份 JSON 写回文件（跨 UAC 拿不到管道）。
    let mut result_file: Option<std::path::PathBuf> = None;
    if argv.len() >= 2 && argv[0] == "--result-file" {
        match allowed_result_file(std::path::Path::new(&argv[1])) {
            Ok(p) => result_file = Some(p),
            Err(e) => {
                let _ = emit_envelope(
                    &json!({
                        "version": env!("CARGO_PKG_VERSION"),
                        "ok": false,
                        "command": "elevate",
                        "error": e.to_json(),
                    }),
                    None,
                    pretty,
                );
                std::process::exit(1);
            }
        }
        argv.drain(0..2);
        // 看门狗线程也要知道结果文件：超时要写回原因，不能只打 stdout。
        *RESULT_FILE.lock().unwrap() = result_file.clone();
    }

    // help 直接输出纯文本，不套 JSON 信封；其他命令一律 JSON。
    if matches!(
        argv.first().map(|s| s.as_str()),
        Some("help") | Some("--help") | Some("-h")
    ) {
        let topic = argv.get(1).map(|s| s.as_str()).unwrap_or("");
        println!("{}", cli::help_for(topic));
        return;
    }

    let args = Args::new(argv.get(1..).unwrap_or(&[]));
    let observe = argv.first().map(|c| commands::mutates(c)).unwrap_or(false)
        && !args.has("--no-observe")
        && std::env::var("DESK_OBSERVE")
            .map(|v| v != "0")
            .unwrap_or(true);
    let observation = if observe {
        Some(observe::capture(&args))
    } else {
        None
    };

    match run(&argv) {
        Ok((cmd, mut data)) => {
            if let Some(obs) = &observation {
                data["after"] = observe::after(obs);
            }
            let restored = settle_foreground(fg0, &cmd);
            let envelope = json!({
                "version": env!("CARGO_PKG_VERSION"),
                "ok": true,
                "command": cmd,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "side_effects": { "foreground_restored": restored },
                "data": data,
            });
            emit_envelope(&envelope, result_file.as_deref(), pretty);
        }
        Err(e) => {
            let cmd = argv.first().cloned().unwrap_or_default();
            let restored = settle_foreground(fg0, &cmd);
            let mut envelope = json!({
                "version": env!("CARGO_PKG_VERSION"),
                "ok": false,
                "command": cmd,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "side_effects": { "foreground_restored": restored },
                "error": e.to_json(),
            });
            // 动作可能已经落地（retry=unsafe）时，顺手把界面变化带上，省一次 snap。
            if !e.retry_safe
                && let Some(obs) = &observation
            {
                envelope["after"] = observe::after(obs);
            }
            emit_envelope(&envelope, result_file.as_deref(), pretty);
            std::process::exit(1);
        }
    }
}

/// 提权结果文件只允许落在 state_dir 下、以 elevated- 开头的 json。
fn allowed_result_file(path: &std::path::Path) -> Result<std::path::PathBuf> {
    let dir = state_dir();
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let shape_ok = name.starts_with("elevated-") && name.ends_with(".json") && name.len() <= 64;
    let dir_canon = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
    let parent = path.parent().unwrap_or(std::path::Path::new(""));
    let parent_canon = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    if !shape_ok || parent_canon != dir_canon {
        return Err(DeskError::new(
            "BAD_ARGS",
            "--result-file 只允许提权子进程使用，且必须落在 desk 状态目录下",
        ));
    }
    if path.exists() {
        return Err(DeskError::new(
            "BAD_ARGS",
            "--result-file 目标已存在；提权结果只允许新建，不覆盖",
        ));
    }
    Ok(path.to_path_buf())
}

/// 命令结束后把前台还原回去，返回是否真的还原了。
/// 只还原"我们碰过的窗口/进程"造成的变化；用户自己切走的不动。
fn settle_foreground(fg0: HWND, cmd: &str) -> bool {
    if fg0.0.is_null() {
        return false;
    }
    let fg = unsafe { GetForegroundWindow() };
    if fg == fg0 || fg.0.is_null() {
        return false;
    }
    let fg_pid = win::pid_of(fg);
    let ours = commands::restore_any_foreground(cmd)
        || win::touched_windows().contains(&(fg.0 as usize))
        || win::touched_processes().contains(&fg_pid);
    if !ours {
        return false;
    }
    unsafe {
        let _ = SetForegroundWindow(fg0);
    }
    std::thread::sleep(Duration::from_millis(20));
    unsafe { GetForegroundWindow() == fg0 }
}

/// 看门狗预算：命令自己声明了超时（--timeout-ms / --seconds）就按它加余量，
/// 否则用 DESK_WATCHDOG_MS 或默认 60 秒。不按命令名特判。
fn watchdog_budget(args: &Args) -> Duration {
    let declared_ms = args.int("--timeout-ms").ok().flatten().or_else(|| {
        args.int("--seconds")
            .ok()
            .flatten()
            .map(|s| s.saturating_mul(1000))
    });
    if let Some(ms) = declared_ms {
        return Duration::from_millis(ms.clamp(1_000, 600_000) as u64 + 30_000);
    }
    let from_env = std::env::var("DESK_WATCHDOG_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok());
    Duration::from_millis(from_env.unwrap_or(60_000).max(5_000))
}

fn start_watchdog(limit: Duration, result_file: Option<std::path::PathBuf>) {
    WATCHDOG_STARTED.store(now_ms(), Ordering::SeqCst);
    WATCHDOG_DEADLINE.store(
        now_ms().saturating_add(limit.as_millis() as u64),
        Ordering::SeqCst,
    );
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(200));
            let deadline = WATCHDOG_DEADLINE.load(Ordering::SeqCst);
            if deadline == 0 || now_ms() < deadline {
                continue;
            }
            let envelope = json!({
                "version": env!("CARGO_PKG_VERSION"),
                "ok": false,
                "command": "watchdog",
                "elapsed_ms": now_ms().saturating_sub(WATCHDOG_STARTED.load(Ordering::SeqCst)),
                "side_effects": { "foreground_restored": false },
                "error": {
                    "code": "WATCHDOG_TIMEOUT",
                    "message": "命令超过看门狗预算没有返回（目标进程可能挂死），已强制退出",
                    "retry": "unsafe",
                    "suggestion": "先 `desk status` / `desk doctor` 确认目标状态；必要时重启目标程序"
                }
            });
            if emit_envelope(&envelope, result_file.as_deref(), false) {
                std::process::exit(5);
            }
            return;
        }
    });
}

/// 把看门狗截止时间往后推（只推不缩）。
fn extend_watchdog(extra_ms: u64) {
    let cur = WATCHDOG_DEADLINE.load(Ordering::SeqCst);
    if cur == 0 {
        return;
    }
    let want = now_ms().saturating_add(extra_ms);
    if want > cur {
        WATCHDOG_DEADLINE.store(want, Ordering::SeqCst);
    }
}

fn emit_envelope(envelope: &Value, file: Option<&std::path::Path>, pretty: bool) -> bool {
    let text = if pretty {
        serde_json::to_string_pretty(envelope).unwrap_or_default()
    } else {
        serde_json::to_string(envelope).unwrap_or_default()
    };
    let _guard = EMIT_LOCK.lock();
    if EMITTED.swap(true, Ordering::SeqCst) {
        return false;
    }
    match file {
        Some(p) => {
            // create_new：不允许覆盖/删除已存在的文件（提权结果通道只写自己那份）。
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(p)
            {
                Ok(mut f) => {
                    if let Err(e) = f.write_all(text.as_bytes()) {
                        eprintln!("desk: 写结果文件失败：{e}");
                    }
                }
                Err(e) => eprintln!("desk: 创建结果文件失败：{e}"),
            }
        }
        None => {
            // 管道已关闭时 println! 会 panic；这里把写入失败当普通错误吞掉，
            // 至少不会再产生一个没有信封的 EXIT 101。
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{text}");
        }
    }
    true
}

/// 管理员策略完全由 `commands::COMMANDS` 决定：表里 needs_admin=false 的命令
/// 在非管理员下也能跑，其余一律 NEED_ADMIN。
fn ensure_admin() -> Result<()> {
    match win::integrity_rid(std::process::id()) {
        Some(rid) if rid >= 0x3000 => Ok(()),
        Some(rid) => Err(DeskError::new(
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
        // 读不到自己的令牌 = 无法判断 = 拒绝（fail-closed）。
        None => Err(DeskError::new(
            "NEED_ADMIN",
            "读不到本进程的完整性级别，无法确认权限；按 fail-closed 拒绝",
        )
        .with_suggestion("用普通交互式管理员会话重新启动 desk")
        .with_details(json!({"integrity_unreadable": true}))),
    }
}

fn run(argv: &[String]) -> Result<(String, Value)> {
    let Some(cmd) = argv.first().map(|s| s.as_str()) else {
        return Err(DeskError::new("BAD_ARGS", "缺少子命令").with_suggestion("desk help"));
    };
    let args = Args::new(&argv[1..]);
    // 未知命令优先报错：`desk frobnicate --help` 不能返回一份"成功"的帮助。
    if commands::get(cmd).is_none() {
        return Err(
            DeskError::new("BAD_ARGS", format!("未知子命令：{cmd}")).with_suggestion("desk help")
        );
    }
    if args.has("--help") {
        return Ok((cmd.to_string(), json!({ "help": cli::help_for(cmd) })));
    }
    args.validate(cmd)?;
    if commands::needs_admin(cmd) {
        ensure_admin()?;
    }
    // 看门狗：UIA / PrintWindow 都是阻塞调用，目标挂死时整条命令会永远不返回。
    start_watchdog(
        watchdog_budget(&args),
        RESULT_FILE.lock().ok().and_then(|g| g.clone()),
    );
    dispatch(cmd, &args)
}

/// 真正的命令分发。`run` 负责校验/闸门/看门狗，`run` 命令的每一步直接调它，
/// 不再为每一步新起一个看门狗（那会杀掉后续步骤）。
fn dispatch(cmd: &str, args: &Args) -> Result<(String, Value)> {
    match cmd {
        "help" => Ok(("help".into(), json!({ "help": cli::help_for("") }))),
        "status" => Ok(("status".into(), status()?)),
        "doctor" => Ok(("doctor".into(), doctor_cmd(args)?)),
        "windows" => Ok(("windows".into(), windows_cmd(args)?)),
        "snap" => Ok(("snap".into(), snap_cmd(args)?)),
        "find" => Ok(("find".into(), find_cmd(args)?)),
        "click" => click_cmd(args),
        "rclick" | "dclick" => {
            if let Some(xy) = args.opt("--xy") {
                let hwnd = target_window(args)?;
                let (x, y) = parse_xy(&xy)?;
                let v = if cmd == "rclick" {
                    act::click_at(hwnd, x, y, input::Button::Right)?
                } else {
                    act::double_click_at(hwnd, x, y)?
                };
                return Ok((cmd.into(), v));
            }
            let ref_id = ref_arg(args).ok_or_else(|| need("<@ref> 或 --xy X,Y"))?;
            let uia = Uia::new()?;
            let v = if cmd == "rclick" {
                act::right_click(&uia, &ref_id)?
            } else {
                act::double_click(&uia, &ref_id)?
            };
            Ok((cmd.into(), v))
        }
        "info" => {
            let ref_id = ref_arg(args).ok_or_else(|| need("<@ref>"))?;
            // --window 只能和 ref 指向同一个窗口；不一致说明调用方拿错了引用。
            if let Some(id) = args.opt("--window") {
                let (sid, _) = ref_id
                    .trim_start_matches('@')
                    .split_once(':')
                    .ok_or_else(|| DeskError::new("BAD_REF", "ref 格式不对"))?;
                let snap = store::load(sid)?;
                if !snap.window_id.eq_ignore_ascii_case(&id) {
                    return Err(DeskError::new(
                        "BAD_ARGS",
                        format!("ref 属于窗口 {}，但 --window 给的是 {id}", snap.window_id),
                    ));
                }
            }
            let uia = Uia::new()?;
            Ok(("info".into(), act::info(&uia, &ref_id)?))
        }
        "scrollitem" | "scrollinto" => {
            let ref_id = ref_arg(args).ok_or_else(|| need("<@ref>"))?;
            let uia = Uia::new()?;
            let v = act::scroll_into_view(&uia, &ref_id)?;
            Ok((cmd.into(), v))
        }
        "select" | "toggle" | "expand" | "collapse" => {
            let ref_id = ref_arg(args).ok_or_else(|| need("<@ref>"))?;
            let uia = Uia::new()?;
            let v = match cmd {
                "select" => act::select(&uia, &ref_id)?,
                "toggle" => act::toggle(&uia, &ref_id)?,
                "expand" => act::expand(&uia, &ref_id, true)?,
                _ => act::expand(&uia, &ref_id, false)?,
            };
            Ok((cmd.into(), v))
        }
        "set" => {
            let r = args.positionals();
            // --ref 显式给出时，位置参数全都属于"值"（值本身可以以 @ 开头）。
            let (ref_id, value) = if let Some(rf) = args.opt("--ref") {
                let v = r.first().cloned().ok_or_else(|| need("<@ref> <value>"))?;
                (rf, v)
            } else {
                let rf = r.first().ok_or_else(|| need("<@ref> <value>"))?;
                let v = r.get(1).ok_or_else(|| need("<@ref> <value>"))?.clone();
                (rf.clone(), v)
            };
            let uia = Uia::new()?;
            Ok((cmd.into(), act::set_value(&uia, &ref_id, &value)?))
        }
        "key" => key_cmd(args),
        "type" => type_cmd(args),
        "wheel" => {
            let hwnd = target_window(args)?;
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
            let ref_id = ref_arg(args);
            match ref_id {
                Some(ref_id) if ref_id.starts_with('@') => {
                    let uia = Uia::new()?;
                    Ok(("scroll".into(), act::scroll_ref(&uia, &ref_id, delta)?))
                }
                _ => {
                    let hwnd = target_window(args)?;
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
            let hwnd = target_window(args)?;
            Ok((cmd.into(), act::drag(hwnd, from, to)?))
        }
        "shot" => Ok(("shot".into(), shot_cmd(args)?)),
        "clipboard" => clipboard_cmd(args),
        "wait" => Ok(("wait".into(), wait_cmd(args)?)),
        "guard" => Ok(("guard".into(), guard_cmd(args)?)),
        "elevate" => Ok(("elevate".into(), elevate_cmd(args)?)),
        "run" => Ok(("run".into(), run_cmd(args)?)),
        "selftest" => Ok(("selftest".into(), selftest::run()?)),
        "focus" => Err(
            DeskError::new("DISABLED", "focus 已禁用：本工具不会把窗口切到前台")
                .with_suggestion("用 snap/click 在后台操作"),
        ),
        "minimize" | "maximize" | "restore" | "close" => {
            Ok((cmd.into(), window_action(cmd, args)?))
        }
        "move" | "resize" => Ok((cmd.into(), window_geometry(cmd, args)?)),
        "launch" => Ok(("launch".into(), launch_cmd(args)?)),
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

/// 窗口级命令的 id：位置参数 w-xxxx、--window，或 --ref 所指快照的窗口。
fn window_id_arg(args: &Args) -> Option<String> {
    if let Some(id) = args.positionals().into_iter().find(|s| s.starts_with("w-")) {
        return Some(id);
    }
    if let Some(id) = args.opt("--window") {
        return Some(id);
    }
    let r = args.opt("--ref")?;
    let sid = r.trim_start_matches('@').split(':').next()?;
    store::load(sid).ok().map(|s| s.window_id)
}

/// 元素引用：位置参数里的 @ref 优先，其次 --ref（两种写法命令都认）。
fn ref_arg(args: &Args) -> Option<String> {
    args.positionals()
        .into_iter()
        .find(|s| s.starts_with('@'))
        .or_else(|| args.opt("--ref"))
}

pub fn state_dir() -> std::path::PathBuf {
    let base = std::env::var_os("USERPROFILE")
        .map(std::path::PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty() && p.is_absolute())
        .unwrap_or_else(std::env::temp_dir);
    base.join(".dsh").join("desk")
}

fn status() -> Result<Value> {
    let all = win::enum_windows(false);
    let foreground = all.iter().find(|w| w.focused);
    let rid = win::integrity_rid(std::process::id());
    Ok(json!({
        "exe": std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default(),
        "pid": std::process::id(),
        "elevated": win::is_elevated(std::process::id()),
        "integrity": win::integrity_label(std::process::id()),
        "needs_admin": rid.map(|r| r < 0x3000).unwrap_or(true),
        "windows": all.len(),
        "foreground": foreground.map(|w| json!({"id": w.id, "title": w.title, "exe": w.exe})),
        "state_dir": state_dir().display().to_string(),
    }))
}

/// 预检：在动手之前把"为什么动不了"说清楚。
fn doctor_cmd(args: &Args) -> Result<Value> {
    use windows::Win32::UI::WindowsAndMessaging::{SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_NULL};
    let hwnd = target_window(args)?;
    let Some(info) = (unsafe { win::describe(hwnd, true) }) else {
        return Ok(json!({
            "verdict": "blocked",
            "issues": [{"code": "WINDOW_GONE", "severity": "blocked", "message": "窗口已不存在"}],
        }));
    };
    let mut issues: Vec<Value> = Vec::new();
    let mut issue = |code: &str, severity: &str, message: String, suggestion: &str| {
        issues.push(json!({"code": code, "severity": severity, "message": message, "suggestion": suggestion}));
    };
    if info.minimized {
        issue(
            "WINDOW_MINIMIZED",
            "warn",
            "窗口已最小化：语义动作通常还能用，坐标点击不行".into(),
            "先 `desk restore <id>`",
        );
    }
    if !info.visible {
        issue(
            "WINDOW_HIDDEN",
            "warn",
            "窗口不可见：UIA 可能读不到界面".into(),
            "先 restore，或确认它在当前虚拟桌面",
        );
    }
    match win::can_reach(info.pid) {
        Ok(()) => {}
        Err((None, _)) => issue(
            "TOOL_TOKEN_UNREADABLE",
            "blocked",
            "读不到本进程完整性，无法判断能否投递".into(),
            "用交互式管理员会话重新启动",
        ),
        Err((Some(me), Some(target))) => issue(
            "ELEVATION_MISMATCH",
            "blocked",
            format!(
                "目标完整性 {} 高于本工具 {}，UIPI 会丢弃消息",
                win::integrity_name(target),
                win::integrity_name(me)
            ),
            "用管理员运行，或 `desk elevate -- ...`",
        ),
        Err(_) => issue(
            "TARGET_PROTECTED",
            "blocked",
            "读不到目标进程令牌（PPL/受保护进程）".into(),
            "走目标自己的官方接口",
        ),
    }
    let mut out: usize = 0;
    let responsive = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_NULL,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            1000,
            Some(&mut out as *mut usize),
        )
    }
    .0 != 0;
    if !responsive {
        issue(
            "WINDOW_HUNG",
            "warn",
            "窗口线程 1 秒内没有响应 WM_NULL，可能正忙/挂起".into(),
            "等待或重启目标程序",
        );
    }
    let (uia_ok, elements, uia_ms) = match Uia::new() {
        Ok(uia) => {
            let t = Instant::now();
            match uia.snapshot(
                hwnd,
                "sdoc",
                &WalkOptions {
                    max_depth: 8,
                    limit: 200,
                    raw_view: false,
                },
            ) {
                Ok((nodes, stats)) => (
                    true,
                    json!({"count": nodes.len(), "truncated": stats.truncated()}),
                    t.elapsed().as_millis() as u64,
                ),
                Err(e) => (
                    false,
                    json!({"error": e.code}),
                    t.elapsed().as_millis() as u64,
                ),
            }
        }
        Err(e) => (false, json!({"error": e.code}), 0),
    };
    if !uia_ok {
        issue(
            "UIA_UNREADABLE",
            "blocked",
            "读不到 UIA 树：目标可能没有无障碍支持或正忙".into(),
            "试试 `desk shot` 后用 `click --xy`，或换目标",
        );
    }
    let verdict = if issues.iter().any(|i| i["severity"] == "blocked") {
        "blocked"
    } else if issues.is_empty() {
        "ok"
    } else {
        "warn"
    };
    Ok(json!({
        "verdict": verdict,
        "window": {
            "id": info.id, "title": info.title, "exe": info.exe, "pid": info.pid,
            "class": info.class, "minimized": info.minimized, "visible": info.visible,
            "rect": {"x": info.rect.x, "y": info.rect.y, "w": info.rect.width, "h": info.rect.height},
            "dpi": info.dpi,
        },
        "integrity": {"tool": win::integrity_label(std::process::id()), "target": info.integrity},
        "responsive": responsive,
        "uia": elements,
        "uia_ms": uia_ms,
        "issues": issues,
    }))
}

fn windows_cmd(args: &Args) -> Result<Value> {
    let mut all = win::enum_windows(args.has("--all"));
    if !args.has("--all") {
        all.retain(|w| !w.title.is_empty() && w.pid != std::process::id());
        all.sort_by(|a, b| a.pid.cmp(&b.pid).then(a.title.cmp(&b.title)));
    }
    Ok(json!(all))
}

fn walk_options(args: &Args) -> Result<WalkOptions> {
    let depth = args.int("--depth")?.unwrap_or(16);
    if !(1..=64).contains(&depth) {
        return Err(DeskError::new(
            "BAD_ARGS",
            format!("--depth 必须在 1..=64，收到：{depth}"),
        ));
    }
    let limit = args.int("--limit")?.unwrap_or(300);
    if !(1..=20000).contains(&limit) {
        return Err(DeskError::new(
            "BAD_ARGS",
            format!("--limit 必须在 1..=20000，收到：{limit}"),
        ));
    }
    Ok(WalkOptions {
        max_depth: depth as usize,
        limit: limit as usize,
        raw_view: args.has("--raw"),
    })
}

fn snap_cmd(args: &Args) -> Result<Value> {
    let uia = Uia::new()?;
    snap_with(&uia, args)
}

/// 快照 + 过滤。find 复用同一个 UIA 实例，不再重复初始化。
fn snap_with(uia: &Uia, args: &Args) -> Result<Value> {
    let hwnd = target_window(args)?;
    let opts = walk_options(args)?;
    let mut out = act::snap(uia, hwnd, &opts, true)?;
    if let Some(name) = args.opt("--name").or(args.opt("--text")) {
        if name.is_empty() {
            return Err(DeskError::new("BAD_ARGS", "--name 不能是空串"));
        }
        let needle = name.to_lowercase();
        filter_elements(&mut out, |e| {
            e.get("name")
                .and_then(|n| n.as_str())
                .map(|n| n.to_lowercase().contains(&needle))
                .unwrap_or(false)
        });
    }
    if args.has("--actionable") {
        filter_elements(&mut out, |e| {
            e.get("actions")
                .and_then(|a| a.as_array())
                .is_some_and(|a| {
                    a.iter().any(|v| {
                        matches!(
                            v.as_str(),
                            Some("click" | "select" | "toggle" | "set" | "expand" | "scrollitem")
                        )
                    })
                })
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
    let mut out = snap_with(&uia, args)?;
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
    if let Some(xy) = args.opt("--xy") {
        if args.has("--mode") {
            return Err(DeskError::new(
                "BAD_ARGS",
                "--xy 只走后台注入，不能和 --mode 一起用",
            ));
        }
        let hwnd = target_window(args)?;
        let (x, y) = parse_xy(&xy)?;
        return Ok((
            "click".into(),
            act::click_at(hwnd, x, y, input::Button::Left)?,
        ));
    }
    let ref_id = ref_arg(args).ok_or_else(|| need("<@ref> 或 --xy X,Y"))?;
    let mode = match args.opt("--mode") {
        Some(m) => Some(act::parse_mode(&m)?),
        None => None,
    };
    let uia = Uia::new()?;
    let mut out = act::click(&uia, &ref_id, mode)?;

    if args.has("--verify") {
        // 定向验证：用同一个 ref 重新解析，读几个状态属性即可。
        std::thread::sleep(Duration::from_millis(120));
        out["verify"] = match act::resolve(&uia, &ref_id) {
            Ok(again) => json!({
                "still_present": true,
                "checked": uia.is_toggled(&again.element),
                "selected": uia.is_selected(&again.element),
                "value": if again.node.password { Value::Null } else { json!(uia.value_of(&again.element)) },
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
            // 只有原本是文本才能还原；图片/文件类剪贴板无法还原，如实告知。
            let had_text = clipboard::has_text();
            let previous = if had_text {
                clipboard::get_text().ok()
            } else {
                None
            };
            clipboard::set_text(text)?;
            let key_hwnd = typing_hwnd(hwnd);
            let (result, stolen) =
                input::with_foreground_guard(key_hwnd, || input::paste(key_hwnd));
            let restored = match &previous {
                Some(prev) => clipboard::set_text(prev).is_ok(),
                None => {
                    if had_text {
                        clipboard::clear().is_ok()
                    } else {
                        false
                    }
                }
            };
            result?;
            let mut out = json!({
                "mode": "paste",
                "chars": text.chars().count(),
                "paste_confirmed": false,
                "clipboard_restored": restored,
                "clipboard_had_text": had_text,
            });
            if stolen {
                out["focus_restored"] = json!(true);
            }
            Ok(("type".into(), out))
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
        "get" => {
            if !clipboard::has_text() {
                return Ok((
                    "clipboard".into(),
                    json!({"text": Value::Null, "has_text": false}),
                ));
            }
            Ok((
                "clipboard".into(),
                json!({"text": clipboard::get_text()?, "has_text": true}),
            ))
        }
        "set" => {
            let text = r.get(1).ok_or_else(|| need("set <文本>"))?;
            clipboard::set_text(text)?;
            Ok((
                "clipboard".into(),
                json!({"text": clipboard::get_text()?, "has_text": true, "chars": text.chars().count()}),
            ))
        }
        other => Err(DeskError::new(
            "BAD_ARGS",
            format!("clipboard 只支持 get / set，收到：{other}"),
        )),
    }
}

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

fn elevate_cmd(args: &Args) -> Result<Value> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{GetProcessId, TerminateProcess, WaitForSingleObject};
    use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

    // `--` 之后的全部是子命令与它的参数。
    let rest: Vec<String> = args.positionals();
    if rest.is_empty() {
        return Err(need("<命令>").with_suggestion("例：desk elevate -- click @s1:e3"));
    }
    let child = &rest[0];
    if commands::get(child).is_none() {
        return Err(
            DeskError::new("BAD_ARGS", format!("elevate 不支持这个子命令：{child}"))
                .with_suggestion("desk help 查看可用命令"),
        );
    }
    if matches!(child.as_str(), "elevate" | "run") {
        return Err(DeskError::new(
            "BAD_ARGS",
            "elevate 不能嵌套 elevate/run（会递归弹 UAC）",
        ));
    }
    // 弹 UAC 之前把整条子命令的参数校验完，别让人点了确认才看到拼错。
    Args::new(&rest[1..]).validate(child)?;
    if rest.iter().any(|a| a.chars().any(|c| (c as u32) < 0x20)) {
        return Err(DeskError::new("BAD_ARGS", "参数里不允许控制字符"));
    }

    let exe = std::env::current_exe()
        .map_err(|e| DeskError::new("ELEVATE_FAILED", format!("取自身路径失败：{e}")))?;
    let dir = state_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| DeskError::new("ELEVATE_FAILED", format!("创建状态目录失败：{e}")))?;
    let nonce = format!("{}{:x}", store::new_id(), std::process::id());
    let out = dir.join(format!("elevated-{nonce}.json"));
    let _ = std::fs::remove_file(&out);

    let mut params = format!("--result-file {}", quote_arg(&out.display().to_string()));
    for a in &rest {
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
    // 父进程的等待预算 = 子命令自己声明的看门狗预算 + 15s 余量。
    let child_budget = watchdog_budget(&Args::new(&rest[1..]));
    let wait_ms = (child_budget.as_millis() as u32).saturating_add(15_000);
    let timed_out = if !h.is_invalid() {
        let wait = unsafe { WaitForSingleObject(h, wait_ms) };
        if wait.0 == 0x102 {
            let _ = unsafe { TerminateProcess(h, 1) };
            true
        } else {
            false
        }
    } else {
        false
    };
    let pid = if !h.is_invalid() {
        unsafe { GetProcessId(h) }
    } else {
        0
    };
    if !h.is_invalid() {
        let _ = unsafe { CloseHandle(h) };
    }

    let read = std::fs::read_to_string(&out);
    let _ = std::fs::remove_file(&out);
    if timed_out {
        return Err(DeskError::new("ELEVATE_TIMEOUT", "提权子进程超时，已终止").delivered());
    }
    let text = read.map_err(|e| {
        DeskError::new(
            "ELEVATE_FAILED",
            format!("提权进程没有留下结果（可能被取消）：{e}"),
        )
        .delivered()
    })?;
    if text.len() > 16 * 1024 * 1024 {
        return Err(DeskError::new("ELEVATE_FAILED", "提权结果过大，已拒绝").delivered());
    }
    let child_json: Value = serde_json::from_str(&text).map_err(|e| {
        DeskError::new("ELEVATE_FAILED", format!("提权结果解析失败：{e}")).delivered()
    })?;
    // 结果文件路径可预测且目录可写，至少要核对它确实是这条命令的输出。
    if child_json.get("command").and_then(|v| v.as_str()) == Some("watchdog") {
        return Err(DeskError::new(
            "ELEVATE_TIMEOUT",
            "提权子进程被看门狗终止（子命令没有返回）",
        )
        .delivered());
    }
    if child_json.get("command").and_then(|v| v.as_str()) != Some(child.as_str()) {
        return Err(
            DeskError::new("ELEVATE_FAILED", "提权结果不属于这条命令，拒绝采用").delivered(),
        );
    }
    if child_json.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        Ok(json!({ "elevated": true, "pid": pid, "result": child_json }))
    } else {
        Err(
            DeskError::new("ELEVATED_COMMAND_FAILED", "提权执行的命令返回失败")
                .with_details(json!({ "result": child_json }))
                .delivered(),
        )
    }
}

fn guard_cmd(args: &Args) -> Result<Value> {
    let secs = args.int("--seconds")?.unwrap_or(30).clamp(1, 540) as u64;
    let mut exe = args.opt("--exe");
    if exe.is_none()
        && let Some(w) = window_id_arg(args)
    {
        let hwnd = win::parse_window_id(&w)?;
        exe = Some(
            unsafe { win::describe(hwnd, true) }
                .map(|i| i.exe)
                .filter(|e| !e.is_empty())
                .ok_or_else(|| {
                    DeskError::new("BAD_ARGS", "读不到 --window 的进程名，无法限定 guard 范围")
                        .with_suggestion("改用 --exe <名> 明确指定要压住的程序")
                })?,
        );
    }
    let keep = unsafe { GetForegroundWindow() };
    if keep.0.is_null() {
        return Err(DeskError::new("NO_FOREGROUND", "当前没有前台窗口"));
    }
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut restores = 0u32;
    let mut failed = 0u32;
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
                let ok = unsafe { SetForegroundWindow(keep) }.as_bool();
                if ok {
                    restores += 1;
                } else {
                    failed += 1;
                }
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
        "restore_failed": failed,
        "note": "期间目标程序抢前台会被压回去；用户切到别的程序就自动停止",
    }))
}

fn wait_cmd(args: &Args) -> Result<Value> {
    let timeout = args
        .int("--timeout-ms")?
        .unwrap_or(10000)
        .clamp(100, 600_000) as u64;
    if let Some(title) = args.opt("--title") {
        if title.is_empty() {
            return Err(DeskError::new("BAD_ARGS", "--title 不能是空串"));
        }
        let exe = args.opt("--exe");
        let deadline = Instant::now() + Duration::from_millis(timeout);
        loop {
            let hit = win::enum_windows(false).into_iter().find(|w| {
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
    if text.as_deref() == Some("") || element.as_deref() == Some("") {
        return Err(DeskError::new(
            "BAD_ARGS",
            "--text/--element 不能是空串（空串会立即匹配一切）",
        ));
    }
    if text.is_none() && element.is_none() {
        return Err(need("--text 或 --element")
            .with_suggestion("desk wait --window w-1 --text 完成 --timeout-ms 5000"));
    }
    let deadline = Instant::now() + Duration::from_millis(timeout);
    // 一次循环只建一个 Uia，一次遍历同时判 text/element（原来各建各的，等于同一棵树走两遍）。
    let uia = Uia::new()?;
    let opts = WalkOptions {
        max_depth: 14,
        limit: 900,
        raw_view: false,
    };
    let mut last_seen = Value::Null;
    loop {
        let title = win::window_title(hwnd);
        if let Some(t) = &text
            && title.contains(t.as_str())
        {
            return Ok(json!({ "matched": "title", "value": title }));
        }
        let nodes = match uia.snapshot(hwnd, "swait", &opts) {
            Ok((nodes, _)) => nodes,
            Err(_) => Vec::new(),
        };
        if let Some(t) = &text {
            if let Some(n) = nodes.iter().find(|n| {
                n.name.contains(t.as_str())
                    || (!n.password && n.value.as_deref().is_some_and(|v| v.contains(t.as_str())))
            }) {
                return Ok(json!({
                    "matched": "element",
                    "element": {"role": n.role, "name": n.name, "value": if n.password { Value::Null } else { json!(n.value) }},
                }));
            }
            last_seen = json!(title);
        }
        if let Some(sel) = &element {
            let (role, name) = match sel.split_once(':') {
                Some((r, n)) if is_role(r) => (Some(r), n),
                _ => (None, sel.as_str()),
            };
            let found = nodes.iter().any(|n| {
                let role_ok = role.map(|r| n.role == r).unwrap_or(true);
                role_ok && n.name.contains(name)
            });
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
    let positionals = args.positionals();
    if positionals.first().map(|s| s.as_str()) != Some("-") {
        return Err(
            DeskError::new("BAD_ARGS", "run 只从 stdin 读：用法 `desk run -`")
                .with_suggestion("例：Get-Content steps.json | desk run -"),
        );
    }
    let fail_fast = args.has("--fail-fast");
    // 上限 8 MB：stdin 来自 agent，不能无界读进内存。
    let mut buf = String::new();
    {
        use std::io::Read;
        let mut stdin = std::io::stdin().take(8 * 1024 * 1024);
        stdin
            .read_to_string(&mut buf)
            .map_err(|e| DeskError::new("BAD_ARGS", format!("读取 stdin 失败：{e}")))?;
    }
    let doc: Value = serde_json::from_str(&buf)
        .map_err(|e| DeskError::new("BAD_ARGS", format!("步骤 JSON 解析失败：{e}")))?;
    let steps = doc
        .get("steps")
        .and_then(|s| s.as_array())
        .ok_or_else(|| DeskError::new("BAD_ARGS", "步骤 JSON 需要一个 steps 数组"))?;
    // 各步自己声明的超时要把入口看门狗的预算往后推，否则 run 里的长 wait 会被误杀。
    let mut declared_ms: u64 = 0;
    for step in steps {
        let Value::Array(a) = step else {
            continue;
        };
        let argv: Vec<String> = a
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect();
        let Some(cmd) = argv.first() else {
            continue;
        };
        let _ = cmd;
        let step_args = Args::new(&argv[1..]);
        if let Ok(Some(ms)) = step_args.int("--timeout-ms") {
            declared_ms = declared_ms.saturating_add(ms.max(0) as u64);
        } else if let Ok(Some(s)) = step_args.int("--seconds") {
            declared_ms = declared_ms.saturating_add((s.max(0) as u64).saturating_mul(1000));
        }
    }
    if declared_ms > 0 {
        extend_watchdog(declared_ms.saturating_add(30_000));
    }
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
        let Some(cmd) = argv.first() else {
            results.push(json!({"i": i, "ok": false, "error": "空步骤"}));
            continue;
        };
        if commands::get(cmd).is_none() {
            results.push(json!({
                "i": i, "ok": false,
                "error": DeskError::new("BAD_ARGS", format!("未知子命令：{cmd}")).to_json(),
            }));
            if fail_fast {
                break;
            }
            continue;
        }
        let step_args = Args::new(&argv[1..]);
        let outcome = step_args
            .validate(cmd)
            .and_then(|_| dispatch(cmd, &step_args));
        match outcome {
            Ok((name, data)) => results.push(json!({
                "i": i, "ok": true, "command": name,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "data": data,
            })),
            Err(e) => {
                let failed = e.retry_safe;
                results.push(json!({
                    "i": i, "ok": false, "error": e.to_json(),
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }));
                if fail_fast || !failed {
                    break;
                }
            }
        }
    }
    let failed = results.iter().filter(|r| r["ok"] == false).count();
    let value = json!({"count": results.len(), "failed": failed, "results": results});
    if failed > 0 {
        return Err(
            DeskError::new("RUN_FAILED", format!("批量执行有 {failed} 步失败"))
                .with_details(value)
                .delivered(),
        );
    }
    Ok(value)
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
    if !args.positionals().is_empty() {
        return Err(DeskError::new(
            "BAD_ARGS",
            "shot 不接受位置参数；--screen 是开关，不跟显示器序号",
        )
        .with_suggestion("例：desk shot --screen --out C:\\temp\\a.png"));
    }
    let mut img = if args.has("--screen") {
        if args.has("--window") || args.has("--ref") {
            return Err(DeskError::new(
                "BAD_ARGS",
                "--screen 和 --window 只能二选一",
            ));
        }
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
            // 同一毫秒两次 shot 不能互相覆盖。
            dir.join(format!("shot-{}-{}.png", now_ms(), std::process::id()))
        }
    };
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| DeskError::new("BAD_ARGS", format!("截图输出目录不可创建：{e}")))?;
    }

    let mut marks_out = Vec::new();
    let mut marks_total = 0usize;
    let mut snapshot_id = Value::Null;
    if args.has("--marks") {
        if args.has("--screen") {
            return Err(DeskError::new("BAD_ARGS", "--marks 只支持 --window"));
        }
        let hwnd = target_window(args)?;
        let opts = WalkOptions {
            max_depth: 16,
            limit: 300,
            raw_view: false,
        };
        let uia = Uia::new()?;
        let snap = act::snap(&uia, hwnd, &opts, true)?;
        snapshot_id = snap["snapshot_id"].clone();
        let (ox, oy) = img.origin();
        let image_area = img.width as i64 * img.height as i64;
        let mut marks = Vec::new();
        if let Some(elements) = snap["elements"].as_array() {
            for e in elements {
                let role = e["role"].as_str().unwrap_or("");
                if matches!(role, "document" | "window" | "pane" | "titlebar" | "text") {
                    continue;
                }
                let actionable = e["actions"].as_array().is_some_and(|a| {
                    a.iter().any(|v| {
                        matches!(
                            v.as_str(),
                            Some("click" | "select" | "toggle" | "set" | "expand")
                        )
                    })
                });
                if !actionable {
                    continue;
                }
                let (Some(x), Some(y), Some(w), Some(h)) = (
                    e["rect"]["x"].as_i64(),
                    e["rect"]["y"].as_i64(),
                    e["rect"]["w"].as_i64(),
                    e["rect"]["h"].as_i64(),
                ) else {
                    continue;
                };
                // 整窗/大半窗的框会把画面盖住，编号也没有意义。
                if w <= 0 || h <= 0 || w * h > image_area / 2 {
                    continue;
                }
                let (x, y) = (x as i32 - ox, y as i32 - oy);
                if x < 0
                    || y < 0
                    || x + w as i32 > img.width as i32
                    || y + h as i32 > img.height as i32
                {
                    continue;
                }
                marks_total += 1;
                if marks.len() >= 60 {
                    continue;
                }
                let n = marks.len() + 1;
                marks.push(shot::Mark {
                    n,
                    x,
                    y,
                    w: w as i32,
                    h: h as i32,
                });
                marks_out.push(json!({
                    "n": n,
                    "ref": e["ref"],
                    "role": e["role"],
                    "name": e["name"],
                    "rect": {"x": x + ox, "y": y + oy, "w": w, "h": h},
                }));
            }
        }
        shot::draw_marks(&mut img, &marks);
    }

    shot::save_png(&img, &path)?;
    Ok(json!({
        "path": path.display().to_string(),
        "width": img.width,
        "height": img.height,
        "origin": {"x": img.origin_x, "y": img.origin_y},
        "blank": img.blank,
        "snapshot_id": snapshot_id,
        "marks_total": marks_total,
        "marks_truncated": marks_total > marks_out.len(),
        "marks": marks_out,
    }))
}

fn window_action(cmd: &str, args: &Args) -> Result<Value> {
    let id = window_id_arg(args).ok_or_else(|| need("<窗口 id>"))?;
    let hwnd = win::parse_window_id(&id)?;
    act::ensure_reachable(hwnd)?;
    win::touch(hwnd);
    match cmd {
        "minimize" => {
            // SW_MINIMIZE 会激活窗口；SW_SHOWMINNOACTIVE 只最小化。
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOWMINNOACTIVE);
            }
            Ok(json!({"id": id, "minimized": unsafe { IsIconic(hwnd).as_bool() }}))
        }
        "maximize" => {
            unsafe {
                let _ = ShowWindow(hwnd, SW_MAXIMIZE);
            }
            let value = json!({"id": id, "maximized": unsafe { IsZoomed(hwnd).as_bool() }});
            Ok(value)
        }
        "restore" => {
            // SetWindowPlacement 不会激活窗口，又能同时取消最小化和最大化。
            use windows::Win32::UI::WindowsAndMessaging::{
                GetWindowPlacement, SW_SHOWNORMAL, SetWindowPlacement, WINDOWPLACEMENT,
            };
            let mut wp = WINDOWPLACEMENT {
                length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
                ..Default::default()
            };
            let got = unsafe { GetWindowPlacement(hwnd, &mut wp) }.is_ok();
            if got {
                wp.showCmd = SW_SHOWNORMAL.0 as u32;
                wp.flags = Default::default();
                unsafe {
                    let _ = SetWindowPlacement(hwnd, &wp);
                }
            } else {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                }
            }
            let minimized = unsafe { IsIconic(hwnd).as_bool() };
            let maximized = unsafe { IsZoomed(hwnd).as_bool() };
            Ok(json!({"id": id, "restored": !minimized && !maximized,
                "minimized": minimized, "maximized": maximized}))
        }
        _ => {
            let ok = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) }.is_ok();
            if !ok {
                return Err(DeskError::new("BG_DELIVER_FAILED", "WM_CLOSE 投递失败").delivered());
            }
            // 投递成功 ≠ 窗口关了：等它真的消失再报 closed。
            let deadline = Instant::now() + Duration::from_millis(1500);
            while Instant::now() < deadline {
                if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
                    return Ok(json!({"id": id, "closed": true}));
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(DeskError::new(
                "CLOSE_NOT_CONFIRMED",
                "WM_CLOSE 已投递，但窗口 1.5 秒内没有关闭",
            )
            .with_suggestion("程序可能在弹确认框；用 windows 看新窗口")
            .delivered())
        }
    }
}

fn window_geometry(cmd: &str, args: &Args) -> Result<Value> {
    let r = args.positionals();
    let id = window_id_arg(args).ok_or_else(|| need("<窗口 id> <x> <y>|<w> <h>"))?;
    let nums: Vec<&String> = r.iter().filter(|s| s.parse::<i32>().is_ok()).collect();
    let a = parse_i32_arg(nums.first().copied(), "坐标/宽度")?;
    let b = parse_i32_arg(nums.get(1).copied(), "坐标/高度")?;
    let hwnd = win::parse_window_id(&id)?;
    act::ensure_reachable(hwnd)?;
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
    win::touch(hwnd);
    unsafe { SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE) }
        .map_err(|e| DeskError::new("WINDOW_MOVE_FAILED", format!("移动/缩放窗口失败：{e}")))?;
    Ok(json!({"id": id, "rect": {"x": x, "y": y, "w": w, "h": h}}))
}

fn parse_i32_arg(v: Option<&String>, what: &str) -> Result<i32> {
    let s = v.ok_or_else(|| need("<x> <y> 或 <w> <h>"))?;
    s.parse::<i32>()
        .map_err(|_| DeskError::new("BAD_ARGS", format!("{what}不是整数：{s}")))
}

fn launch_cmd(args: &Args) -> Result<Value> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::GetProcessId;
    use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE;
    let r = args.positionals();
    let target = r.first().ok_or_else(|| need("<exe 或路径>"))?;
    let dir = args.opt("--dir");
    if let Some(d) = &dir
        && !std::path::Path::new(d).is_dir()
    {
        return Err(DeskError::new("BAD_ARGS", format!("--dir 不是目录：{d}")));
    }
    let dir_h = dir.as_deref().map(windows::core::HSTRING::from);
    let verb = windows::core::HSTRING::from("open");
    let file = windows::core::HSTRING::from(target.as_str());
    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: windows::core::PCWSTR(verb.as_ptr()),
        lpFile: windows::core::PCWSTR(file.as_ptr()),
        lpDirectory: dir_h
            .as_ref()
            .map(|h| windows::core::PCWSTR(h.as_ptr()))
            .unwrap_or(windows::core::PCWSTR::null()),
        nShow: SW_SHOWNOACTIVATE.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut sei) }.map_err(|e| {
        DeskError::new("LAUNCH_FAILED", format!("启动失败：{e}"))
            .with_suggestion("确认路径存在，或直接给完整 exe 路径")
    })?;
    let pid = if sei.hProcess.is_invalid() {
        0
    } else {
        let pid = unsafe { GetProcessId(sei.hProcess) };
        let _ = unsafe { CloseHandle(sei.hProcess) };
        pid
    };
    Ok(json!({"launched": target, "pid": pid}))
}

fn is_role(s: &str) -> bool {
    // 只认 uia::role_name 这张表，别再抄第二份。
    (50000..=50040).any(|t| uia::role_name(t) == s)
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
