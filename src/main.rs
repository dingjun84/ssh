mod files;
mod hostkey;
mod secrets;
mod sftp;
mod ssh;
mod store;

use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use iced::font::Font;
use iced::widget::{button, column, container, row, text};
use iced::{Color, Element, Length, Subscription, Task};
use iced_term::settings::{
    BackendSettings, FontSettings, Settings as TermSettings, ThemeSettings,
};
use iced_term::{ColorPalette, Command as TermCommand, Terminal, TerminalView, Transport};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::Mutex;

use ssh::{Auth, SshEvent, SshRequest};
use store::{AuthKind, Session, Store};

const BG: Color = Color::from_rgb(0.118, 0.122, 0.110);
const SIDEBAR: Color = Color::from_rgb(0.133, 0.137, 0.122);
const BAR: Color = Color::from_rgb(0.098, 0.102, 0.090);
const FG: Color = Color::from_rgb(0.973, 0.973, 0.949);
const DIM: Color = Color::from_rgb(0.459, 0.443, 0.369);
const ACCENT: Color = Color::from_rgb(0.651, 0.886, 0.180);
const WARN: Color = Color::from_rgb(0.992, 0.592, 0.122);
const DANGER: Color = Color::from_rgb(0.976, 0.149, 0.447);

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn runtime() -> tokio::runtime::Handle {
    RUNTIME
        .get()
        .expect("tokio runtime must be initialised before use")
        .handle()
        .clone()
}

fn monokai() -> ColorPalette {
    ColorPalette {
        foreground: "#f8f8f2".into(),
        background: "#1e1f1c".into(),
        black: "#272822".into(),
        red: "#f92672".into(),
        green: "#a6e22e".into(),
        yellow: "#e6db74".into(),
        blue: "#66d9ef".into(),
        magenta: "#ae81ff".into(),
        cyan: "#a1efe4".into(),
        white: "#f8f8f2".into(),
        bright_black: "#75715e".into(),
        bright_red: "#f92672".into(),
        bright_green: "#a6e22e".into(),
        bright_yellow: "#e6db74".into(),
        bright_blue: "#66d9ef".into(),
        bright_magenta: "#ae81ff".into(),
        bright_cyan: "#a1efe4".into(),
        bright_white: "#f9f8f5".into(),
        bright_foreground: Some("#f8f8f2".into()),
        dim_foreground: "#a8a8a2".into(),
        dim_black: "#1e1f1c".into(),
        dim_red: "#9d1f4a".into(),
        dim_green: "#6a8f1c".into(),
        dim_yellow: "#938a4a".into(),
        dim_blue: "#3f8a99".into(),
        dim_magenta: "#6f529f".into(),
        dim_cyan: "#669892".into(),
        dim_white: "#9e9e98".into(),
    }
}

