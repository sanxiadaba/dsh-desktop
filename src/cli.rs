//! 命令行解析：零依赖的小解析器，帮助文本用中文。
//!
//! 只有一张选项表（`OPTIONS`）：解析、位置参数、校验、帮助都从它派生。
//! 规则：`--flag`、`--opt value`、`--opt=value`；`--` 之后全部是位置参数。

use crate::error::{DeskError, Result};

#[derive(Debug, Clone, Copy)]
pub struct Opt {
    pub name: &'static str,
    /// 是否吃一个值。false = 纯开关。
    pub value: bool,
}

const fn o(name: &'static str, value: bool) -> Opt {
    Opt { name, value }
}

/// **唯一**的选项清单。新增选项只改这里。
pub const OPTIONS: &[Opt] = &[
    o("--window", true),
    o("--depth", true),
    o("--limit", true),
    o("--out", true),
    o("--mode", true),
    o("--ref", true),
    o("--xy", true),
    o("--timeout-ms", true),
    o("--amount", true),
    o("--delta", true),
    o("--method", true),
    o("--name", true),
    o("--text", true),
    o("--element", true),
    o("--sel", true),
    o("--title", true),
    o("--exe", true),
    o("--dir", true),
    o("--seconds", true),
    o("--format", true),
    o("--all", false),
    o("--actionable", false),
    o("--raw", false),
    o("--verify", false),
    o("--screen", false),
    o("--pretty", false),
    o("--marks", false),
    o("--no-observe", false),
    o("--fail-fast", false),
    o("--help", false),
];

/// 所有命令都接受的选项。
const GLOBAL: &[&str] = &["--pretty", "--help"];

pub struct Args {
    raw: Vec<String>,
    positionals: Vec<String>,
    opts: Vec<(String, Option<String>)>,
    flags: Vec<String>,
    errors: Vec<String>,
}

impl Args {
    pub fn new(raw: &[String]) -> Self {
        let mut out = Self {
            raw: raw.to_vec(),
            positionals: Vec::new(),
            opts: Vec::new(),
            flags: Vec::new(),
            errors: Vec::new(),
        };
        out.parse();
        out
    }

    fn parse(&mut self) {
        let mut i = 0;
        let mut only_positionals = false;
        while i < self.raw.len() {
            let a = self.raw[i].clone();
            if only_positionals {
                self.positionals.push(a);
                i += 1;
                continue;
            }
            if a == "--" {
                only_positionals = true;
                i += 1;
                continue;
            }
            let Some(body) = a.strip_prefix("--") else {
                self.positionals.push(a);
                i += 1;
                continue;
            };
            let (name, inline) = match body.split_once('=') {
                Some((n, v)) => (format!("--{n}"), Some(v.to_string())),
                None => (format!("--{body}"), None),
            };
            let Some(spec) = find_opt(&name) else {
                // 未知选项先记下来，等 validate 统一报（这里继续吃掉它的值没有意义）。
                self.errors.push(format!("未知选项：{name}"));
                self.flags.push(name);
                i += 1;
                continue;
            };
            if !spec.value {
                if inline.is_some() {
                    self.errors
                        .push(format!("{} 是开关，不接受 =值", spec.name));
                }
                self.flags.push(spec.name.to_string());
                i += 1;
                continue;
            }
            let value = match inline {
                Some(v) => {
                    i += 1;
                    v
                }
                None => match self.raw.get(i + 1) {
                    Some(next) if next != "--" && !next.starts_with("--") => {
                        i += 2;
                        next.clone()
                    }
                    _ => {
                        self.errors.push(format!("选项 {} 缺少值", spec.name));
                        i += 1;
                        continue;
                    }
                },
            };
            if self.opts.iter().any(|(n, _)| n == spec.name) {
                self.errors.push(format!("选项 {} 重复出现", spec.name));
            }
            self.opts.push((spec.name.to_string(), Some(value)));
        }
    }

