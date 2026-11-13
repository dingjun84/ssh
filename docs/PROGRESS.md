# SSH 工作台 — 进度交接

> 写给下一个 session。读完这份就能接着干，不用重新摸一遍坑。
> 最后更新：2026-11-13

## 一句话现状

**真实 SSH 链路已经跑通**：能连 `root@43.139.142.161`，登录、执行命令、中文/颜色/alt-screen 渲染、窗口尺寸同步全部验证过。
**还没有会话库**——侧栏是硬编码的两条，SFTP 也没做。下一步是 SQLite + Keychain。

## 技术栈（已锁定，不要换）

| 层 | 选型 | 备注 |
|---|---|---|
| GUI | iced 0.14（features: canvas, tokio, advanced） | 纯 Rust，零 Node |
| 终端组件 | `crates/iced_term`（vendored，从 0.8.0 改） | 见下面"必须 vendored 的原因" |
| VT 内核 | alacritty_terminal 0.25.1（由 iced_term 传递依赖） | 不要单独加，版本会冲突 |
| SSH | russh 0.65.1 | 纯 Rust + tokio |
| 异步 | tokio（full） | 自建 multi_thread runtime，存在 `RUNTIME` OnceLock |

编译：`cargo build`；运行：`./target/debug/ssh-client`。

## 已完成

### 1. 终端渲染层（`crates/iced_term`）

上游 0.8.0 有三处必须改，否则中文不可用：

- 跳过 `WIDE_CHAR_SPACER` 占位格（否则汉字重复绘制）
- 宽字符绘制锚点从"单格中心"改成"跨双格中心"（原实现每个汉字溢出半格，相邻互覆盖，表现为"每隔一个字丢一个"）
- CJK 显式走 PingFang SC，拉丁走等宽字体（`view.rs` 里按码位分流）。**不能依赖 cosmic-text 的隐式字体回退**——`Font::MONOSPACE` 缺 CJK 字形时它静默丢字形，不报错

### 2. Transport 抽象（`crates/iced_term/src/backend.rs`）

上游把 backend 硬编码成"启动本地 PTY 进程"，没暴露 trait，所以无法接 russh。已改：

```rust
pub trait Transport: Send + Sync {
    fn write(&self, data: &[u8]);
    fn resize(&self, cols: u16, rows: u16);
}
enum Sink { Local(Notifier), Remote(Arc<dyn Transport>) }
```

`Terminal::new_remote(id, settings, transport)` 是新增构造器，`Terminal::new` 保持本地 PTY 行为。
入站：`Terminal::feed(&[u8])` 直接把远端字节喂进 VT 解析器（不经 IPC）。

### 3. SSH 传输层（`src/ssh.rs`）

- 认证：密码、私钥（含 passphrase）。ssh-agent / keyboard-interactive 未做
- RSA 密钥必须走 `session.best_supported_rsa_hash()` 协商，**传 `None` 会退回 `ssh-rsa`(SHA-1)，现代 sshd 直接拒绝**（这就是"公钥认证被拒绝"的根因）
- PTY + shell，请求 `xterm-256color`
- 保活：`keepalive_interval: 30s` / `keepalive_max: 4`（协议层 `keepalive@openssh.com`，不污染终端输出）
- 读写放在**同一个 `select!` 循环**里。曾经用 `tokio::spawn` 单开写任务，偶发永不调度、写入静默丢失，已废弃该写法

### 4. 界面骨架（`src/main.rs`）

Sublime2 风格的 Monokai 配色：左侧会话树 + 顶部标签栏 + 主区终端 + 底部状态栏。
状态栏显示连接状态、keepalive 参数。窗口用 `Level::AlwaysOnTop` + 固定位置（方便截图验证）。

## 未完成（按优先级）