fn term_settings() -> TermSettings {
    TermSettings {
        font: FontSettings {
            size: 13.0,
            scale_factor: 1.3,
            font_type: Font::with_name("Menlo"),
        },
        theme: ThemeSettings::new(Box::new(monokai())),
        backend: BackendSettings::default(),
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Status {
    Local,
    Connecting,
    Connected,
    Closed,
    /// Waiting out an exponential backoff before the next attempt.
    Reconnecting { attempt: u32, delay: u64 },
    Failed(String),
}

impl Status {
    fn label(&self) -> String {
        match self {
            Status::Local => "本地".into(),
            Status::Connecting => "连接中".into(),
            Status::Connected => "已连接".into(),
            Status::Closed => "已断开".into(),
            Status::Reconnecting { attempt, delay } => {
                format!("重连中 · 第{attempt}次 · {delay}s 后")
            },
            Status::Failed(_) => "失败".into(),
        }
    }

    fn color(&self) -> Color {
        match self {
            Status::Local | Status::Connected => ACCENT,
            Status::Connecting | Status::Reconnecting { .. } => WARN,
            Status::Closed => DIM,
            Status::Failed(_) => DANGER,
        }
    }
}

struct Tab {
    id: u64,
    /// `None` for local shells, which never reconnect.
    session_id: Option<i64>,
    title: String,
    term: Terminal,
    transport: Option<Arc<ssh::SshTransport>>,
    events: Option<Arc<Mutex<UnboundedReceiver<SshEvent>>>>,
    status: Status,
    fingerprint: Option<String>,
    /// Trust-on-first-use note shown next to the fingerprint.
    fingerprint_note: Option<&'static str>,
    /// How many automatic reconnect attempts have been spent on this tab.
    connect_attempts: u32,
    /// Bumped on every reconnect. `Subscription::run_with` caches by hash, so
    /// without this the widget would keep listening on the *old*, closed event
    /// channel and never see anything again.
    generation: u64,
    sftp: Option<sftp::Handle>,
    sftp_events: Option<Arc<Mutex<UnboundedReceiver<sftp::SftpEvent>>>>,
    /// Bumped whenever the SFTP worker is rebuilt, for the same hash reason as
    /// `generation`.
    sftp_generation: u64,
    sftp_ui: files::Ui,
}

/// Backoff schedule: 1, 2, 4, 8, 16, then a 30s ceiling.
fn backoff(attempt: u32) -> u64 {
    (1u64 << attempt.min(4)).min(30)
}

/// Give up after this many automatic attempts; the status bar still offers a
/// manual retry.
const MAX_ATTEMPTS: u32 = 8;

struct App {
    store: Store,
    store_warning: Option<String>,
    groups: Vec<(String, Vec<Session>)>,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    /// `Some` while the "add session" drawer is open.
    draft: Option<Draft>,
    /// Session id awaiting delete confirmation.
    pending_delete: Option<i64>,
    main_window: Option<iced::window::Id>,
    modifiers: iced::keyboard::Modifiers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DraftField {
    Group,
    Label,
    Host,
    Port,
    User,
    KeyPath,
    Secret,
}

#[derive(Debug, Clone)]
struct Draft {
    /// `Some` when the drawer is editing an existing row.
    editing: Option<i64>,
    group: String,
    label: String,
    host: String,
    port: String,
    user: String,
    use_key: bool,
    key_path: String,
    /// Password or key passphrase, depending on `use_key`. When editing, an
    /// empty value means "keep the stored secret".
    secret: String,
    error: Option<String>,
}

impl Draft {
    fn new(group: Option<&str>) -> Self {
        Draft {
            editing: None,
            group: group.unwrap_or("生产环境").to_string(),
            label: String::new(),
            host: String::new(),
            port: "22".into(),
            user: "root".into(),
            use_key: true,
            key_path: "~/.ssh/id_rsa".into(),
            secret: String::new(),
            error: None,
        }
    }

    /// Prefills the drawer from an existing row. The secret is never read back
    /// into the form — it stays in the Keychain until deliberately replaced.
    fn from_session(session: &Session) -> Self {
        let (use_key, key_path) = match &session.auth {
            AuthKind::Key { path } => (true, path.clone()),
            AuthKind::Password => (false, String::new()),
        };
        Draft {
            editing: Some(session.id),
            group: session.group.clone(),
            label: session.label.clone(),
            host: session.host.clone(),
            port: session.port.to_string(),
            user: session.user.clone(),
            use_key,
            key_path,
            secret: String::new(),
            error: None,
        }
    }

    fn title(&self) -> &'static str {
        if self.editing.is_some() {
            "编辑会话"
        } else {
            "新建会话"
        }
    }

    fn field_mut(&mut self, field: DraftField) -> &mut String {
        match field {
            DraftField::Group => &mut self.group,
            DraftField::Label => &mut self.label,
            DraftField::Host => &mut self.host,
            DraftField::Port => &mut self.port,
            DraftField::User => &mut self.user,
            DraftField::KeyPath => &mut self.key_path,
            DraftField::Secret => &mut self.secret,
        }
    }

    /// Validates and converts into a store row; the label falls back to the
    /// host so the sidebar never shows a blank line.
    fn into_new_session(&self) -> Result<store::NewSession, String> {
        let host = self.host.trim();
        if host.is_empty() {
            return Err("主机不能为空".into());
        }
        let port: u16 = self
            .port
            .trim()
            .parse()
            .map_err(|_| format!("端口不是合法数字: {}", self.port))?;
        let user = self.user.trim();
        if user.is_empty() {
            return Err("用户不能为空".into());
        }
        let group = self.group.trim();
        Ok(store::NewSession {
            group: if group.is_empty() { "未分组" } else { group }.to_string(),
            label: {
                let label = self.label.trim();
                if label.is_empty() {
                    host.to_string()
                } else {
                    label.to_string()
                }
            },
            host: host.to_string(),
            port,
            user: user.to_string(),
            auth: if self.use_key {
                let path = self.key_path.trim();
                if path.is_empty() {
                    return Err("私钥路径不能为空".into());
                }
                AuthKind::Key {
                    path: path.to_string(),
                }
            } else {
                AuthKind::Password
            },
        })
    }
}

#[derive(Debug, Clone)]
enum Message {
    Term(iced_term::Event),
    Ssh(u64, SshEvent),
    /// Utility tabs that are not part of the session library.
    OpenUtility(&'static str),
    OpenSession(i64),
    SelectTab(usize),
    CloseTab(usize),
    NewSession,
    EditSession(i64),
    DraftEdited(DraftField, String),
    DraftAuth(bool),
    DraftSave,
    DraftClose,
    AskDelete(i64),
    ConfirmDelete,
    CancelDelete,
    /// Backoff timer fired for a tab.
    Reconnect(u64),
    /// User pressed the status-bar retry button.
    Retry,
    /// SFTP worker event for a tab.
    Sftp(u64, sftp::SftpEvent),
    SftpToggle,
    Files(u64, files::Message),
    Window(iced::window::Id, iced::window::Event),
    Modifiers(iced::keyboard::Modifiers),
    WindowOpened,

}

fn spawn_local(id: u64, program: &str, args: Vec<String>) -> Terminal {
    let mut settings = term_settings();
    settings.backend = BackendSettings {
        program: program.to_string(),
        args,
        env: Default::default(),
        working_directory: None,
    };
    Terminal::new(id, settings).expect("failed to create terminal")
}

/// Rebuilds the SFTP worker for a tab. Called on first connect and on every
/// reconnect: the worker caches an `sftp` subsystem channel, and that channel
/// dies together with the SSH connection it was opened on.
fn attach_sftp(tab: &mut Tab, slot: Arc<ssh::SessionSlot>) {
    tab.sftp_ui.dismiss_conflict();
    tab.sftp_ui.remaining = 0;
    tab.sftp_ui.busy = false;
    let (handle, rx) = sftp::spawn(&runtime(), slot);
    tab.sftp = Some(handle);
    tab.sftp_events = Some(Arc::new(Mutex::new(rx)));
    // New receiver, so the subscription hash must change or iced keeps reading
    // the old channel (same trap as `Tab::generation`).
    tab.sftp_generation += 1;
}

/// Seed data for a fresh install: the box this project is developed against.
fn seed_sessions() -> Vec<store::NewSession> {
    vec![store::NewSession {
        group: "生产环境".into(),
        label: "43.139.142.161".into(),
        host: "43.139.142.161".into(),
        port: 22,
        user: "root".into(),
        auth: AuthKind::Key {
            path: "~/.ssh/id_rsa".into(),
        },
    }]
}

impl App {
    fn boot() -> (Self, Task<Message>) {
        let mut store_warning = None;
        let store = match Store::open_default() {
            Ok(store) => store,
            Err(err) => {
                // A broken database must not take the whole app down; run on an
                // in-memory library so the user can still open ad-hoc tabs.
                store_warning = Some(format!("{err}（已退化为临时会话库）"));
                Store::open(std::path::Path::new(":memory:"))
                    .expect("in-memory session store should always open")
            },
        };

        if let Err(err) = store.seed_if_empty(&seed_sessions()) {
            store_warning.get_or_insert(err);
        }

        let groups = store.groups().unwrap_or_default();
        let mut app = App {
            store,
            store_warning,
            groups,
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            draft: None,
            pending_delete: None,
            main_window: None,
            modifiers: Default::default(),
        };

        // Unattended smoke test hook: lets CI (and me) open a real session
        // without clicking through the UI.
        if std::env::var("SSH_AUTOCONNECT").is_ok() {
            if let Some(first) = app.first_session_id() {
                app.open_session(first);
            }
        } else {
            app.open_local();
        }

        // Same idea for the session form, which otherwise needs a click.
        // `SSH_OPEN_FORM=edit` prefills it from the first session.
        if let Ok(form) = std::env::var("SSH_OPEN_FORM") {
            let group = app.groups.first().map(|(name, _)| name.clone());
            app.draft = Some(if form == "edit" {
                app.groups
                    .first()
                    .and_then(|(_, list)| list.first())
                    .map(Draft::from_session)
                    .unwrap_or_else(|| Draft::new(group.as_deref()))
            } else {
                Draft::new(group.as_deref())
            });
        }
        let (id, task) = iced::window::open(iced::window::Settings {
            size: iced::Size::new(1280.0,800.0), exit_on_close_request: false, ..Default::default()
        });
        app.main_window = Some(id);
        (app, task.map(|_| Message::WindowOpened))
    }

    fn first_session_id(&self) -> Option<i64> {
        self.groups
            .first()
            .and_then(|(_, list)| list.first())
            .map(|s| s.id)
    }

    fn reload_groups(&mut self) {
        match self.store.groups() {
            Ok(groups) => self.groups = groups,
            Err(err) => self.store_warning = Some(err),
        }
    }

    fn find_session(&self, id: i64) -> Option<Session> {
        self.groups
            .iter()
            .flat_map(|(_, list)| list.iter())
            .find(|s| s.id == id)
            .cloned()
    }

    fn open_local(&mut self) {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let id = self.next_id;
        self.next_id += 1;
        self.tabs.push(Tab {
            id,
            session_id: None,
            title: "local".into(),
            term: spawn_local(id, &shell, vec![]),
            transport: None,
            events: None,
            status: Status::Local,
            fingerprint: None,
            fingerprint_note: None,
            connect_attempts: 0,
            generation: 0,
            sftp: None,
            sftp_events: None,
            sftp_generation: 0,
            sftp_ui: files::Ui::default(),
        });
        self.active = self.tabs.len() - 1;
    }

    fn open_selftest(&mut self) {
        let script = r#"
printf '\033[1;33m===== 1. 基础色 (16) =====\033[0m\n'
for i in 0 1 2 3 4 5 6 7; do printf "\033[4${i}m    \033[0m"; done; printf '\n'
for i in 0 1 2 3 4 5 6 7; do printf "\033[10${i}m    \033[0m"; done; printf '\n'
printf '\033[1;33m===== 2. Truecolor =====\033[0m\n'
printf '\033[38;2;255;110;0m橙色文字\033[0m \033[48;2;40;80;160m蓝色背景\033[0m\n'
printf '\033[1;33m===== 3. 中文宽字符对齐 =====\033[0m\n'
printf '| 主机名       | 状态     | CPU |\n'
printf '|--------------|----------|-----|\n'
printf '| prod-web-01  | 正常     | 12%% |\n'
printf '| 数据库主节点 | 严重告警 | 98%% |\n'
printf '|--------------|----------|-----|\n'
printf '\033[1;33m===== 4. Emoji 与符号 =====\033[0m\n'
printf '🚀 ✅ ❌ 🔥 ⚠️  →  ←  ↑  ↓  ●○◆\n'
printf '\033[1;33m===== 5. 256 色环 =====\033[0m\n'
for i in $(seq 16 15 255); do printf "\033[48;5;${i}m \033[0m"; done; printf '\n'
printf '\033[1;37;41m 反色 / 粗体 / 下划线 \033[0m\n'
printf '\033[1;33m===== 6. 备用屏幕切换 (2s) =====\033[0m\n'
sleep 1
printf '\033[?1049h\033[2J\033[H\033[1;35m ALT-SCREEN 已进入：vim / htop 走这条路径 \033[0m'
sleep 2
printf '\033[?1049l'
printf '\033[1;32m✓ 已回到主屏幕，alt-screen 切换正常\033[0m\n'
printf '\033[1;32m=== 自检输出结束，保持 30 分钟供缩放测试 ===\033[0m\n'
sleep 1800
"#;
        let id = self.next_id;
        self.next_id += 1;
        self.tabs.push(Tab {
            id,
            session_id: None,
            title: "渲染自检".into(),
            term: spawn_local(
                id,
                "/bin/zsh",
                vec!["-f".to_string(), "-c".to_string(), script.to_string()],
            ),
            transport: None,
            events: None,
            status: Status::Local,
            fingerprint: None,
            fingerprint_note: None,
            connect_attempts: 0,
            generation: 0,
            sftp: None,
            sftp_events: None,
            sftp_generation: 0,
            sftp_ui: files::Ui::default(),
        });
        self.active = self.tabs.len() - 1;
    }

    fn open_session(&mut self, session_id: i64) {
        // Focus the existing tab instead of stacking duplicates.
        if let Some(index) = self
            .tabs
            .iter()
            .position(|t| t.session_id == Some(session_id))
        {
            self.active = index;
            return;
        }

        let Some(session) = self.find_session(session_id) else {
            self.store_warning = Some(format!("会话 {session_id} 不在库中"));
            return;
        };

        let id = self.next_id;
        self.next_id += 1;

        let (transport, rx, slot) = match self.connect_session(&session) {
            Ok(connected) => connected,
            Err(err) => return self.open_failed(&session, err),
        };

        let term = Terminal::new_remote(id, term_settings(), transport.clone());

        let mut tab = Tab {
            id,
            session_id: Some(session.id),
            title: session.label.clone(),
            term,
            transport: Some(transport),
            events: Some(Arc::new(Mutex::new(rx))),
            status: Status::Connecting,
            fingerprint: None,
            fingerprint_note: None,
            connect_attempts: 0,
            generation: 0,
            sftp: None,
            sftp_events: None,
            sftp_generation: 0,
            sftp_ui: files::Ui::default(),
        };
        // SFTP rides on the same session; give it the handle as soon as the
        // handshake publishes one.
        attach_sftp(&mut tab, slot);
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        let _ = self.store.touch(session.id);
    }

    /// Resolves the credentials and kicks off a connection attempt. Shared by
    /// the first connect and every reconnect.
    fn connect_session(
        &self,
        session: &Session,
    ) -> Result<
        (
            Arc<ssh::SshTransport>,
            UnboundedReceiver<SshEvent>,
            Arc<ssh::SessionSlot>,
        ),
        String,
    > {
        let auth = match &session.auth {
            AuthKind::Key { path } => Auth::Key {
                path: shellexpand(path),
                passphrase: secrets::passphrase(session.id)?,
            },
            AuthKind::Password => match secrets::password(session.id)? {
                Some(password) => Auth::Password(password),
                None => return Err("会话库里没有保存密码".into()),
            },
        };

        Ok(ssh::spawn(
            &runtime(),
            SshRequest {
                host: session.host.clone(),
                port: session.port,
                user: session.user.clone(),
                auth,
                cols: 100,
                rows: 30,
            },
        ))
    }

    /// Opens a tab that shows nothing but the failure, so the reason is
    /// visible instead of being swallowed by a silent no-op.
    fn open_failed(&mut self, session: &Session, error: String) {
        let id = self.next_id;
        self.next_id += 1;
        let mut term = Terminal::new_remote(id, term_settings(), Arc::new(NullTransport));
        term.feed(format!("\x1b[1;31m[无法连接] {error}\x1b[0m\r\n").as_bytes());
        self.tabs.push(Tab {
            id,
            session_id: Some(session.id),
            title: session.label.clone(),
            term,
            transport: None,
            events: None,
            status: Status::Failed(error),
            fingerprint: None,
            fingerprint_note: None,
            connect_attempts: 0,
            generation: 0,
            sftp: None,
            sftp_events: None,
            sftp_generation: 0,
            sftp_ui: files::Ui::default(),
        });
        self.active = self.tabs.len() - 1;
    }

    fn tab_index_for(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    /// Rebuilds the transport for a tab that lost its connection. The terminal
    /// is recreated too — a fresh remote shell means a fresh VT, and iced_term
    /// has no way to swap the transport underneath an existing one.
    fn reconnect(&mut self, tab_id: u64) -> Task<Message> {
        let Some(index) = self.tab_index_for(tab_id) else {
            return Task::none();
        };
        if !matches!(
            self.tabs[index].status,
            Status::Closed | Status::Failed(_) | Status::Reconnecting { .. }
        ) {
            // Already connected or mid-handshake: a stale timer, ignore it.
            return Task::none();
        }
        let Some(session_id) = self.tabs[index].session_id else {
            return Task::none();
        };
        let Some(session) = self.find_session(session_id) else {
            self.tabs[index].status = Status::Failed("会话已从库中删除".into());
            return Task::none();
        };

        match self.connect_session(&session) {
            Ok((transport, rx, slot)) => {
                let tab = &mut self.tabs[index];
                // Keep the terminal (and its scrollback); only the pipe
                // underneath changes.
                tab.term.set_transport(transport.clone());
                tab.term
                    .feed(format!("\r\n\x1b[2m—— 重连 {} ——\x1b[0m\r\n", session.label).as_bytes());
                tab.transport = Some(transport);
                tab.events = Some(Arc::new(Mutex::new(rx)));
                tab.generation += 1;
                tab.status = Status::Connecting;
                tab.fingerprint = None;
                tab.fingerprint_note = None;
                // The old SFTP channel died with the connection; rebuild the
                // worker around the freshly published session.
                attach_sftp(tab, slot);
                let _ = self.store.touch(session.id);
                Task::none()
            },
            Err(err) => {
                self.tabs[index].status = Status::Failed(err);
                self.schedule_retry(index)
            },
        }
    }

    /// Arms the backoff timer for the next attempt.
    fn schedule_retry(&mut self, index: usize) -> Task<Message> {
        let tab = &mut self.tabs[index];
        if tab.connect_attempts >= MAX_ATTEMPTS || tab.session_id.is_none() {
            tab.status = Status::Failed(match &tab.status {
                Status::Failed(err) => err.clone(),
                _ => "重连次数用尽".into(),
            });
            return Task::none();
        }

        let delay = backoff(tab.connect_attempts);
        tab.connect_attempts += 1;
        tab.status = Status::Reconnecting {
            attempt: tab.connect_attempts,
            delay,
        };
        let tab_id = tab.id;
        Task::perform(
            async move {
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            },
            move |_| Message::Reconnect(tab_id),
        )
    }
}

/// Placeholder sink for tabs that never reach a host.
struct NullTransport;

impl Transport for NullTransport {
    fn write(&self, _data: &[u8]) {}
    fn resize(&self, _cols: u16, _rows: u16) {}
}

fn update(state: &mut App, message: Message) -> Task<Message> {
    match message {
        Message::Term(iced_term::Event::BackendCall(id, cmd)) => {
            if let Some(tab) = state.tabs.iter_mut().find(|t| t.id == id) {
                tab.term.handle(TermCommand::ProxyToBackend(cmd));
            }
        },
        Message::Ssh(id, event) => {
            if let Some(index) = state.tab_index_for(id) {
                let tab = &mut state.tabs[index];
                match event {
                    SshEvent::Data(bytes) => tab.term.feed(&bytes),
                    SshEvent::HostKey { fingerprint, learned } => {
                        tab.fingerprint = Some(fingerprint);
                        tab.fingerprint_note = Some(if learned {
                            "首次连接，已记录"
                        } else {
                            "与 known_hosts 一致"
                        });
                    },
                    SshEvent::Connected => {
                        tab.status = Status::Connected;
                        // A successful handshake resets the backoff so the next
                        // drop starts at 1s again.
                        tab.connect_attempts = 0;
                        if std::env::var_os("SSH_SFTP_PROBE").is_some() && tab.sftp.is_some() {
                            return open_sftp_window(tab);
                        }
                    },
                    SshEvent::Error(err) => {
                        tab.status = Status::Failed(err.clone());
                        let message = format!("\r\n\x1b[1;31m[连接错误] {err}\x1b[0m\r\n");
                        tab.term.feed(message.as_bytes());
                    },
                    SshEvent::Closed => {
                        if !matches!(tab.status, Status::Failed(_)) {
                            tab.status = Status::Closed;
                        }
                        // Local tabs have no session to reconnect to; remote
                        // ones retry with backoff until MAX_ATTEMPTS.
                        if tab.session_id.is_some() {
                            return state.schedule_retry(index);
                        }
                    },
                }
            }
        },
        Message::Reconnect(tab_id) => return state.reconnect(tab_id),
        Message::Retry => {
            if let Some(id) = state.tabs.get(state.active).map(|t| t.id) {
                // A manual retry should not be blocked by the spent budget.
                if let Some(index) = state.tab_index_for(id) {
                    state.tabs[index].connect_attempts = 0;
                }
                return state.reconnect(id);
            }
        },
        Message::Sftp(id, event) => {
            if let Some(index) = state.tab_index_for(id) {
                let tab = &mut state.tabs[index];
                match event {
                    sftp::SftpEvent::Overwrite(conflict) => {
                        if tab.sftp_ui.window.is_some() { tab.sftp_ui.conflict = Some(conflict); }
                        else if let Some(tx) = conflict.reply.lock().unwrap().take() { let _ = tx.send(false); }
                    },
                    sftp::SftpEvent::Listing { path, entries } => {
                        tab.sftp_ui.busy = tab.sftp_ui.remaining > 0;
                        tab.sftp_ui.selected_remote = Default::default();
                        tab.sftp_ui.remote_input = path.clone();
                        tab.sftp_ui.path = path;
                        tab.sftp_ui.status = format!("{} 项", entries.len());
                        tab.sftp_ui.entries = entries;
                    },
                    sftp::SftpEvent::Progress { label, done, total } => {
                        tab.sftp_ui.status = if total == 0 {
                            format!("{label} · {}", sftp::human_size(done))
                        } else {
                            format!(
                                "{label} {:.0}% · {} / {}",
                                done as f64 * 100.0 / total as f64,
                                sftp::human_size(done),
                                sftp::human_size(total)
                            )
                        };
                    },
                    sftp::SftpEvent::Done(message) => {
                        tab.sftp_ui.remaining = tab.sftp_ui.remaining.saturating_sub(1);
                        tab.sftp_ui.busy = tab.sftp_ui.remaining > 0;
                        tab.sftp_ui.refresh_local();
                        tab.sftp_ui.status = message;
                        // Uploads change the listing, so re-read what is on
                        // screen instead of showing a stale directory.
                        if !tab.sftp_ui.path.is_empty()
                            && let Some(handle) = &tab.sftp
                        {
                            handle.send(sftp::Command::List(tab.sftp_ui.path.clone()));
                        }
                    },
                    sftp::SftpEvent::Error(err) => {
                        tab.sftp_ui.remaining = tab.sftp_ui.remaining.saturating_sub(1);
                        tab.sftp_ui.busy = tab.sftp_ui.remaining > 0;
                        tab.sftp_ui.status = err;
                    },
                }
            }
        },
        Message::WindowOpened => {},
        Message::Modifiers(m) => state.modifiers = m,
        Message::Window(id, event) => match event {
            iced::window::Event::FileDropped(local) => {
                if let Some(tab) = state.tabs.iter_mut().find(|t| t.sftp_ui.window == Some(id)) {
                    if let Some(name) = local.file_name().and_then(|n| n.to_str()) {
                        let remote = sftp::join_remote(&current_sftp_path(tab), name);
                        if let Some(handle) = &tab.sftp {
                            handle.send(sftp::Command::Upload { local, remote });
                            tab.sftp_ui.remaining += 1;
                            tab.sftp_ui.busy = true;
                        }
                    }
                }
            },
            iced::window::Event::CloseRequested if state.main_window == Some(id) => return iced::exit(),
            iced::window::Event::CloseRequested => {
                if let Some(tab) = state.tabs.iter_mut().find(|t| t.sftp_ui.window == Some(id)) {
                    tab.sftp_ui.dismiss_conflict();
                    tab.sftp_ui.window = None;
                }
                return iced::window::close(id);
            },
            _ => {},
        },
        Message::SftpToggle => {
            if let Some(tab) = state.tabs.get_mut(state.active) {
                return open_sftp_window(tab);
            }
        },
        Message::Files(id, message) => {
            let Some(tab) = state.tabs.iter_mut().find(|t| t.id == id) else { return Task::none(); };
            let ui = &mut tab.sftp_ui;
            use files::{Message as F, Side};
            match message {
                F::Answer(answer) => {
                    if let Some(c) = ui.conflict.take() { if let Some(tx) = c.reply.lock().unwrap().take() { let _ = tx.send(answer); } }
                },
                F::Dismiss => ui.menu = None,
                F::Click(side, i) | F::Context(side, i) => {
                    let selection = if side == Side::Local { &mut ui.selected_local } else { &mut ui.selected_remote };
                    if matches!(message, F::Context(..)) {
                        if !selection.indices.contains(&i) { selection.click(i, Default::default()); }
                        ui.menu = Some((side,i));
                    } else { selection.click(i, state.modifiers); ui.menu = None; }
                },
                F::Transfer(side) if !ui.busy => {
                    let entries = if side == Side::Local { &ui.local_entries } else { &ui.entries };
                    let selection = if side == Side::Local { &ui.selected_local } else { &ui.selected_remote };
                    if let Some(handle) = &tab.sftp {
                        for i in &selection.indices {
                            if let Some(e) = entries.get(*i) {
                                let remote = sftp::join_remote(if ui.path.is_empty() { "." } else { &ui.path }, &e.name);
                                let local = ui.local.join(&e.name);
                                handle.send(if side == Side::Local { sftp::Command::Upload { local, remote } } else { sftp::Command::Download { remote, local } });
                                ui.remaining += 1;
                            }
                        }
                        ui.busy = ui.remaining > 0; ui.status = format!("传输队列: {} 项",ui.remaining);
                    }
                    ui.menu = None;
                },
                F::Path(side, value) => if side == Side::Local { ui.local_input = value } else { ui.remote_input = value },
                F::Refresh | F::Up(_) | F::Open(..) | F::Go(_) if !ui.busy => {
                    let side = match message { F::Up(s) | F::Open(s,_) | F::Go(s) => Some(s), _ => None };
                    if side != Some(Side::Remote) {
                        match message {
                            F::Up(_) => { if let Some(p) = ui.local.parent() { ui.local = p.into(); } },
                            F::Open(_,i) => { if let Some(e) = ui.local_entries.get(i).filter(|e| e.is_dir) { ui.local = ui.local.join(&e.name); } },
                            F::Go(_) => ui.local = PathBuf::from(&ui.local_input), _ => {},
                        }
                        ui.refresh_local();
                    }
                    if side != Some(Side::Local) {
                        let path = if ui.path.is_empty() { ".".to_string() } else { ui.path.clone() };
                        let target = match message {
                            F::Up(_) => sftp::parent_remote(&path),
                            F::Open(_,i) => { let Some(e) = ui.entries.get(i).filter(|e| e.is_dir) else { return Task::none(); }; sftp::join_remote(&path,&e.name) },
                            F::Go(_) => ui.remote_input.clone(), _ => path,
                        };
                        if let Some(handle) = &tab.sftp { ui.busy = true; handle.send(sftp::Command::List(target)); }
                    }
                    ui.menu = None;
                },
                _ => {},
            }
        },
        Message::OpenUtility(target) => match target {
            "local" => state.open_local(),
            "selftest" => state.open_selftest(),
            _ => {},
        },
        Message::OpenSession(id) => state.open_session(id),
        Message::SelectTab(index) => {
            if index < state.tabs.len() {
                state.active = index;
            }
        },
        Message::CloseTab(index) => {
            if index < state.tabs.len() {
                let mut tab = state.tabs.remove(index);
                tab.sftp_ui.dismiss_conflict();
                let window = tab.sftp_ui.window;
                if state.active >= state.tabs.len() && !state.tabs.is_empty() {
                    state.active = state.tabs.len() - 1;
                }
                if let Some(id) = window { return iced::window::close(id); }
            }
        },
        Message::NewSession => {
            let group = state.groups.first().map(|(name, _)| name.as_str());
            state.draft = Some(Draft::new(group));
            state.pending_delete = None;
            return iced::widget::operation::focus(draft_input_id(DraftField::Label));
        },
        Message::DraftEdited(field, value) => {
            if let Some(draft) = &mut state.draft {
                *draft.field_mut(field) = value;
            }
        },
        Message::DraftAuth(use_key) => {
            if let Some(draft) = &mut state.draft {
                draft.use_key = use_key;
            }
        },
        Message::DraftSave => {
            let Some(draft) = state.draft.clone() else {
                return Task::none();
            };
            let edited = draft.into_new_session();
            let new_session = match edited {
                Ok(session) => session,
                Err(err) => {
                    if let Some(draft) = &mut state.draft {
                        draft.error = Some(err);
                    }
                    return Task::none();
                },
            };

            let id = match draft.editing {
                Some(id) => match state.store.update(id, &new_session) {
                    Ok(()) => id,
                    Err(err) => {
                        if let Some(draft) = &mut state.draft {
                            draft.error = Some(err);
                        }
                        return Task::none();
                    },
                },
                None => match state.store.insert(&new_session) {
                    Ok(id) => id,
                    Err(err) => {
                        if let Some(draft) = &mut state.draft {
                            draft.error = Some(err);
                        }
                        return Task::none();
                    },
                },
            };

            // Secrets go to the Keychain, keyed by the row id we just got. An
            // empty field while editing means "leave the stored one alone".
            let secret_result = if draft.secret.is_empty() {
                Ok(())
            } else if draft.use_key {
                secrets::set_passphrase(id, &draft.secret)
            } else {
                secrets::set_password(id, &draft.secret)
            };
            if let Err(err) = secret_result {
                if let Some(draft) = &mut state.draft {
                    draft.error = Some(err);
                }
                // The row is already saved; keep the form open so the user can
                // fix the secret without retyping everything.
                state.reload_groups();
                return Task::none();
            }

            state.draft = None;
            // Rename open tabs so the title tracks the edited label.
            for tab in &mut state.tabs {
                if tab.session_id == Some(id) {
                    tab.title = new_session.label.clone();
                }
            }
            state.reload_groups();
        },
        Message::DraftClose => state.draft = None,
        Message::EditSession(id) => {
            if let Some(session) = state.find_session(id) {
                state.draft = Some(Draft::from_session(&session));
                state.pending_delete = None;
                return iced::widget::operation::focus(draft_input_id(DraftField::Label));
            }
        },
        Message::AskDelete(id) => state.pending_delete = Some(id),
        Message::CancelDelete => state.pending_delete = None,
        Message::ConfirmDelete => {
            if let Some(id) = state.pending_delete.take() {
                if let Err(err) = state.store.delete(id) {
                    state.store_warning = Some(err);
                } else {
                    secrets::forget(id);
                    let mut close = Vec::new();
                    for tab in state.tabs.iter_mut().filter(|t| t.session_id == Some(id)) {
                        tab.sftp_ui.dismiss_conflict();
                        if let Some(window) = tab.sftp_ui.window { close.push(iced::window::close(window)); }
                    }
                    // Close tabs that belong to the removed session.
                    state.tabs.retain(|t| t.session_id != Some(id));
                    if state.active >= state.tabs.len() {
                        state.active = state.tabs.len().saturating_sub(1);
                    }
                    state.reload_groups();
                    return Task::batch(close);
                }
            }
        },
    }
    Task::none()
}

/// Expands a leading `~/`, since our key paths come from the database rather
/// than from a shell.
/// Remote directory the panel is showing; `.` stands in until the first
/// listing resolves an absolute path.
fn open_sftp_window(tab: &mut Tab) -> Task<Message> {
    if let Some(id) = tab.sftp_ui.window {
        return iced::window::gain_focus(id);
    }
    let (id, task) = iced::window::open(iced::window::Settings {
        size: iced::Size::new(1100.0, 720.0),
        exit_on_close_request: false,
        ..Default::default()
    });
    tab.sftp_ui.window = Some(id);
    tab.sftp_ui.refresh_local();
    if let Some(handle) = &tab.sftp {
        if !tab.sftp_ui.busy {
            tab.sftp_ui.busy = true;
            handle.send(sftp::Command::List(current_sftp_path(tab)));
        }
    }
    task.map(|_| Message::WindowOpened)
}

fn current_sftp_path(tab: &Tab) -> String {
    if tab.sftp_ui.path.is_empty() {
        ".".to_string()
    } else {
        tab.sftp_ui.path.clone()
    }
}

fn shellexpand(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return format!("{}/{}", home.to_string_lossy(), rest);
    }
    path.to_string()
}

#[derive(Clone)]
struct SshStream {
    id: u64,
    /// Distinguishes successive connections of the same tab, so a reconnect
    /// produces a new subscription instead of reusing the dead channel.
    generation: u64,
    rx: Arc<Mutex<UnboundedReceiver<SshEvent>>>,
}

impl Hash for SshStream {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        self.generation.hash(state);
    }
}

fn ssh_stream(s: &SshStream) -> iced::futures::stream::BoxStream<'static, (u64, SshEvent)> {
    use iced::futures::StreamExt;

    let id = s.id;
    let rx = s.rx.clone();
    iced::stream::channel(128, async move |mut out| {
        use iced::futures::SinkExt;

        loop {
            let mut guard = rx.lock().await;
            match guard.recv().await {
                Some(event) => {
                    if out.send((id, event)).await.is_err() {
                        break;
                    }
                },
                None => break,
            }
        }
    })
    .boxed()
}

#[derive(Clone)]
struct SftpStream {
    id: u64,
    /// Changes when the worker is rebuilt on reconnect.
    generation: u64,
    rx: Arc<Mutex<UnboundedReceiver<sftp::SftpEvent>>>,
}

impl Hash for SftpStream {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        self.generation.hash(state);
    }
}

