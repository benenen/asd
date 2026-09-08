# TUI 文件上传设计（GUI 上传方案的补充）

状态：设计提案，未实现。2026-09-08 核查。本文只增加设计文档，不改变代码、配置或运行进程。

## 1. 推荐结论与边界

采用“显式进入上传模式 → 接收路径文本或选择文件 → 后台传输 → 完成后用户选择插入路径”的交互。
第一版建议键位 `Ctrl+A u`，允许配置重绑；该键位是提案，不是现有命令。

**纯 TUI 没有跨平台原生 FileDrop 事件。** 文件管理器的拖入由外层终端模拟器处理；它可能把文件路径转换为粘贴文本，也可能启动自己的上传工具。asd 只能处理终端实际传下来的数据，不能从 `MouseEventKind::Drag` 得到文件。

**源文件必须能被负责上传的进程读取。** 本机 TUI 可以读本机路径；`ssh server` 后运行的 TUI 不能凭电脑上的路径读取电脑上的文件。后者需要本地助手/支持文件传输的终端，不能仅靠 ratatui 或新增一个 OSC 完成。

默认不截获普通会话中的路径粘贴，不自动上传猜测出的路径。用户先打开上传面板，再拖入/粘贴或选择文件；这样既支持没有 paste 边界的终端，也不会破坏 Agent 正常输入。

## 2. 现有代码接缝与前置条件

核查基础：当前 main `53db555`；已交付的任务审查分支 `feature/session-task-review` 的 `bb22744`。实现时应重新确认集成分支；下列是现状和拟接缝，不代表上传已存在。

- [Cargo.toml](../Cargo.toml)：ratatui 0.30.2，crossterm 0.29 系列，并全工作区固定到 crossterm PR #1030 的 `94ed7154e35d24a2f643c61760e1eb05b8ca9e63`。不能只凭 crates.io 0.29 文档推断本仓库 Windows 行为。
- [TUI lib.rs](../crates/asd-tui/src/lib.rs)：`run` 启用 raw mode、alternate screen、mouse capture 和 bracketed paste；同步主循环先处理连接事件，再按 dirty 绘制，再 `event::poll/read` 分派 Key/Mouse/Paste/Resize。它不是已有的 Tokio `EventStream` 主循环。
- `App::on_paste`：modal 优先，否则转换为目标终端的 paste 字节，经 `Cmd::Input` 发送；等待 attach Snapshot 时暂存粘贴。
- [conn.rs](../crates/asd-tui/src/conn.rs)：连接 actor 在独立线程的 Tokio current-thread runtime 上运行，向主线程发送带连接 generation 的事件。任务审查分支已有独立异步 review 请求，可参考生命周期处理。
- [ui.rs](../crates/asd-tui/src/ui.rs)、[modal.rs](../crates/asd-tui/src/modal.rs)、[keymap.rs](../crates/asd-tui/src/keymap.rs)：分别承接绘制、输入状态和可配置键位。
- 目前 TUI 的连接入口是 `socket: PathBuf`，并没有 GUI 那套多主机 SSH `RemoteSpec` 选择器。若要本机 TUI 直接上传到远端，必须先有可复用的远端连接 factory（或显式配置的协议代理端点）；不能假定 `asd ui --host` 已存在。
- 当前 TUI attach 写死 `read_only: false`，不是已经支持了只读 UI。已有协议支持只读 attach；上传设计必须新增/贯通当前视图的可写能力状态。

## 3. 拖拽、粘贴、OSC 与 tmux 的真实语义

### 3.1 crossterm 能看到什么

`Event` 包括 Key、Mouse、Paste(String)、Resize 和焦点事件，没有带 OS 文件句柄/路径列表的 FileDrop。`MouseEventKind::Drag(button)` 是终端坐标中的鼠标按键拖动，用于选区/分隔条，不是桌面文件拖入。ratatui 负责渲染，不补充系统 DnD 数据源。[C1][C2]

外层终端若在 bracketed paste 模式下把路径发送为：

