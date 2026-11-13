# SSH 工作台 · 交接文档

> 给接手的人（或下一个 AI session）。目标：读完这一份，30 分钟内能改代码、能验证、不重踩坑。
> 交接时间：2026-11-13 · 仓库 `master` @ `7532d39`

## 1. 这是什么

一个 macOS 桌面 SSH 客户端，纯 Rust 实现（iced 做 GUI，russh 做协议，alacritty_terminal 做 VT 解析），**零 Node、零 WebView、零外部二进制依赖**。

定位是自己用的生产工具，不做商业化、不做公证分发。当前状态：**日常可用**——真实主机上登录、执行命令、中文/颜色/alt-screen 渲染、窗口尺寸同步全部验证通过；会话库、主机指纹校验、断线自动重连已落地。

一句话进度：**P0 只剩「真机敲一遍 ⌘C/⌘V」，之后是 SFTP。**

## 2. 环境与构建

| 项 | 值 |
|---|---|
| 工具链 | rustc / cargo 1.98.1（edition 2024） |
| 平台 | **仅 macOS**。`secrets.rs` 直接依赖 `security-framework`（Cargo 里声明在 `target_os = "macos"`），没有跨平台分支，换平台需自己接 keyring |
| 构建 | `cargo build` |
| 运行 | `./target/debug/ssh-client` |
| 测试 | `cargo test`（会话库 + 指纹逻辑，秒级） |
| 产物 | 单二进制，无资源目录。打包 `.app` 是 P3，没做 |

> ⚠️ 必须**在真实终端里启动**。通过构建工具 / CI 通道拉起 GUI 会被沙箱掐住：读 `~/.ssh` 报 `Operation not permitted (os error 1)`，外连报 `Network is unreachable (os error 51)`。这不是代码 bug。

## 3. 架构

```
┌──────────────────────── main.rs（App / update / view） ────────────────────────┐
│  sidebar ── 会话树（数据来自 store）        tabbar ── Tab[]                     │
│                    │                            │                              │
│              Draft 表单（新建/编辑）        Terminal（iced_term）               │
└────────────────────┼────────────────────────────┼──────────────────────────────┘
                     │                            │ feed(&[u8])   ▲ Transport
        ┌────────────▼──────────┐      ┌──────────▼───────────┐   │
        │ store.rs   (SQLite)   │      │ ssh.rs  (russh)      │───┘
        │ secrets.rs (Keychain) │      │ hostkey.rs (TOFU)    │
        └───────────────────────┘      └──────────────────────┘
```

关键设计决策，改动前先理解：

1. **iced_term 必须 vendored**（`crates/iced_term`）。上游 0.8.0 把 backend 硬编码成"起本地 PTY"，没有 trait 抽象，接不进 russh；且 CJK 渲染有硬 bug。改动点见 §4.2。
2. **重连不重建 Terminal**。`Subscription::run_with` 按 hash 缓存订阅，`TerminalSubscriptionData` 只 hash `id`；同一 id 换新 Terminal 会一直听已关闭的旧通道。所以重连只调 `Terminal::set_transport()`，VT 状态和 scrollback 都保留。
3. **传输层单任务 `select!`**。读方向和写方向必须在同一个 task 里 `select!`，不要拆成 `tokio::spawn` 两个任务——曾经偶发写任务永不调度，`data_bytes` 返回 Ok 但远端收不到。
4. **密码不落库**。SQLite 只记"去哪取"，真正的密码/口令在 macOS 登录钥匙串。
5. **known_hosts 就是唯一真相源**。不另建私有指纹库，与命令行 `ssh` 双向互通。

## 4. 代码地图

### 4.1 根 crate（`src/`）

| 文件 | 行数 | 职责 | 关键入口 |
|---|---|---|---|
| `main.rs` | 1423 | 全部 UI + 应用状态机 + 重连调度 | `App::boot` / `update` / `view` / `subscription` |
| `ssh.rs` | 266 | russh 连接、认证、PTY、keepalive | `ssh::spawn(SshRequest)`、`SshEvent`、`SshTransport` |
| `store.rs` | 455 | SQLite 会话库、分组、CRUD、迁移 | `Store::open_default`、`groups()`、`insert/update/delete` |
| `hostkey.rs` | 162 | 主机指纹校验（TOFU） | `verify_at(host, port, key)` → `Verdict` |
| `secrets.rs` | 56 | 钥匙串读写 | `set_password/password/set_passphrase/passphrase/forget` |
| `bin/rsstest.rs` | 103 | **诊断工具，别删** | 最小 russh 连接，`WRITE_MODE=split` 复现写丢失 |

`main.rs` 内部结构（1423 行不好一口气读，按这个索引跳）：