fn sftp_stream(s: &SftpStream) -> iced::futures::stream::BoxStream<'static, (u64, sftp::SftpEvent)> {
    use iced::futures::StreamExt;

    let id = s.id;
    let rx = s.rx.clone();
    iced::stream::channel(64, async move |mut out| {
        use iced::futures::SinkExt;

        loop {
            let mut guard = rx.lock().await;
            match guard.recv().await {
                Some(event) => {
                    if out.send((id, event)).await.is_err() {
                        break;
                    }
                },
                None => break,
            }
        }
    })
    .boxed()
}

fn subscription(state: &App) -> Subscription<Message> {
    let mut subs: Vec<Subscription<Message>> = Vec::new();

    for tab in &state.tabs {
        subs.push(tab.term.subscription().map(Message::Term));

        if let Some(rx) = &tab.events {
            subs.push(
                Subscription::run_with(
                    SshStream {
                        id: tab.id,
                        generation: tab.generation,
                        rx: rx.clone(),
                    },
                    ssh_stream,
                )
                .map(|(id, event)| Message::Ssh(id, event)),
            );
        }

        if let Some(rx) = &tab.sftp_events {
            subs.push(
                Subscription::run_with(
                    SftpStream {
                        id: tab.id,
                        generation: tab.sftp_generation,
                        rx: rx.clone(),
                    },
                    sftp_stream,
                )
                .map(|(id, event)| Message::Sftp(id, event)),
            );
        }
    }

    subs.push(iced::window::events().map(|(id,event)| Message::Window(id,event)));
    subs.push(iced::event::listen_with(|event,_,_| match event {
        iced::Event::Keyboard(iced::keyboard::Event::ModifiersChanged(m)) => Some(Message::Modifiers(m)), _ => None,
    }));

    Subscription::batch(subs)
}

