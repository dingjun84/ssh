# SSH 工作台 — 进度交接

> 写给下一个 session。读完这份就能接着干，不用重新摸一遍坑。
> 最后更新：2026-11-13

## 本轮接续 2：SFTP 面板（2026-11-13）

- 复用现有 SSH 会话：`ssh::run` 认证后把 `Arc<client::Handle>` 放进 `SessionSlot`，`sftp.rs` 拿它开第二个 channel，`request_subsystem(true, "sftp")` + `SftpSession::new(channel.into_stream())`。没有第二份认证、没有第二条 TCP。
- 面板：标签栏 `SFTP` 开关 → 右侧 290px。列目录（目录优先、大小右对齐）、点目录进入、`↑` 返回、`⟳` 刷新、点文件下载到 `~/Downloads`（重名自动 ` (1)`）、把文件拖进窗口上传到当前目录。
- 重连：`russh::client::Handle` **不是 `Clone`**，共享靠 `Arc`；worker 每条命令都核对 `SessionSlot` 的 generation，对不上就重建 SFTP 会话。`main.rs` 在重连时直接重建 worker（`attach_sftp`）并让 `sftp_generation` 递增，否则 iced 订阅按 hash 复用死通道。
- 传输：64 KiB 分块，进度事件 150 ms 节流；`Done` 之后自动重列目录。
- 依赖：`russh-sftp` 3.0.1。它把 russh 只列为 dev-dependency，因此不会锁死 russh 0.65。之前那次 `rsproxy.cn` DNS 失败是瞬时的，本次索引可用。
- **未真机验证**：沙箱无外网（`Network is unreachable`）。`SSH_SFTP_PROBE=1 SSH_AUTOCONNECT=1` 可让面板在连上后自动打开并列家目录。
- 顺带修测试竞态：`store` / `hostkey` 的临时目录从时间戳改为「pid + 原子计数器」。macOS 时钟精度不足以区分并发测试，撞名后互相删库，`update_moves_group_and_switches_auth` 会偶发失败。

## 本轮接续：剪贴板回归修复（2026-11-13）

- ⌘C/⌘V 使用按键事件自带的修饰键，避免焦点切换后缓存过期；Ctrl+C 保持发送 `0x03`，未绑定的 Command 组合不输入字符。
- 复制调用 alacritty 的选区文本接口，修复多行没有换行、中文宽字符占位空格混入的问题；空选区不覆盖剪贴板。
- 粘贴支持括号粘贴模式，内容中的 ESC 被移除；普通模式 LF/CRLF 转 CR。空剪贴板也消费快捷键，不向远端发送字节。
- 新增 Cargo workspace 和 6 项针对真实键盘处理器/VT 选区的回归测试；`cargo test --workspace --offline` 共 37 项通过，`cargo build --offline` 通过。
- 真机快捷键验证仍待完成：桌面工具拒绝访问 Terminal 应用，未标记 P0 完成。操作清单见 `docs/HANDOFF.md` 文首。
- SFTP 官方接入方式已核对，需复用当前 SSH session 开 subsystem；依赖下载因 `rsproxy.cn` DNS 失败受阻，已撤回依赖改动，尚未实现文件面板。

## 一句话现状

**真实 SSH 链路已经跑通**：能连 `root@43.139.142.161`，登录、执行命令、中文/颜色/alt-screen 渲染、窗口尺寸同步全部验证过。
**会话库、主机指纹、断线自动重连都到位了**：侧栏数据驱动（SQLite），密码/口令进钥匙串，指纹走 `~/.ssh/known_hosts` TOFU，
掉线后按 1/2/4/8/16/30s 退避重连（最多 8 次，之后状态栏给手动重连按钮）。
下一步是 **SFTP 面板** 和 **编辑已有会话**。

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
重连时用 `Terminal::set_transport(arc)` 换掉底层管道，VT 状态和 scrollback 都保留。

另外修了两处上游问题，否则日常用会崩：

- `terminal_subscription_stream` 在事件通道关闭时 `panic!`。关标签页（`Terminal` 被 drop）就会触发，
  整个 UI 卡死。改成 `break` 正常结束流
- `Subscription::run_with` **按 hash 缓存**，`TerminalSubscriptionData` 只 hash `id`。同一 id 换新
  `Terminal` 会复用旧订阅（听死通道），所以重连**不要重建 Terminal**，改用 `set_transport`