- `Status`（L87）— `Local / Connecting / Connected / Closed / Reconnecting{attempt,delay} / Failed(String)`，带 `label()` 和 `color()`
- `Tab`（L121）— 一个标签页：`term` + `transport` + `events` + `status` + `generation`
- `backoff()` / `MAX_ATTEMPTS`（L142）— 1/2/4/8/16/30s，封顶 30，最多 8 次
- `App`（L150）— `store` / `store_warning` / `groups` / `tabs` / `active` / `draft`
- `Draft`（L175）— 新建与编辑共用；`from_session()` 预填普通字段，密码/口令栏**永远留空**（留空 = 不改钥匙串）
- `Message`（L294）— 全部消息枚举，加交互从这里开始
- `App::boot`（L343）— **注意：`boot()` 不返回就没有窗口**，进程活着也可能是零 window
- `connect_session`（L535）/ `reconnect`（L593）/ `schedule_retry`（L637）— 重连三段
- `SshStream`（L865）— `hash` 里**必须**带 `generation`
- `view`（L1276）— 三栏布局

### 4.2 vendored `crates/iced_term`（相对上游的全部改动）

| 文件 | 改了什么 | 为什么 |
|---|---|---|
| `backend.rs` | 新增 `trait Transport { write, resize }` + `enum Sink { Local(Notifier), Remote(Arc<dyn Transport>) }` + `Terminal::new_remote` / `set_transport` | 上游只能起本地 PTY |
| `view.rs` | 跳过 `WIDE_CHAR_SPACER`；宽字符锚点改"跨双格中心"；CJK 显式走 PingFang SC，拉丁走等宽 | 否则中文重复绘制 / 每隔一个字丢一个 |
| `terminal.rs` | 事件通道关闭时 `break` 而非 `panic!` | 关标签页 drop Terminal 会让整个 UI 卡死 |
| `bindings.rs` | ⌘C/⌘V 等剪贴板绑定（未真机验证） | — |

> **不要**单独在 `Cargo.toml` 里加 `alacritty_terminal`——它由 iced_term 传递依赖，版本一旦分叉就会因为类型不互通而编不过。

## 5. 数据与安全模型

**会话库**：`~/Library/Application Support/ssh-client/sessions.db`，WAL，外键开启。

```sql
groups   (id, name, sort_order)
sessions (id, group_id, label, host, port, user, auth_kind,
          key_path, sort_order, connect_count, last_used_at)
```

- 库打不开 → 退化到 `:memory:` + 侧栏顶部警告，不让 app 起不来
- 首次启动播种一条开发主机（`43.139.142.161` / root / `~/.ssh/id_rsa`），见 `seed_sessions()`
- 打开会话按 session_id 去重，同一会话只开一个标签

**密钥**：macOS 登录钥匙串，`service=ssh-client`，`account=session:<id>:password | :passphrase`。删除会话时同步 `secrets::forget()`。

**指纹**：读写 `~/.ssh/known_hosts`（可用 `SSH_KNOWN_HOSTS` 覆盖）。TOFU 首次 pin，之后每次比对，**变更即拒绝连接**，报错带"服务器出示 X / 第 N 行是 Y"。
非 22 端口按 `[host]:port` 命名空间隔离，与 openssh 一致。

## 6. 已完成（含提交）

| 提交 | 内容 |
|---|---|
| `205a636` | iced 终端原型 + CJK 宽字符修复 |
| `9942bc4` | 接入 russh，打通真实 SSH 链路 |
| `05f21e6` | 会话库落 SQLite + 指纹 TOFU |
| `7c604b6` | iced_term 事件通道不再 panic + 支持换 transport |
| `731a2ec` | 断线自动重连（指数退避） |
| `7532d39` | 会话可编辑（复用抽屉，密码留空即不改） |

真机验证矩阵（`root@43.139.142.161`，OpenCloudOS 9 / kernel 6.6.119）全部通过：公钥认证、banner、命令回显、中文表格对齐、truecolor/256/16 色、`ls --color`、alt-screen 往返、emoji、`stty size` 46×154 尺寸同步、TOFU pin 与二次一致、写方向 `SSH_PROBE`。

## 7. 未完成（按优先级）

| 优先级 | 事项 | 说明 / 起点 |
|---|---|---|
| **P0** | 真机验证 ⌘C/⌘V | 代码已就绪（`bindings.rs` + `view.rs` 剪贴板读写），只差人手敲一遍。做完即可划掉 |
| P1 | SFTP 面板 | **复用同一条连接**开 `sftp` subsystem，别另起连接。双栏 + 拖拽 + 传输队列 |
| P1 | `~/.ssh/config` 导入 | 解析 Host/HostName/User/Port/IdentityFile/ProxyJump，批量灌库 |
| P1 | ProxyJump / 端口转发 | 生产环境必需 |
| P1 | 快捷命令 / 批量广播 / 分屏 | 效率件 |
| P2 | 状态栏双指标 | 协议层 RTT + 终端最后输出时间。**keepalive 通不代表 shell 活着**，现在只靠通道关闭事件判掉线，遇 TCP 半死会假死 |
| P2 | 合盖唤醒后主动探测一轮 | 监听系统唤醒事件 |
| P2 | 滚动缓冲 / 终端内搜索 | alacritty 有 scrollback，但要确认 iced_term 逐格 `fill_text` 的绘制是否支持滚动偏移 |
| P3 | 打包 `.app` + ad-hoc 签名 | 顺带解决钥匙串每次重新授权 |

