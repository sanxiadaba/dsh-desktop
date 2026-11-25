# dsh-desktop

给 DSH（DeepSeek Harness）用的 Windows 桌面控制工具。一个 exe，零运行时依赖。

**它不做物理输入：不发 `SendInput`、不移动光标、不把窗口切到前台。**

## 为什么要有它

常见的桌面自动化工具只有两层动作：UIA 语义动作，和物理输入（SendInput）。
语义动作在"有树、没动作"的控件上直接失败；物理输入则必须把窗口拉到前台、真实移动光标。

中间那层——**把鼠标/键盘消息直接投递给目标窗口，不动光标也不抢前台**——通常没有。
Qt 程序、Win32 自绘界面、把控件拆成子 HWND 的上位机，恰好只有这一层能用。

`dsh-desktop` 补的就是这一层，并把它做成默认兜底：语义动作走不通时自动改用消息注入。

## 能力

| 类别 | 命令 |
| --- | --- |
| 观察 | `windows`、`status`、`snap`（UIA 树，带 `@ref`）、`info`、`find`、`shot`（客户区 1:1 PNG） |
| 语义动作 | `click`、`select`、`toggle`、`set`、`expand`/`collapse`、`scrollitem` |
| 后台注入 | `click --mode bg`、`rclick`、`dclick`、`key`、`type`、`scroll`、`drag` |
| 窗口 | `minimize`、`maximize`、`restore`、`close`、`move`、`resize`、`launch`（都不激活窗口） |
| 其它 | `wait`、`guard`、`run`（批量步骤）、`clipboard`、`selftest` |

设计细节：

- **快照是扁平的**（带 `depth`），输出是紧凑 JSON（`--pretty` 可读）；默认 `limit 300`，截断时 `truncated:true`。`n- 快照只保留最近 30 份；`wait` 和 `--verify` 不落盘，不会挤掉你的 ref。
- **元素身份 = 路径 +（角色、名字、automation_id、矩形）**，窗口里有两个「打开」按钮也能精确定位。
- **属性走 UIA CacheRequest**，一个元素一次跨进程调用。Qt 窗口 130 个元素约 30 ms。
- **每个动作后的回读验证**：`select`/`toggle` 读状态、列表项 `invoke` 读选中态、`set` 返回 `value_matches`；
  程序收了没反应就自动退回后台点击，结果里的 `mode` 会写成 `semantic:*` 或 `bg:*-fallback`。
- **后台点击会做客户区边界检查**：元素滚出视口时直接报 `POINT_OUTSIDE`，不会静默点空。
- **消息投给坐标下最深的子窗口**：经典 Win32 程序把页签/工具条做成子 HWND，投给顶层窗口等于没点。
- **键盘消息发给真正有焦点的子控件**（线程 `hwndFocus` → 可聚焦的 edit/document → 顶层窗口），
  否则 Win32 程序收不到。
- **文本输入**逐字投 `WM_CHAR`，ASCII 和中文都走这条路（绕过输入法）；
  `--method paste` 只在目标不认 `WM_CHAR` 时用。
- **截图是客户区 1:1 像素**：和 UIA 报的屏幕坐标能直接对上。

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
& $desk snap --window w-1234 --actionable   # 读界面，拿 @ref
& $desk click '@smwvx9pfk:e7' --window w-1234   # 语义优先，走不通自动后台注入
& $desk type "hello" --window w-1234  # 后台打字，不抢前台
& $desk shot --window w-1234 --out C:\temp\a.png
& $desk selftest                       # 自测，不需要任何外部程序
```

每个命令输出一个 JSON：

```json
{"version":"0.1.0","ok":true,"command":"snap","elapsed_ms":31,
 "side_effects":{"foreground_restored":false},"data":{...}}
{"version":"0.1.0","ok":false,"command":"click",
 "error":{"code":"STALE_REF","message":"界面已变化…","retry":"safe"}}
```

