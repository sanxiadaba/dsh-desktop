//! 命令表：**唯一**的命令清单。
//!
//! 所有"这条命令要不要管理员、会不会改变前台、帮助里怎么列"都从这里取，
//! 不允许在别处再写一份按命令名分支的清单——那就是特例，迟早漂移。
//!
//! 增加命令时只改这里 + `main.rs` 的 dispatch；测试会检查两边是否对得上。

#[derive(Debug, Clone, Copy)]
pub struct CmdSpec {
    /// 命令名，和 dispatch 的分支一致。
    pub name: &'static str,
    /// 是否需要管理员完整性（读界面、投消息都需要）。
    pub needs_admin: bool,
    /// 这条命令是否可能让别的窗口抢到前台、并需要事后还原。
    ///
    /// `launch` 为 true：启动的新程序常常自己激活，必须还原。
    /// 其余命令靠"当前前台是不是这次碰过的窗口"判断，不需要这个开关。
    pub restore_any_foreground: bool,
}

const fn cmd(name: &'static str, needs_admin: bool) -> CmdSpec {
    CmdSpec {
        name,
        needs_admin,
        restore_any_foreground: false,
    }
}

pub const COMMANDS: &[CmdSpec] = &[
    // 诊断类：不需要管理员
    cmd("help", false),
    cmd("status", false),
    cmd("elevate", false),
    // 观察
    cmd("windows", true),
    cmd("snap", true),
    cmd("find", true),
    cmd("info", true),
    cmd("shot", true),
    // 动作
    cmd("click", true),
    cmd("rclick", true),
    cmd("dclick", true),
    cmd("select", true),
    cmd("toggle", true),
    cmd("set", true),
    cmd("expand", true),
    cmd("collapse", true),
    cmd("scrollitem", true),
    cmd("scrollinto", true),
    cmd("scroll", true),
    cmd("wheel", true),
    cmd("drag", true),
    cmd("key", true),
    cmd("type", true),
    // 窗口
    cmd("minimize", true),
    cmd("maximize", true),
    cmd("restore", true),
    cmd("close", true),
    cmd("move", true),
    cmd("resize", true),
    // 其它
    cmd("clipboard", true),
    cmd("wait", true),
    cmd("guard", true),
    cmd("run", true),
    cmd("selftest", true),
    CmdSpec {
        name: "launch",
        needs_admin: true,
        restore_any_foreground: true,
    },
    // focus 已禁用，保留名字是为了给出明确的 DISABLED，而不是"未知命令"。
    cmd("focus", true),
];

pub fn get(name: &str) -> Option<&'static CmdSpec> {
    COMMANDS.iter().find(|c| c.name == name)
}

pub fn needs_admin(name: &str) -> bool {
    get(name).map(|c| c.needs_admin).unwrap_or(true)
}

pub fn restore_any_foreground(name: &str) -> bool {
    get(name).map(|c| c.restore_any_foreground).unwrap_or(false)
}
