---
name: desktop
description: >
  Control Windows desktop applications without stealing the user's screen:
  list windows, read the UIA tree, and click/type/scroll by ref. Actions go
  semantic first and fall back to background window-message injection, so the
  cursor does not move and the foreground window does not change. Use for any
  request to operate a GUI program (PowerWriter, Notepad--, Explorer, an
  industrial control panel, a settings dialog), read on-screen state, fill a
  field, press a shortcut, take a screenshot, or 后台操作/控制桌面/点击按钮.
---

# 桌面控制

`desk.exe` 是这个技能的入口，一个自包含的 Windows 命令行工具：观察界面、执行动作、验证结果。

路径固定，先定义一个变量再开始：

```powershell
$desk = "$env:USERPROFILE\.dsh\skills\desktop\desk.exe"
if (-not (Test-Path $desk)) { throw "desk.exe 不存在，先运行 $env:USERPROFILE\.dsh\skills\desktop\build.cmd" }
```

## 前提：必须以管理员身份运行

这个技能要求执行它的进程具备管理员权限（完整性级别 `high`）。
非管理员运行时，除 `status` / `help` / `elevate` 之外的所有命令都会直接返回
`NEED_ADMIN`（不会"看起来成功、实际没动"）。

只有单条命令需要提权时：

```powershell
& $desk elevate -- click '@s1:e3'
```

会弹一次 UAC，子进程的结果原样带回。长期使用请直接以管理员身份启动 DSH / 终端。

每个命令输出一个 JSON 信封：`{"ok":true,"command":"...","elapsed_ms":31,"data":{...}}`，
失败时是 `{"ok":false,"error":{"code":"...","message":"...","retry":"safe|unsafe"}}`。
用 `| ConvertFrom-Json` 解析，**不要**用返回文字去猜。

## 必须遵守的三条

1. **动作前必须先看**：`snap` 一次，拿到 `@ref` 再动手。不要凭记忆复用一个旧 ref。
2. **动作后必须验证**：重新 `snap`、看窗口标题、或读元素的值，确认真的生效了。
   返回 `ok:true` 只代表消息投递成功，不代表界面按预期变了。
3. **不要抢用户的前台**：这个工具没有物理输入，也没有 `focus` 命令。
   默认路径本来就是后台的。

## 主循环

```powershell
# 1. 找窗口（字段在 .data 里）
$wins = (& $desk windows | ConvertFrom-Json).data
$win  = $wins | Where-Object { $_.title -like '*记事本*' } | Select-Object -First 1
$win.id, $win.title

# 2. 读界面（默认 limit 300，截断时 truncated=true）
$s = & $desk snap --window $win.id --actionable | ConvertFrom-Json
$s.data.truncated
$s.data.elements | Select-Object ref, role, name, actions | Format-Table -AutoSize

# 3. 动手（ref 必须来自本次输出）
$ref = ($s.data.elements | Where-Object { $_.name -eq '保存' } | Select-Object -First 1).ref
& $desk click $ref --window $win.id | ConvertFrom-Json

# 4. 验证
& $desk snap --window $win.id --actionable --name 保存 | ConvertFrom-Json
```

> **所有动作类命令都必须显式给 `--window` 或 `--ref`**，工具不会默认操作前台窗口。
> 输出是紧凑 JSON（一行），需要人看时加 `--pretty`。

`snap` 的输出是扁平列表，`depth` 表示层级：

```json
{"ref":"@smwvx9pfk:e7","role":"button","name":"应用设置","depth":3,
 "actions":["click"],"rect":{"x":820,"y":430,"w":96,"h":30}}
```

`actions` 决定这个元素能怎么动：

| actions 里有什么 | 该用的命令 |
| --- | --- |
| `click` | `click`（走 Invoke） |
| `select` | `click` 或 `select`（单选、列表项、页签） |
| `toggle` | `toggle`（勾选框，再 `click` 也行） |
| `set` | `set <@ref> "值"`，或 `type --method value --ref @ref` |
| `expand` | `expand` / `collapse`（菜单、树节点、下拉框） |
| `scrollitem` | `scrollitem`（把滚出视口的元素滚回来） |