/// Colour of the status dot next to a session, if that session is open.
fn session_dot(state: &App, session_id: i64) -> Option<Color> {
    state
        .tabs
        .iter()
        .find(|t| t.session_id == Some(session_id))
        .map(|t| t.status.color())
}

fn sidebar(state: &App) -> Element<'_, Message> {
    let mut col = column![].spacing(2).padding(10);

    if let Some(warning) = &state.store_warning {
        col = col.push(text(format!("⚠ {warning}")).size(10).color(DANGER));
        col = col.push(container(text("")).height(Length::Fixed(6.0)));
    }

    if let Some(id) = state.pending_delete {
        let name = state
            .find_session(id)
            .map(|s| s.label)
            .unwrap_or_else(|| format!("#{id}"));
        col = col.push(container(
            column![
                text(format!("删除「{name}」？")).size(11).color(FG),
                text("密码也会从钥匙串移除").size(10).color(DIM),
                row![
                    button(text("确认删除").size(11).color(DANGER))
                        .on_press(Message::ConfirmDelete)
                        .padding([3, 8])
                        .style(flat_button),
                    button(text("取消").size(11).color(DIM))
                        .on_press(Message::CancelDelete)
                        .padding([3, 8])
                        .style(flat_button),
                ]
                .spacing(4),
            ]
            .spacing(4),
        )
        .padding(6)
        .style(|_| container::Style {
            background: Some(Color::from_rgba(0.976, 0.149, 0.447, 0.12).into()),
            border: iced::Border {
                radius: 4.0.into(),
                ..Default::default()
            },
            ..Default::default()
        }));
        col = col.push(container(text("")).height(Length::Fixed(6.0)));
    }

    for (group, sessions) in &state.groups {
        col = col.push(
            text(format!("{group} · {}", sessions.len()))
                .size(11)
                .color(DIM),
        );
        for session in sessions {
            let dot = session_dot(state, session.id);
            let label = row![
                text("●")
                    .size(8)
                    .color(dot.unwrap_or(Color::TRANSPARENT)),
                column![
                    text(session.label.clone()).size(12).color(FG),
                    text(session.subtitle()).size(10).color(DIM),
                ]
                .spacing(1),
            ]
            .spacing(6);

            col = col.push(
                row![
                    button(label)
                        .on_press(Message::OpenSession(session.id))
                        .width(Length::Fill)
                        .style(flat_button),
                    button(text("✎").size(10).color(DIM))
                        .on_press(Message::EditSession(session.id))
                        .padding([2, 3])
                        .style(ghost_button),
                    button(text("×").size(11).color(DIM))
                        .on_press(Message::AskDelete(session.id))
                        .padding([2, 3])
                        .style(ghost_button),
                ]
                .spacing(0)
                .align_y(iced::Alignment::Center),
            );
        }
        col = col.push(container(text("")).height(Length::Fixed(8.0)));
    }

    col = col.push(
        button(text("＋ 添加会话").size(12).color(ACCENT))
            .on_press(Message::NewSession)
            .width(Length::Fill)
            .style(flat_button),
    );
    col = col.push(container(text("")).height(Length::Fixed(6.0)));
    col = col.push(
        button(text("本地 shell").size(12).color(DIM))
            .on_press(Message::OpenUtility("local"))
            .width(Length::Fill)
            .style(flat_button),
    );
    col = col.push(
        button(text("渲染自检").size(12).color(DIM))
            .on_press(Message::OpenUtility("selftest"))
            .width(Length::Fill)
            .style(flat_button),
    );

    col = col.push(container(text("")).height(Length::Fixed(12.0)));
    col = col.push(
        text(format!("{} 个会话", state.tabs.len()))
            .size(10)
            .color(DIM),
    );

    container(col)
        .width(Length::Fixed(196.0))
        .height(Length::Fill)
        .style(|_| container::Style {
            background: Some(SIDEBAR.into()),
            ..Default::default()
        })
        .into()
}