    pub fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name) || self.opts.iter().any(|(n, _)| n == name)
    }

    /// 取选项值：支持 `--opt value` 与 `--opt=value`。
    pub fn opt(&self, name: &str) -> Option<String> {
        self.opts
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| v.clone())
    }

    /// 位置参数（跳过选项与其值；`--` 之后全部保留）。
    pub fn positionals(&self) -> Vec<String> {
        self.positionals.clone()
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

    /// 全局校验：未知选项、缺值、重复、以及"这个命令不该有这个选项"。
    pub fn validate(&self, cmd: &str) -> Result<()> {
        if let Some(first) = self.errors.first() {
            return Err(DeskError::new("BAD_ARGS", first.clone())
                .with_suggestion("desk help 查看该命令的可用选项"));
        }
        let allowed = allowed_for(cmd);
        for (name, _) in &self.opts {
            self.check_allowed(cmd, name, allowed)?;
        }
        for name in &self.flags {
            self.check_allowed(cmd, name, allowed)?;
        }
        Ok(())
    }

    fn check_allowed(&self, cmd: &str, name: &str, allowed: &[&str]) -> Result<()> {
        if GLOBAL.contains(&name) || allowed.contains(&name) {
            return Ok(());
        }
        Err(
            DeskError::new("BAD_ARGS", format!("命令 {cmd} 不接受选项 {name}"))
                .with_suggestion("desk help 查看该命令的可用选项"),
        )
    }
}

pub fn find_opt(name: &str) -> Option<&'static Opt> {
    OPTIONS.iter().find(|o| o.name == name)
}

/// 某条命令允许的选项（GLOBAL 另算）。新增命令必须在这里列全。
fn allowed_for(cmd: &str) -> &'static [&'static str] {
    const TARGET: &[&str] = &["--window", "--ref"];
    const SNAP: &[&str] = &[
        "--window",
        "--ref",
        "--depth",
        "--limit",
        "--raw",
        "--actionable",
        "--name",
        "--text",
    ];
    match cmd {
        "help" => &[],
        "status" => &[],
        "doctor" => TARGET,
        "windows" => &["--all"],
        "snap" => SNAP,
        "find" => SNAP,
        "info" => TARGET,
        "shot" => &[
            "--window",
            "--ref",
            "--screen",
            "--out",
            "--marks",
            "--no-observe",
        ],
        "click" | "rclick" | "dclick" => &[
            "--window",
            "--ref",
            "--mode",
            "--verify",
            "--xy",
            "--no-observe",
        ],
        "select" | "toggle" | "expand" | "collapse" | "scrollitem" | "scrollinto" => {
            &["--window", "--ref", "--no-observe"]
        }
        "set" => &["--window", "--ref", "--no-observe"],
        "scroll" | "wheel" => &[
            "--window",
            "--ref",
            "--xy",
            "--delta",
            "--amount",
            "--no-observe",
        ],
        "drag" => &["--window", "--ref", "--no-observe"],
        "key" => &["--window", "--ref", "--no-observe"],
        "type" => &["--window", "--ref", "--method", "--no-observe"],
        "minimize" | "maximize" | "restore" | "close" => &["--window", "--ref"],
        "move" | "resize" => &["--window", "--ref"],
        "launch" => &["--dir"],
        "clipboard" => &[],
        "wait" => &[
            "--window",
            "--ref",
            "--title",
            "--exe",
            "--text",
            "--element",
            "--sel",
            "--timeout-ms",
            "--no-observe",
        ],
        "guard" => &["--window", "--ref", "--exe", "--seconds"],
        "elevate" => &[],
        "run" => &["--timeout-ms", "--fail-fast", "--no-observe"],
        "selftest" => &[],
        // focus 已禁用，但仍允许 --window，好给出明确的 DISABLED。
        "focus" => &["--window", "--ref"],
        _ => &[],
    }
}