```text
ESC [ 200 ~ /local/path/image.png ESC [ 201 ~
```

crossterm 才能提供一个有边界的 Paste 文本事件；没有该边界时可能是多个 Key/字符事件。模式 2004 是请求，不保证所有终端的所有拖入途径都遵守。Paste 没有原生来源信息，不能分辨拖入、菜单粘贴和模拟粘贴。[C1][X1]

不采用“短时间收到大量键盘事件就是拖入”的推断；这会误伤输入法、宏和正常粘贴。上传面板中的字符逐个进入路径输入框即可，用户显式确认后才验证文件。

### 3.2 OSC 52 不传文件

OSC 52 用于设置/查询终端剪贴板选择数据，通常承载编码后的文本，并受终端策略限制。它不是“文件拖拽通知”，不能取得本地文件列表或凭空读取电脑磁盘。完成后的“复制路径”可使用现有剪贴板能力；不可用时保留可选择的路径文本。[X1][T1]

OSC 7 可报告 shell 工作目录，但它同样不能传文件字节。不要把目录提示当作文件已在远端的证据。

### 3.3 tmux 的角色

典型输入路径：桌面文件管理器 → 终端生成路径文本 → tmux → asd TUI。tmux 可依自身模式处理/转发 paste 和鼠标输入，但不会把文本升级为文件对象。copy-mode、键位拦截、嵌套层数都可能改变所见输入，需要组合测试。

tmux 的 `set-clipboard`/`Ms` 和输出 escape passthrough 解决的是剪贴板/终端控制问题，不提供通用拖入上传。不能把“开启 passthrough”当作修复 FileDrop 的办法。[T1][T2]

`trzsz` 则是独立文件传输协议，需要本机集成/包装器与远端 receiver 配合；它支持 tmux 和拖入上传，值得研究为远端 TUI 的后续桥接方案，不是直接加进 ratatui 就能获得的事件。[Z1]

## 4. 各平台能力矩阵

下表“路径可读”始终指 **asd TUI 所在机器/命名空间**，不是屏幕所在电脑。

| 场景 | TUI 可能收到的输入 | 文件读取与设计 | 必须真机验证 |
|---|---|---|---|
| Windows Terminal + Windows 原生 asd | 当前上游取路径、按 profile 的 PathTranslationStyle 转换及引用、经 PasteText 输入 [W1]；是否封装 Paste 仍需实测 | Windows 路径；仅接受明确的单路径格式。保留盘符/反斜线，不套用 POSIX shell parser | Explorer 拖入、中文/空格/引号、普通与提权终端、IME、VT 模式恢复 |
| Windows Terminal + WSL asd | Windows Terminal 支持 profile 路径转换选项；自定义/旧版 profile 仍可能送 Windows 路径 [W2] | 仅在确认本进程处于 WSL 时按显式策略转换，或要求用户选择可读的 `/mnt/...` 路径；检查实际挂载，不能硬编码所有盘符映射 | 原生路径转 WSL、UNC/网络盘、发行版、挂载配置、Paste 边界 |
| macOS Terminal.app | Finder 拖入由 Terminal 转为绝对路径文本 [A1] | 本机可读；上传面板识别保守的路径/转义形式；不同 shell 引用方式必须确认 | 空格、单引号、Unicode、多个文件、bracketed paste |
| macOS iTerm2 | 普通拖入和 Option-drag 可能是不同功能；后者在 shell integration 中可直接上传 [I1] | 首版要求普通拖入到 asd 上传面板；不得把 iTerm2 已上传后生成的远端路径再次当成本地文件 | 修饰键、shell integration 开关、远端 cwd、是否被终端自行接管 |
| macOS Ghostty | 当前上游 performDragOperation 使用 sendText [G1]，不能据此保证 Paste 事件 | 上传模式同时支持逐字符输入；文件选择器保底 | Finder、路径引用、下游sendText是否加边界、mouse capture |
| Linux Ghostty GTK | 当前上游接收 GDK 文件列表，转本地路径并转义后调用 Clipboard.paste [G2] | 已有终端侧拖入实现，但具体发行版和非本地文件行为需验证 | GTK文件管理器、Wayland/X11、转义及Paste边界 |
| Linux 其他终端（GNOME Terminal 等） | GNOME Terminal 当前源码将文件/URI转义后交给 VTE paste [L1]；其他终端不保证相同 | 不宣称 Linux 统一有文件事件；只解析已验证格式，不认识则留在输入框供修正 | 实际终端/桌面组合、`file://`、多个文件、网络位置 |
| 任意终端 → ssh → 远端 asd ui | 电脑路径可能完整穿过 SSH，但文件字节没有随之传输 | 默认提示“本 TUI 位于远端，无法读取电脑文件”；使用本地助手，或先用现有传输工具上传 | 不能因为远端碰巧有同名路径就当成同一个本地文件；远端模式默认禁用电脑拖入解释 |
| 任意平台再套 tmux | paste/鼠标经过 tmux，中间没有文件元数据 | 上传模式接受最终文字；不能识别时用文件选择器；本地/远端可读性规则不变 | 一层/多层 tmux、copy-mode、鼠标开关、pane 切换 |