### 3. SSH 传输层（`src/ssh.rs`）

- 认证：密码、私钥（含 passphrase）。ssh-agent / keyboard-interactive 未做
- RSA 密钥必须走 `session.best_supported_rsa_hash()` 协商，**传 `None` 会退回 `ssh-rsa`(SHA-1)，现代 sshd 直接拒绝**（这就是"公钥认证被拒绝"的根因）
- PTY + shell，请求 `xterm-256color`
- 保活：`keepalive_interval: 30s` / `keepalive_max: 4`（协议层 `keepalive@openssh.com`，不污染终端输出）
- 读写放在**同一个 `select!` 循环**里。曾经用 `tokio::spawn` 单开写任务，偶发永不调度、写入静默丢失，已废弃该写法

### 4. 界面骨架（`src/main.rs`）

Sublime2 风格的 Monokai 配色：左侧会话树 + 顶部标签栏 + 主区终端 + 底部状态栏。
状态栏显示连接状态、主机指纹、keepalive 参数。窗口用 `Level::AlwaysOnTop` + 固定位置（方便截图验证）。

### 5. 会话库（`src/store.rs` + `src/secrets.rs`）

- SQLite 落在 `~/Library/Application Support/ssh-client/sessions.db`，WAL 模式，外键打开
- 表结构：`groups(id,name,sort_order)` + `sessions(id,group_id,label,host,port,user,auth_kind,key_path,sort_order,connect_count,last_used_at)`。**密码不落库**，`sessions` 只记去哪取
- 密码/私钥口令进 macOS 登录钥匙串：`service=ssh-client`，`account=session:<id>:password|passphrase`（`security-framework` 的 `passwords` 模块）
- 首次启动自动播种一条开发主机（`43.139.142.161` / root / `~/.ssh/id_rsa`）
- 侧栏改成数据驱动：分组 → 会话，点开自动去重（同会话只开一个标签）。右侧抽屉是"添加会话"表单，`×` 走二次确认删除并清理钥匙串条目
- 行尾 `✎` 打开同一个抽屉做**编辑**：字段预填，密码/口令栏永远空着，留空即"不改动钥匙串里的值"；保存后已开标签页的标题跟着改
- 库打不开时退化为内存库并在侧栏顶部给出警告，不让整个 app 起不来

### 6. 主机指纹校验（`src/hostkey.rs`）

- 直接复用 `~/.ssh/known_hosts` 当唯一真相源，不另建私有存储：这里学到的记录命令行 `ssh` 也认，反之亦然
- TOFU：首次连接 pin 住指纹（`learn_known_hosts_path`），之后每次比对；**变更即拒绝连接**，错误信息带上"服务器出示 X / known_hosts 第 N 行是 Y"和处置建议
- 非 22 端口按 `[host]:port` 命名空间隔离，与 openssh 一致
- 连接成功后在状态栏显示 `SHA256:...` + "首次连接，已记录"/"与 known_hosts 一致"

### 7. 断线重连（`src/main.rs`）

- 退避：`1, 2, 4, 8, 16, 30, 30, 30`（`backoff()` 封顶 30s），最多 `MAX_ATTEMPTS = 8` 次
- 触发点是 `SshEvent::Closed`；本地 shell（`session_id == None`）不重连
- 连上后 `connect_attempts` 归零，下次掉线又从 1s 开始
- 次数用尽后状态改为"失败"，状态栏出现 `↻ 重连` 按钮（手动重连会把 attempt 计数清零）
- 重连时在终端里留一行 `—— 重连 <label> ——` 作分隔，旧输出不清
- 定时器用 `Task::perform(tokio::time::sleep)`，没有额外线程
- **`SshStream` 的 hash 必须带上 `generation`**：`Subscription::run_with` 按 hash 缓存，重连换了
  receiver 但 hash 不变的话，UI 会一直听那条已关闭的旧通道（表现为状态永远停在"连接中"）

## 未完成（按优先级）

