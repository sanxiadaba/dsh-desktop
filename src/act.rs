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

    // 先按快照路径取：命中的元素身份必须对得上，否则说明界面动过了。
    if let Some(el) = uia.element_at_path_cached(hwnd, &node) {
        return Ok(Resolved {
            node,
            element: el,
            window_id: snap.window_id,
        });
    }

    // 路径失效再按身份找，只有唯一匹配才继续。
    let el = uia.locate(hwnd, &node)?;
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
    Physical,
}

pub fn parse_mode(s: &str) -> Result<Mode> {
    match s {
        "semantic" | "auto" => Ok(Mode::Semantic),
        "bg" | "background" => Ok(Mode::Background),
        "physical" => Ok(Mode::Physical),
        other => Err(DeskError::new(
            "BAD_ARGS",
            format!("--mode 只能是 semantic / bg / physical，收到：{other}"),
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

    if matches!(mode, Some(Mode::Physical)) {
        return Err(DeskError::new(
            "NOT_IMPLEMENTED",
            "物理点击还没接上（它会抢前台，故意不默认开）",
        ));
    }

    let force_bg = matches!(mode, Some(Mode::Background));
    if !force_bg {
        if has("click") {
            uia.invoke(&r.element)?;
            return Ok(json!({
                "ref": ref_id, "mode": "semantic:invoke", "actions": tried,
                "element": {"role": r.node.role, "name": r.node.name},
            }));
        }
        if has("select") {
            uia.select(&r.element)?;
            return Ok(json!({
                "ref": ref_id, "mode": "semantic:select", "actions": tried,
                "element": {"role": r.node.role, "name": r.node.name},
            }));
        }
        if has("toggle") {
            uia.toggle(&r.element)?;
            return Ok(json!({
                "ref": ref_id, "mode": "semantic:toggle", "actions": tried,
                "element": {"role": r.node.role, "name": r.node.name},
            }));
        }
    }

    // 语义路走不通：退回后台注入（不抢前台、不动光标）。
    let (x, y) = center(&r.node);
    let hwnd = win::parse_window_id(&r.window_id)?;
    ensure_inside(hwnd, x, y)?;
    input::click_screen(hwnd, x, y, input::Button::Left)?;
    Ok(json!({
        "ref": ref_id, "mode": if force_bg {"bg"} else {"bg:fallback"},
        "actions": tried,
        "at": {"x": x, "y": y},
        "element": {"role": r.node.role, "name": r.node.name},
        "note": "该元素没有可用的 UIA 动作，已改为投递后台鼠标消息",
    }))
}

/// 右键 / 双击：只走后台注入（语义层没有这两个动作）。
pub fn right_click(uia: &Uia, ref_id: &str) -> Result<Value> {
    let r = resolve(uia, ref_id)?;
    let (x, y) = center(&r.node);
    let hwnd = win::parse_window_id(&r.window_id)?;
    ensure_inside(hwnd, x, y)?;
    input::click_screen(hwnd, x, y, input::Button::Right)?;
    Ok(json!({"ref": ref_id, "mode": "bg:right", "at": {"x": x, "y": y}}))
}

pub fn double_click(uia: &Uia, ref_id: &str) -> Result<Value> {
    let r = resolve(uia, ref_id)?;
    let (x, y) = center(&r.node);
    let hwnd = win::parse_window_id(&r.window_id)?;
    ensure_inside(hwnd, x, y)?;
    let (cx, cy) = win::screen_to_client(hwnd, x, y);
    input::double_click_client(hwnd, cx, cy)?;
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
        "window_id": r.window_id,
    }))
}

pub fn scroll_into_view(uia: &Uia, ref_id: &str) -> Result<Value> {
    let r = resolve(uia, ref_id)?;
    uia.scroll_into_view(&r.element)?;
    Ok(json!({"ref": ref_id, "mode": "semantic:scrollinto",
        "element": {"role": r.node.role, "name": r.node.name}}))
}

pub fn set_focus(uia: &Uia, ref_id: &str) -> Result<Value> {
    let r = resolve(uia, ref_id)?;
    uia.set_focus(&r.element)?;
    Ok(json!({"ref": ref_id, "mode": "semantic:focus",
        "element": {"role": r.node.role, "name": r.node.name},
        "note": "元素获得键盘焦点；窗口可能被激活"}))
}

pub fn select(uia: &Uia, ref_id: &str) -> Result<Value> {
    let r = resolve(uia, ref_id)?;
    uia.select(&r.element)?;
    Ok(json!({"ref": ref_id, "mode": "semantic:select",
        "element": {"role": r.node.role, "name": r.node.name}}))
}

pub fn toggle(uia: &Uia, ref_id: &str) -> Result<Value> {
    let r = resolve(uia, ref_id)?;
    uia.toggle(&r.element)?;
    Ok(json!({"ref": ref_id, "mode": "semantic:toggle",
        "element": {"role": r.node.role, "name": r.node.name}}))
}

pub fn set_value(uia: &Uia, ref_id: &str, value: &str) -> Result<Value> {
    let r = resolve(uia, ref_id)?;
    uia.set_value(&r.element, value)?;
    let after = uia.value_of(&r.element);
    Ok(
        json!({"ref": ref_id, "mode": "semantic:set", "value": after,
        "element": {"role": r.node.role, "name": r.node.name}}),
    )
}

pub fn expand(uia: &Uia, ref_id: &str, open: bool) -> Result<Value> {
    let r = resolve(uia, ref_id)?;
    uia.expand(&r.element, open)?;
    Ok(
        json!({"ref": ref_id, "mode": if open {"expand"} else {"collapse"},
        "element": {"role": r.node.role, "name": r.node.name}}),
    )
}

/// 后台键盘 / 文本：直接发给窗口，不抢前台。
pub fn key(hwnd: HWND, combo: &str) -> Result<Value> {
    input::key_combo(hwnd, combo)?;
    Ok(json!({"mode": "bg:key", "combo": combo}))
}

pub fn type_text(hwnd: HWND, text: &str) -> Result<Value> {
    input::type_text(hwnd, text)?;
    Ok(json!({"mode": "bg:type", "chars": text.chars().count()}))
}

pub fn wheel(hwnd: HWND, x: i32, y: i32, delta: i32) -> Result<Value> {
    input::wheel_screen(hwnd, x, y, delta)?;
    Ok(json!({"mode": "bg:wheel", "delta": delta}))
}

pub fn drag(hwnd: HWND, from: (i32, i32), to: (i32, i32)) -> Result<Value> {
    let (cx0, cy0) = win::screen_to_client(hwnd, from.0, from.1);
    let (cx1, cy1) = win::screen_to_client(hwnd, to.0, to.1);
    input::drag_client(hwnd, (cx0, cy0), (cx1, cy1), 24)?;
    Ok(json!({"mode": "bg:drag", "from": {"x": from.0, "y": from.1}, "to": {"x": to.0, "y": to.1}}))
}

/// 快照 + 落盘，返回给 CLI 的 JSON。
pub fn snap(uia: &Uia, hwnd: HWND, opts: &WalkOptions) -> Result<Value> {
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
    };
    store::save(&file)?;
    let elements: Vec<Value> = file.nodes.iter().map(compact).collect();
    Ok(json!({
        "snapshot_id": sid,
        "window": {"id": info.id, "title": info.title, "exe": info.exe},
        "count": file.nodes.len(),
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