Windows 历史记录显示本项目曾修复 stock crossterm 的多行粘贴按键化问题；当前核查也确认固定 revision 中存在 Windows bracketed-paste VT 输入处理。这是源码及历史依据，**不是本轮已在 Windows 真机验证拖拽**。不要删除这份补丁或新增第二套读取 stdin 的实现。

## 5. UI 与交互状态

### 5.1 进入与选择

`Ctrl+A u` 打开上传面板，立即冻结目标：endpoint ID、daemon epoch/连接代次、SessionIdentity、view_id；名称只是可更新显示标签，不参与身份匹配。

```text
┌ 上传文件 ─────────────────────────────────────────────────┐
│ 上传到：服务器 A / 会话 asd                                │
│ 来源：运行本 TUI 的机器                                    │
│ 文件路径：[                                              ] │
│ 拖入/粘贴单个文件路径，或 F2 选择文件                        │
│ Enter 检查文件并继续                      Esc 关闭          │
└───────────────────────────────────────────────────────────┘
```

面板内 Paste 与 Key 文本都只编辑路径字段；鼠标事件只操作面板，不流入终端。拖拽没有可靠目标坐标时，以打开面板时锁定的会话为目标，不推断鼠标落在哪个后台行。外部路径普通粘贴保持原有行为。

路径解析是数据解析，禁止调用 shell、`eval` 或执行 `$(...)`。第一版仅单个普通文件：原样绝对路径、确认过的单层引用格式、受限本地 `file://` URI；相对路径以明确显示的源目录解析。多路径/有歧义/控制字符/非 UTF-8 路径显示错误，允许手动修正。URI 只解码一次，并限制 authority；不把 `file://其他主机/...` 当本地文件。文件类型/大小检查及打开操作在后台执行；无自动目录递归。

检查成功后显示解析后的路径、大小、目标和“开始上传”；此处 Enter 只是面板操作，不是发送给 Agent 的回车。20 MiB 单文件上限、64 KiB chunk 为初始建议。

### 5.2 传输与取消

```text
上传到：服务器 A / 会话 asd
截图.png  [████████████░░░░░░░░]  61%  12.2 / 20.0 MiB
状态：传输中                  Esc 请求取消
```

百分比以 daemon 已确认接收的 bytes/total 计算，不能拿排入本地 socket 的字节冒充远端已保存。total=0 时不除零，显示“正在保存空文件”；校验/落盘阶段显示“传输完成，正在确认”，最终 Ack 前不展示上传成功。速率/ETA 是可选估算，不影响状态判定。

