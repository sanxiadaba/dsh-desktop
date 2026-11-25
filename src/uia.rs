//! UIA 读取与语义动作。
//!
//! 设计要点：
//! - 属性统一走 CacheRequest：一个元素只回一次跨进程调用，整棵树在百毫秒级完成。
//! - 快照是扁平的（带 depth），比嵌套树省一半 token。
//! - 元素身份 = 路径 + (角色, 名字, automation_id, 矩形)；同名控件靠路径和矩形区分，
//!   所以"窗口里有两个同名按钮"不再是死路。

use crate::error::{DeskError, Result};
use crate::win::Rect;
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCacheRequest, IUIAutomationElement,
    IUIAutomationExpandCollapsePattern, IUIAutomationInvokePattern,
    IUIAutomationLegacyIAccessiblePattern, IUIAutomationScrollItemPattern,
    IUIAutomationScrollPattern, IUIAutomationSelectionItemPattern, IUIAutomationTogglePattern,
    IUIAutomationTreeWalker, IUIAutomationValuePattern, UIA_AutomationIdPropertyId,
    UIA_BoundingRectanglePropertyId, UIA_ClassNamePropertyId, UIA_ControlTypePropertyId,
    UIA_ExpandCollapsePatternId, UIA_HasKeyboardFocusPropertyId, UIA_InvokePatternId,
    UIA_IsEnabledPropertyId, UIA_IsKeyboardFocusablePropertyId, UIA_IsOffscreenPropertyId,
    UIA_LegacyIAccessiblePatternId, UIA_NamePropertyId, UIA_ScrollItemPatternId,
    UIA_ScrollPatternId, UIA_SelectionItemIsSelectedPropertyId, UIA_SelectionItemPatternId,
    UIA_TogglePatternId, UIA_ValuePatternId, UIA_ValueValuePropertyId,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub ref_id: String,
    pub role: String,
    pub name: String,
    pub automation_id: String,
    pub class_name: String,
    pub rect: Rect,
    pub actions: Vec<String>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub checked: Option<bool>,
    #[serde(default)]
    pub selected: Option<bool>,
    pub enabled: bool,
    pub offscreen: bool,
    pub focused: bool,
    pub depth: usize,
    pub path: Vec<u32>,
    pub children: usize,
}

impl Node {
    pub fn label(&self) -> String {
        if self.name.is_empty() {
            self.role.clone()
        } else {
            format!("{}:{}", self.role, self.name)
        }
    }
}

fn rect_from(r: RECT) -> Rect {
    Rect {
        x: r.left,
        y: r.top,
        width: r.right - r.left,
        height: r.bottom - r.top,
    }
}

fn overlap_ratio(a: &Rect, b: &Rect) -> f32 {
    let x = (a.x + a.width).min(b.x + b.width) - a.x.max(b.x);
    let y = (a.y + a.height).min(b.y + b.height) - a.y.max(b.y);
    if x <= 0 || y <= 0 {
        return 0.0;
    }
    let inter = (x * y) as f32;
    let area_a = (a.width.max(1) * a.height.max(1)) as f32;
    let area_b = (b.width.max(1) * b.height.max(1)) as f32;
    inter / area_a.min(area_b)
}

pub fn role_name(control_type: i32) -> &'static str {
    match control_type {
        50000 => "button",
        50001 => "calendar",
        50002 => "checkbox",
        50003 => "combobox",
        50004 => "edit",
        50005 => "link",
        50006 => "image",
        50007 => "listitem",
        50008 => "list",
        50009 => "menu",
        50010 => "menubar",
        50011 => "menuitem",
        50012 => "progressbar",
        50013 => "radio",
        50014 => "scrollbar",
        50015 => "slider",
        50016 => "spinner",
        50017 => "statusbar",
        50018 => "tab",
        50019 => "tabitem",
        50020 => "text",
        50021 => "toolbar",
        50022 => "tooltip",
        50023 => "tree",
        50024 => "treeitem",
        50025 => "custom",
        50026 => "group",
        50027 => "thumb",
        50028 => "datagrid",
        50029 => "dataitem",
        50030 => "document",
        50031 => "splitbutton",
        50032 => "window",
        50033 => "pane",
        50034 => "header",
        50035 => "headeritem",
        50036 => "table",
        50037 => "titlebar",
        50038 => "separator",
        50039 => "semanticzoom",
        50040 => "appbar",
        _ => "unknown",
    }
}