| 优先级 | 事项 | 说明 |
|---|---|---|
| **P0** | **会话库（SQLite + Keychain）** | 侧栏目前硬编码。需要 sessions 表 + 分组 + 密码进 Keychain |
| **P0** | **主机指纹校验** | 现在 `check_server_key` 直接 `Ok(true)`，**接受任意主机密钥，等于没有中间人防护**。至少要 TOFU：首次存指纹，变更时告警 |
| P0 | 断线重连（指数退避） | 现在断了就是断了，状态栏变"已断开" |
| P0 | 复制粘贴 | 未验证 iced_term 是否处理 ⌘C/⌘V，很可能是缺的 |
| P1 | SFTP 面板 | 复用同一个连接开 sftp subsystem，双栏 + 拖拽 + 传输队列 |
| P1 | `~/.ssh/config` 导入 | 解析 Host/HostName/User/Port/IdentityFile/ProxyJump |
| P1 | 密码认证 UI | `Auth::Password` 已实现但没有输入入口 |
| P1 | ProxyJump / 端口转发 | 生产环境必需 |
| P1 | 快捷命令 / 批量广播 / 分屏 | 效率件 |
| P2 | 状态栏双指标 | 协议层 RTT + 终端最后输出时间（keepalive 通不代表 shell 活着） |
| P2 | 合盖唤醒后立即探测一轮 | 系统唤醒事件监听 |
| P2 | 滚动缓冲 / 终端内搜索 | alacritty 有 scrollback，但要确认 iced_term 的绘制是否支持滚动偏移 |
| P3 | 打包 .app + ad-hoc 签名 | 本地自用，不做公证 |

## 已知坑（别再踩一遍）

1. **`$(cat file)` 会吃掉末尾换行**。用 env var 传测试命令时务必 `"$(cat f)"$'\n'`，否则命令被输入但永远不回车。这个坑排查了半小时。
2. **russh RSA + `None` hash** → 服务端返回裸拒绝，没有任何提示。见上。
3. **`tokio::spawn` 开的写任务可能不跑**。表现：读正常、写入 `data_bytes` 返回 Ok 但远端收不到。改用单任务 `select!`。
4. **中文每隔一个字丢一个** → 两个独立原因叠加：宽字符锚点错 + cosmic-text 静默丢字形。两个都得修。
5. **`boot()` 在 tokio 任务里执行**，它不返回就不会有窗口——app 进程活着但一个 window 都没有，别误判成渲染问题。用 `swift` + `CGWindowListCopyWindowInfo` 查窗口是否存在最可靠。
6. Apple Events 未授权（`-1743`），`osascript` 无法前置/操作窗口。截图用 `screencapture -x -o -l <windowID>`，windowID 从上面的 Swift 脚本拿。
7. iced_term 的 `TerminalView` 是逐格调用 `fill_text`，功能键/剪贴板等交互能力要逐个确认，别假设它有。

## 怎么验证（无键盘也能跑）

```bash
# 1. 起一个真实会话并截图
SSH_AUTOCONNECT=1 ./target/debug/ssh-client &
swift tools/winlist.swift | grep "SSH 工作台"      # 拿 windowID
screencapture -x -o -l <windowID> /tmp/shot.png

# 2. 连上后自动在远端执行一段命令（验证写入方向）
SSH_PROBE="stty size
" SSH_AUTOCONNECT=1 ./target/debug/ssh-client &
# 期望终端里出现 46 154，与本地 resize 日志一致

# 3. 单独验证 russh（排除终端层干扰）
cargo build --bin rsstest && ./target/debug/rsstest
WRITE_MODE=split ./target/debug/rsstest
```

`src/bin/rsstest.rs` 是刻意保留的诊断工具：最小 russh 连接 + 两种写入策略，用来区分"库的问题"和"我的集成问题"。别删。

`tools/winlist.swift` 把屏幕上的窗口列表打出来（id / layer / 是否在屏）。查“窗口到底存不存在”时用它。

## 验证记录（2026-11-13）

真实主机 `root@43.139.142.161`（OpenCloudOS 9，kernel 6.6.119）：

| 项 | 结果 |
|---|---|
| 公钥认证 | 通过（`best_supported_rsa_hash` 协商到 Sha512） |
| 登录 banner + 提示符 | 通过 |
| 命令执行 + 回显 | 通过 |
| 中文表格对齐 | 通过（`主机名/状态/正常/严重告警` 全部显示） |
| truecolor / 256 色 / 16 色 | 通过 |
| `ls --color` | 通过 |
| alt-screen 往返（`tput smcup/rmcup`） | 通过 |
| 窗口尺寸同步 | 通过（本地 154x46 → 远端 `stty size` 返回 `46 154`） |
| emoji | 通过 |

## 下一步建议

先做**会话库 + 主机指纹**，这两件是"能不能日常用"的门槛；SFTP 放之后再上。
做会话库时顺手把侧栏从硬编码切成数据驱动，`open_ssh` 的入参就该换成 `&Session`。