| 优先级 | 事项 | 说明 |
|---|---|---|
| P0 | 复制粘贴 | 代码层面已具备：`view.rs` 有 clipboard 读写，`bindings.rs` 绑了 ⌘C/⌘V，但**没有手动敲键验证过**（当前测试环境点不到窗口，也没法按键）。真机自测一遍再划掉 |
| P1 | SFTP 面板 | 复用同一个连接开 sftp subsystem，双栏 + 拖拽 + 传输队列 |
| P1 | `~/.ssh/config` 导入 | 解析 Host/HostName/User/Port/IdentityFile/ProxyJump，批量灌进会话库 |
| P1 | ProxyJump / 端口转发 | 生产环境必需 |
| P1 | 快捷命令 / 批量广播 / 分屏 | 效率件 |
| P2 | 状态栏双指标 | 协议层 RTT + 终端最后输出时间（keepalive 通不代表 shell 活着） |
| P2 | 合盖唤醒后立即探测一轮 | 系统唤醒事件监听 |
| P2 | 滚动缓冲 / 终端内搜索 | alacritty 有 scrollback，但要确认 iced_term 的绘制是否支持滚动偏移 |
| P3 | 打包 .app + ad-hoc 签名 | 本地自用，不做公证；顺带解决钥匙串每次重新授权的问题 |

## 已知坑（别再踩一遍）

1. **`$(cat file)` 会吃掉末尾换行**。用 env var 传测试命令时务必 `"$(cat f)"$'\n'`，否则命令被输入但永远不回车。这个坑排查了半小时。
2. **russh RSA + `None` hash** → 服务端返回裸拒绝，没有任何提示。见上。
3. **`tokio::spawn` 开的写任务可能不跑**。表现：读正常、写入 `data_bytes` 返回 Ok 但远端收不到。改用单任务 `select!`。
4. **中文每隔一个字丢一个** → 两个独立原因叠加：宽字符锚点错 + cosmic-text 静默丢字形。两个都得修。
5. **`boot()` 在 tokio 任务里执行**，它不返回就不会有窗口——app 进程活着但一个 window 都没有，别误判成渲染问题。用 `swift` + `CGWindowListCopyWindowInfo` 查窗口是否存在最可靠。
6. Apple Events 未授权（`-1743`），`osascript` 无法前置/操作窗口。截图用 `screencapture -x -o -l <windowID>`，windowID 从上面的 Swift 脚本拿。
7. iced_term 的 `TerminalView` 是逐格调用 `fill_text`，功能键/剪贴板等交互能力要逐个确认，别假设它有。
8. **russh 的 `check_known_hosts` 把密钥注释算进相等性**（`ssh_key::PublicKey` 的 `PartialEq` 派生覆盖 `comment`）。用它的返回值判断"密钥是否变更"会在注释不一致时误报中间人攻击。已改成自己按指纹比对。
9. **`learn_known_hosts` 给空文件写记录会先写一个空行**（`seek(End(-1))` 在 0 字节文件上失败 → 判定"没有换行"）。openssh 会跳过空行，不影响功能，但会污染用户的 known_hosts。要么自己写追加逻辑，要么别往空文件里写。
10. **通过构建/CI 通道启动 GUI 会被沙箱掐住**：`~/.ssh` 读私钥报 `Operation not permitted (os error 1)`，外连报 `Network is unreachable (os error 51)`。这不是代码 bug，在终端里手动 `./target/debug/ssh-client` 启动就正常。验证连接一定要在真实终端里跑。
11. `SSH_PROBE` 现在在传输层发送并**延时 1.2s**：太早写会在远端 shell 还没起来时被 PTY 丢掉（表现为"命令输入了但没输出"）。
12. **`Terminal` 初始是 0 行高**，在第一次 layout/resize 之前 `feed()` 进去的字节会被丢掉。所以别指望"连上之前"塞的欢迎语能显示，重连分隔线偶尔也会被吞。
13. `Subscription::run_with` 用 hash 当 key，**任何"换内部通道"的操作都必须让 hash 变**。这个坑踩了两次（自己的 SshStream、iced_term 的 Terminal 订阅各一次），表现都是"状态卡住不动、终端空白"。

## 怎么验证（无键盘也能跑）

