# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目定位

`asd` = GPU 终端客户端 + headless mux daemon，定位 **shpool 而非 tmux**：一个 session 即一个 PTY，不做 pane/window 分屏。规格与里程碑文档在 Obsidian：`idea/spacs/gmux GPU 终端 Spec`、`idea/plans/gmux GPU 终端计划`（文档用 `gmux-*` 命名，本仓库对应为 `asd-*`）。当前完成 M0 的 asd-proto / asd-vt / asd-daemon / asd-cli 四个模块；`asd-gui`（iced/wgpu，M0 第 6 步）尚未创建。

**单二进制分发（有意偏离 spec §2 的双二进制契约，用户决定）**：只产出一个 `asd` 可执行文件，daemon 以 `asd daemon` 子命令运行；asd-daemon 是 library crate，被 asd-cli 内嵌。自愈拉起 = re-exec `current_exe()` + `daemon` 参数。

## 代码规范

- **代码中的注释一律使用英文**（doc comments、行内注释、Cargo.toml 注释都算）。
- **协议加帧必须 bump `asd-proto` 的 `PROTO_VERSION`**（双端同升，不做多版本兼容运行），并在 `crates/asd-proto/tests/codec.rs` 的 `all_frames()` 里补 roundtrip 用例。当前 `PROTO_VERSION = 1`（v1 加了 scrollback 的 `FetchHistory`/`History` 与 `Refresh`）。
- crate 依赖边界是硬契约（spec §3），违反即架构回归：

| crate | 职责 | 禁止依赖 |
|---|---|---|
| asd-proto | 帧枚举、postcard 编解码、framed reader/writer、路径契约 | tokio 之外的运行时、任何业务 crate |
| asd-vt | `VtBackend` trait + libghostty-vt 实现（逃生门边界） | iced/wgpu、portable-pty、asd-proto |
| asd-daemon（lib） | session 管理、UDS 服务 | iced/wgpu（含传递依赖） |
| asd-cli（唯一 bin `asd`） | 调试客户端、`attach --stdio` 代理、内嵌 daemon（`asd daemon`） | iced/wgpu |
| asd-gui（未建） | 渲染、输入、拨号 | **portable-pty 及一切 PTY/进程管理**（Windows 客户端可行性的根基） |

## 常用命令

```bash
cargo build --workspace              # 首次构建会用 Zig 编译 libghostty-vt-sys（vendored），需 PATH 里有 zig 0.15.x
cargo test --workspace               # e2e 测试会真实拉起 asd-daemon 进程（独立 socket，互不干扰）
cargo test -p asd-vt                 # 单 crate
cargo test -p asd-cli --test e2e sigterm   # 单个测试（按名字过滤）
cargo clippy --workspace --all-targets
cargo fmt --all
```

手工冒烟：`cargo run -p asd-cli -- attach -A demo`（自动拉起 daemon + 创建 session；detach 键 Ctrl-\）。

**`asd attach` 是 VT 渲染客户端（2026-07-14，对标 boo 的 `boo ui`）**，不是哑管道：客户端自带一份 `GhosttyVt`（asd-cli 因此依赖 asd-vt），把 daemon 的 Snapshot+Output 喂进去、自己渲染（`render.rs`：RenderSnapshot→ANSI）。这份本地 VT 模型让 **live 视图同时具备三件事**——① 交替屏 detach 恢复原屏；② 滚轮直接往回滚历史（滚自己的视口 `set_scroll`，**客户端本地、不影响其他 attach 的人**）；③ 鼠标拖选高亮 + 松开经 `selection_text` 取文本、**OSC52** 写系统剪贴板（不靠终端原生选区，所以鼠标上报可以全程开着截获滚轮）。当 session 程序自己在 alt 屏或要鼠标（vim/htop，客户端用 `is_alt_screen`/`is_mouse_tracking` 精确判断）时，滚轮和点击转发给它。

