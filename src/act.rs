//! 动作层：解析 ref → 优先语义动作 → 失败再后台注入（永不默认抢前台）。

use crate::error::{DeskError, Result};
use crate::input;
use crate::store;
use crate::uia::{Node, Uia, WalkOptions};
use crate::win;
use serde_json::{Value, json};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Accessibility::IUIAutomationElement;

pub struct Resolved {
    pub node: Node,
    pub element: IUIAutomationElement,
    pub window_id: String,
}

fn split_ref(ref_id: &str) -> Result<(String, String)> {
    let body = ref_id.strip_prefix('@').ok_or_else(|| {
        DeskError::new("BAD_REF", format!("ref 应该以 @ 开头：{ref_id}"))
            .with_suggestion("ref 形如 @s1a2b:e3，由 `desk snap` 给出")
    })?;
    let (sid, item) = body
        .split_once(':')
        .ok_or_else(|| DeskError::new("BAD_REF", format!("ref 缺少快照 id：{ref_id}")))?;
    // 快照 id 只允许 s + 小写字母数字：挡掉 `@..\..\x:e1` 这类路径穿越和 UNC。
    let ok = sid.len() >= 2
        && sid.len() <= 25
        && sid.starts_with('s')
        && sid[1..]
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if !ok {
        return Err(DeskError::new("BAD_REF", format!("快照 id 不合法：{sid}"))
            .with_suggestion("ref 必须来自 `desk snap` 的输出"));
    }
    Ok((sid.to_string(), item.to_string()))
}

pub fn resolve(uia: &Uia, ref_id: &str) -> Result<Resolved> {
    let (sid, _item) = split_ref(ref_id)?;
    let snap = store::load(&sid)?;
    let node = snap
        .find(ref_id)
        .cloned()
        .ok_or_else(|| DeskError::new("BAD_REF", format!("快照里没有这个 ref：{ref_id}")))?;
    let hwnd = win::parse_window_id(&snap.window_id)?;

    // UIPI 前置检查：无论走哪条路，只要目标比我们高就不动手。
    ensure_reachable(hwnd)?;

    let mut node = node;
    // 先按快照路径取：命中的元素身份必须对得上，否则说明界面动过了。
    if let Some(el) = uia.element_at_path_cached(hwnd, &node, snap.raw_view) {
        // 用实时矩形覆盖快照里的旧矩形：窗口被移动/缩放后坐标才是准的。
        if let Some(fresh) = uia.rect_of(&el) {
            node.rect = fresh;
        }
        return Ok(Resolved {
            node,
            element: el,
            window_id: snap.window_id,
        });
    }

    // 路径失效再按身份找，只有唯一匹配才继续。
    let el = uia.locate(hwnd, &node, snap.raw_view)?;
    if let Some(fresh) = uia.rect_of(&el) {
        node.rect = fresh;
    }
    Ok(Resolved {
        node,
        element: el,
        window_id: snap.window_id,
    })
}

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Semantic,
    Background,
}

pub fn parse_mode(s: &str) -> Result<Mode> {
    match s {
        "semantic" | "auto" => Ok(Mode::Semantic),
        "bg" | "background" => Ok(Mode::Background),
        other => Err(DeskError::new(
            "BAD_ARGS",
            format!("--mode 只能是 semantic / bg，收到：{other}"),
        )),
    }
}

/// 后台点击必须落在窗口客户区内，否则一定点空（比如列表被滚到视口外）。
fn ensure_inside(hwnd: HWND, sx: i32, sy: i32) -> Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
    let (cx, cy) = win::screen_to_client(hwnd, sx, sy);
    let mut rc = windows::Win32::Foundation::RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    if cx < 0 || cy < 0 || cx >= rc.right || cy >= rc.bottom {
        return Err(DeskError::new(
            "POINT_OUTSIDE",
            format!("元素中心 ({sx},{sy}) 落在窗口客户区外（客户区坐标 {cx},{cy}），点不到"),
        )
        .with_suggestion("先用 scrollitem 把它滚进可见区，或不加 --mode bg 走语义动作"));
    }
    Ok(())
}