## 8. 已知坑

### 8.1 根因型（已修复，但改相关代码时会被重新引入）

1. **`$(cat f)` 吃掉末尾换行** → 命令被输入但不执行。传测试命令用 `"$(cat f)"$'\n'`。
2. **russh RSA 传 `None` hash** → 退回 `ssh-rsa`(SHA-1)，现代 sshd 裸拒绝、无提示。必须 `session.best_supported_rsa_hash()`。
3. **拆 `tokio::spawn` 写任务** → 偶发不调度，读正常、写静默丢失。改单任务 `select!`。
4. **中文每隔一个字丢一个** → 两个独立原因叠加：宽字符锚点错 + cosmic-text 缺 CJK 字形时**静默丢字形**（不报错）。两个都得修。
5. **`check_known_hosts` 把密钥 comment 算进 `PublicKey` 相等性** → 注释不同就误报"密钥已变更/中间人攻击"。必须自己按 SHA256 指纹比。
6. **`learn_known_hosts` 写空文件会先塞一个空行**（`seek(End(-1))` 在 0 字节文件上失败）。openssh 会跳过空行，不影响功能但污染用户文件。
7. **`Subscription::run_with` 按 hash 当 key** → 任何"换内部通道"的操作都必须让 hash 变。踩了两次（`SshStream`、iced_term 的 Terminal 订阅各一次），症状都是「状态卡住 + 终端空白」。
8. **`Terminal` 初始 0 行高**，首次 layout 前 `feed()` 的字节会被丢掉。所以"连上之前的欢迎语"显示不出来，重连分隔线偶尔被吞。

### 8.2 环境 / 调试型

9. **`boot()` 在 tokio 任务里执行**，不返回就零 window。别误判成渲染问题，用 `tools/winlist.swift`（CGWindowList）查窗口是否存在。
10. **Apple Events 未授权（-1743）**，`osascript` 不能前置/操作窗口。截图用 `screencapture -x -o -l <windowID>`。
11. **`SSH_PROBE` 必须延时 1.2s**：太早写会在远端 shell 起来前被 PTY 丢掉，表现为"输入了但没输出"。
12. **iced_term 是逐格 `fill_text`**，功能键、选区、剪贴板等交互要逐个确认，别假设它有。

## 9. 怎么验证（无键盘也能跑）

```bash
# 1. 起真实会话并截图（必须在真实终端里）
SSH_AUTOCONNECT=1 ./target/debug/ssh-client &
swift tools/winlist.swift | grep "SSH 工作台"        # 拿 windowID
screencapture -x -o -l <windowID> /tmp/shot.png

# 2. 隔离冒烟：不碰真实库和 known_hosts
mkdir -p /tmp/smoke && : > /tmp/smoke/known_hosts
SSH_CLIENT_DB=/tmp/smoke/sessions.db SSH_KNOWN_HOSTS=/tmp/smoke/known_hosts \
SSH_AUTOCONNECT=1 SSH_PROBE="hostname; echo FP_OK" ./target/debug/ssh-client &

# 3. 只看表单布局（不需要点按钮）
SSH_OPEN_FORM=1 ./target/debug/ssh-client &          # 新建
SSH_OPEN_FORM=edit ./target/debug/ssh-client &       # 编辑（预填第一条）

# 4. 尺寸同步：期望终端出现 "46 154"
SSH_PROBE="stty size" SSH_AUTOCONNECT=1 ./target/debug/ssh-client &

# 5. 排除终端层，单独验 russh
cargo build --bin rsstest && ./target/debug/rsstest
WRITE_MODE=split ./target/debug/rsstest               # 复现"写丢失"

# 6. 单测
cargo test
```

测试环境变量：`SSH_CLIENT_DB`（换库文件）、`SSH_KNOWN_HOSTS`（换 known_hosts）、`SSH_AUTOCONNECT`（自动开第一个会话）、`SSH_OPEN_FORM[=edit]`（自动打开表单）、`SSH_PROBE`（连上后往远端敲一条命令）。

## 10. 接手注意事项

- **`crates/iced_term` 是 vendored fork，不是普通依赖**。`cargo update` 不会覆盖它，但重装/替换目录会丢改动。所有改动点已列在 §4.2。
- **`src/bin/rsstest.rs` 和 `tools/winlist.swift` 是刻意保留的诊断工具，别删**。
- 提交粒度按「一事一提交」，信息用 `type(scope): 中文描述`，正文写清为什么改。
- 项目没有 CI，也没有 `AI_RULES.md` 之类的硬门槛；`cargo test` + §9 的冒烟脚本就是验收标准。
- 更细的历史流水（每轮踩了什么、验证截图结论）在 `docs/PROGRESS.md`，这份文档是它的收敛版；两者冲突时以本文为准。