fn draft_input_id(field: DraftField) -> iced::widget::Id {
    iced::widget::Id::new(match field {
        DraftField::Group => "draft-group",
        DraftField::Label => "draft-label",
        DraftField::Host => "draft-host",
        DraftField::Port => "draft-port",
        DraftField::User => "draft-user",
        DraftField::KeyPath => "draft-key-path",
        DraftField::Secret => "draft-secret",
    })
}

fn input_field<'a>(
    title: &'a str,
    value: &'a str,
    field: DraftField,
    placeholder: &'a str,
    secure: bool,
) -> Element<'a, Message> {
    let mut input = iced::widget::text_input(placeholder, value)
        .id(draft_input_id(field))
        .size(12)
        .padding(6)
        .on_input(move |v| Message::DraftEdited(field, v))
        .style(dark_input);
    if secure {
        input = input.secure(true);
    }
    column![text(title).size(10).color(DIM), input]
        .spacing(2)
        .into()
}

fn dark_input(_theme: &iced::Theme, _status: iced::widget::text_input::Status) -> iced::widget::text_input::Style {
    iced::widget::text_input::Style {
        background: Color::from_rgb(0.075, 0.078, 0.070).into(),
        border: iced::Border {
            radius: 3.0.into(),
            width: 1.0,
            color: Color::from_rgba(1.0, 1.0, 1.0, 0.08),
        },
        icon: DIM,
        placeholder: DIM,
        value: FG,
        selection: Color::from_rgba(0.651, 0.886, 0.180, 0.35),
    }
}