/// Windows 的 UIPI：完整性级别低的一方不能把消息/输入投给高的一方。
/// 这里提前拦下，给出具体级别和解决办法，而不是让程序"点了没反应"。
pub fn ensure_reachable(hwnd: HWND) -> Result<()> {
    let Some(info) = (unsafe { win::describe(hwnd, true) }) else {
        return Ok(());
    };
    match win::can_reach(info.pid) {
        Ok(()) => Ok(()),
        Err((Some(me), Some(target))) => Err(DeskError::new(
            "ELEVATION_MISMATCH",
            format!(
                "目标进程完整性（{}）高于本工具（{}），Windows 的 UIPI 会丢弃所有窗口消息和输入",
                win::integrity_name(target),
                win::integrity_name(me)
            ),
        )
        .with_details(json!({
            "tool_integrity": win::integrity_name(me),
            "target_integrity": win::integrity_name(target),
            "target_pid": info.pid,
            "target_exe": info.exe,
        }))
        .with_suggestion(
            "用管理员身份运行 DSH/终端；或 `desk elevate -- <同样的命令>` 以更高完整性重新执行",
        )),
        Err(_) => Err(DeskError::new(
            "TARGET_PROTECTED",
            format!(
                "读不到目标进程（pid {}）的完整性级别，多半是受保护进程；动作会被内核丢弃",
                info.pid
            ),
        )
        .with_details(json!({"target_pid": info.pid, "target_exe": info.exe}))
        .with_suggestion("这类进程（杀软、反作弊）用户态无法自动化，只能走它的官方接口")),
    }
}

/// 执行一个动作并保持前台不变；返回值里带上"是否发生过抢占"。
fn guarded<F, T>(f: F) -> (Result<T>, bool)
where
    F: FnOnce() -> Result<T>,
{
    input::with_foreground_guard(f)
}

/// 调用 Invoke 并用选中状态回读确认。
/// 列表项/页签这类元素有的程序把 Invoke 实现成空操作（PowerWriter 的芯片列表就是），
/// 回读发现没选中就退回后台点击，避免"返回 ok 但界面没动"。
/// 轮询直到状态变化（或超时），返回最后一次读到的值。
/// 动作的验证方式：不同控件能回读的状态不一样，但"动作→回读→回退"这条策略只有一条。
#[derive(Clone, Copy, PartialEq)]
enum Verify {
    /// 没有可回读的状态（普通按钮、菜单项）：发完即返回，不补点。
    None,
    /// 期望变成"已选中"（选中类元素）。
    Becomes,
    /// 期望状态翻转（勾选类元素）。
    Flips,
}

/// 轮询直到状态变化（或超时），返回最后一次读到的值。
fn poll_state(
    mut read: impl FnMut() -> Option<bool>,
    before: Option<bool>,
    budget_ms: u128,
) -> Option<bool> {
    let start = std::time::Instant::now();
    let mut last = read();
    while last == before && start.elapsed().as_millis() < budget_ms {
        std::thread::sleep(std::time::Duration::from_millis(15));
        last = read();
    }
    last
}

/// 执行一个语义动作，按 `verify` 回读确认；程序收了没反应就退回后台点击。
///
/// 所有"需要验证的动作"都走这一条：invoke/select/toggle 不再各写一份。
fn act_verified<F>(
    _uia: &Uia,
    r: &Resolved,
    label: &str,
    verify: Verify,
    mut read: impl FnMut() -> Option<bool>,
    act: F,
) -> Result<(String, bool)>
where
    F: FnOnce() -> Result<()>,
{
    let unchanged = |before: Option<bool>, after: Option<bool>| match verify {
        Verify::None => true,
        Verify::Becomes => after == Some(true) || before == Some(true),
        Verify::Flips => after != before,
    };

    // 没有可回读状态：信任语义动作，绝不补第二次点击（会变成点两下）。
    if matches!(verify, Verify::None) {
        let (res, stolen) = guarded(act);
        res?;
        return Ok((format!("semantic:{label}"), stolen));
    }
    let before = read();
    if before.is_none() {
        let (res, stolen) = guarded(act);
        res?;
        return Ok((format!("semantic:{label}"), stolen));
    }

    let (res, mut stolen) = guarded(act);
    res?;
    let after = poll_state(&mut read, before, 150);
    if unchanged(before, after) {
        return Ok((format!("semantic:{label}"), stolen));
    }

    // 程序收下了但没生效：按元素矩形投一次后台点击，再回读。
    let (x, y) = ensure_center(_uia, r)?;
    let hwnd = win::parse_window_id(&r.window_id)?;
    ensure_inside(hwnd, x, y)?;
    let (res2, stolen2) = guarded(|| input::click_screen(hwnd, x, y, input::Button::Left));
    res2?;
    stolen |= stolen2;
    let after2 = poll_state(&mut read, after, 150);
    if unchanged(before, after2) {
        Ok((format!("bg:{label}-fallback"), stolen))
    } else {
        Ok((format!("bg:{label}-unverified"), stolen))
    }
}