Esc 在传输中发独立取消信号，进入 Cancelling；不向 PTY 发 Ctrl+C。等待取消 Ack 或连接关闭后再显示 Cancelled。若完成和取消竞态，最终以服务端提交状态为准：显示“文件已保存，未插入”，不删除已完成文件。退出 TUI 取消本 TUI 所有未完成任务，保留已完成文件；断线状态显示为“结果待确认”，根据 upload_id 查询服务端结果。

第一版只允许一个活动上传，面板内不切换会话。系统侧仍可能发生改名、删除、view 撤销、重连，均必须处理。后台上传及最小化进度条可以后续再扩展。

### 5.3 完成与路径插入

```text
已上传到：服务器 A / 会话 asd
远端文件：/home/user/.local/share/asd/uploads/<id>/image.png
待插入文本：[ /home/user/.../image.png                      ]
[I] 插入目标输入位置    [C] 复制路径    [Esc] 关闭
不会自动提交消息；上传成功不代表 Agent 已读取图片。
```

asd TUI 当前主体是 Agent 的终端视图，**不存在一个可直接赋值的通用 Agent 输入框**。新增的是 asd 自己的完成面板字段；I 才把字段文本以粘贴方式送入目标 PTY，终端内真正的编辑器由 shell/Claude/Codex 自己负责。

- 单独使用 I，不把最终“插入”绑定到连续 Enter，避免按键连发导致意外提交。路径编辑与动作区通过 Tab 切换；I/C 快捷键只在动作区生效，编辑区按普通字符处理。
- 发送前重新验证 endpoint/epoch/identity/view_id、可写权限、Snapshot 已收敛、目标仍是原视图，且不存在撤销或关闭状态。
- 不用上传专用连接上的 attach-free `SendInput` 绕过当前视图/只读语义；沿当前已验证附件路径插入。推荐新增绑定 identity+view_id 的 Paste/Insert 命令，由连接 actor 和 daemon 在实际写入点核验，而不是仅 UI 发之前比较一次。
- 复用 `asd_client::terminal::paste_bytes` 的模式 2004 包装/终止标记清理；模式来自当前目标 Snapshot/VT，不依据宿主终端是否启用 bracketed paste 推断。
- 不追加 CR/LF，也不调用 `send --enter`；不会清空既有 composer。未知 shell/Agent 的转义规则需用户预览选择“原始路径/引用路径”，默认复制保底；不能承诺任意 TUI 接收文本都不会触发自身快捷操作。
- 明确选择“插入”且目标就绪才允许发送；已识别 Working/Blocked 或未知输入上下文时禁用自动式插入，保留复制。检测 Idle 也不能证明输入框为空，因此始终保留用户预览。
- 改名可以更新标签但身份不变；切换到别的视图、同名会话重建、断线重连都禁用原任务的 I，保留路径，要求重新定位目标并确认。

## 6. 主循环与后台任务

拟新增模块 `crates/asd-tui/src/upload.rs` 管理纯状态/reducer，`upload_ui.rs` 绘制；传输放 `asd-client` 的共享 upload 客户端，daemon 保存模块与 GUI 复用。平台路径解析/文件源适配集中在各 crate 的 `platform/`；不在 call site 散布 OS cfg，不给 TUI 引入 GUI 框架或 PTY 管理依赖。

拟数据结构（示意，尚未编码）：

```text
UploadTarget = { endpoint_id, daemon_epoch, connection_generation,
                 session_identity, view_id, display_host, display_session }
UploadState  = Selecting | Validating | Ready | Uploading | Finalizing
             | Cancelling | Completed | Cancelled | Failed | Uncertain
UploadEvent  = Validated | Progress(acked_bytes,total) | Committed(path,digest)
             | Cancelled | Failed(kind,message) | ConnectionLost
每个事件必须带 upload_id + attempt_generation + target 身份
```

