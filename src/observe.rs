//! 动作后的观测：把"界面因此变了什么"直接放进返回。
//!
//! 以前 agent 只能自己再 `snap` 一次：多一次进程、多一次整树遍历，还要自己比。
//! 这里在动作前后各看一眼目标窗口，给出 appeared/disappeared/value_changed/
//! new_windows 和窗口状态。观测不落盘，也不返回 ref（那些 ref 没有存储可解析）。

use crate::cli::Args;
use crate::store;
use crate::uia::{Node, Uia, WalkOptions};
use crate::win;
use serde_json::{Value, json};
use std::collections::HashMap;
use windows::Win32::Foundation::HWND;

pub struct Observation {
    target: Option<HWND>,
    baseline: Vec<Node>,
    title: String,
    windows: Vec<String>,
    /// 基线快照的遍历上限：after 必须用同一组上限，否则会把"没截到的部分"误报成新出现。
    limit: usize,
    depth: usize,
    raw_view: bool,
}

/// 在动作之前调用：记下目标窗口、它当前的树、以及顶层窗口集合。
pub fn capture(args: &Args) -> Observation {
    let mut target = None;
    let mut baseline = Vec::new();
    let mut title = String::new();
    let mut limit = 300usize;
    let mut depth = 16usize;
    let mut raw_view = false;

    // 元素动作：--ref 优先（set 的值可能是以 @ 开头的字面量），其次位置参数里的 @ref。
    let ref_id = args
        .opt("--ref")
        .or_else(|| args.positionals().into_iter().find(|s| s.starts_with('@')));
    if let Some(ref_id) = ref_id
        && let Some(body) = ref_id.strip_prefix('@')
        && let Some((sid, _)) = body.split_once(':')
        && let Ok(snap) = store::load(sid)
        && let Ok(hwnd) = win::parse_window_id(&snap.window_id)
    {
        target = Some(hwnd);
        title = snap.title.clone();
        baseline = snap.nodes;
        limit = snap.limit;
        depth = snap.depth;
        raw_view = snap.raw_view;
    }

    // 窗口级动作：--window 或位置参数里的 w-xxxx。
    if target.is_none() {
        let id = args
            .opt("--window")
            .or_else(|| args.positionals().into_iter().find(|s| s.starts_with("w-")));
        if let Some(id) = id
            && let Ok(hwnd) = win::parse_window_id(&id)
        {
            target = Some(hwnd);
            // 用该窗口最近一次 snap 当基线：没有就只报窗口状态，不假装"全部是新出现的"。
            if let Some(snap) = store::latest_for_window(&id) {
                title = snap.title.clone();
                baseline = snap.nodes;
                limit = snap.limit;
                depth = snap.depth;
                raw_view = snap.raw_view;
            } else {
                title = unsafe { win::describe(hwnd, true) }
                    .map(|i| i.title)
                    .unwrap_or_default();
            }
        }
    }

    Observation {
        target,
        baseline,
        title,
        // 连不可见/工具窗口一起记，才能发现"菜单弹窗"这类无标题的新 HWND。
        windows: win::enum_windows(true).into_iter().map(|w| w.id).collect(),
        limit,
        depth,
        raw_view,
    }
}

fn fingerprint(n: &Node) -> String {
    let id = if n.automation_id.is_empty() {
        &n.name
    } else {
        &n.automation_id
    };
    format!("{}|{}|{}", n.role, id, n.class_name)
}

fn brief(n: &Node) -> Value {
    let mut o = json!({"role": n.role, "name": n.name});
    if !n.automation_id.is_empty() {
        o["id"] = json!(n.automation_id);
    }
    if !n.rect.is_empty() {
        o["rect"] = json!({"x": n.rect.x, "y": n.rect.y, "w": n.rect.width, "h": n.rect.height});
    }
    if let Some(v) = &n.value {
        o["value"] = json!(v);
    }
    if let Some(c) = n.checked {
        o["checked"] = json!(c);
    }
    if let Some(s) = n.selected {
        o["selected"] = json!(s);
    }
    if n.focused {
        o["focused"] = json!(true);
    }
    o
}

/// 两个节点是不是"同一个位置"（窗口内坐标，允许几像素抖动）。
fn same_place(a: &Node, b: &Node) -> bool {
    if a.rect.is_empty() || b.rect.is_empty() {
        return false;
    }
    let dx = (a.rect.x + a.rect.width / 2) - (b.rect.x + b.rect.width / 2);
    let dy = (a.rect.y + a.rect.height / 2) - (b.rect.y + b.rect.height / 2);
    dx.abs() <= 8 && dy.abs() <= 8
}

fn group(nodes: &[Node]) -> HashMap<String, Vec<&Node>> {
    let mut map: HashMap<String, Vec<&Node>> = HashMap::new();
    for n in nodes {
        map.entry(fingerprint(n)).or_default().push(n);
    }
    map
}