```bash
# 1. 起一个真实会话并截图（必须在真实终端里跑，见坑 10）
SSH_AUTOCONNECT=1 ./target/debug/ssh-client &
swift tools/winlist.swift | grep "SSH 工作台"      # 拿 windowID
screencapture -x -o -l <windowID> /tmp/shot.png

# 2. 隔离冒烟：不碰真实会话库和 known_hosts
mkdir -p /tmp/smoke && : > /tmp/smoke/known_hosts
SSH_CLIENT_DB=/tmp/smoke/sessions.db \
SSH_KNOWN_HOSTS=/tmp/smoke/known_hosts \
SSH_AUTOCONNECT=1 SSH_PROBE="hostname; echo FP_OK" ./target/debug/ssh-client &

# 3. 只看"添加会话"表单布局（不需要点按钮）
SSH_OPEN_FORM=1 ./target/debug/ssh-client &
# 编辑已有会话的表单（用第一条会话预填）
SSH_OPEN_FORM=edit ./target/debug/ssh-client &

# 4. 连上后自动在远端执行一段命令（验证写入方向）
SSH_PROBE="stty size" SSH_AUTOCONNECT=1 ./target/debug/ssh-client &
# 期望终端里出现 46 154，与本地 resize 日志一致

# 5. 单独验证 russh（排除终端层干扰）
cargo build --bin rsstest && ./target/debug/rsstest
WRITE_MODE=split ./target/debug/rsstest

# 6. 单元测试（会话库 + 指纹逻辑都是纯函数，跑得很快）
cargo test
```

可用的测试环境变量：`SSH_CLIENT_DB`（换库文件）、`SSH_KNOWN_HOSTS`（换 known_hosts）、`SSH_AUTOCONNECT`（自动开第一个会话）、`SSH_OPEN_FORM`（自动打开表单，`=edit` 走编辑模式）、`SSH_PROBE`（连上后往远端敲一条命令）。

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

### 会话库 / 指纹（2026-11-13 第二轮）

| 项 | 结果 |
|---|---|
| 首次启动播种 + 侧栏数据驱动 | 通过（`生产环境 · 1`，`43.139.142.161 / root · 私钥`） |
| 会话写入 → 落库 → 读回 | 通过（`cargo test` 覆盖分组、端口、私钥路径往返） |
| TOFU 首次 pin | 通过（空 known_hosts 连一次后写入 `43.139.142.161 ssh-ed25519 AAAA…DVHLV`） |
| 二次连接判定为已知 | 通过（状态栏"与 known_hosts 一致"，且与 openssh 自己记的 ed25519 指纹相同） |
| 密钥变更拒绝 | 通过（单测：换一把 key 后报"主机密钥已变更"） |
| 写方向（`SSH_PROBE`） | 通过（远端执行 `hostname; echo FP_OK` → `VM-0-2-opencloudos` / `FP_OK`） |
| 添加会话表单渲染 | 通过（右侧抽屉，字段/私钥密码切换/保存取消按钮布局正常） |

### 断线重连（2026-11-13 第三轮）

| 项 | 结果 |
|---|---|
| 连接被拒时的退避 | 通过（`127.0.0.1:1` 连续失败，状态栏依次 `第1次 1s` → `第4次 8s` → `第5次 16s`） |
| 真实掉线自动重连 | 通过（`SSH_PROBE="echo WILL_EXIT; sleep 3; exit"`：logout 后用 1s 重连，出现第二条登录 banner + 新提示符） |
| 重连后 scrollback | 通过（旧输出仍在，`—— 重连 <host> ——` 作分隔） |
| 重连后事件通道 | 通过（gen 修复前卡在"连接中"，修复后正常收到数据） |
| `connect_count` 计数 | 通过（一次首连 + 一次重连 = 17→19） |
| 关标签页不再 panic | 通过（原先 `terminal event channel closed unexpected` 会让整个 UI 卡死） |

### 编辑会话（2026-11-13 第四轮）

| 项 | 结果 |
|---|---|
| 编辑表单预填 | 通过（`SSH_OPEN_FORM=edit`：标题"编辑会话"，名称/主机/端口/用户/分组/私钥路径全部带出） |
| 口令栏语义 | 通过（标签"新私钥口令（留空不改）"，不把钥匙串里的值读回表单） |
| 改分组 + 换认证方式的落库 | 通过（单测 `update_moves_group_and_switches_auth`：group/label/host/port/auth 全部生效） |
| 行尾 `✎` / `×` 入口 | 通过（两个 10px 图标按钮，删除仍走二次确认） |

## 下一步建议

P0 只剩一件：**真机验证 ⌘C/⌘V**（代码已经有了，只差人手敲一遍）。
之后上 SFTP：复用同一条连接开 `sftp` subsystem，别另起连接。

再往下值得考虑的：状态栏双指标（协议层 RTT + 终端最后输出时间），因为 keepalive 通不代表 shell 活着——
现在掉线判断完全依赖通道关闭事件，遇到"TCP 半死"会假死。
