# dsh-desktop

给 DSH（DeepSeek Harness）用的 Windows 桌面控制工具。一个 exe，零运行时依赖。

**它不做物理输入：不发 `SendInput`、不移动光标、不把窗口切到前台。**

## 为什么要有它

常见的桌面自动化工具只有两层动作：UIA 语义动作，和物理输入（SendInput）。
语义动作在"有树、没动作"的控件上直接失败；物理输入则必须把窗口拉到前台、真实移动光标。

中间那层——**把鼠标/键盘消息直接投递给目标窗口，不动光标也不抢前台**——通常没有。
Qt 程序、Win32 自绘界面、把控件拆成子 HWND 的上位机，恰好只有这一层能用。

`dsh-desktop` 补的就是这一层，并把它做成默认兜底：语义动作走不通时自动改用消息注入。

## 与同类工具的关系

| 工具 | 形态 | 观测 | 输入 | 我们的取舍 |
|---|---|---|---|---|
| [Windows-MCP](https://github.com/CursorTouch/Windows-MCP) | MCP server / Python | UIA 快照 + 截图 | 物理输入 + 桌面所有权/同意 | 我们不抢前台，也不做服务器 |
| [desktop-touch-mcp](https://github.com/Harusame64/desktop-touch-mcp) | MCP server / TS+Rust | UIA 深读、OCR、SoM、CDP | 后台消息 + 物理兜底 | 我们吸收"动作后返回变化"，但不引入 Node/.NET/OCR |
| [UI-TARS-desktop](https://github.com/bytedance/UI-TARS-desktop) | Electron + VLM | 纯截图 | 物理 | 我们不做视觉模型，只把 UIA 树画到截图上给模型看 |
| [pywinauto](https://github.com/pywinauto/pywinauto) | Python 库 | UIA/MSAA | UIA/Win32 消息 | 我们是单 exe + JSON 契约，给 agent 用 |

**v0.2 补上的三件事**（对比后最明显的差距）：

- **动作闭环**：每条动作返回 `after`——界面因此变了什么（新出现/消失/值变化/新窗口），
  agent 不用再盲目 `snap` 一次。
- **预检**：`doctor` 在动手前回答"为什么动不了"（最小化、UIPI、挂起、UIA 读不到）。
- **视觉桥**：`shot --marks` 把可动元素编号画在截图上，像素和 `@ref` 能对上。

## 能力

| 类别 | 命令 |
| --- | --- |
| 观察 | `windows`、`status`、`doctor`、`snap`（UIA 树，带 `@ref`）、`find`、`info`、`shot`（客户区 1:1 PNG，可 `--marks`） |
| 语义动作 | `click`、`select`、`toggle`、`set`、`expand`/`collapse`、`scrollitem` |
| 后台注入 | `click --mode bg` / `click --xy`、`rclick`、`dclick`、`key`、`type`、`scroll`、`wheel`、`drag` |
| 窗口 | `minimize`、`maximize`、`restore`、`close`、`move`、`resize`、`launch`（返回 pid） |
| 其它 | `wait`、`guard`、`run`、`clipboard`、`selftest` |

设计细节：

- **快照是扁平的**（带 `depth`），输出是紧凑 JSON（`--pretty` 可读）；默认 `limit 300`，
  被 `--limit` 或 `--depth` 截断时 `truncated:true`。快照只保留最近 30 份；
  `wait` 和动作后的观测不落盘，也不会挤掉你的 ref。
- **元素身份 = 路径 +（角色、名字、automation_id、矩形）**；路径失效时按身份+旧矩形重新定位，
  只有"旧位置附近恰好一个"才继续，否则 `STALE_REF` / `AMBIGUOUS_TARGET`，不会静默点邻居。
- **属性走 UIA CacheRequest**；有状态属性（value/checked/selected）按元素现读，
  所以"一个元素一次跨进程调用"只对静态属性成立。
- **每个动作后的回读**：`select`/`toggle` 读状态、列表项 `invoke` 读选中态、`set` 返回 `value_matches`；
  程序收了没反应就自动退回后台点击，两次都不生效则返回 `ACTION_FAILED`（`retry=unsafe`），
  不会出现"返回 ok 但界面没动"。
- **动作后的 `after` 观测**：`changed`、`appeared`、`disappeared`、`value_changed`、`moved`、
  `new_windows`（含菜单弹窗这类无标题窗口）、`window` 状态。默认开启，`--no-observe` 或
  `DESK_OBSERVE=0` 关闭。
- **密码框不采集**：`UIA_IsPassword` 为真时 value 只输出 `***`，不落盘、不参与 `wait --text`。
- **后台点击会做客户区边界检查**：元素滚出视口时直接报 `POINT_OUTSIDE`，不会静默点空。
- **消息投给坐标下最深的子窗口**：经典 Win32 程序把页签/工具条做成子 HWND，投给顶层窗口等于没点。
- **键盘消息发给真正有焦点的子控件**（线程 `hwndFocus` → 可验证属于目标窗口 → 可聚焦的
  edit/document → 顶层窗口）。`key` 只保证投递：单键可靠；带 ctrl/alt/shift/win 的快捷键依赖
  目标线程键盘状态，通常不触发，返回 `effective:"unverified"`。
- **文本输入**逐字投 `WM_CHAR`，ASCII 和中文都走这条路（绕过输入法）；
  `--method paste` 只在目标不认 `WM_CHAR` 时用，不再回退 Ctrl+V。
- **截图是客户区 1:1 像素**，返回 `origin`（图像 (0,0) 对应的屏幕坐标）；
  最小化窗口直接 `SHOT_EMPTY`，整幅同色会带 `blank:true`。

## 安装

需要 Rust 1.85+ 和 MSVC Build Tools（VS 2022 的"使用 C++ 的桌面开发"）：

```powershell
winget install Rustlang.Rustup
# 装好 VS Build Tools 后：
git clone https://github.com/sanxiadaba/dsh-desktop.git D:\repo\dsh-desktop
cd D:\repo\dsh-desktop
.\build.cmd        # 产出仓库根目录的 desk.exe（已 gitignore）
```

真正给 DSH 用时，技能目录直接指向这个仓库（junction），本机不留副本：

```powershell
New-Item -ItemType Junction -Path "$env:USERPROFILE\.dsh\skills\desktop" -Target "D:\repo\dsh-desktop"
```

## 用法

```powershell
$desk = "$env:USERPROFILE\.dsh\skills\desktop\desk.exe"

& $desk windows                        # 有哪些窗口
& $desk doctor --window w-1234         # 先问：这个窗口能操作吗
& $desk snap --window w-1234 --actionable   # 读界面，拿 @ref
& $desk click '@smwvx9pfk:e7' --window w-1234   # 语义优先，走不通自动后台注入
& $desk type "hello" --window w-1234  # 后台打字，不抢前台
& $desk shot --window w-1234 --marks --out C:\temp\a.png
& $desk selftest                       # 自测，不需要任何外部程序
```

每个命令输出一个 JSON（`help` 例外，是纯文本）：

```json
{"version":"0.2.0","ok":true,"command":"snap","elapsed_ms":31,
 "side_effects":{"foreground_restored":false},"data":{...}}
{"version":"0.2.0","ok":false,"command":"click",
 "error":{"code":"STALE_REF","message":"界面已变化…","retry":"safe"}}
```

动作类命令还会在 `data.after` 里带回这次动作造成的界面变化：

```json
{"after":{"changed":true,"appeared":[...],"disappeared":[],
          "value_changed":[{"role":"edit","name":"文本编辑器","from":"","to":"abc"}],
          "new_windows":[],"window":{"alive":true,"title":"*无标题 - 记事本"}}}
```

## 运行前提：管理员

本技能要求执行它的进程具备管理员权限（完整性 `high`）。非管理员时除 `status`/`help`/`elevate`
之外的命令一律返回 `NEED_ADMIN`——桌面自动化在低完整性下只会"看起来成功、实际没动"，
所以在入口就拒绝；读不到自身令牌时同样拒绝（fail-closed）。

## 安全模型

**这个工具里没有任何物理输入。** 代码中不存在 `SendInput`、`SetCursorPos`、`mouse_event`、
`keybd_event`，所以它不可能移动你的鼠标、也不可能往你的前台窗口里打字。

- 动作只有两条路：UIA 语义（Invoke/Select/Toggle/SetValue/Expand/Scroll/ScrollItem），
  或向目标窗口投递消息（`WM_LBUTTONDOWN/UP`、`WM_CHAR`、`WM_KEYDOWN`、`WM_MOUSEWHEEL`）。
- 每条命令结束时核对前台：只有"被我们碰过的窗口/进程"抢走才还原，
  用户自己切走的不动；`side_effects.foreground_restored` 说明发生过并已还原。
- `launch` 用 `SW_SHOWNOACTIVATE`，`minimize` 用 `SW_SHOWMINNOACTIVE`，`move`/`resize` 用
  `SWP_NOACTIVATE`；`focus` 命令已删除，`--mode physical` 已删除。
- `elevate` 的结果文件只允许落在 `~/.dsh/desk/elevated-*.json`，且会核对它确实是这条命令的输出。
- 需要临时压住"自己定时置顶"的上位机时用 `desk guard --seconds N`。

## 完整性级别（UIPI）

Windows 给每个进程一个完整性级别：`untrusted < low < medium < high < system < protected`。
UIPI 规定低级别进程不能把窗口消息、钩子、模拟输入投给高级别进程；低打高会被内核直接丢弃，
表现就是"点了没反应"或 `ERROR_ACCESS_DENIED`。

本工具的处理：

- `status` 报自身级别，`windows`/`info` 报目标级别；`doctor` 在动作前给结论。
- 动作前比较：低于目标直接 `ELEVATION_MISMATCH`，读不到令牌则 `TARGET_PROTECTED`/`TOOL_TOKEN_UNREADABLE`。
- `desk elevate -- <命令>`：以管理员身份重新执行同一条命令（弹一次 UAC），
  结果放在 `data.result` 里原样带回。

## 已知边界

- **Chromium / Electron 的网页内容**：投递鼠标消息通常进不去（它只看真实输入）。
  这类目标应该用 CDP（`edge-browser` 技能）；`shot --marks` 只能覆盖 UIA 能读到的部分。
- **经典 Win32 菜单条**在非客户区：坐标点击点不到，用元素动作（`click` 会自动走 `expand`）。
- **最小化窗口**：语义动作通常还能用；坐标点击和截图不行，先 `restore`。
- **带修饰键的快捷键**：`key ctrl+s` 只保证消息投递，目标线程多半看不到 Ctrl 状态；
  要可靠就用元素动作，或 `type --method value`。
- **输入法**：`type` 默认走 `WM_CHAR`，会绕过输入法；`--method paste` 会临时占用剪贴板，
  命令结束会恢复原文本；原本不是文本时 `clipboard_restored:false`。
- **看门狗**：单条命令默认 60 秒（`DESK_WATCHDOG_MS` 可调）没返回就报 `WATCHDOG_TIMEOUT`；
  `--timeout-ms`/`--seconds` 会按声明值放宽。
- **混合 DPI**：本进程是 Per-Monitor V2；对 DPI-unaware 目标的坐标换算未做适配。
- 只支持 Windows 10 1809+ / Windows 11。

## 开发

```powershell
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
& .\target\debug\desk.exe selftest   # 自己开窗验证消息注入，不碰你的桌面
```

CI（GitHub Actions，windows-latest）跑 fmt/clippy/test/build；`selftest` 依赖真实桌面会话，
只在本地跑。发布走 tag `v*`，见 `.github/workflows/release.yml`。

## 许可

MIT
