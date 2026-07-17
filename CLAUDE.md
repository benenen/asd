# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目定位

`asd` = GPU 终端客户端 + headless mux daemon，定位 **shpool 而非 tmux**：一个 session 即一个 PTY，不做 pane/window 分屏。规格与里程碑文档在 Obsidian：`idea/spacs/gmux GPU 终端 Spec`、`idea/plans/gmux GPU 终端计划`（文档用 `gmux-*` 命名，本仓库对应为 `asd-*`）。M0 五个模块 asd-proto / asd-vt / asd-daemon / asd-cli / asd-gui 均已落地。

**单二进制分发（有意偏离 spec §2，用户决定）**：只产出**一个** `asd` 可执行文件，CLI + 内嵌 daemon + GUI 全在里面。**bin 是 workspace 根 package `asd`（`Cargo.toml` 既是 `[workspace]` 又是 `[package]`，`src/main.rs`）**；asd-daemon/asd-cli/asd-gui/asd-dioxus 都是 **library crate**，根 bin 用 **feature** 组合：`local`（=asd-cli，带 daemon/portable-pty）+ GUI 二选一——**`dioxus`（=asd-dioxus，webview+ghostty-web，默认）或 `iced`（=asd-gui，wgpu）**，`default=["local","dioxus"]`；两个 GUI feature 同开时 **iced 优先**（默认已含 dioxus，iced 出现即显式要求；`gui` 是兼容别名=dioxus）。**裸 `asd` 开 GUI**，`asd gui [session]` 也开 GUI，`new/attach/list/kill/daemon/restart` 是 CLI 子命令。**Windows/纯客户端 = `--no-default-features --features dioxus`（或 iced）**（无 portable-pty，`cargo tree` 验证 0 个）；**服务器端只装 daemon/CLI 用 `--no-default-features --features local`**（不链 GUI 库——dioxus 版链 libwebkit2gtk，拷到没有 WebKitGTK 的服务器会起不来）。daemon 仍以 `asd daemon` 子命令运行，自愈拉起 = re-exec `current_exe()` + `daemon`。GUI 入口 `asd_gui::run(session)` / `asd_dioxus::run(session)`；CLI 入口 `asd_cli::run(gui: Option<GuiLauncher>)`（GUI 启动器由 bin 注入，asd-cli 不碰任何 GUI 框架）。

## 代码规范

- **代码中的注释一律使用英文**（doc comments、行内注释、Cargo.toml 注释都算）。
- **协议加帧或改帧结构必须 bump `asd-proto` 的 `PROTO_VERSION`**（双端同升，不做多版本兼容运行），并在 `crates/asd-proto/tests/codec.rs` 的 `all_frames()` 里补/改 roundtrip 用例。当前 `PROTO_VERSION = 2`（v1 加了 scrollback 的 `FetchHistory`/`History` 与 `Refresh`；v2 给 `SessionInfo` 加了 `command` 字段——daemon 报**实时前台进程**：`tcgetpgrp(pty master fd)` 取前台进程组 → **Linux** 读 `/proc/<pgid>/cmdline`（argv0 basename、剥 `sh -c` 包装前缀，有 args）；**macOS** 用 `sysctl(KERN_PROCARGS2)` 拿完整 argv（`[argc][exec_path\0][pad][argv…]`，`parse_procargs2` 解析——argv0 basename、剥 `sh -c`、与 Linux 同形），失败回退 libproc `libc::proc_pidpath`（只可执行名）；其他平台回退。都最终回退到 spawn 命令/默认 shell。交互式 shell 里跑的作业靠 job control 各自成组，故能显 `vim`/`npm run dev`/`top`。CLI `list` 与 GUI 侧栏显示。master fd 存在 `SessionMeta.pty_master_fd`（borrow 不 dup，否则 slave 收不到 hangup）。`proc_command` 按 `cfg(target_os)` 三分支；macOS 分支沙箱(Linux)编译不到——`parse_procargs2` 用 `cfg(any(macos,test))` 在 Linux test 里跑纯解析单测，整段 macOS FFI 靠隔离 crate `cargo check --target x86_64-apple-darwin` 验证签名。
- crate 依赖边界是硬契约（spec §3），违反即架构回归：

