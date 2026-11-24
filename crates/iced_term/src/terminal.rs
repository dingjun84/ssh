use crate::actions::Action;
use crate::backend;
use crate::bindings::{Binding, BindingAction, BindingsLayout, InputKind};
use crate::font::TermFont;
use crate::settings::{FontSettings, Settings, ThemeSettings};
use crate::theme::{ColorPalette, Theme};
use crate::AlacrittyEvent;
use iced::futures::stream::BoxStream;
use iced::futures::{SinkExt, StreamExt};
use iced::widget::canvas::Cache;
use iced::Subscription;
use std::hash::{Hash, Hasher};
use std::io::Result;
use std::sync::Arc;
use tokio::sync::mpsc::{self, Receiver};
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub enum Event {
    BackendCall(u64, backend::Command),
}

#[derive(Debug, Clone)]
pub enum Command {
    ChangeTheme(Box<ColorPalette>),
    ChangeFont(FontSettings),
    AddBindings(Vec<(Binding<InputKind>, BindingAction)>),
    ProxyToBackend(backend::Command),
}

pub struct Terminal {
    pub id: u64,
    widget_id: iced::widget::Id,
    pub(crate) font: TermFont,
    pub(crate) theme: Theme,
    pub(crate) cache: Cache,
    pub(crate) bindings: BindingsLayout,
    pub(crate) backend: backend::Backend,
    backend_event_rx: Arc<Mutex<Receiver<AlacrittyEvent>>>,
}

impl Terminal {
    pub fn new(id: u64, settings: Settings) -> Result<Self> {
        let (backend_event_tx, backend_event_rx) = mpsc::channel(100);
        let theme = Theme::new(settings.theme);
        let font = TermFont::new(settings.font);

        Ok(Self {
            id,
            widget_id: iced::widget::Id::unique(),
            font,
            theme,
            bindings: BindingsLayout::default(),
            cache: Cache::default(),
            backend: backend::Backend::new(
                id,
                backend_event_tx,
                settings.backend,
            )?,
            backend_event_rx: Arc::new(Mutex::new(backend_event_rx)),
        })
    }

    pub fn widget_id(&self) -> &iced::widget::Id {
        &self.widget_id
    }

    /// Creates a terminal whose bytes are pumped in by the caller instead of
    /// a local PTY. Used for SSH sessions: the transport owns the outbound
    /// channel, and [`Terminal::feed`] delivers whatever the remote sends.
    pub fn new_remote(
        id: u64,
        settings: Settings,
        transport: std::sync::Arc<dyn backend::Transport>,
    ) -> Self {
        let (backend_event_tx, backend_event_rx) = mpsc::channel(100);
        let theme = Theme::new(settings.theme);
        let font = TermFont::new(settings.font);

        let mut terminal = Self {
            id,
            widget_id: iced::widget::Id::unique(),
            font,
            theme,
            cache: Cache::default(),
            bindings: BindingsLayout::default(),
            backend: backend::Backend::new_remote(
                id,
                backend_event_tx,
                transport,
            ),
            backend_event_rx: Arc::new(Mutex::new(backend_event_rx)),
        };

        terminal.sync_font();
        terminal
    }

