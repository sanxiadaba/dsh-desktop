//! 命令行解析：零依赖的小解析器，帮助文本用中文。
//!
//! 规则：`--flag`、`--opt value`、`--opt=value`、位置参数按顺序取。

use crate::error::{DeskError, Result};

pub struct Args {
    raw: Vec<String>,
}

impl Args {
    pub fn new(raw: &[String]) -> Self {
        Self {
            raw: raw.to_vec(),
        }
    }

    pub fn raw(&self) -> &[String] {
        &self.raw
    }

    pub fn has(&self, name: &str) -> bool {
        self.raw.iter().any(|a| a == name)
    }

    /// 取选项值：支持 `--opt value` 与 `--opt=value`。
    pub fn opt(&self, name: &str) -> Option<String> {
        let mut it = self.raw.iter().enumerate();
        while let Some((i, a)) = it.next() {
            if a == name {
                return self.raw.get(i + 1).cloned();
            }
            if let Some(rest) = a.strip_prefix(&format!("{name}=")) {
                return Some(rest.to_string());
            }
        }
        None
    }

    pub fn require_opt(&self, name: &str) -> Result<String> {
        self.opt(name).ok_or_else(|| {
            DeskError::new("BAD_ARGS", format!("缺少必需选项 {name}"))
                .with_suggestion("运行 `desk help` 查看用法")
        })
    }

    /// 位置参数（跳过所有 `--` 开头的项及其值）。
    pub fn positionals(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.raw.len() {
            let a = &self.raw[i];
            if a.starts_with("--") {
                // `--opt value` 形式：吃掉值；`--opt=value` 自成一体。
                if !a.contains('=') {
                    if let Some(next) = self.raw.get(i + 1) {
                        if !next.starts_with('-') && takes_value(a) {
                            i += 2;
                            continue;
                        }
                    }
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

    pub fn one_of(&self, name: &str, allowed: &[&str], default: &str) -> Result<String> {
        let v = self.opt(name).unwrap_or_else(|| default.to_string());
        if !allowed.contains(&v.as_str()) {
            return Err(DeskError::new(
                "BAD_ARGS",
                format!("{name} 只能是 {}，收到：{v}", allowed.join(" / ")),
            ));
        }
        Ok(v)
    }

    /// 拒绝未知的 `--` 选项，避免拼错被静默忽略。
    pub fn reject_unknown(&self, known: &[&str]) -> Result<()> {
        for a in &self.raw {
            if !a.starts_with("--") {
                continue;
            }
            let name = a.split('=').next().unwrap_or(a);
            if name == "--help" {
                continue;
            }
            if !known.contains(&name) {
                return Err(DeskError::new("BAD_ARGS", format!("未知选项：{name}"))
                    .with_suggestion(format!("可用选项：{}", known.join(" "))));
            }
        }
        Ok(())
    }
}

fn takes_value(flag: &str) -> bool {
    matches!(
        flag,
        "--window"
            | "--depth"
            | "--out"
            | "--screen"
            | "--mode"
            | "--ref"
            | "--xy"
            | "--timeout-ms"
            | "--amount"
            | "--delta"
            | "--steps"
            | "--sel"
            | "--value"
            | "--method"
            | "--role"
            | "--name"
            | "--text"
            | "--index"
            | "--title"
            | "--exe"
            | "--pid"
            | "--file"
            | "--kind"
            | "--iters"
            | "--seconds"
            | "--ms"
            | "--from"
            | "--to"
    )
}

pub const HELP_MAIN: &str = r#"desk — 桌面控制工具（观察 / 语义动作 / 后台注入 / 物理兜底）

用法：desk <命令> [选项]

观察
  windows  [--all]                     列出窗口（默认只要标题非空的）
  status                               环境与前台窗口
  snap     [--window <id>] [--depth N] [--all]
                                       读 UIA 树，输出带 @ref 的元素（默认前台窗口）
  find     <role:Name> [--window <id>] 在树里找元素
  shot     [--window <id>] [--out P]   截图成 PNG（默认前台窗口；--screen 截整屏）

动作（默认先语义、失败再后台注入，绝不抢前台）
  click    <@ref> [--mode semantic|bg|physical] [--verify]
  select   <@ref>
  toggle   <@ref>
  set      <@ref> <value>
  expand   <@ref>       collapse <@ref>
  scroll   <@ref> <amount>            或 --window <id> --xy X,Y
  key      <组合键> [--window <id>] [--mode bg|physical]
  type     <文本> [--ref <@ref>] [--window <id>] [--method value|keys|paste]
  drag     <x1,y1> <x2,y2> [--window <id>]
  paste    [--window <id>]            把剪贴板内容粘贴到目标

窗口
  focus|minimize|maximize|restore|close <id>
  move <id> <x> <y> | resize <id> <w> <h>
  launch <exe 或路径> [参数...]

其它
  clipboard get|set <文本>
  wait     --window <id> [--text T] [--timeout-ms N]
  run      -                          从 stdin 读 JSON 步骤批量执行
  selftest                            自测：不需要任何 GUI
  help [命令]                         本帮助

通用选项
  --json    紧凑输出（默认就是 JSON）
  --quiet   只输出结果路径/值
"#;

pub fn help_for(cmd: &str) -> &'static str {
    match cmd {
        "snap" => "desk snap [--window <id>] [--depth N] [--all]\n读 UIA 树。每个可操作元素带 @ref，供后续 click/select/set 使用。",
        "click" => "desk click <@ref> [--mode semantic|bg|physical] [--verify]\nsemantic=UIA 模式（不动鼠标）；bg=投递窗口消息（不动光标、不抢前台）；physical=SendInput（会动你的鼠标，明确选择才用）。",
        "type" => "desk type <文本> [--ref <@ref>] [--window <id>] [--method value|keys|paste]\nvalue=UIA ValuePattern；keys=WM_CHAR 逐字投递；paste=剪贴板+粘贴（中文用这个）。",
        _ => HELP_MAIN,
    }
}