1. 主循环处理 Key/Paste/Mouse，只更新本地字段或发送 worker 命令；不做磁盘访问、hash、网络等待。Resize 仍走原布局路径。
2. 连接 actor 调度独立上传 task，使用专用协议 stream；块背压不能堵住 attach stream、用户输入或取消通道。现有 actor 位于独立线程，没必要每个 chunk 再开线程。
3. 文件读取使用异步文件 IO 或受限 blocking worker；长 hash 不占满 current-thread runtime。UI 主线程独占 App/VT/ratatui Terminal；worker 不改 App，也不写 stdout。
4. 进度使用 watch/latest slot 合并为最新值，建议最多 10 次/秒；状态结果用有界可靠队列，最终完成/失败不可丢。取消用独立 token，不排在满的数据队列后面。
5. 主循环每轮以有限事件数/时间预算读取上传事件；匹配完整任务代次后归约为新 UI 状态，设 dirty。不能把每个64KiB进度塞进现有无界事件通道，导致 PTY/resize 饥饿。
6. 沿用 `loop_timing` 与 `FrameBuf` 单帧单次写入。进度更新到来后只在下一轮绘制；无新进度不强制持续动画。最迟响应是现有 poll timeout 加本轮预算，实际耗时需要测量，不承诺固定30/60FPS。
7. 绘制层只读 UploadState；`Gauge.ratio` clamp 到0..1，u64字节计数防溢出，最终 Ack 单独驱动 Completed。窄屏换成文字百分比，长路径按显示宽度省略，避免 CJK 错位。
8. “传输中切会话”即使第一版 UI 不提供，也要让外部切换/撤销失效旧请求。上传结果与当前焦点分离保存，迟到事件绝不自动插入新会话。

## 7. 远端 TUI 的可行传输拓扑

| 运行方式 | 第一版能做什么 | 必要前提 |
|---|---|---|
| 本机 asd ui → 本机 daemon | 从本机选文件，导入 daemon 管理的上传目录 | 本机文件可读；共享上传协议 |
| 本机 asd ui → 明确的远端 asd 协议端点 | 本机读文件，经专用连接上传到远端 | TUI 新增/复用远端 connection factory；当前只有 socket 入口，不能直接套用 GUI SSH 配置 |
| ssh A 后运行 asd ui | 选择 A 上的文件；电脑文件不能直接读 | 明确显示“文件选择器浏览服务器 A”，禁止以电脑路径存在性猜测来源 |
| 电脑文件 → A 上的 TUI | 第二阶段提供本地 helper/终端传输集成 | helper 有本地文件访问权，显式绑定目标 daemon/identity，走认证通道；不能只加远端库 |
| A 会话内部又 ssh B | 只知道 A daemon，不自动猜 B | 上传到 A 与 B 的命名空间不同；提示选择 B 的显式连接，或使用外部传输工具 |

`SSH_CONNECTION` 等环境变量只用于提示，并非完整可靠的机器来源证明；容器、WSL、SSH 包装器和继承环境都可能改变它。客户端应展示源端和目的端，要求来源模式显式确认；远端 TUI 默认选择“服务器上的文件”。

上传到 daemon 的路径若不在 Agent 容器/沙箱可见范围，同样不能声称可读取；显示“已上传，Agent 可见性未验证”，用户选择实际可访问位置或挂载后再插入。

## 8. 只读、嵌套 SSH 与错误态

| 条件 | TUI 展示及行为 |
|---|---|
| 只读附件/已撤销视图 | I 置灰并显示原因；默认禁止为该视图发起上传，允许查看已有结果/复制。服务端也校验插入权限，不能仅靠 UI 按钮 |
| snapshot_pending | 显示“正在连接目标会话”；不把路径丢进通用 pending_pastes，以免切换后误发 |
| 识别到嵌套 SSH | 展示实际上传主机和“终端可能位于另一台主机”，不自动插入；检测不到不代表不存在，提供人工标记 |
| 文件不存在/不支持的引用/不可读 | 在路径字段下显示错误，保留输入供修改，不把路径发给 PTY |
| 超限/目录/特殊文件 | 显示大小限制或类型原因；仅普通文件，读取过程中仍检查大小上限 |
| 网络中断/超时 | 保留文件和目标信息，标为 Uncertain；先按 upload_id 查询，结果未知时不盲目重复提交 |
| 磁盘满/权限不足/校验失败 | Failed，给出可读错误及重试/重新选目标按钮，不显示有效远端路径 |
| 取消超时 | 提示“取消结果待确认”，没有成功插入，也不谎称临时文件已清理 |
| 同名重建/目标退出 | 文件若已提交则保留其结果；禁止向替代会话插入 |

