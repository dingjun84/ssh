mod ssh;

use std::hash::{Hash, Hasher};
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
    Failed(String),
}

impl Status {
    fn label(&self) -> &str {
        match self {
            Status::Local => "本地",
            Status::Connecting => "连接中",
            Status::Connected => "已连接",
            Status::Closed => "已断开",
            Status::Failed(_) => "失败",
        }
    }

    fn color(&self) -> Color {
        match self {
            Status::Local | Status::Connected => ACCENT,
            Status::Connecting => WARN,
            Status::Closed => DIM,
            Status::Failed(_) => DANGER,
        }
    }
}

struct Tab {
    id: u64,
    title: String,
    term: Terminal,
    transport: Option<Arc<ssh::SshTransport>>,
    events: Option<Arc<Mutex<UnboundedReceiver<SshEvent>>>>,
    status: Status,
}

struct App {
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
}

#[derive(Debug, Clone)]
enum Message {
    Term(iced_term::Event),
    Ssh(u64, SshEvent),
    Open(&'static str),
    SelectTab(usize),
    CloseTab(usize),
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

impl App {
    fn boot() -> (Self, Task<Message>) {
        let mut app = App {
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
        };
        // Unattended smoke test hook: lets CI (and me) open a real session
        // without clicking through the UI.
        if std::env::var("SSH_AUTOCONNECT").is_ok() {
            app.open_demo_host();
        } else {
            app.open_local();
        }
        (app, Task::none())
    }

    fn open_demo_host(&mut self) {
        self.open_ssh(
            "43.139.142.161",
            "43.139.142.161",
            22,
            "root",
            Auth::Key {
                path: shellexpand("~/.ssh/id_rsa"),
                passphrase: None,
            },
        );
    }

    fn open_local(&mut self) {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let id = self.next_id;
        self.next_id += 1;
        self.tabs.push(Tab {
            id,
            title: "local".into(),
            term: spawn_local(id, &shell, vec![]),
            transport: None,
            events: None,
            status: Status::Local,
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
            title: "渲染自检".into(),
            term: spawn_local(
                id,
                "/bin/zsh",
                vec!["-f".to_string(), "-c".to_string(), script.to_string()],
            ),
            transport: None,
            events: None,
            status: Status::Local,
        });
        self.active = self.tabs.len() - 1;
    }

    fn open_ssh(&mut self, label: &str, host: &str, port: u16, user: &str, auth: Auth) {
        let id = self.next_id;
        self.next_id += 1;

        let (transport, rx) = ssh::spawn(
            &runtime(),
            SshRequest {
                host: host.to_string(),
                port,
                user: user.to_string(),
                auth,
                cols: 100,
                rows: 30,
            },
        );

        let term = Terminal::new_remote(id, term_settings(), transport.clone());

        self.tabs.push(Tab {
            id,
            title: label.to_string(),
            term,
            transport: Some(transport),
            events: Some(Arc::new(Mutex::new(rx))),
            status: Status::Connecting,
        });
        self.active = self.tabs.len() - 1;
    }
}

fn update(state: &mut App, message: Message) -> Task<Message> {
    match message {
        Message::Term(iced_term::Event::BackendCall(id, cmd)) => {
            if let Some(tab) = state.tabs.iter_mut().find(|t| t.id == id) {
                tab.term.handle(TermCommand::ProxyToBackend(cmd));
            }
        },
        Message::Ssh(id, event) => {
            if let Some(tab) = state.tabs.iter_mut().find(|t| t.id == id) {
                match event {
                    SshEvent::Data(bytes) => tab.term.feed(&bytes),
                    SshEvent::Connected => {
                        tab.status = Status::Connected;
                        // Unattended smoke test: SSH_PROBE feeds a command to the
                        // remote shell right after login so the whole chain can be
                        // verified without a keyboard.
                        if let Ok(probe) = std::env::var("SSH_PROBE")
                            && let Some(t) = &tab.transport
                        {
                            t.write(probe.as_bytes());
                        }
                    },
                    SshEvent::Error(err) => {
                        tab.status = Status::Failed(err.clone());
                        let message =
                            format!("\r\n\x1b[1;31m[连接错误] {err}\x1b[0m\r\n");
                        tab.term.feed(message.as_bytes());
                    },
                    SshEvent::Closed => {
                        if !matches!(tab.status, Status::Failed(_)) {
                            tab.status = Status::Closed;
                        }
                    },
                }
            }
        },
        Message::Open(target) => match target {
            "local" => state.open_local(),
            "selftest" => state.open_selftest(),
            "demo" => state.open_demo_host(),
            _ => {},
        },
        Message::SelectTab(index) => {
            if index < state.tabs.len() {
                state.active = index;
            }
        },
        Message::CloseTab(index) => {
            if index < state.tabs.len() {
                state.tabs.remove(index);
                if state.active >= state.tabs.len() && !state.tabs.is_empty() {
                    state.active = state.tabs.len() - 1;
                }
            }
        },
    }
    Task::none()
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
    rx: Arc<Mutex<UnboundedReceiver<SshEvent>>>,
}

impl Hash for SshStream {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
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

fn subscription(state: &App) -> Subscription<Message> {
    let mut subs: Vec<Subscription<Message>> = Vec::new();

    for tab in &state.tabs {
        subs.push(tab.term.subscription().map(Message::Term));

        if let Some(rx) = &tab.events {
            subs.push(
                Subscription::run_with(
                    SshStream {
                        id: tab.id,
                        rx: rx.clone(),
                    },
                    ssh_stream,
                )
                .map(|(id, event)| Message::Ssh(id, event)),
            );
        }
    }

    Subscription::batch(subs)
}

fn sidebar(state: &App) -> Element<'_, Message> {
    let mut col = column![].spacing(2).padding(10);
    col = col.push(text("会话").size(11).color(DIM));
    col = col.push(
        button(text("本地 shell").size(12).color(FG))
            .on_press(Message::Open("local"))
            .width(Length::Fill)
            .style(flat_button),
    );
    col = col.push(
        button(text("渲染自检").size(12).color(DIM))
            .on_press(Message::Open("selftest"))
            .width(Length::Fill)
            .style(flat_button),
    );
    col = col.push(container(text("")).height(Length::Fixed(8.0)));

    col = col.push(text("生产环境 · 1").size(11).color(DIM));
    col = col.push(
        button(
            column![
                text("43.139.142.161").size(12).color(FG),
                text("root · 私钥").size(10).color(DIM),
            ]
            .spacing(1),
        )
        .on_press(Message::Open("demo"))
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
    container(bar)
        .width(Length::Fill)
        .height(Length::Fixed(30.0))
        .style(|_| container::Style {
            background: Some(BAR.into()),
            ..Default::default()
        })
        .into()
}

fn view(state: &App) -> Element<'_, Message> {
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

    let main = column![tabbar(state), terminal].height(Length::Fill);

    let active_status = state.tabs.get(state.active).map(|t| &t.status);
    let status_text = match active_status {
        Some(Status::Failed(err)) => err.clone(),
        Some(status) => status.label().to_string(),
        None => "—".to_string(),
    };

    let status = container(
        row![
            text("●")
                .size(9)
                .color(active_status.map(Status::color).unwrap_or(DIM)),
            text(status_text).size(10).color(FG),
            text("keepalive 30s / max 4").size(10).color(DIM),
            text("alacritty 0.25 · iced 0.14").size(10).color(DIM),
        ]
        .spacing(12),
    )
    .width(Length::Fill)
    .height(Length::Fixed(24.0))
    .padding([0, 10])
    .style(|_| container::Style {
        background: Some(BAR.into()),
        ..Default::default()
    });

    container(column![
        row![sidebar(state), main].height(Length::Fill),
        status
    ])
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

    iced::application(App::boot, update, view)
        .title("SSH 工作台")
        .subscription(subscription)
        .theme(app_theme)
        .window(iced::window::Settings {
            size: iced::Size::new(1280.0, 800.0),
            position: iced::window::Position::Specific(iced::Point::new(40.0, 40.0)),
            ..Default::default()
        })
        .level(iced::window::Level::AlwaysOnTop)
        .run()
}