动作返回里的 `mode` 告诉你实际走了哪条路：`semantic:*` 是 UIA 动作，`bg:*` 是后台注入。
`select`/`toggle` 和列表项的 `click` 会回读状态，程序收了却没反应时自动退回后台点击
（`mode` 会是 `bg:select-fallback` / `bg:invoke-fallback`），所以不会出现"返回 ok 但界面没动"。

没有可用 actions 的元素也照样能点：`click` 会自动退回**后台消息注入**（不动物理光标）。
需要强制走注入就加 `--mode bg`。

## 动作

```powershell
& $desk click  '@s1:e7'                  # 语义优先 → 后台注入兜底
& $desk click  '@s1:e7' --window w-1234 --mode bg   # 强制后台注入（不动光标）
& $desk click  '@s1:e7' --verify         # 动作后自动重新读一次状态
& $desk toggle '@s1:e9' --window w-1234   # 勾选框
& $desk set    '@s1:e4' "C:\temp\a.hex" --window w-1234   # 直接赋值，返回 value_matches
& $desk scrollitem '@s1:e120' --window w-1234   # 滚进可见区

```

键盘与文本，默认打给**目标窗口里真正有焦点的子控件**，不要求窗口在前台：

```powershell
& $desk key  "ctrl+s" --window $win.id   # best-effort：少数程序不认投递的快捷键
& $desk key  "enter"  --ref '@s1:e7'
& $desk type "hello 中文" --window $win.id        # 逐字 WM_CHAR，ASCII 和中文都行
& $desk type "中文" --ref '@s1:e4' --window $win.id --method value   # 直接写值，最稳
& $desk type "中文" --window $win.id --method paste  # 只在目标不认 WM_CHAR 时用（剪贴板会恢复）
```

鼠标：`scroll`、`wheel`、`drag`、`rclick`、`dclick` 都是后台注入，
坐标既可以是元素 ref，也可以是窗口内坐标。

```powershell
& $desk rclick '@s1:e7' --window w-1234   # 右键（上下文菜单）
& $desk dclick '@s1:e12' --window w-1234  # 双击（会补 WM_LBUTTONDBLCLK）
& $desk scroll --window $win.id --delta -360
& $desk drag "100,200" "400,200" --window $win.id
```

## 窗口

```powershell
& $desk launch "C:\Windows\System32\notepad.exe"
& $desk wait --title "记事本" --timeout-ms 8000     # 等窗口出现，返回 id
& $desk move   $win.id 100 80
& $desk resize $win.id 1200 800
& $desk minimize $win.id
& $desk restore  $win.id
& $desk close    $win.id
```

`restore` 只恢复不激活；工具里没有把窗口切到前台的命令。

## 其它命令

```powershell
& $desk find "button:保存" --window $win.id      # 等于 snap + 过滤
& $desk info '@s1:e7' --window $win.id           # 单个元素完整信息（含 native_hwnd / 完整性）
& $desk clipboard get                            # 读剪贴板文本
& $desk clipboard set "文本"                      # 写剪贴板文本
& $desk maximize $win.id                         # 最大化（不激活）
& $desk guard --seconds 30 --window $win.id      # 临时钉住前台：目标自己跳出来就压回去
```

## 观察别的形式

```powershell
& $desk shot --window $win.id --out C:\temp\ui.png   # 客户区 1:1，被遮挡也能拍
& $desk shot --screen 0 --out C:\temp\screen.png     # 整屏
& $desk info '@s1:e7'                                # 单个元素的完整信息
& $desk wait --window $win.id --text "完成" --timeout-ms 15000   # 标题或界面文本
& $desk wait --window $win.id --element "button:保存" --timeout-ms 15000
```

读不到 UIA 的程序（自绘界面、游戏、部分 Electron 页面）只能靠截图自己看：
`shot` 出来再用 `read_image` 看，然后按坐标 `click --mode bg` 或 `drag`。

## 批量执行

一次进程里跑多步，省掉重复启动开销（慢程序尤其明显）：

```powershell
@'
{"steps":[
  ["click","@s1:e7"],
  ["set","@s1:e4","C:\\firmware.hex"],
  ["click","@s1:e9"],
  ["wait","--window","w-1234","--text","成功","--timeout-ms","20000"]
]}
'@ | & $desk run -
```