显示错误、文件名和路径前过滤终端控制及 bidi 控制；原始路径作为数据保存，不把过滤后的显示字符串当真实文件名。上传保存使用随机私有目录、不覆盖文件、临时文件原子提交；失败重试、成功文件保留策略沿上轮 daemon 设计实现。

## 9. 组件选择与最小 widget

| 组件 | 建议 |
|---|---|
| ratatui `Gauge` / `LineGauge` | 直接使用现有 crate 的标准组件；弹窗用 Gauge，紧凑底栏用 LineGauge [R1] |
| `Block`、`Paragraph`、`Clear`、`Layout` | 拼上传面板、错误区、路径编辑预览；复用项目现有 modal 的按字符编辑行为 |
| 路径输入 | 第一版一个受限单路径字段即可，无需引入完整多行编辑器；光标与截断需按字符/显示宽度处理 |
| 文件选择器 | 最小目录列表+父目录+单选普通文件；在后台列目录，限制结果数量。选文件发生在 TUI 所在主机，不是自动打开用户电脑的文件管理器 |
| 系统 DnD crate / winit / GTK drop target | 依赖原生窗口所有权，不会给另一个终端窗口内部的 TUI 增加 FileDrop；不引入 TUI |
| trzsz / tssh | 可借鉴传输握手、取消、进度与本地桥接；需要完整端到端集成，不能当作 ratatui widget [Z1] |

没有核实到一个能在任意终端中给 crossterm 提供原生文件对象的通用 crate；不把普通鼠标拖拽组件称为文件上传组件。

最小 `UploadPanel::render(&UploadState, Rect, Buffer)` 是纯绘制函数：
上部目标及文件 → 中部路径字段或 Gauge → 下部状态及当前可用键位。
零 IO、零 stdout、零任务调度；测试用 ratatui TestBackend 验证80列/40列/窄屏、CJK、错误态和只读禁用提示。

## 10. 验证与实施顺序

### 自动化验收（未来实现时运行，本轮未运行）

- 路径 parser：Windows盘符/UNC、POSIX空格和引号、本地URI、歧义多文件拒绝、NUL/换行/命令替换不执行；WSL转换限定场景。
- 事件归约：旧upload_id、旧attempt、同名新identity、view变化、连接epoch变化不能插入；完成/取消/断线各种排列。
- 输入隔离：面板的Key/Paste/Mouse不进入PTY；I只发送一次粘贴，不含自动提交CR/LF；只读/撤销/Working/Blocked不可插入。
- 上传：空文件、上限、大于上限、读期间文件变化、源文件读失败、慢网络、背压、错误digest、磁盘满、取消落盘竞态。
- UI：稳定进度、终态可靠送达、resize与高频PTY输出并发、1秒内大量进度不撑爆队列；测量输入/绘制延迟而非只看进度增长。
- 端到端：上传文件的远端大小/hash与原文件一致，随后在目标Agent实际读取；终端显示路径不是图片已被模型读取的证明。

### 真机门槛

必须在 Windows Terminal 原生、Windows Terminal+WSL、macOS Terminal、iTerm2、Ghostty（macOS/Linux）、至少一个 Linux GTK 终端逐项验证：

1. 文件拖入到底产生 Paste 还是 Key，具体引用与编码，是否能在 mouse capture 下传到上传模式。
2. 空格/中文/引号/长路径、多文件、提权窗口、不可读文件；第一版不支持项有明确错误。
3. 直接运行、tmux内运行、SSH远端运行、嵌套SSH；源文件和最终主机不得混淆。
4. 上传时resize/切换/撤销/断线/取消，完成后不自动回车；已有Agent输入不被清空。
5. Claude/Codex及至少一个普通shell真实粘贴行为；只读能力若新增必须验证服务端拒绝。