    /// Pushes bytes received from the remote host through the VT parser.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.backend.feed(bytes);
        // Views read a snapshot, so publish the parsed state before redrawing.
        self.backend.sync();
        self.redraw();
    }

    /// Re-attaches the terminal to a new transport without touching the VT
    /// state. Reconnecting an SSH session uses this instead of building a new
    /// `Terminal`: the scrollback survives, and — more importantly — dropping a
    /// `Terminal` closes its event channel, which used to panic the
    /// subscription stream.
    pub fn set_transport(&mut self, transport: std::sync::Arc<dyn backend::Transport>) {
        self.backend.set_transport(transport);
    }

    /// Text currently selected in the grid, empty when nothing is selected.
    /// The host application uses this to offer a "copy" action in its own
    /// context menu.
    pub fn selection(&self) -> String {
        self.backend.selectable_content()
    }

    pub fn subscription(&self) -> Subscription<Event> {
        let data = TerminalSubscriptionData {
            id: self.id,
            event_receiver: self.backend_event_rx.clone(),
        };

        Subscription::run_with(data, terminal_subscription_stream)
    }

    pub fn handle(&mut self, cmd: Command) -> Action {
        let mut action = Action::default();

        match cmd {
            Command::ChangeTheme(color_pallete) => {
                self.theme = Theme::new(ThemeSettings::new(color_pallete));
            },
            Command::ChangeFont(font_settings) => {
                self.font = TermFont::new(font_settings);
            },
            Command::AddBindings(bindings) => {
                self.bindings.add_bindings(bindings);
            },
            Command::ProxyToBackend(cmd) => {
                action = self.backend.handle(cmd);
            },
        };

        self.sync_and_redraw();
        action
    }

    fn sync_and_redraw(&mut self) {
        self.sync_font();
        self.backend.sync();
        self.redraw();
    }

    fn sync_font(&mut self) {
        self.font.sync();
        self.backend
            .handle(backend::Command::Resize(None, Some(self.font.measure)));
    }

    fn redraw(&mut self) {
        self.cache.clear();
    }
}

#[derive(Clone)]
struct TerminalSubscriptionData {
    id: u64,
    event_receiver: Arc<Mutex<Receiver<AlacrittyEvent>>>,
}

impl Hash for TerminalSubscriptionData {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

fn terminal_subscription_stream(
    data: &TerminalSubscriptionData,
) -> BoxStream<'static, Event> {
    let id = data.id;
    let event_receiver = data.event_receiver.clone();
    iced::stream::channel(1000, async move |mut output| {
        loop {
            let mut event_receiver = event_receiver.lock().await;
            match event_receiver.recv().await {
                Some(event) => {
                    output
                        .send(Event::BackendCall(id, backend::Command::ProcessAlacrittyEvent(event)))
                        .await
                        .unwrap_or_else(|_| {
                            panic!("iced_term stream {}: sending BackendEventReceived event is failed", id)
                        });
                },
                None => {
                    // Channel closed because the `Terminal` was dropped (tab
                    // closed). Ending the stream is the correct outcome; the
                    // upstream `panic!` here wedged the entire UI event loop.
                    break;
                },
            }
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::index::{Column, Line, Point};
    use alacritty_terminal::term::TermMode;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct RecordingTransport {
        resizes: AtomicUsize,
    }

    impl backend::Transport for RecordingTransport {
        fn write(&self, _: &[u8]) {}

        fn resize(&self, _: u16, _: u16) {
            self.resizes.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn feed_updates_rendered_text_without_user_input() {
        let mut terminal = Terminal::new_remote(
            1, Settings::default(), Arc::new(RecordingTransport::default()),
        );
        terminal.backend.handle(backend::Command::Resize(
            Some(iced_core::Size::new(800.0, 240.0)),
            Some(iced_core::Size::new(10.0, 20.0)),
        ));

        for (bytes, expected) in [
            (b"hello".as_slice(), "hello"),
            (b" world".as_slice(), "hello world"),
        ] {
            terminal.feed(bytes);

            let content = terminal.backend.renderable_content();
            let actual: String = (0..expected.len())
                .map(|col| content.grid[Point::new(Line(0), Column(col))].c)
                .collect();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn feed_updates_terminal_modes_without_resizing_transport() {
        let transport = Arc::new(RecordingTransport::default());
        let mut terminal = Terminal::new_remote(
            1, Settings::default(), transport.clone(),
        );
        let initial_resizes = transport.resizes.load(Ordering::Relaxed);

        for (bytes, enabled) in [
            (b"\x1b[?2004h".as_slice(), true),
            (b"\x1b[?2004l".as_slice(), false),
        ] {
            terminal.feed(bytes);
            assert_eq!(
                terminal.backend.renderable_content().terminal_mode
                    .contains(TermMode::BRACKETED_PASTE),
                enabled,
            );
        }

        assert_eq!(transport.resizes.load(Ordering::Relaxed), initial_resizes);
    }
}