| crate | 职责 | 禁止依赖 |
|---|---|---|
| asd-proto | 帧枚举、postcard 编解码、framed reader/writer、路径契约 | tokio 之外的运行时、任何业务 crate |
| asd-vt | `VtBackend` trait + libghostty-vt 实现（逃生门边界） | iced/wgpu、portable-pty、asd-proto |
| asd-daemon（lib） | session 管理、UDS 服务 | iced/wgpu（含传递依赖） |
| asd-cli（**lib**，`pub fn run`） | 调试客户端、`attach --stdio` 代理、内嵌 daemon（`asd daemon`）；被根 bin 的 `local` feature 组合 | iced/wgpu（GUI 启动器由 bin 注入，不直接依赖 iced） |
| asd-gui（**lib**，`pub fn run`，iced/wgpu） | 渲染、输入、拨号、SSH remote；被根 bin 的 `iced` feature 组合 | **portable-pty 及一切 PTY/进程管理**（Windows 客户端可行性的根基）；可依赖 asd-vt/asd-proto。**SSH 走纯 Rust `russh`（网络客户端，不 spawn 进程/不用 ssh.exe，不违反边界）**，不是 spawn `ssh` 子进程 |
| asd-dioxus（**lib**，`pub fn run`，Dioxus Desktop+ghostty-web） | 同 asd-gui 的功能面（host 侧栏/SSH remote/设置），渲染交给 webview 里的 ghostty-web（吃原始 PTY 字节，无 asd-vt）；被根 bin 的 `dioxus` feature（默认）组合 | 与 asd-gui 同边界：**无 portable-pty/进程管理**，SSH 走 russh。JS 依赖由 npm+esbuild 打包（build.rs 驱动，见 crate README），产物 include_str! 内嵌保单二进制 |
| asd（**根 package**，唯一 bin `asd`） | 组合上面的 lib 成单一可执行文件（feature `local`/`dioxus`/`iced`）；除组合外无逻辑 | 直接依赖 GUI 框架或 portable-pty（应经由 feature 拉对应 lib，保持边界纯净） |

## 常用命令

```bash
cargo build --workspace              # 首次构建会用 Zig 编译 libghostty-vt-sys（vendored），需 PATH 里有 zig 0.15.x；asd-dioxus 的 build.rs 会跑 npm install + esbuild（需 node/npm）
cargo test --workspace               # e2e 测试会真实拉起 asd-daemon 进程（独立 socket，互不干扰）
cargo test -p asd-vt                 # 单 crate
cargo test --test e2e sigterm        # 单个 e2e（e2e 在根 package tests/，非 asd-cli）
cargo clippy --workspace --all-targets
cargo fmt --all
```

手工冒烟：`cargo run -- attach -A demo`（根 bin `asd`；自动拉起 daemon + 创建 session；detach 键 Ctrl-\）。`cargo run` 裸跑 = 开 GUI。`cargo build --workspace` 编所有 crate；`cargo test --workspace` 跑所有测试（`cargo test` 不带 `--workspace` 只测根 package）。

**`asd attach` 是 VT 渲染客户端（2026-07-14，对标 boo 的 `boo ui`）**，不是哑管道：客户端自带一份 `GhosttyVt`（asd-cli 因此依赖 asd-vt），把 daemon 的 Snapshot+Output 喂进去、自己渲染（`render.rs`：RenderSnapshot→ANSI）。本地 VT 模型让 live 视图同时有：① 交替屏（`1049h`）detach 恢复原屏；② 滚回历史（滚自己的视口 `set_scroll`，**滚轮** 或键盘 `Shift+PageUp/PageDown/Home/End`；**客户端本地、不影响其他 attach 的人**）；③ 拖选复制。