pub const HELP_MAIN: &str = r#"desk — Windows 桌面控制（观察 / 语义动作 / 后台注入）

本工具不做物理输入：不发 SendInput、不移动光标、不把窗口切到前台。动作只走两条路——UIA 语义，或向目标窗口投递消息。
动作类命令必须显式给目标：--window <id> 或（元素动作）位置参数 @ref。

观察
  windows  [--all]                      列出窗口（默认只要标题非空、非本进程的；--all 含不可见）
  status                                环境、完整性级别、前台窗口
  doctor   --window <id> | --ref <@ref> 预检：窗口是否活着/最小化/挂起、UIA 是否可读、UIPI 是否允许
  snap     --window <id> [--depth N] [--limit N] [--raw]
           [--actionable] [--name 文本]  读 UIA 树，输出带 @ref 的元素
  find     <role:名字> --window <id>     在树里找元素（等于 snap + 过滤）
  info     <@ref> [--window <id>]        单个元素的完整信息（含 native_hwnd）
  shot     --window <id> [--out 路径] [--marks]
           --screen                      截图 PNG；--marks 在图上标出可动元素编号

动作（全部在后台完成；默认动作后回一份 after 观测）
  click    <@ref> | --xy X,Y [--mode semantic|bg] [--verify]
  rclick   <@ref> | --xy X,Y             右键
  dclick   <@ref> | --xy X,Y             双击
  select   <@ref>                        选中（页签、列表项、单选）
  toggle   <@ref>                        切换勾选状态
  set      <@ref> <值>                   直接写值（ValuePattern）
  expand   <@ref> / collapse <@ref>      展开 / 折叠
  scrollitem <@ref>   (别名 scrollinto)   把滚出视口的元素滚回可见区
  scroll   <@ref> <增量> | --window <id> --xy X,Y
  wheel    --window <id> [--delta -120] [--xy X,Y]
  drag     "x1,y1" "x2,y2" --window <id>   屏幕坐标
  key      <组合键> --window <id>|--ref <@ref>
  type     <文本> --window <id>|--ref <@ref> [--method auto|value|keys|paste]

窗口
  launch   <exe 或路径> [--dir 目录]     返回 pid；不激活窗口
  minimize | maximize | restore | close  <窗口 id>
  move     <窗口 id> <x> <y>
  resize   <窗口 id> <宽> <高>

其它
  clipboard get | set <文本>
  wait     --title <文本> [--exe <名>] [--timeout-ms N]
  wait     --window <id> [--text <文本> | --element <role:名字>] [--timeout-ms N]
  guard    --seconds N [--window <id>|--exe <名>]   临时钉住前台
  elevate  -- <命令...>                 以管理员身份执行一条命令
  run      - [--fail-fast]              从 stdin 读 {"steps":[["click","@s1:e3"], ...]}
  selftest                               自测：自己开窗验证注入，不碰你的桌面
  help [命令]                            本帮助（纯文本，是唯一不进 JSON 信封的输出）

ref 形如 @s1a2b:e3，由 snap 生成；界面一变就重新 snap。
--no-observe / DESK_OBSERVE=0 关闭动作后的 after 观测。
错误输出里 retry=safe 才能重试；unsafe 表示动作可能已经生效，先读状态再决定。
"#;