fn form_panel(state: &App) -> Element<'_, Message> {
    let Some(draft) = &state.draft else {
        return container(text("")).into();
    };

    let auth_buttons = row![
        button(text("私钥").size(11).color(if draft.use_key { BG } else { DIM }))
            .on_press(Message::DraftAuth(true))
            .padding([3, 10])
            .style(move |theme, status| toggle_style(theme, status, draft.use_key)),
        button(text("密码").size(11).color(if draft.use_key { DIM } else { BG }))
            .on_press(Message::DraftAuth(false))
            .padding([3, 10])
            .style(move |theme, status| toggle_style(theme, status, !draft.use_key)),
    ]
    .spacing(2);

    let mut col = column![
        text(draft.title()).size(12).color(FG),
        input_field("名称（留空用主机名）", &draft.label, DraftField::Label, "prod-web-01", false),
        input_field("主机", &draft.host, DraftField::Host, "10.0.0.1 或 example.com", false),
        row![
            column![
                input_field("端口", &draft.port, DraftField::Port, "22", false)
            ]
            .width(Length::Fixed(64.0)),
            column![
                input_field("用户", &draft.user, DraftField::User, "root", false)
            ]
            .width(Length::Fill),
        ]
        .spacing(6),
        input_field("分组", &draft.group, DraftField::Group, "生产环境", false),
        text("认证").size(10).color(DIM),
        auth_buttons,
    ]
    .spacing(8);

    if draft.use_key {
        col = col.push(input_field(
            "私钥路径",
            &draft.key_path,
            DraftField::KeyPath,
            "~/.ssh/id_ed25519",
            false,
        ));
    }
    let secret_title = match (draft.editing.is_some(), draft.use_key) {
        (true, true) => "新私钥口令（留空不改）",
        (true, false) => "新密码（留空不改）",
        (false, true) => "私钥口令（没有就留空）",
        (false, false) => "密码（存入钥匙串）",
    };
    col = col.push(input_field(
        secret_title,
        &draft.secret,
        DraftField::Secret,
        "••••••••",
        !draft.use_key,
    ));

    if let Some(err) = &draft.error {
        col = col.push(text(err.clone()).size(10).color(DANGER));
    }

    col = col.push(
        row![
            button(text("保存").size(11).color(BG))
                .on_press(Message::DraftSave)
                .padding([5, 14])
                .style(primary_button),
            button(text("取消").size(11).color(DIM))
                .on_press(Message::DraftClose)
                .padding([5, 10])
                .style(flat_button),
        ]
        .spacing(4),
    );

    container(col)
        .width(Length::Fixed(248.0))
        .height(Length::Fill)
        .padding(12)
        .style(|_| container::Style {
            background: Some(SIDEBAR.into()),
            border: iced::Border {
                width: 1.0,
                color: Color::from_rgba(1.0, 1.0, 1.0, 0.06),
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
}

fn primary_button(_theme: &iced::Theme, status: button::Status) -> button::Style {
    let alpha = match status {
        button::Status::Hovered | button::Status::Pressed => 0.85,
        _ => 1.0,
    };
    button::Style {
        background: Some(
            Color {
                a: alpha,
                ..ACCENT
            }
            .into(),
        ),
        text_color: BG,
        border: iced::Border {
            radius: 3.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn toggle_style(_theme: &iced::Theme, _status: button::Status, on: bool) -> button::Style {
    button::Style {
        background: Some(
            if on {
                ACCENT
            } else {
                Color::from_rgba(1.0, 1.0, 1.0, 0.06)
            }
            .into(),
        ),
        text_color: FG,
        border: iced::Border {
            radius: 3.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn tabbar(state: &App) -> Element<'_, Message> {
    let mut bar = row![].spacing(1).padding([0, 4]);
    for (index, tab) in state.tabs.iter().enumerate() {
        let is_active = index == state.active;
        let label = row![
            text("●").size(8).color(tab.status.color()),
            text(tab.title.clone())
                .size(11)
                .color(if is_active { FG } else { DIM }),
            button(text("×").size(11).color(DIM))
                .on_press(Message::CloseTab(index))
                .padding([0, 4])
                .style(ghost_button),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center);

        bar = bar.push(
            button(label)
                .on_press(Message::SelectTab(index))
                .padding([5, 10])
                .style(move |theme, status| tab_style(theme, status, is_active)),
        );
    }
    // Only remote tabs can host a file panel; local shells have no session.
    if state
        .tabs
        .get(state.active)
        .map(|tab| tab.session_id.is_some())
        .unwrap_or(false)
    {
        let open = state
            .tabs
            .get(state.active)
            .map(|tab| tab.sftp_ui.window.is_some())
            .unwrap_or(false);
        bar = bar.push(
            button(
                text("SFTP")
                    .size(10)
                    .color(if open { ACCENT } else { DIM }),
            )
            .on_press(Message::SftpToggle)
            .padding([4, 10])
            .style(ghost_button),
        );
    }

    container(bar)
        .width(Length::Fill)
        .height(Length::Fixed(30.0))
        .style(|_| container::Style {
            background: Some(BAR.into()),
            ..Default::default()
        })
        .into()
}

fn view(state: &App, window: iced::window::Id) -> Element<'_, Message> {
    if let Some(tab) = state.tabs.iter().find(|t| t.sftp_ui.window == Some(window)) {
        return files::view(&tab.sftp_ui).map(move |m| Message::Files(tab.id,m));
    }
    let terminal: Element<'_, Message> = if let Some(tab) = state.tabs.get(state.active) {
        TerminalView::show(&tab.term).map(Message::Term)
    } else {
        container(text("没有打开的会话").size(12).color(DIM))
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    };

    let workspace = row![terminal].height(Length::Fill);

    let main = column![tabbar(state), workspace].height(Length::Fill);

    let mut body = row![sidebar(state), main].height(Length::Fill);
    if state.draft.is_some() {
        body = body.push(form_panel(state));
    }

    let active_tab = state.tabs.get(state.active);
    let status_text = match active_tab.map(|t| &t.status) {
        Some(Status::Failed(err)) => err.clone(),
        Some(status) => status.label(),
        None => "—".to_string(),
    };
    let fingerprint = active_tab
        .and_then(|t| t.fingerprint.clone())
        .unwrap_or_else(|| "未校验".to_string());
    let fingerprint_note = active_tab
        .and_then(|t| t.fingerprint_note)
        .unwrap_or("");
    // Retry is offered whenever the tab has a session but no live connection.
    let can_retry = active_tab
        .map(|t| {
            t.session_id.is_some()
                && matches!(t.status, Status::Closed | Status::Failed(_))
        })
        .unwrap_or(false);

    let mut status_row = row![
        text("●")
            .size(9)
            .color(active_tab.map(|t| t.status.color()).unwrap_or(DIM)),
        text(status_text).size(10).color(FG),
        text(fingerprint).size(10).color(DIM),
        text(fingerprint_note).size(10).color(ACCENT),
        text("keepalive 30s / max 4").size(10).color(DIM),
        text("alacritty 0.25 · iced 0.14").size(10).color(DIM),
    ]
    .spacing(12)
    .align_y(iced::Alignment::Center);

    if can_retry {
        status_row = status_row.push(
            button(text("↻ 重连").size(10).color(ACCENT))
                .on_press(Message::Retry)
                .padding([1, 6])
                .style(flat_button),
        );
    }

    let status = container(status_row)
    .width(Length::Fill)
    .height(Length::Fixed(24.0))
    .padding([0, 10])
    .style(|_| container::Style {
        background: Some(BAR.into()),
        ..Default::default()
    });

    container(column![body, status])
    .width(Length::Fill)
    .height(Length::Fill)
    .style(|_| container::Style {
        background: Some(BG.into()),
        ..Default::default()
    })
    .into()
}

fn flat_button(_theme: &iced::Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered | button::Status::Pressed => {
            Some(Color::from_rgba(1.0, 1.0, 1.0, 0.07))
        },
        _ => None,
    };
    button::Style {
        background: bg.map(Into::into),
        text_color: FG,
        border: iced::Border {
            radius: 3.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn ghost_button(_theme: &iced::Theme, _status: button::Status) -> button::Style {
    button::Style {
        background: None,
        text_color: DIM,
        ..Default::default()
    }
}

fn tab_style(_theme: &iced::Theme, _status: button::Status, is_active: bool) -> button::Style {
    button::Style {
        background: Some(
            if is_active {
                BG
            } else {
                Color::TRANSPARENT
            }
            .into(),
        ),
        text_color: FG,
        border: iced::Border {
            radius: 0.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn app_theme(_state: &App) -> iced::Theme {
    iced::Theme::Dark
}

fn main() -> iced::Result {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    let _ = RUNTIME.set(rt);

    iced::daemon(App::boot, update, view)
        .title(|state: &App, id| state.tabs.iter().find(|t| t.sftp_ui.window == Some(id)).map(|t| format!("SFTP · {}",t.title)).unwrap_or_else(|| "SSH 工作台".into()))
        .subscription(subscription)
        .theme(|state: &App, _| app_theme(state))
        .run()
}