pub struct Uia {
    automation: IUIAutomation,
    cache: IUIAutomationCacheRequest,
}

pub struct WalkOptions {
    pub max_depth: usize,
    pub limit: usize,
    pub raw_view: bool,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            max_depth: 16,
            limit: 1200,
            raw_view: false,
        }
    }
}

impl Uia {
    pub fn new() -> Result<Self> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let automation: IUIAutomation =
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).map_err(|e| {
                    DeskError::new("UIA_INIT", format!("初始化 UIA 失败：{e}"))
                        .with_suggestion("确认当前会话是可交互桌面（不是服务、锁屏或 Session 0）")
                })?;
            let cache = automation
                .CreateCacheRequest()
                .map_err(|e| DeskError::new("UIA_INIT", format!("创建缓存请求失败：{e}")))?;
            for id in [
                UIA_NamePropertyId,
                UIA_AutomationIdPropertyId,
                UIA_ClassNamePropertyId,
                UIA_ControlTypePropertyId,
                UIA_IsEnabledPropertyId,
                UIA_IsOffscreenPropertyId,
                UIA_BoundingRectanglePropertyId,
                UIA_HasKeyboardFocusPropertyId,
                UIA_IsKeyboardFocusablePropertyId,
                UIA_ValueValuePropertyId,
                UIA_SelectionItemIsSelectedPropertyId,
            ] {
                let _ = cache.AddProperty(id);
            }
            for id in [
                UIA_InvokePatternId,
                UIA_SelectionItemPatternId,
                UIA_TogglePatternId,
                UIA_ValuePatternId,
                UIA_ExpandCollapsePatternId,
                UIA_ScrollPatternId,
                UIA_ScrollItemPatternId,
                UIA_LegacyIAccessiblePatternId,
            ] {
                let _ = cache.AddPattern(id);
            }
            Ok(Self { automation, cache })
        }
    }

    pub fn walker(&self, raw: bool) -> Result<IUIAutomationTreeWalker> {
        let w = unsafe {
            if raw {
                self.automation.RawViewWalker()
            } else {
                self.automation.ControlViewWalker()
            }
        };
        w.map_err(|e| DeskError::new("UIA_FAILED", format!("取树遍历器失败：{e}")))
    }

    pub fn element_from_handle(&self, hwnd: HWND) -> Result<IUIAutomationElement> {
        unsafe {
            self.automation
                .ElementFromHandleBuildCache(hwnd, &self.cache)
        }
        .map_err(|e| DeskError::new("UIA_FAILED", format!("读取窗口 UIA 根节点失败：{e}")))
    }

    /// 整棵树的扁平快照，ref 形如 `@<sid>:eN`。
    pub fn snapshot(&self, hwnd: HWND, sid: &str, opts: &WalkOptions) -> Result<Vec<Node>> {
        let root = self.element_from_handle(hwnd)?;
        let walker = self.walker(opts.raw_view)?;
        let mut out = Vec::new();
        let mut path = Vec::new();
        self.walk(&walker, &root, 0, &mut path, sid, opts, &mut out);
        Ok(out)
    }

    // 遍历要同时带 walker、路径、sid、选项和输出，参数多是刻意的。
    #[allow(clippy::too_many_arguments)]
    fn walk(
        &self,
        walker: &IUIAutomationTreeWalker,
        parent: &IUIAutomationElement,
        depth: usize,
        path: &mut Vec<u32>,
        sid: &str,
        opts: &WalkOptions,
        out: &mut Vec<Node>,
    ) -> usize {
        if depth >= opts.max_depth || out.len() >= opts.limit {
            return 0;
        }
        let mut seen = 0usize;
        let mut child = unsafe { walker.GetFirstChildElementBuildCache(parent, &self.cache) };
        while let Ok(el) = child {
            if out.len() >= opts.limit {
                break;
            }
            let index = seen as u32;
            seen += 1;
            path.push(index);
            let node = self.describe(&el, depth + 1, path.clone(), out.len(), sid);
            let slot = out.len();
            out.push(node);
            let kids = self.walk(walker, &el, depth + 1, path, sid, opts, out);
            if let Some(n) = out.get_mut(slot) {
                n.children = kids;
            }
            path.pop();
            child = unsafe { walker.GetNextSiblingElementBuildCache(&el, &self.cache) };
        }
        seen
    }

    fn describe(
        &self,
        el: &IUIAutomationElement,
        depth: usize,
        path: Vec<u32>,
        seq: usize,
        sid: &str,
    ) -> Node {
        let control_type = unsafe { el.CachedControlType() }.map(|t| t.0).unwrap_or(0);
        let name = unsafe { el.CachedName() }
            .map(|b| b.to_string())
            .unwrap_or_default();
        let automation_id = unsafe { el.CachedAutomationId() }
            .map(|b| b.to_string())
            .unwrap_or_default();
        let class_name = unsafe { el.CachedClassName() }
            .map(|b| b.to_string())
            .unwrap_or_default();
        let rect = unsafe { el.CachedBoundingRectangle() }
            .map(rect_from)
            .unwrap_or(Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            });
        let enabled = unsafe { el.CachedIsEnabled() }
            .map(|b| b.as_bool())
            .unwrap_or(true);
        let offscreen = unsafe { el.CachedIsOffscreen() }
            .map(|b| b.as_bool())
            .unwrap_or(false);
        let focused = unsafe { el.CachedHasKeyboardFocus() }
            .map(|b| b.as_bool())
            .unwrap_or(false);

        let (actions, value, checked, selected) = self.patterns(el);
        Node {
            ref_id: format!("@{sid}:e{}", seq + 1),
            role: role_name(control_type).to_string(),
            name,
            automation_id,
            class_name,
            rect,
            actions,
            value,
            checked,
            selected,
            enabled,
            offscreen,
            focused,
            depth,
            path,
            children: 0,
        }
    }

    fn patterns(
        &self,
        el: &IUIAutomationElement,
    ) -> (Vec<String>, Option<String>, Option<bool>, Option<bool>) {
        let mut out = Vec::new();
        let mut value = None;
        let mut checked = None;
        let mut selected = None;
        unsafe {
            if el
                .GetCachedPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                .is_ok()
            {
                out.push("click".to_string());
            }
            if let Ok(p) = el
                .GetCachedPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)
            {
                out.push("select".to_string());
                selected = p.CachedIsSelected().ok().map(|b| b.as_bool());
            }
            if let Ok(p) = el.GetCachedPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
            {
                out.push("toggle".to_string());
                checked = p.CachedToggleState().ok().map(|s| s.0 == 1);
            }
            if let Ok(p) = el.GetCachedPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) {
                out.push("set".to_string());
                value = p.CachedValue().ok().map(|b| b.to_string());
            }
            if el
                .GetCachedPatternAs::<IUIAutomationExpandCollapsePattern>(
                    UIA_ExpandCollapsePatternId,
                )
                .is_ok()
            {
                out.push("expand".to_string());
            }
            if el
                .GetCachedPatternAs::<IUIAutomationScrollPattern>(UIA_ScrollPatternId)
                .is_ok()
            {
                out.push("scroll".to_string());
            }
            if el
                .GetCachedPatternAs::<IUIAutomationScrollItemPattern>(UIA_ScrollItemPatternId)
                .is_ok()
            {
                out.push("scrollitem".to_string());
            }
            if el
                .GetCachedPatternAs::<IUIAutomationLegacyIAccessiblePattern>(
                    UIA_LegacyIAccessiblePatternId,
                )
                .is_ok()
            {
                out.push("legacy".to_string());
            }
        }
        (out, value, checked, selected)
    }

    /// 按快照路径取元素；身份对不上就返回 None（由调用方回退到 locate）。
    pub fn element_at_path_cached(&self, hwnd: HWND, node: &Node) -> Option<IUIAutomationElement> {
        let root = self.element_from_handle(hwnd).ok()?;
        let walker = self.walker(false).ok()?;
        let mut cur = root;
        for step in &node.path {
            let mut child = unsafe { walker.GetFirstChildElement(&cur) }.ok()?;
            let mut i = 0u32;
            while i < *step {
                child = unsafe { walker.GetNextSiblingElement(&child) }.ok()?;
                i += 1;
            }
            cur = child;
        }
        if self.identity_ok(&cur, node) {
            Some(cur)
        } else {
            None
        }
    }

    fn identity_ok(&self, el: &IUIAutomationElement, node: &Node) -> bool {
        if self.role_of(el) != node.role {
            return false;
        }
        let auto = self.automation_id_of(el);
        if !node.automation_id.is_empty() {
            return auto == node.automation_id;
        }
        self.name_of(el) == node.name
    }

    pub fn role_of(&self, el: &IUIAutomationElement) -> String {
        unsafe { el.CurrentControlType() }
            .map(|t| role_name(t.0).to_string())
            .unwrap_or_else(|_| "unknown".to_string())
    }

    pub fn name_of(&self, el: &IUIAutomationElement) -> String {
        unsafe { el.CurrentName() }
            .map(|b| b.to_string())
            .unwrap_or_default()
    }

    pub fn automation_id_of(&self, el: &IUIAutomationElement) -> String {
        unsafe { el.CurrentAutomationId() }
            .map(|b| b.to_string())
            .unwrap_or_default()
    }

    pub fn value_of(&self, el: &IUIAutomationElement) -> Option<String> {
        unsafe {
            let p = el
                .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                .ok()?;
            p.CurrentValue().ok().map(|b| b.to_string())
        }
    }

    /// 界面变了以后按 (角色, automation_id/名字, 矩形重叠) 重新定位，要求唯一。
    pub fn locate(&self, hwnd: HWND, node: &Node) -> Result<IUIAutomationElement> {
        let root = self.element_from_handle(hwnd)?;
        let walker = self.walker(false)?;
        let mut hits: Vec<(IUIAutomationElement, Rect)> = Vec::new();
        self.collect(&walker, &root, node, &mut hits, 0);
        match hits.len() {
            1 => Ok(hits.remove(0).0),
            0 => Err(DeskError::new(
                "STALE_REF",
                format!("界面已变化，找不到 {} 这个元素了", node.label()),
            )
            .with_suggestion("重新 `desk snap` 再操作")),
            _ => {
                let near: Vec<_> = hits
                    .iter()
                    .filter(|(_, r)| overlap_ratio(r, &node.rect) > 0.3)
                    .collect();
                if near.len() == 1 {
                    Ok(near[0].0.clone())
                } else {
                    Err(DeskError::new(
                        "AMBIGUOUS_TARGET",
                        format!(
                            "窗口里有 {} 个同名元素，无法确定是哪一个（{}）",
                            hits.len(),
                            node.label()
                        ),
                    )
                    .with_suggestion("重新 `desk snap`，用新的 ref 精确指定"))
                }
            }
        }
    }

    fn collect(
        &self,
        walker: &IUIAutomationTreeWalker,
        parent: &IUIAutomationElement,
        want: &Node,
        out: &mut Vec<(IUIAutomationElement, Rect)>,
        depth: usize,
    ) {
        if depth > 32 || out.len() > 8 {
            return;
        }
        let mut child = unsafe { walker.GetFirstChildElement(parent) };
        while let Ok(el) = child {
            if self.identity_ok(&el, want) {
                let rect = unsafe { el.CurrentBoundingRectangle() }
                    .map(rect_from)
                    .unwrap_or(Rect {
                        x: 0,
                        y: 0,
                        width: 0,
                        height: 0,
                    });
                out.push((el.clone(), rect));
            }
            self.collect(walker, &el, want, out, depth + 1);
            child = unsafe { walker.GetNextSiblingElement(&el) };
        }
    }

    /// 元素当前是否被选中（列表项、页签、单选）。拿不到就返回 None。
    pub fn is_selected(&self, el: &IUIAutomationElement) -> Option<bool> {
        unsafe {
            let p = el
                .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                    UIA_SelectionItemPatternId,
                )
                .ok()?;
            p.CurrentIsSelected().ok().map(|b| b.as_bool())
        }
    }

    /// 元素当前是否被切换（勾选框）。拿不到就返回 None。
    pub fn is_toggled(&self, el: &IUIAutomationElement) -> Option<bool> {
        unsafe {
            let p = el
                .GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                .ok()?;
            p.CurrentToggleState().ok().map(|s| s.0 == 1)
        }
    }

    pub fn invoke(&self, el: &IUIAutomationElement) -> Result<()> {
        unsafe {
            let p = el
                .GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                .map_err(|e| DeskError::new("NO_PATTERN", format!("该元素不支持 click：{e}")))?;
            p.Invoke()
                .map_err(|e| DeskError::new("ACTION_FAILED", format!("Invoke 失败：{e}")))
        }
    }

    pub fn select(&self, el: &IUIAutomationElement) -> Result<()> {
        unsafe {
            let p = el
                .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                    UIA_SelectionItemPatternId,
                )
                .map_err(|e| DeskError::new("NO_PATTERN", format!("该元素不支持 select：{e}")))?;
            p.Select()
                .map_err(|e| DeskError::new("ACTION_FAILED", format!("Select 失败：{e}")))
        }
    }

    pub fn toggle(&self, el: &IUIAutomationElement) -> Result<()> {
        unsafe {
            let p = el
                .GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                .map_err(|e| DeskError::new("NO_PATTERN", format!("该元素不支持 toggle：{e}")))?;
            p.Toggle()
                .map_err(|e| DeskError::new("ACTION_FAILED", format!("Toggle 失败：{e}")))
        }
    }

    pub fn set_value(&self, el: &IUIAutomationElement, value: &str) -> Result<()> {
        unsafe {
            let p = el
                .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                .map_err(|e| DeskError::new("NO_PATTERN", format!("该元素不支持 set：{e}")))?;
            p.SetValue(&windows::core::BSTR::from(value))
                .map_err(|e| DeskError::new("ACTION_FAILED", format!("SetValue 失败：{e}")))
        }
    }

    /// 适合接收键盘输入的子控件：优先可聚焦的 edit/document，
    /// 找不到就返回 None（由调用方退回顶层窗口）。
    pub fn typing_target(&self, hwnd: HWND) -> Option<HWND> {
        let root = self.element_from_handle(hwnd).ok()?;
        let walker = self.walker(false).ok()?;
        let mut best: Option<HWND> = None;
        self.find_typing(&walker, &root, 0, &mut best);
        best
    }

    fn find_typing(
        &self,
        walker: &IUIAutomationTreeWalker,
        el: &IUIAutomationElement,
        depth: usize,
        best: &mut Option<HWND>,
    ) {
        if depth > 24 || best.is_some() {
            return;
        }
        let role = self.role_of(el);
        let focusable = unsafe { el.CurrentIsKeyboardFocusable() }
            .map(|b| b.as_bool())
            .unwrap_or(false);
        if focusable
            && (role == "edit" || role == "document")
            && let Ok(h) = unsafe { el.CurrentNativeWindowHandle() }
            && !h.0.is_null()
        {
            *best = Some(h);
            return;
        }
        let mut child = unsafe { walker.GetFirstChildElement(el) };
        while let Ok(c) = child {
            self.find_typing(walker, &c, depth + 1, best);
            if best.is_some() {
                return;
            }
            child = unsafe { walker.GetNextSiblingElement(&c) };
        }
    }

    pub fn scroll_into_view(&self, el: &IUIAutomationElement) -> Result<()> {
        unsafe {
            let p = el
                .GetCurrentPatternAs::<IUIAutomationScrollItemPattern>(UIA_ScrollItemPatternId)
                .map_err(|e| DeskError::new("NO_PATTERN", format!("该元素不支持滚入视野：{e}")))?;
            p.ScrollIntoView()
                .map_err(|e| DeskError::new("ACTION_FAILED", format!("滚入视野失败：{e}")))
        }
    }

    pub fn set_focus(&self, el: &IUIAutomationElement) -> Result<()> {
        unsafe {
            el.SetFocus()
                .map_err(|e| DeskError::new("ACTION_FAILED", format!("设置焦点失败：{e}")))
        }
    }

    pub fn expand(&self, el: &IUIAutomationElement, open: bool) -> Result<()> {
        unsafe {
            let p = el
                .GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                    UIA_ExpandCollapsePatternId,
                )
                .map_err(|e| DeskError::new("NO_PATTERN", format!("该元素不支持展开/折叠：{e}")))?;
            let r = if open { p.Expand() } else { p.Collapse() };
            r.map_err(|e| DeskError::new("ACTION_FAILED", format!("展开/折叠失败：{e}")))
        }
    }
}
