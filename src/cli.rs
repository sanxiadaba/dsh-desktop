//! 命令行解析：零依赖的小解析器，帮助文本用中文。
//!
//! 规则：`--flag`、`--opt value`、`--opt=value`、位置参数按顺序取。

use crate::error::{DeskError, Result};

pub struct Args {
    raw: Vec<String>,
}

impl Args {
    pub fn new(raw: &[String]) -> Self {
        Self { raw: raw.to_vec() }
    }

    pub fn has(&self, name: &str) -> bool {
        self.raw.iter().any(|a| a == name)
    }

    /// 取选项值：支持 `--opt value` 与 `--opt=value`。
    pub fn opt(&self, name: &str) -> Option<String> {
        for (i, a) in self.raw.iter().enumerate() {
            if a == name {
                return self.raw.get(i + 1).cloned();
            }
            if let Some(rest) = a.strip_prefix(&format!("{name}=")) {
                return Some(rest.to_string());
            }
        }
        None
    }

    /// 位置参数（跳过所有 `--` 开头的项及其值）。
    pub fn positionals(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.raw.len() {
            let a = &self.raw[i];
            if a.starts_with("--") {
                if !a.contains('=')
                    && let Some(next) = self.raw.get(i + 1)
                    && !next.starts_with('-')
                    && takes_value(a)
                {
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
            out.push(a.clone());
            i += 1;
        }
        out
    }

    pub fn int(&self, name: &str) -> Result<Option<i64>> {
        match self.opt(name) {
            None => Ok(None),
            Some(v) => v
                .parse::<i64>()
                .map(Some)
                .map_err(|_| DeskError::new("BAD_ARGS", format!("{name} 需要一个整数，收到：{v}"))),
        }
    }
}

fn takes_value(flag: &str) -> bool {
    matches!(
        flag,
        "--window"
            | "--depth"
            | "--limit"
            | "--out"
            | "--screen"
            | "--mode"
            | "--ref"
            | "--xy"
            | "--timeout-ms"
            | "--amount"
            | "--delta"
            | "--method"
            | "--name"
            | "--text"
            | "--element"
            | "--sel"
            | "--title"
            | "--exe"
            | "--dir"
    )
}

/// 所有接受值的选项。放在一处，解析位置参数和校验都靠它。
pub const VALUE_FLAGS: &[&str] = &[
    "--window",
    "--depth",
    "--limit",
    "--out",
    "--mode",
    "--ref",
    "--xy",
    "--timeout-ms",
    "--amount",
    "--delta",
    "--method",
    "--name",
    "--text",
    "--element",
    "--sel",
    "--title",
    "--exe",
    "--dir",
    "--seconds",
];

/// 纯开关。
pub const BOOL_FLAGS: &[&str] = &[
    "--all",
    "--actionable",
    "--raw",
    "--verify",
    "--screen",
    "--json",
    "--help",
];

impl Args {
    /// 全局校验：未知选项、需要值却没给值，都在这里挡住。
    /// 这一步是为了消灭"拼错选项 → 静默回落到前台窗口"这类误伤。
    pub fn validate(&self) -> Result<()> {
        let mut i = 0;
        while i < self.raw.len() {
            let a = &self.raw[i];
            if a == "--" {
                i += 1;
                continue;
            }
            if !a.starts_with("--") {
                i += 1;
                continue;
            }
            let (name, has_value) = match a.split_once('=') {
                Some((n, _)) => (n, true),
                None => (a.as_str(), false),
            };
            if BOOL_FLAGS.contains(&name) {
                i += 1;
                continue;
            }
            if !VALUE_FLAGS.contains(&name) {
                return Err(DeskError::new("BAD_ARGS", format!("未知选项：{name}"))
                    .with_suggestion("desk help 查看可用选项"));
            }
            if has_value {
                i += 1;
                continue;
            }
            match self.raw.get(i + 1) {
                Some(v) if !v.starts_with("--") => i += 2,
                _ => {
                    return Err(DeskError::new("BAD_ARGS", format!("选项 {name} 缺少值"))
                        .with_suggestion(format!("例如 {name} <值>"))
                        .with_details(serde_json::json!({ "option": name })));
                }
            }
        }
        Ok(())
    }
}
pub const HELP_MAIN: &str = r#"desk — Windows 桌面控制（观察 / 语义动作 / 后台注入）

本工具不做物理输入：不发 SendInput、不移动光标、不把窗口切到前台。动作只走两条路——UIA 语义，或向目标窗口投递消息。

观察
  windows  [--all]                      列出窗口（默认只要标题非空、非本进程的）
  status                                环境、完整性级别、前台窗口
  snap     --window <id> [--depth N] [--limit N] [--raw]
           [--actionable] [--name 文本]  读 UIA 树，输出带 @ref 的元素（必须给 --window）
  find     <role:名字> [--window <id>]   在树里找元素（等于 snap + 过滤）
  info     <@ref>                        单个元素的完整信息（含 native_hwnd）
  shot     --window <id> [--out 路径] | --screen
                                         截图 PNG；窗口被遮挡也能拍

动作（全部在后台完成）
  click    <@ref> [--mode semantic|bg] [--verify]
  rclick   <@ref>                        右键
  dclick   <@ref>                        双击
  select   <@ref>                        选中（页签、列表项、单选）
  toggle   <@ref>                        切换勾选状态
  set      <@ref> <值>                   直接写值（ValuePattern）
  expand   <@ref> / collapse <@ref>      展开 / 折叠（菜单、树节点、下拉框）
  scrollitem <@ref>   (别名 scrollinto)   把滚出视口的元素滚回可见区
  scroll   <@ref> <增量>                 或 --window <id> --xy X,Y
  wheel    --window <id> [--delta -120] [--xy X,Y]
  drag     "x1,y1" "x2,y2" [--window <id>]
  key      <组合键> [--window <id>|--ref <@ref>]
  type     <文本> [--window <id>|--ref <@ref>] [--method auto|value|keys|paste]

窗口
  launch   <exe 或路径> [--dir 目录]
  minimize | maximize | restore | close  <窗口 id>   （都不激活窗口）
  move     <窗口 id> <x> <y>
  resize   <窗口 id> <宽> <高>

其它
  clipboard get | set <文本>
  wait     --title <文本> [--exe <名>] [--timeout-ms N]
  wait     --window <id> [--text <文本> | --element <role:名字>] [--timeout-ms N]
  guard    --seconds N [--window <id>|--exe <名>]   临时钉住前台：程序自己跳出来就压回去
  elevate  -- <命令...>                 以管理员身份执行一条命令（目标完整性更高时用）
  run      -                             从 stdin 读 {"steps":[["click","@s1:e3"], ...]}
  selftest                               自测：自己开窗验证注入，不碰你的桌面
  help [命令]                            本帮助

ref 形如 @s1a2b:e3，由 snap 生成；界面一变就重新 snap。
错误输出里 retry=safe 才能重试；unsafe 表示动作可能已经生效，先读状态再决定。
"#;

pub fn help_for(cmd: &str) -> &'static str {
    match cmd {
        "snap" => {
            "desk snap [--window <id>] [--depth N] [--limit N] [--raw] [--actionable] [--name 文本]\n读 UIA 树。每个元素带 @ref、role、name、actions、rect、depth。\n--actionable 只留能动的元素；--raw 用 RawView（含不可见脚手架）。"
        }
        "click" => {
            "desk click <@ref> [--mode semantic|bg] [--verify]\nsemantic=UIA 模式；bg=投递窗口消息。两条路都不会移动光标、不会改变前台。\n--verify 会在动作后重新读一次界面并回报元素状态。"
        }
        "type" => {
            "desk type <文本> [--window <id>|--ref <@ref>] [--method auto|value|keys|paste]\nauto/keys=逐字 WM_CHAR（ASCII 和中文都行，绕过输入法）；value=UIA ValuePattern 直接写值；paste=剪贴板+粘贴。"
        }
        "key" => {
            "desk key <组合键> [--window <id>|--ref <@ref>]\n组合键写法：ctrl+s、alt+F4、shift+tab、enter、f5。发给目标窗口里有焦点的子控件，不抢前台。"
        }
        "run" => {
            "desk run -\n从 stdin 读 {\"steps\":[[\"click\",\"@s1:e3\"],[\"type\",\"hi\",\"--window\",\"w-1\"]]}，一个进程里连续执行。"
        }
        "selftest" => {
            "desk selftest\n不依赖外部程序：自己建一个不抢前台的窗口，验证鼠标/键盘/滚轮注入、窗口枚举和截图。"
        }
        "elevate" => {
            "desk elevate -- <命令...>\n当目标进程完整性级别更高、UIPI 会拦住消息时，用管理员身份重新执行同一条命令。\n会弹一次 UAC 确认；结果通过临时文件带回并原样返回。"
        }
        _ => HELP_MAIN,
    }
}