## 运行前提：管理员

本技能要求执行它的进程具备管理员权限（完整性 `high`）。非管理员时除 `status`/`help`/`elevate`
之外的命令一律返回 `NEED_ADMIN`——桌面自动化在低完整性下只会"看起来成功、实际没动"，
所以在入口就拒绝，而不是事后打补丁。

## 安全模型

**这个工具里没有任何物理输入。** 代码中不存在 `SendInput`、`SetCursorPos`、`mouse_event`、`keybd_event`，
所以它不可能移动你的鼠标、也不可能往你的前台窗口里打字。

- 动作只有两条路：UIA 语义（Invoke/Select/Toggle/SetValue/Expand/Scroll/ScrollItem），
  或向目标窗口投递消息（`WM_LBUTTONDOWN/UP`、`WM_CHAR`、`WM_KEYDOWN`、`WM_MOUSEWHEEL`）。
- 每条命令结束时都会核对前台窗口：被目标程序（Qt、模态对话框）抢走就立刻还原，
  `side_effects.foreground_restored` 说明发生过并已还原。
- `launch` / `restore` 用 `SW_SHOWNOACTIVATE`，`move` / `resize` 用 `SWP_NOACTIVATE`，都不激活窗口。
- `focus` 命令已删除（调用返回 `DISABLED`）；`--mode physical` 已删除（返回 `BAD_ARGS`）。
- 需要临时压住"自己定时置顶"的上位机时用 `desk guard --seconds N`。

## 完整性级别（UIPI）

Windows 给每个进程一个完整性级别：`untrusted < low < medium < high < system < protected`。
UIPI（User Interface Privilege Isolation）规定：**低级别进程不能把窗口消息、钩子、模拟输入投给高级别进程**，
高打低没问题，低打高会被内核直接丢弃，表现就是"点了没反应"或 `ERROR_ACCESS_DENIED`。

本工具的处理：

- `status` 报自身的级别，`windows` / `info` 报目标的级别（如 `high (0x3000)`）。
- 动作前先比较：低于目标就直接返回 `ELEVATION_MISMATCH`，附上两边的级别和目标进程，
  不会让动作"静默失败"。
- `desk elevate -- <命令>`：以管理员身份重新执行同一条命令（弹一次 UAC），
  结果通过临时文件带回并原样返回。

彻底解决只有三条路：

1. **让 DSH / 终端以管理员身份运行**——最简单，之后所有命令天然同级。
2. **只在个别目标上提权**——用 `desk elevate -- ...` 包一层。
3. **protected / PPL 进程**（杀软、反作弊等）用户态无法投递，也别想着注入 DLL 或关掉 UIPI，
   那是降低整机安全，不是解决问题；这类目标只能走它自己的官方接口。

## 已知边界

- **Chromium / Electron 的网页内容**：投递鼠标消息通常进不去（它只看真实输入），
  这类目标应该用 CDP（`edge-browser` 技能）。
- **最小化窗口**：UIA 语义动作通常还能用；坐标点击不行（没有布局），先 `restore`。
- **提权窗口**：本进程需要与目标同级或更高完整性，否则消息会被 UIPI 丢弃。
- **输入法**：`type` 默认走 `WM_CHAR`，会绕过输入法；`--method paste` 会临时占用剪贴板，命令结束会恢复原文本。`n- **`key` 是 best-effort**：投递的是窗口消息，不经过目标线程的 TranslateAccelerator，少数程序的快捷键可能不触发；文档与返回里的 `mode` 会说明走的是投递路径。
- 只支持 Windows 10 1809+ / Windows 11。

## 开发

```powershell
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test                       # 18 个纯单元测试，不需要桌面
& .\target\debug\desk.exe selftest   # 7 项，自己开窗验证消息注入，不碰你的桌面
```

没有远程 CI：这个工具的行为依赖真实桌面环境，验证在本地做。

## 许可

MIT