fn invoke_verified(uia: &Uia, r: &Resolved) -> Result<(String, bool)> {
    let selectable = matches!(
        r.node.role.as_str(),
        "listitem" | "tabitem" | "radio" | "dataitem" | "treeitem" | "menuitem"
    );
    let verify = if selectable {
        Verify::Becomes
    } else {
        Verify::None
    };
    act_verified(
        uia,
        r,
        "invoke",
        verify,
        || uia.is_selected(&r.element),
        || uia.invoke(&r.element),
    )
}

fn select_verified(uia: &Uia, r: &Resolved) -> Result<(String, bool)> {
    act_verified(
        uia,
        r,
        "select",
        Verify::Becomes,
        || uia.is_selected(&r.element),
        || uia.select(&r.element),
    )
}

fn toggle_verified(uia: &Uia, r: &Resolved) -> Result<(String, bool)> {
    act_verified(
        uia,
        r,
        "toggle",
        Verify::Flips,
        || uia.is_toggled(&r.element),
        || uia.toggle(&r.element),
    )
}
/// 取元素中心点；拿不到矩形（列表项未实例化/在视口外）就先滚进来再读。
fn ensure_center(uia: &Uia, r: &Resolved) -> Result<(i32, i32)> {
    if !r.node.rect.is_empty() {
        return Ok(center(&r.node));
    }
    let _ = uia.scroll_into_view(&r.element);
    std::thread::sleep(std::time::Duration::from_millis(80));
    if let Some(rect) = uia.rect_of(&r.element)
        && !rect.is_empty()
    {
        return Ok((rect.x + rect.width / 2, rect.y + rect.height / 2));
    }
    Err(
        DeskError::new("POINT_UNKNOWN", "拿不到元素坐标（元素在视口外且滚不进来）")
            .with_suggestion("改用语义动作（不加 --mode bg），或先 `desk scrollitem <@ref>`"),
    )
}

fn center(node: &Node) -> (i32, i32) {
    (
        node.rect.x + node.rect.width / 2,
        node.rect.y + node.rect.height / 2,
    )
}

/// 点击：语义 → 后台注入 → （只有显式要求才）物理。
pub fn click(uia: &Uia, ref_id: &str, mode: Option<Mode>) -> Result<Value> {
    let r = resolve(uia, ref_id)?;
    let has = |a: &str| r.node.actions.iter().any(|x| x == a);
    let tried: Vec<&str> = r.node.actions.iter().map(|s| s.as_str()).collect();

    let force_bg = matches!(mode, Some(Mode::Background));
    if !force_bg {
        if has("click") {
            let (mode_name, stolen) = invoke_verified(uia, &r)?;
            let mut out = json!({
                "ref": ref_id, "mode": mode_name, "actions": tried,
                "element": {"role": r.node.role, "name": r.node.name},
            });
            if stolen {
                out["focus_restored"] = json!(true);
            }
            return Ok(out);
        }
        if has("select") || has("toggle") {
            let (mode_name, stolen) = if has("select") {
                select_verified(uia, &r)?
            } else {
                toggle_verified(uia, &r)?
            };
            let mut out = json!({
                "ref": ref_id, "mode": mode_name, "actions": tried,
                "element": {"role": r.node.role, "name": r.node.name},
            });
            if stolen {
                out["focus_restored"] = json!(true);
            }
            return Ok(out);
        }
    }

    // 语义路走不通：退回后台注入（不抢前台、不动光标）。
    let (x, y) = ensure_center(uia, &r)?;
    let hwnd = win::parse_window_id(&r.window_id)?;
    ensure_inside(hwnd, x, y)?;
    let (res, stolen) =
        input::with_foreground_guard(|| input::click_screen(hwnd, x, y, input::Button::Left));
    res?;
    let mut out = json!({
        "ref": ref_id, "mode": if force_bg {"bg"} else {"bg:fallback"},
        "actions": tried,
        "at": {"x": x, "y": y},
        "element": {"role": r.node.role, "name": r.node.name},
        "note": if force_bg {
            "按 --mode bg 直接投递后台鼠标消息"
        } else {
            "该元素没有可用的 UIA 动作，已改为投递后台鼠标消息"
        },
    });
    if stolen {
        out["focus_restored"] = json!(true);
    }
    Ok(out)
}