Linux模拟PTY测试只能验证指定输入字节序列，不能替代 Explorer/Finder/Wayland 的原生拖拽验证。历史Windows paste成功也不等价于本功能拖拽已通过。

建议交付顺序：先做显式上传面板+可读路径/选择器+共享传输；再打通本机TUI远端连接；最后增加已验证终端的拖入格式适配及远端TUI本地助手。GUI原生拖拽不作为TUI首版可用的前提。

## 11. 结论的证据级别

平台矩阵的终端源码条目来自本轮上游主分支核查，不能当作所有已发行版本承诺。crossterm API、XTerm与tmux协议说明是文档事实；上传模式、状态机、事件限流与传输拓扑是为asd提出的设计。真机拖拽、进度性能、Agent图片读取均未在本轮验证。

## 12. 一手资料

- [C1] [crossterm 0.29 Event](https://docs.rs/crossterm/0.29.0/crossterm/event/enum.Event.html)：Event变体及Paste条件。
- [C2] [crossterm event 源码](https://github.com/crossterm-rs/crossterm/blob/94ed7154e35d24a2f643c61760e1eb05b8ca9e63/src/event.rs)：Drag与文件事件的区别；[Windows输入实现](https://github.com/crossterm-rs/crossterm/blob/94ed7154e35d24a2f643c61760e1eb05b8ca9e63/src/event/sys/windows.rs)。
- [X1] [XTerm控制序列](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html)：OSC52、DECSET2004及粘贴边界。
- [T1] [tmux Clipboard](https://github.com/tmux/tmux/wiki/Clipboard)：OSC52、set-clipboard及嵌套行为。
- [T2] [tmux manual](https://man.openbsd.org/tmux)：paste-buffer、bracketed paste、鼠标与passthrough。
- [A1] [Apple Terminal：从其他窗口拖入文件](https://support.apple.com/guide/terminal/drag-items-into-a-terminal-window-trml106/mac)。
- [I1] [iTerm2 Shell Integration](https://iterm2.com/documentation-shell-integration.html)：Option-drag原生上传。
- [R1] [Ratatui Gauge示例](https://ratatui.rs/examples/widgets/gauge/)：Gauge/LineGauge及渲染方式。
- [Z1] [trzsz官方仓库](https://github.com/trzsz/trzsz)：文件传输依赖本地集成与远端程序、tmux支持。
- 对照：[cmux SSH拖拽上传](https://cmux.com/docs/ssh#drag-and-drop)：SCP/ControlMaster的终端原生实现，不等于纯TUI事件支持。

- [W1] [Windows Terminal TermControl.cpp](https://github.com/microsoft/terminal/blob/main/src/cascadia/TerminalControl/TermControl.cpp)：_DragDropHandler、PathTranslationStyle、DragDropDelimiter、PasteText。
- [W2] [Windows Terminal 路径转换发布说明](https://github.com/microsoft/terminal/discussions/18516)；[Microsoft WSL文件系统](https://learn.microsoft.com/en-us/windows/wsl/filesystems)。
- [G1] [Ghostty AppKit SurfaceView](https://github.com/ghostty-org/ghostty/blob/main/macos/Sources/Ghostty/Surface%20View/SurfaceView_AppKit.swift)：performDragOperation、sendText。
- [G2] [Ghostty GTK surface](https://github.com/ghostty-org/ghostty/blob/main/src/apprt/gtk/class/surface.zig)：dtDrop、ShellEscapeWriter、Clipboard.paste。
- [L1] [GNOME Terminal screen源码](https://github.com/GNOME/gnome-terminal/blob/master/src/terminal-screen.cc)：文件/URI处理、g_shell_quote、vte_terminal_paste_text。