/// 在动作之后调用：重新看一眼，算出变化摘要。
pub fn after(obs: &Observation) -> Value {
    let started = std::time::Instant::now();
    let mut out = json!({
        "changed": false,
        "appeared": [],
        "disappeared": [],
        "value_changed": [],
        "state_changed": [],
        "moved": 0,
        "new_windows": [],
    });

    // 新出现的顶层窗口（模态框、菜单弹窗、被 launch 拉起的窗口）。
    // 菜单弹窗通常是空标题的 #32768，所以不能只看"标题非空"。
    let target_pid = obs.target.map(win::pid_of).unwrap_or_default();
    let mut new_windows = Vec::new();
    for info in win::enum_windows(true) {
        if obs.windows.contains(&info.id) || !info.visible {
            continue;
        }
        let interesting = !info.title.is_empty()
            || (target_pid != 0 && info.pid == target_pid)
            || info.class == "#32768";
        if interesting {
            new_windows.push(json!({
                "id": info.id,
                "title": info.title,
                "class": info.class,
                "exe": info.exe,
            }));
        }
    }
    new_windows.truncate(5);
    if !new_windows.is_empty() {
        out["changed"] = json!(true);
        out["new_windows"] = json!(new_windows);
    }

    let Some(hwnd) = obs.target else {
        out["ms"] = json!(started.elapsed().as_millis() as u64);
        return out;
    };
    let Some(info) = (unsafe { win::describe(hwnd, true) }) else {
        out["window"] = json!({"alive": false});
        out["changed"] = json!(true);
        out["ms"] = json!(started.elapsed().as_millis() as u64);
        return out;
    };
    out["window"] = json!({
        "id": info.id,
        "title": info.title,
        "alive": true,
        "minimized": info.minimized,
        "focused": info.focused,
    });
    if info.title != obs.title {
        out["changed"] = json!(true);
    }
    if info.minimized {
        // 最小化窗口 UIA 仍可读：继续做元素级 diff，否则 toggle 之类会误报 changed=false。
        out["window"]["minimized"] = json!(true);
    }
    // 没有基线就不要做元素级 diff：否则每个元素都会被算成"新出现"。
    if obs.baseline.is_empty() {
        out["note"] =
            json!("没有该窗口的基线快照；只报窗口状态与新窗口，先 `desk snap` 再做元素级 diff");
        out["ms"] = json!(started.elapsed().as_millis() as u64);
        return out;
    }

    // 重新读一遍树（不落盘）。读失败不影响动作结果，只记在 after.error。
    let uia = match Uia::new() {
        Ok(u) => u,
        Err(e) => {
            out["error"] = json!(e.code);
            out["ms"] = json!(started.elapsed().as_millis() as u64);
            return out;
        }
    };
    let opts = WalkOptions {
        max_depth: obs.depth,
        limit: obs.limit,
        raw_view: obs.raw_view,
    };
    let nodes = match uia.snapshot(hwnd, "sobs", &opts) {
        Ok((nodes, _)) => nodes,
        Err(e) => {
            out["error"] = json!(e.code);
            out["ms"] = json!(started.elapsed().as_millis() as u64);
            return out;
        }
    };

    let before_map = group(&obs.baseline);
    let after_map = group(&nodes);
    let mut appeared = Vec::new();
    let mut disappeared = Vec::new();
    let mut value_changed = Vec::new();
    let mut state_changed = Vec::new();
    let mut moved = 0usize;

    // 用"同一位置是否已有节点"来配对，避免同名兄弟被删后报错实例。
    for (key, now) in &after_map {
        let old = before_map.get(key).cloned().unwrap_or_default();
        if now.len() > old.len() {
            for n in now
                .iter()
                .filter(|n| !old.iter().any(|o| same_place(o, n)))
                .take(10)
            {
                appeared.push(brief(n));
            }
        }
    }
    for (key, before) in &before_map {
        let now = after_map.get(key).cloned().unwrap_or_default();
        if before.len() > now.len() {
            for n in before
                .iter()
                .filter(|n| !now.iter().any(|o| same_place(o, n)))
                .take(10)
            {
                disappeared.push(brief(n));
            }
        }
    }
    for (key, now) in &after_map {
        let Some(before) = before_map.get(key) else {
            continue;
        };
        if before.len() != now.len() {
            continue;
        }
        for (b, a) in before.iter().zip(now.iter()) {
            if b.value != a.value && (b.value.is_some() || a.value.is_some()) {
                value_changed.push(json!({
                    "role": a.role, "name": a.name,
                    "from": b.value, "to": a.value,
                }));
            }
            // 勾选/选中/焦点变化也是"界面变了"，不能只比 value。
            if b.checked != a.checked || b.selected != a.selected || b.focused != a.focused {
                state_changed.push(json!({
                    "role": a.role, "name": a.name,
                    "from": {"checked": b.checked, "selected": b.selected, "focused": b.focused},
                    "to": {"checked": a.checked, "selected": a.selected, "focused": a.focused},
                }));
            }
            if !b.rect.is_empty()
                && !a.rect.is_empty()
                && (b.rect.x != a.rect.x || b.rect.y != a.rect.y)
            {
                moved += 1;
            }
        }
    }
    appeared.truncate(10);
    disappeared.truncate(10);
    value_changed.truncate(5);
    state_changed.truncate(5);
    if !appeared.is_empty()
        || !disappeared.is_empty()
        || !value_changed.is_empty()
        || !state_changed.is_empty()
        || moved > 0
    {
        out["changed"] = json!(true);
    }
    out["appeared"] = json!(appeared);
    out["disappeared"] = json!(disappeared);
    out["value_changed"] = json!(value_changed);
    out["state_changed"] = json!(state_changed);
    out["moved"] = json!(moved);
    out["count_before"] = json!(obs.baseline.len());
    out["count_after"] = json!(nodes.len());
    out["ms"] = json!(started.elapsed().as_millis() as u64);
    out
}