/// 右键 / 双击：只走后台注入（语义层没有这两个动作）。
pub fn right_click(uia: &Uia, ref_id: &str) -> Result<Value> {
    let r = resolve(uia, ref_id)?;
    let (x, y) = ensure_center(uia, &r)?;
    let hwnd = win::parse_window_id(&r.window_id)?;
    ensure_inside(hwnd, x, y)?;
    input::click_screen(hwnd, x, y, input::Button::Right)?;
    Ok(json!({"ref": ref_id, "mode": "bg:right", "at": {"x": x, "y": y}}))
}

pub fn double_click(uia: &Uia, ref_id: &str) -> Result<Value> {
    let r = resolve(uia, ref_id)?;
    let (x, y) = ensure_center(uia, &r)?;
    let hwnd = win::parse_window_id(&r.window_id)?;
    ensure_inside(hwnd, x, y)?;
    input::double_click_screen(hwnd, x, y)?;
    Ok(json!({"ref": ref_id, "mode": "bg:double", "at": {"x": x, "y": y}}))
}

/// 单个元素的完整信息（调试和精确定位用）。
pub fn info(uia: &Uia, ref_id: &str) -> Result<Value> {
    let r = resolve(uia, ref_id)?;
    let native = unsafe { r.element.CurrentNativeWindowHandle() }
        .ok()
        .map(|h| format!("{:x}", h.0 as usize))
        .unwrap_or_default();
    Ok(json!({
        "ref": ref_id,
        "role": r.node.role,
        "name": r.node.name,
        "automation_id": r.node.automation_id,
        "class": r.node.class_name,
        "rect": {"x": r.node.rect.x, "y": r.node.rect.y, "w": r.node.rect.width, "h": r.node.rect.height},
        "actions": r.node.actions,
        "value": uia.value_of(&r.element),
        "focused": unsafe { r.element.CurrentHasKeyboardFocus() }.map(|b| b.as_bool()).unwrap_or(false),
        "keyboard_focusable": unsafe { r.element.CurrentIsKeyboardFocusable() }.map(|b| b.as_bool()).unwrap_or(false),
        "native_hwnd": native,
        "target_integrity": unsafe { win::describe(win::parse_window_id(&r.window_id)?, true) }
            .map(|i| i.integrity)
            .unwrap_or_else(|| "unknown".to_string()),
        "tool_integrity": win::integrity_label(std::process::id()),
        "window_id": r.window_id,
    }))
}

pub fn scroll_into_view(uia: &Uia, ref_id: &str) -> Result<Value> {
    let r = resolve(uia, ref_id)?;
    let (res, _stolen) = guarded(|| uia.scroll_into_view(&r.element));
    res?;
    Ok(json!({"ref": ref_id, "mode": "semantic:scrollinto",
        "element": {"role": r.node.role, "name": r.node.name}}))
}

pub fn select(uia: &Uia, ref_id: &str) -> Result<Value> {
    let r = resolve(uia, ref_id)?;
    let (mode_name, stolen) = select_verified(uia, &r)?;
    let mut out = json!({"ref": ref_id, "mode": mode_name,
        "element": {"role": r.node.role, "name": r.node.name}});
    if stolen {
        out["focus_restored"] = json!(true);
    }
    Ok(out)
}

pub fn toggle(uia: &Uia, ref_id: &str) -> Result<Value> {
    let r = resolve(uia, ref_id)?;
    let (mode_name, stolen) = toggle_verified(uia, &r)?;
    let mut out = json!({"ref": ref_id, "mode": mode_name,
        "element": {"role": r.node.role, "name": r.node.name}});
    if stolen {
        out["focus_restored"] = json!(true);
    }
    Ok(out)
}

pub fn set_value(uia: &Uia, ref_id: &str, value: &str) -> Result<Value> {
    let r = resolve(uia, ref_id)?;
    let (res, stolen) = guarded(|| uia.set_value(&r.element, value));
    res?;
    let after = uia.value_of(&r.element);
    // 回读确认：程序可能忽略、截断或格式化写入的值。
    let matches = after.as_deref() == Some(value);
    let mut out = json!({"ref": ref_id, "mode": "semantic:set", "value": after,
        "value_matches": matches,
        "element": {"role": r.node.role, "name": r.node.name}});
    if stolen {
        out["focus_restored"] = json!(true);
    }
    Ok(out)
}

pub fn expand(uia: &Uia, ref_id: &str, open: bool) -> Result<Value> {
    let r = resolve(uia, ref_id)?;
    let (res, stolen) = guarded(|| uia.expand(&r.element, open));
    res?;
    let mut out = json!({"ref": ref_id, "mode": if open {"expand"} else {"collapse"},
        "element": {"role": r.node.role, "name": r.node.name}});
    if stolen {
        out["focus_restored"] = json!(true);
    }
    Ok(out)
}