**鼠标：抢鼠标 + 自绘选区，vim 时镜像转发（2026-07-14 定稿，对标 boo ui）**。关键教训：**用 `1002`（button-event，报拖动）不是 `1000`（只报按下松开，拖选取不到文本）**——boo 就是 `1002h+1006h`。客户端基线在提示符处开 `1002+1006`（`BASE_MOUSE`），于是滚轮被截获→本地滚，拖拽→自绘反显选区、松开经 `selection_text_screen` 取文本 **OSC52** 写系统剪贴板（和 boo 一样，不靠终端原生选区）。**选区锚定屏幕空间（不是视口行）**：`Sel{anchor,head}` 存的是绝对 `(x, screen_row)`（0=最老的 scrollback 行，和 `history_len`/`fetch_history` 同坐标系），滚动只改 `scroll`、`Sel::viewport(scrollback,scroll,…)` 每帧投影回视口行并裁剪到可见区——于是**高亮跟着文字走、滚轮滚动时不再"选区飘在固定屏幕行上盖住别的字"**（对标 boo 的 content-pin 选区）。复制走 `selection_text_screen`（screen-space、与 scroll 无关，滚出视口的部分也能整段拿到）。想用宿主原生选区就 **Shift+拖拽**（终端 bypass）。当 session 自己要鼠标（vim/htop，`is_mouse_tracking`）时，`sync_host_mouse` 改镜像 session 的**确切模式**（`mouse_modes()` 读 DEC `9/1000/1002/1003`+`1005/1006/1015/1016`，`mouse_mode_delta` 只发增量），鼠标事件 `is_mouse_report` 在 live 视图原样透传给 session（宿主镜像了它的编码，坐标 1:1，无需转换）。多人 attach：每客户端各自 vt+host_mouse，天然隔离。渲染要点：`render_frame` 只在 `cursor.position` 有值时才 `?25h`（滚出视口 position=None 时绝不显示，否则右下角留光标残影）。**退出清理**：`ScreenGuard` drop 除了 `?1049l` 还要 `?25h`（`?25` 是全局状态，不随交替屏恢复，否则回到 shell 光标不见）+ `0m`（复位 SGR）+ `0 q`（光标形状）。**退出方式**：detach 后 `attach::run` 末尾直接 `std::process::exit(0)`——`tokio::io::stdin()` 的阻塞读线程无法取消，正常返回会让 runtime 关闭卡在那个 read 上、**直到用户按回车**（tokio 文档明说的坑）；终端已恢复、消息已 flush，硬退干净。

多人 attach 语义：滚动/选区是各客户端本地的、互不影响；只有键盘输入（同一个 shell）和 pty 尺寸（最后 resize 者胜）是共享的。协议里的 `FetchHistory`/`History`/`Refresh`（v1）现在渲染客户端不再用（改用本地 scrollback），但保留给别的客户端/测试，e2e 仍直接测 daemon 的这几帧。`asd new` 也会自动拉起 daemon（tmux 语义）；`list`/`kill`/裸 `attach` 则要求 daemon 已在跑。`asd daemon` 可前台手动跑 daemon。`--socket`/`$ASD_SOCKET` 可把 socket 指到任意路径做隔离实验。注意 daemon 自带 tokio runtime，`main()` 必须在进入 `#[tokio::main]` 之前分发 `Cmd::Daemon`（不能嵌套 runtime）。

## 架构（跨文件才能看懂的部分）

**线程模型（spec §5）**：网络侧全 tokio；每个 session 两个 std 线程——pty 读线程（阻塞 read → channel）+ session 线程（独占 `GhosttyVt`，它是 `!Send`，编译期保证不出线程）。pty 输出、Input、Resize、Attach 全部经 `std::sync::mpsc` 进 session 线程串行处理，这就是顺序保证的来源：attach 时 Snapshot 帧先于任何后续 Output 入同一条出站队列。

**一条连接的数据通路**（`asd-daemon/src/conn.rs`）：入站与出站拆成两个任务，因为 `FrameReader::read_frame` 不是取消安全的，不能放进 `tokio::select!`。所有写 socket 的帧（控制面应答 + session 广播）都汇入同一条 unbounded out-queue 由写任务串行写出。流控（M0 版）：`ClientSink` 只对数据面帧（Snapshot/Output）记字节配额，上限 4 MiB（`session.rs::OUTPUT_QUEUE_CAP`），写任务写出后归还；超限即向 out-queue 发 `Close` 断连该客户端，session 不受影响。

**session 生命周期**：连接断开即 detach（无显式状态）；pty EOF 是 session 终点——收尸（`child.wait()`）、从 registry 摘除、给所有 attached 客户端发 `Error{SESSION_EXITED}`。`Kill` = SIGHUP + 2s 后 SIGKILL 兜底；daemon SIGTERM = 对所有 session SIGHUP → 等 2s → SIGKILL 残余 → 删 socket。session 不持久化，daemon 重启即全没（v1 明确如此）。