每步返回自己的 `ok`/`data`，`failed` 给出失败步数。

## 出错怎么办

| 错误码 | 含义 | 处理 |
| --- | --- | --- |
| `STALE_REF` | 界面变了，旧 ref 失效 | 重新 `snap`，用新 ref |
| `AMBIGUOUS_TARGET` | 同名元素太多，无法确定 | 重新 `snap` 换 ref；或用 `--mode bg` 按坐标点 |
| `POINT_OUTSIDE` | 元素滚出视口，坐标点不到 | 先 `scrollitem`，再点 |
| `NO_PATTERN` | 该元素没有这个 UIA 动作 | 换动作（`select`/`toggle`）或加 `--mode bg` |
| `BG_DELIVER_FAILED` | 目标不处理投递的消息 | 目标可能挂起，或它在 DirectX/UIPI 环境里 |
| `WINDOW_GONE` | 窗口已关闭 | 重新 `windows` 找 |
| `ELEVATION_MISMATCH` | 目标完整性比你高，UIPI 会拦消息 | 用管理员运行 DSH，或 `desk elevate -- <同样的命令>` |
| `WAIT_TIMEOUT` | 等待条件没出现 | 加大 `--timeout-ms`；先确认程序真的在执行 |
| `NEED_ADMIN` | 当前不是管理员 | 用管理员启动 DSH/终端，或 `desk elevate -- ...` |
| `BAD_ARGS` | 参数/选项不对（含拼错、缺值） | 按 `desk help` 改；动作类命令必须给 `--window`/`--ref` |
| `BAD_REF` | ref 格式不对或快照 id 非法 | ref 必须来自本次 `desk snap` |
| `BAD_WINDOW_ID` | 窗口 id 格式不对 | 形如 `w-1a2b3c` |
| `SNAPSHOT_NOT_FOUND` | 快照被清理（只保留最近 30 份） | 重新 `desk snap`；`wait`/`--verify` 不落盘 |
| `ACTION_FAILED` | UIA 动作本身失败 | 换动作或 `--mode bg` |
| `TARGET_PROTECTED` | 读不到目标进程令牌（PPL/受保护进程） | 无法自动化，走官方接口 |
| `ELEVATE_FAILED` / `ELEVATE_TIMEOUT` / `ELEVATED_COMMAND_FAILED` | 提权执行失败/超时/子命令失败 | 看 `details.result` |
| `DISABLED` | 命令已禁用（如 `focus`） | 用后台动作 |

`retry` 字段：`safe` 表示动作肯定没落地，可以重试；`unsafe` 表示可能已经生效，
**先重新读状态再决定**，不要盲目重试（会点两次）。

## 坑

- **窗口里有两个同名按钮**：`ref` 用路径+矩形区分，正常不会拒绝。真遇到
  `AMBIGUOUS_TARGET` 说明它们连位置都一样，那就用 `--mode bg` 按某个具体 ref 的矩形点。
- **列表滚出视口**：UIA 会给出窗口外的坐标（比如 x=6000），后台点击会报 `POINT_OUTSIDE`。
  先 `scrollitem` 再点。
- **最小化窗口**：语义动作通常可用，坐标点击不行。要么 `restore`，要么只用语义动作。
- **键盘打不进去**：先确认 `type` 返回的 `target.class` 是不是 `Edit` 之类的输入控件；
  不是的话用 `set --ref` 直接写值。
- **中文**：`--method paste` 会覆盖剪贴板；命令会回读确认。别在用户正复制东西时用。
- **Electron/Chromium 页面**：网页内容收不到投递消息，改用 `edge-browser` 技能走 CDP。
- **弹窗/对话框**：动作后 `snap` 会看到新的顶层窗口，用 `windows` 找它的 id 再操作。

## 自检

怀疑环境有问题时先跑自测，它不依赖任何外部程序：

```powershell
& $desk selftest            # 7 项：消息注入、子窗口、键盘、文本、滚轮、枚举、截图
```

全绿说明注入层是好的，问题就在目标程序那一侧。
