# dsh-desktop

给 DSH（DeepSeek Harness）用的 Windows 桌面控制工具。一个 exe，零运行时依赖。

**核心区别：它默认不会抢你的鼠标、键盘和前台窗口。**

## 为什么要有它

现有的桌面自动化工具（包括常见的 UIA CLI）只有两层动作：

1. **语义动作**：调用控件的 UIA 模式。稳，但碰到「有树、没动作」的控件就直接失败；
2. **物理输入**：SendInput。能点，但必须把窗口拉到前台、真实移动光标。

中间那层——**把鼠标/键盘消息直接投递给目标窗口，不动光标也不抢前台**——是缺的。
Qt 程序（比如 PowerWriter、Notepad--）、Win32 自绘界面上，这一层恰好是唯一能用的路。

`dsh-desktop` 补的就是这一层，并且把它做成默认的兜底：语义动作走不通时自动改用消息注入，
而不是去抢你的屏幕。

## 能力

| 类别 | 命令 |
| --- | --- |
| 观察 | `windows`、`status`、`snap`（UIA 树，带 `@ref`）、`info`、`find`、`shot`（窗口/整屏 PNG） |
| 语义动作 | `click`、`select`、`toggle`、`set`、`expand`/`collapse`、`scrollitem`、`setfocus` |
| 后台注入 | `click --mode bg`、`rclick`、`dclick`、`key`、`type`、`scroll`、`drag` |
| 物理兜底 | `--mode physical`（明确选择才会动光标） |
| 窗口 | `focus`、`minimize`、`maximize`、`restore`、`close`、`move`、`resize`、`launch` |
| 其它 | `wait`、`run`（批量步骤）、`clipboard`、`selftest` |

设计细节：

- **快照是扁平的**（带 `depth`），同样一棵树比嵌套 JSON 省一半 token。
- **元素身份 = 路径 +（角色、名字、automation_id、矩形）**，所以窗口里有两个「打开」按钮也能精确定位，
  不会像只看名字的工具那样一律拒绝。
- **属性走 UIA CacheRequest**，一个元素一次跨进程调用。Qt 窗口 130 个元素约 30 ms。
- **语义动作会回读验证**：`select`/`toggle` 读完状态、列表项的 `invoke` 读选中态；
  程序收了没反应就自动退回后台点击，结果里的 `mode` 会写成 `semantic:*` 或 `bg:*-fallback`。
- **后台点击会做客户区边界检查**：元素滚出视口时直接报 `POINT_OUTSIDE`，不会静默点空。
- **键盘消息发给真正有焦点的子控件**（线程 `hwndFocus` → 可聚焦的 edit/document → 顶层窗口），
  否则 Win32 程序收不到。
- **文本输入**逐字投 `WM_CHAR`，ASCII 和中文都走这条路（绕过输入法）；`--method paste` 只在目标不认 `WM_CHAR` 时用。

## 安装

```powershell
git clone https://github.com/sanxiadaba/dsh-desktop.git "$env:USERPROFILE\.dsh\skills\desktop"
& "$env:USERPROFILE\.dsh\skills\desktop\build.cmd"
```

真正给 DSH 用时，技能目录直接指向这个仓库（junction），本机不留副本：

```powershell
New-Item -ItemType Junction -Path "$env:USERPROFILE\.dsh\skills\desktop" -Target "D:\path\to\dsh-desktop"
```

`desk.exe` 生成在仓库根目录（已 gitignore）。

## 用法

```powershell
$desk = "$env:USERPROFILE\.dsh\skills\desktop\desk.exe"

& $desk windows                        # 有哪些窗口
& $desk snap --window w-306bc --actionable   # 读界面，拿 @ref
& $desk click '@s1a2b:e7'              # 语义优先，走不通自动后台注入
& $desk type "hello" --window w-306bc  # 后台打字，不抢前台
& $desk shot --window w-306bc --out C:\temp\a.png
& $desk selftest                       # 自测，不需要任何外部程序
```

每个命令输出一个 JSON：

```json
{"version":"0.1.0","ok":true,"command":"snap","elapsed_ms":31,"data":{...}}
{"version":"0.1.0","ok":false,"command":"click","error":{"code":"STALE_REF","message":"界面已变化…","retry":"safe"}}
```

## 安全模型

- 不加 `--mode physical` 就**不会**移动真实光标、**不会**抢前台。
- `focus` 是唯一会主动抢前台的命令，SKILL 里要求只在必要时用。
- 物理输入单独抽成一层，失败时明确回报 `PHYSICAL_FAILED`（被 UIPI 拦、会话语境不对）。

## 已知边界

- **Chromium / Electron 的网页内容**：投递鼠标消息通常进不去（它只看真实输入），
  这类目标应该用 CDP（`edge-browser` 技能），不要指望 `--mode bg`。
- **最小化窗口**：UIA 语义动作通常还能用；坐标点击不行（没有布局），先 `restore`。
- **提权窗口**：本进程需要与目标同级或更高完整性，否则 `SendInput`/消息都会被 UIPI 丢弃。
- **输入法**：`--method paste` 走剪贴板，会覆盖你当前的剪贴板内容（命令会回读确认）。
- 只支持 Windows 10 1809+ / Windows 11。

## 开发

```powershell
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test                       # 10 个纯单元测试，不需要桌面
& .\target\debug\desk.exe selftest           # 6 项，自己开窗验证消息注入
& .\target\debug\desk.exe selftest --physical # 8 项，额外验证 SendInput（会动光标）
```

没有远程 CI：这个工具的行为依赖真实桌面环境，验证在本地做。

## 许可

MIT