**协议（spec §4）**：`u32 LE 长度前缀 + postcard`，单帧上限 4 MiB（超限=协议错误断连，编码/解码两侧都拦）。握手客户端先发 `Hello`，版本不相等 daemon 回 `Error{code=1}` 后断连。`Frame::Kill` 没有 ack 帧——CLI 用「Kill 后紧跟 ListSessions」借 daemon 的按序处理来确认（见 `asd-cli/src/main.rs`）。

**asd-vt 是逃生门边界**：libghostty-vt 0.2.x API 未稳定，所有直接调用收敛在 `crates/asd-vt/src/ghostty.rs`；daemon/GUI 只面向 `VtBackend` trait 和全 `Send` 的 `RenderSnapshot` 纯数据。两个跨层关键点：① `feed()` 期间终端对 DA/DSR 查询的应答积在 `take_pty_responses()`，session 线程必须取出回写 pty，否则 vim 类程序探测挂起；② `snapshot_vt()` 末尾手工补一个 CUP——上游 Formatter 在光标恢复序列之后才发 tabstops/滚动区序列（会挪光标），快照保真性测试（`asd-vt/tests/vt.rs`）钉死这个行为。

**路径契约（spec §2）**：collected in `asd-proto::paths` —— socket 解析优先级 `$ASD_SOCKET` > `$XDG_RUNTIME_DIR/asd.sock` > `/tmp/asd-$UID/asd.sock`（0700），daemon 与所有客户端共用这一份实现；数据目录 `~/.local/share/asd/`（daemon 日志 `daemon.log` 在此，由 `asd attach -A` 拉起时重定向）。

**asd-gui（spec §7，iced 0.14 + wgpu）**：`render.rs` 的 `canvas::Program` 把 `RenderSnapshot` 画到 iced canvas（等宽 `Font::MONOSPACE`，bg 矩形 + fg 字形 + 反显块光标；cell 尺寸按字号估算，ASCII 正确，CJK/样式保真是 M1）。`encode_key` 用渲染终端自己的模式态，DECCKM 等天然同步。**GUI 靠人肉验收**（沙箱/CI 无显示器，只编译 + 跑纯函数单测：`model.rs` 分组/时长/host 解析、`key.rs` 键映射、`render.rs` 网格换算）。

**M2 两栏重构（2026-07-14，对标 boo `boo ui`）**：单窗口从「一条 UDS 连接」升级为 **host 分组的 session 侧栏 + 终端面板 + 底部状态栏**。架构分层：① **supervisor**（跑在 iced subscription 的 `iced::stream::channel` 里，`main.rs::supervisor`）：每个 host 起一个 std 线程 actor，路由 `AppCmd`→对应 host 的 `HostCmd`，把所有 host 的 `UiEvent` 转 Message 汇给 app；app 侧只持纯数据 `model::Model`（Send）。② **host actor**（`conn.rs::run_host`，每 host 一线程 + current-thread runtime + `!Send` `GhosttyVt`）：握手后按 `LIST_INTERVAL` 轮询 `ListSessions`→喂侧栏；**只有当前查看的 session 那台**发 `Attach` 并流式 Output→产 `RenderSnapshot`；切换 session = 同一连接 `Detach`+`Attach`（`attaching` 标志丢弃切换间隙的旧 Output）。传输用 `BoxRead/BoxWrite`（`Box<dyn AsyncRead/Write>`）——本地 `UnixStream`、remote russh `ChannelStream` 共用一条 `drive` loop。③ **SSH remote**（`ssh.rs`，russh 0.62）：`client::connect((host,port),…)`→`check_server_key` 查 `~/.ssh/known_hosts`（未知/变更即拒）→key-file 认证（`~/.ssh/id_*` 无口令；**ssh-agent/口令/2FA 是后续**）→`channel.exec("asd attach --stdio")`→`into_stream()`。remote 侧靠已有的 `--stdio` 透传代理，于是 GUI 对 remote daemon 说的还是同一套协议。**重连 = bump `Seed.generation`**（Hash 变→重启 supervisor→app 在新 `Message::Supervisor` 里重放 AddRemote+SetActive）；**GUI 不能自启 daemon（进程管理边界），本地 daemon 没跑就点侧栏底部「daemon down · click to reconnect」**。侧栏配色见 `theme.rs`：**host origin 用双色 rail 编码——本地 amber、SSH remote cyan**。`SessionInfo` 无命令字段，故侧栏显 uptime+attached 数而非命令行（要显命令得 bump proto 给 `SessionInfo` 加字段）。