多人 attach 语义：滚动/选区是各客户端本地的、互不影响；只有键盘输入（同一个 shell）和 pty 尺寸（最后 resize 者胜）是共享的。协议里的 `FetchHistory`/`History`/`Refresh`（v1）现在渲染客户端不再用（改用本地 scrollback），但保留给别的客户端/测试，e2e 仍直接测 daemon 的这几帧。`asd new` 也会自动拉起 daemon（tmux 语义）；`list`/`kill`/裸 `attach` 则要求 daemon 已在跑。`asd daemon` 可前台手动跑 daemon。`--socket`/`$ASD_SOCKET` 可把 socket 指到任意路径做隔离实验。注意 daemon 自带 tokio runtime，`main()` 必须在进入 `#[tokio::main]` 之前分发 `Cmd::Daemon`（不能嵌套 runtime）。

## 架构（跨文件才能看懂的部分）

**线程模型（spec §5）**：网络侧全 tokio；每个 session 两个 std 线程——pty 读线程（阻塞 read → channel）+ session 线程（独占 `GhosttyVt`，它是 `!Send`，编译期保证不出线程）。pty 输出、Input、Resize、Attach 全部经 `std::sync::mpsc` 进 session 线程串行处理，这就是顺序保证的来源：attach 时 Snapshot 帧先于任何后续 Output 入同一条出站队列。

**一条连接的数据通路**（`asd-daemon/src/conn.rs`）：入站与出站拆成两个任务，因为 `FrameReader::read_frame` 不是取消安全的，不能放进 `tokio::select!`。所有写 socket 的帧（控制面应答 + session 广播）都汇入同一条 unbounded out-queue 由写任务串行写出。流控（M0 版）：`ClientSink` 只对数据面帧（Snapshot/Output）记字节配额，上限 4 MiB（`session.rs::OUTPUT_QUEUE_CAP`），写任务写出后归还；超限即向 out-queue 发 `Close` 断连该客户端，session 不受影响。

**session 生命周期**：连接断开即 detach（无显式状态）；pty EOF 是 session 终点——收尸（`child.wait()`）、从 registry 摘除、给所有 attached 客户端发 `Error{SESSION_EXITED}`。`Kill` = SIGHUP + 2s 后 SIGKILL 兜底；daemon SIGTERM = 对所有 session SIGHUP → 等 2s → SIGKILL 残余 → 删 socket。session 不持久化，daemon 重启即全没（v1 明确如此）。

**协议（spec §4）**：`u32 LE 长度前缀 + postcard`，单帧上限 4 MiB（超限=协议错误断连，编码/解码两侧都拦）。握手客户端先发 `Hello`，版本不相等 daemon 回 `Error{code=1}` 后断连。`Frame::Kill` 没有 ack 帧——CLI 用「Kill 后紧跟 ListSessions」借 daemon 的按序处理来确认（见 `asd-cli/src/main.rs`）。

**asd-vt 是逃生门边界**：libghostty-vt 0.2.x API 未稳定，所有直接调用收敛在 `crates/asd-vt/src/ghostty.rs`；daemon/GUI 只面向 `VtBackend` trait 和全 `Send` 的 `RenderSnapshot` 纯数据。两个跨层关键点：① `feed()` 期间终端对 DA/DSR 查询的应答积在 `take_pty_responses()`，session 线程必须取出回写 pty，否则 vim 类程序探测挂起；② `snapshot_vt()` 末尾手工补一个 CUP——上游 Formatter 在光标恢复序列之后才发 tabstops/滚动区序列（会挪光标），快照保真性测试（`asd-vt/tests/vt.rs`）钉死这个行为。

**路径契约（spec §2）**：collected in `asd-proto::paths` —— socket 解析优先级 `$ASD_SOCKET` > `$XDG_RUNTIME_DIR/asd.sock` > `/tmp/asd-$UID/asd.sock`（0700），daemon 与所有客户端共用这一份实现；数据目录 `~/.local/share/asd/`（daemon 日志 `daemon.log` 在此，由 `asd attach -A` 拉起时重定向）。