pub fn help_for(cmd: &str) -> &'static str {
    match cmd {
        "snap" => {
            "desk snap --window <id> [--depth N] [--limit N] [--raw] [--actionable] [--name 文本]\n读 UIA 树。每个元素带 @ref、role、name、actions、rect、depth、checked/selected（有的话）。\ntruncated=true 表示被 --limit 或 --depth 截断。--actionable 只留能执行动作的元素；--raw 用 RawView。"
        }
        "doctor" => {
            "desk doctor --window <id>\n预检目标窗口：活着/可见/最小化/被 DWM 隐藏、进程与完整性、UIPI、窗口线程是否响应、UIA 树大小与耗时。\n输出 issues[]；verdict=ok|warn|blocked。"
        }
        "click" => {
            "desk click <@ref> | --xy X,Y [--mode semantic|bg] [--verify]\nsemantic 与 auto 等价：先 UIA，失败再后台注入；bg 强制后台注入。--xy 是屏幕坐标，只走后台注入。\n两条路都不移动光标、不改变前台。--verify 会额外回读元素状态。"
        }
        "shot" => {
            "desk shot --window <id> | --screen [--out 路径] [--marks]\n--window 拍客户区 1:1；--screen 拍整个虚拟桌面。--marks 在图上给可动元素画编号框并返回 marks[]（含 ref）。\n返回 origin{x,y}：图像像素 (0,0) 对应的屏幕坐标。"
        }
        "type" => {
            "desk type <文本> --window <id>|--ref <@ref> [--method auto|value|keys|paste]\nauto/keys=逐字 WM_CHAR（ASCII 和中文都行，绕过输入法）；value=UIA ValuePattern 直接写值；paste=剪贴板+粘贴。"
        }
        "key" => {
            "desk key <组合键> --window <id>|--ref <@ref>\n发给目标窗口里有焦点的子控件，不抢前台。只投递窗口消息：单键（enter/tab/f5/字母）可靠；\n带 ctrl/alt/win 的快捷键依赖目标线程键盘状态，常常不触发，返回 effective=unverified。"
        }
        "run" => {
            "desk run - [--fail-fast]\n从 stdin 读 {\"steps\":[[\"click\",\"@s1:e3\"],[\"type\",\"hi\",\"--window\",\"w-1\"]]}，一个进程里连续执行。\n默认任一步失败就以 ok:false 结束；--fail-fast 立即停。"
        }
        "selftest" => {
            "desk selftest\n不依赖外部程序：自己建一个不抢前台的窗口，验证鼠标/键盘/滚轮/拖拽注入、窗口枚举和截图。"
        }
        "elevate" => {
            "desk elevate -- <命令...>\n当目标进程完整性级别更高、UIPI 会拦住消息时，用管理员身份重新执行同一条命令。\n会弹一次 UAC 确认；结果通过临时文件带回，放在 data.result 里。"
        }
        "find" => {
            "desk find <role:名字> --window <id> [--depth N] [--limit N] [--raw] [--actionable]\n等于 snap + 过滤；role 省略时按名字子串匹配。"
        }
        _ => HELP_MAIN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn options_are_unique_and_documented() {
        for (i, a) in OPTIONS.iter().enumerate() {
            for b in &OPTIONS[i + 1..] {
                assert_ne!(a.name, b.name, "重复选项 {}", a.name);
            }
        }
    }

    #[test]
    fn double_dash_ends_option_parsing() {
        let a = Args::new(&argv(&["--window", "w-1", "--", "--window", "-x"]));
        assert_eq!(a.opt("--window").as_deref(), Some("w-1"));
        assert_eq!(a.positionals(), vec!["--window", "-x"]);
        a.validate("click").unwrap();
    }

    #[test]
    fn duplicate_option_is_rejected() {
        let a = Args::new(&argv(&["--window", "w-1", "--window", "w-2"]));
        assert_eq!(a.validate("snap").unwrap_err().code, "BAD_ARGS");
    }

    #[test]
    fn per_command_flags_are_enforced() {
        let a = Args::new(&argv(&["--window", "w-1", "--seconds", "5"]));
        assert_eq!(a.validate("windows").unwrap_err().code, "BAD_ARGS");
        assert!(a.validate("guard").is_ok());
    }

    #[test]
    fn pretty_is_a_flag_not_a_value() {
        let a = Args::new(&argv(&["--pretty", "text"]));
        assert!(a.has("--pretty"));
        assert_eq!(a.positionals(), vec!["text"]);
    }
}