/// 后台键盘 / 文本：直接发给窗口，不抢前台。
pub fn key(hwnd: HWND, combo: &str) -> Result<Value> {
    ensure_reachable(hwnd)?;
    let (res, stolen) = input::with_foreground_guard(|| input::key_combo(hwnd, combo));
    res?;
    let mut out = json!({"mode": "bg:key", "combo": combo});
    if stolen {
        out["focus_restored"] = json!(true);
    }
    Ok(out)
}

pub fn type_text(hwnd: HWND, text: &str) -> Result<Value> {
    ensure_reachable(hwnd)?;
    let (res, stolen) = input::with_foreground_guard(|| input::type_text(hwnd, text));
    res?;
    let mut out = json!({"mode": "bg:type", "chars": text.chars().count()});
    if stolen {
        out["focus_restored"] = json!(true);
    }
    Ok(out)
}

/// 对某个元素滚轮：坐标取元素中心，投给该点下最深的子窗口。
pub fn scroll_ref(uia: &Uia, ref_id: &str, delta: i32) -> Result<Value> {
    let r = resolve(uia, ref_id)?;
    let hwnd = win::parse_window_id(&r.window_id)?;
    let (x, y) = ensure_center(uia, &r)?;
    let (res, stolen) = guarded(|| input::wheel_screen(hwnd, x, y, delta));
    res?;
    let mut out = json!({
        "ref": ref_id, "mode": "bg:scroll", "delta": delta,
        "at": {"x": x, "y": y},
        "element": {"role": r.node.role, "name": r.node.name},
    });
    if stolen {
        out["focus_restored"] = json!(true);
    }
    Ok(out)
}

pub fn wheel(hwnd: HWND, x: i32, y: i32, delta: i32) -> Result<Value> {
    ensure_reachable(hwnd)?;
    let (res, _stolen) = input::with_foreground_guard(|| input::wheel_screen(hwnd, x, y, delta));
    res?;
    Ok(json!({"mode": "bg:wheel", "delta": delta}))
}

pub fn drag(hwnd: HWND, from: (i32, i32), to: (i32, i32)) -> Result<Value> {
    ensure_reachable(hwnd)?;
    let (res, _stolen) = guarded(|| input::drag_screen(hwnd, from, to, 24));
    res?;
    Ok(json!({"mode": "bg:drag", "from": {"x": from.0, "y": from.1}, "to": {"x": to.0, "y": to.1}}))
}

/// 快照 + 落盘，返回给 CLI 的 JSON。
pub fn snap(uia: &Uia, hwnd: HWND, opts: &WalkOptions, persist: bool) -> Result<Value> {
    let info = unsafe { win::describe(hwnd, true) }
        .ok_or_else(|| DeskError::new("WINDOW_GONE", "目标窗口已消失"))?;
    let sid = store::new_id();
    let nodes = uia.snapshot(hwnd, &sid, opts)?;
    let file = store::SnapshotFile {
        id: sid.clone(),
        window_id: info.id.clone(),
        title: info.title.clone(),
        created_ms: now_ms(),
        nodes,
        raw_view: opts.raw_view,
    };
    if persist {
        store::save(&file)?;
    }
    let elements: Vec<Value> = file.nodes.iter().map(compact).collect();
    let truncated = file.nodes.len() >= opts.limit;
    Ok(json!({
        "snapshot_id": sid,
        "window": {"id": info.id, "title": info.title, "exe": info.exe},
        "count": file.nodes.len(),
        "truncated": truncated,
        "elements": elements,
    }))
}

fn compact(n: &Node) -> Value {
    let mut o = json!({
        "ref": n.ref_id,
        "role": n.role,
        "name": n.name,
        "depth": n.depth,
        "actions": n.actions,
    });
    if !n.rect.is_empty() {
        o["rect"] = json!({"x": n.rect.x, "y": n.rect.y, "w": n.rect.width, "h": n.rect.height});
    }
    if !n.automation_id.is_empty() {
        o["id"] = json!(n.automation_id);
    }
    if let Some(v) = &n.value {
        o["value"] = json!(v);
    }
    if !n.enabled {
        o["enabled"] = json!(false);
    }
    if n.offscreen {
        o["offscreen"] = json!(true);
    }
    if n.focused {
        o["focused"] = json!(true);
    }
    o
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
