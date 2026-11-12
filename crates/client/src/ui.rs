//! iced application: connect form -> remote screen (video + input) with a status bar.
//!
//! The remote desktop always has the size of the picture area in physical pixels (1:1, no
//! scaling): chosen on the connect screen (presets fitting the monitor, or full screen), then -
//! with "resize the remote display with the window" - following every window resize and DPI
//! change (debounced). Ctrl+Alt+Break toggles full screen.
use std::sync::Arc;

use iced::futures::channel::mpsc as fmpsc;
use iced::widget::{button, checkbox, column, container, mouse_area, pick_list, row, shader, slider, stack, text, text_input};
use iced::{Alignment, Color, Element, Length, Size, Task, Theme, window};
use protocol::{Event, Hello, Secret, StreamInfo};
use tokio::sync::mpsc::UnboundedSender;

use crate::connbar::{self, Quality};
use crate::display::{self, Choice, Monitor};
use crate::known;
use crate::session::{self, FrameSlot, InputCmd, SessionConfig, Stats, UntrustedCert, Update, VideoFrame};
use crate::video::VideoView;

#[derive(Clone, Debug, Default)]
pub struct Form {
    pub server: String,
    /// explicit pin from the command line (overrides the known-servers file); not shown in the form
    pub pin: String,
    pub user: String,
    pub password: String,
    /// position of the resolution slider (index into `display::choices`); None = full screen
    pub display: Option<u16>,
    /// follow window resizes with the remote resolution
    pub dynamic: bool,
    /// session experience (`protocol::perf`), session-only
    pub performance_flags: u32,
    /// share the clipboard with the session (text, images)
    pub clipboard: bool,
    /// where Windows key combinations (Alt+Tab, Win, ...) go
    pub keys: crate::prefs::KeyCombos,
}

pub struct Launch {
    pub form: Form,
    pub auto_connect: bool,
    pub vsync: bool,
    /// look for a newer release at start
    pub update_check: bool,
}

#[derive(Clone, Debug)]
pub enum Message {
    Server(String),
    User(String),
    Password(String),
    Connect,
    Disconnect,
    TrustRemember(bool),
    TrustAccept,
    TrustCancel,
    Session(Update),
    /// window id, monitor (logical size), scale factor
    Monitor(Option<window::Id>, Option<Size>, f32),
    Resolution(u16),
    Dynamic(bool),
    ExperiencePreset(u32),
    Effect(usize, bool),
    ShowEffects(bool),
    Clipboard(bool),
    Keys(crate::prefs::KeyCombos),
    WindowResized(window::Id, Size),
    Rescaled(f32),
    ApplyResize(u64),
    ToggleFullscreen,
    /// window maximize while the remote screen is in a window -> full screen (Windows: SC_MAXIMIZE)
    MaximizeRequested,
    /// answer of window::is_maximized after a resize (other platforms / snap)
    MaximizedCheck(bool),
    Minimize,
    /// connection bar: pin toggle, mouse over it, mouse at the top edge, delayed hide
    BarPin,
    BarHover(bool),
    BarReveal,
    BarHide(u64),
    Focused,
    Unfocused,
    /// result of the start-up release check
    UpdateFound(Option<update::Release>),
    UpdateClient,
    UpdateDone(Result<(), String>),
    /// native window handle (Windows: native remote pointer)
    Hwnd(Option<isize>),
}

enum Screen {
    Form,
    Remote,
    Trust { cert: UntrustedCert, remember: bool },
}

struct App {
    screen: Screen,
    form: Form,
    status: String,
    frame: Option<Arc<VideoFrame>>,
    frames: Arc<FrameSlot>,
    stream: Option<StreamInfo>,
    stats: Stats,
    input: Option<UnboundedSender<InputCmd>>,
    input_path: Option<u32>,
    cursor_shape: Option<Arc<protocol::CursorShape>>,
    cursor_pos: Option<protocol::CursorPos>,
    /// fingerprint the user accepted for this connection only (not remembered)
    accepted: Option<(String, String)>,
    window: Option<window::Id>,
    monitor: Option<Monitor>,
    scale: f32,
    window_size: Size,
    fullscreen: bool,
    resize_gen: u64,
    last_display: Option<protocol::DisplayRequest>,
    /// --password-env given: connect once the monitor is known
    connect_when_ready: bool,
    show_effects: bool,
    /// our window has the keyboard focus (WM_SETFOCUS / WM_KILLFOCUS)
    focused: bool,
    /// full-screen connection bar
    bar: Bar,
    quality: Quality,
    /// stats.lost / stats.recovered at the previous statistics update
    prev_loss: (u64, u64),
    /// newer release found at start; updating = download in progress
    update: Option<update::Release>,
    updating: bool,
}

/// Full-screen connection bar state (like mstsc: pinned = always shown; unpinned = hides and
/// comes back when the mouse touches the top edge).
struct Bar {
    pinned: bool,
    visible: bool,
    hover: bool,
    generation: u64,
}

impl App {
    fn new(form: Form) -> Self {
        App {
            screen: Screen::Form,
            form,
            status: String::new(),
            frame: None,
            frames: Arc::new(FrameSlot::default()),
            stream: None,
            stats: Stats::default(),
            input: None,
            input_path: None,
            cursor_shape: None,
            cursor_pos: None,
            accepted: None,
            window: None,
            monitor: None,
            scale: 1.0,
            window_size: Size::new(1280.0, 760.0),
            fullscreen: false,
            resize_gen: 0,
            last_display: None,
            connect_when_ready: false,
            show_effects: false,
            focused: true,
            bar: Bar { pinned: true, visible: true, hover: false, generation: 0 },
            quality: Quality(4),
            prev_loss: (0, 0),
            update: None,
            updating: false,
        }
    }

    /// Hide the unpinned bar after `ms` unless the mouse is over it by then.
    fn schedule_bar_hide(&mut self, ms: u64) -> Task<Message> {
        self.bar.generation += 1;
        if self.bar.pinned {
            return Task::none();
        }
        let g = self.bar.generation;
        Task::perform(async move { tokio::time::sleep(std::time::Duration::from_millis(ms)).await }, move |_| Message::BarHide(g))
    }

    fn set_fullscreen(&mut self, on: bool) -> Task<Message> {
        let Some(id) = self.window else { return Task::none() };
        self.fullscreen = on;
        if on {
            self.bar.visible = true;
            let hide = self.schedule_bar_hide(2000);
            // a maximized window is restored first, so leaving full screen gives the old window back
            return Task::batch([window::maximize::<Message>(id, false).chain(window::set_mode(id, window::Mode::Fullscreen)), hide]);
        }
        window::set_mode(id, window::Mode::Windowed)
    }

    /// Lock keys (Caps/Num/Scroll) of the remote session := local state (connect, focus).
    fn sync_leds(&self) {
        if let (Some(i), Some(leds)) = (&self.input, local_leds()) {
            let _ = i.send(InputCmd::Event(protocol::InputEvent::Sync { leds }));
        }
    }

    /// Windows: hands the remote pointer (shape, visibility, picture rectangle) to the OS cursor.
    #[cfg(windows)]
    fn sync_native_cursor(&self) {
        let remote = matches!(self.screen, Screen::Remote) && self.input.is_some();
        let (picture, ratio) = match (&self.frame, remote) {
            (Some(f), true) => {
                let area = display::video_area(self.window_size, self.fullscreen);
                let r = crate::video::fit(iced::Rectangle { x: 0.0, y: 0.0, width: area.width, height: area.height }, f.width, f.height);
                let s = self.scale;
                let rect = ((r.x * s).round() as i32, (r.y * s).round() as i32, (r.width * s).round() as i32, (r.height * s).round() as i32);
                (Some(rect), r.width * s / f.width as f32)
            }
            _ => (None, 1.0),
        };
        let visible = self.cursor_pos.map(|p| p.visible).unwrap_or(false);
        crate::native_cursor::update(self.cursor_shape.as_deref(), visible, picture, ratio);
    }

    fn choice(&self) -> Choice {
        let c = display::choices(self.monitor);
        match self.form.display {
            Some(i) => c.get(i as usize).copied().unwrap_or(Choice::FullScreen),
            None => Choice::FullScreen,
        }
    }

    /// After a resize / rescale: ask for the new remote size once the window settled.
    fn schedule_resize(&mut self) -> Task<Message> {
        if !self.form.dynamic || self.input.is_none() || !matches!(self.screen, Screen::Remote) {
            return Task::none();
        }
        self.resize_gen += 1;
        let g = self.resize_gen;
        Task::perform(async { tokio::time::sleep(std::time::Duration::from_millis(450)).await }, move |_| Message::ApplyResize(g))
    }

    fn apply_resize(&mut self) {
        if !self.form.dynamic || !matches!(self.screen, Screen::Remote) {
            return;
        }
        let (w, h) = display::physical(display::video_area(self.window_size, self.fullscreen), self.scale);
        let req = display::request(w, h, self.scale);
        if self.last_display == Some(req) {
            return;
        }
        if let Some(i) = &self.input {
            let _ = i.send(InputCmd::Display(req));
            self.last_display = Some(req);
            self.status = format!("resizing remote display to {}x{} ({}%)", req.width, req.height, req.scale_percent);
        }
    }

    fn pin_for(&self, server: &str) -> String {
        if !self.form.pin.trim().is_empty() {
            return self.form.pin.trim().to_string();
        }
        if let Some((s, fp)) = &self.accepted
            && s == server
        {
            return fp.clone();
        }
        known::get(server).unwrap_or_default()
    }

    fn connect(&mut self) -> Task<Message> {
        let server = match crate::resolve(&self.form.server) {
            Some(s) => s,
            None => {
                self.status = format!("cannot resolve '{}'", self.form.server.trim());
                return Task::none();
            }
        };
        let choice = self.choice();
        let (w, h) = display::size_of(choice, self.monitor);
        let req = display::request(w, h, self.scale);
        self.fullscreen = choice == Choice::FullScreen;
        self.last_display = Some(req);
        let saved = crate::prefs::Prefs {
            performance_flags: self.form.performance_flags,
            display: self.form.display,
            dynamic: self.form.dynamic,
            clipboard: self.form.clipboard,
            keys: self.form.keys,
        };
        if let Err(e) = crate::prefs::save(self.form.server.trim(), &saved) {
            eprintln!("could not save connection settings: {e}");
        }
        let cfg = SessionConfig {
            server,
            pin: self.pin_for(self.form.server.trim()),
            hello: Hello {
                width: req.width as u16,
                height: req.height as u16,
                refresh_hz: req.refresh_hz as u16,
                client_name: hostname(),
                performance_flags: self.form.performance_flags,
            },
            display: req,
            clipboard: self.form.clipboard.then_some(crate::clipboard::Backend::System),
            domain: session::split_user(&self.form.user).0,
            user: session::split_user(&self.form.user).1,
            password: Secret::from_str(&self.form.password),
            record: None,
        };
        let (tx, rx) = fmpsc::unbounded::<Update>();
        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
        self.input = Some(in_tx);
        self.frames = Arc::new(FrameSlot::default());
        self.frame = None;
        self.stream = None;
        self.input_path = None;
        self.cursor_shape = None;
        self.cursor_pos = None;
        let frames = self.frames.clone();
        std::thread::Builder::new()
            .name("fastrdp-session".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio");
                let tx2 = tx.clone();
                let res = rt.block_on(session::run(
                    cfg,
                    move |u| {
                        let _ = tx2.unbounded_send(u);
                    },
                    frames,
                    in_rx,
                ));
                if let Err(e) = res {
                    let u = match e.downcast::<UntrustedCert>() {
                        Ok(c) => Update::Untrusted(*c),
                        Err(e) => Update::Ended(e.to_string()),
                    };
                    let _ = tx.unbounded_send(u);
                }
            })
            .expect("session thread");
        self.status = format!("connecting to {server} ...");
        self.screen = Screen::Remote;
        // window: picture area = remote desktop, 1:1
        let sizing = match self.window {
            Some(id) if self.fullscreen => window::set_mode(id, window::Mode::Fullscreen),
            Some(id) => window::set_mode::<Message>(id, window::Mode::Windowed)
                .chain(window::maximize(id, false))
                .chain(window::resize(id, display::window_for(req.width, req.height, self.scale))),
            None => Task::none(),
        };
        Task::batch([Task::run(rx, Message::Session), sizing])
    }
}

/// Local lock-key state (`protocol::leds`). Windows only; elsewhere the toggles are not readable
/// through iced/winit, so no sync is sent.
fn local_leds() -> Option<u8> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CAPITAL, VK_NUMLOCK, VK_SCROLL};
        let on = |vk: u16| unsafe { GetKeyState(vk as i32) } & 1 != 0; // SAFETY: trivial query
        let mut l = 0;
        if on(VK_CAPITAL) {
            l |= protocol::leds::CAPS_LOCK;
        }
        if on(VK_NUMLOCK) {
            l |= protocol::leds::NUM_LOCK;
        }
        if on(VK_SCROLL) {
            l |= protocol::leds::SCROLL_LOCK;
        }
        Some(l)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

pub fn hostname() -> String {
    std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "fastrdp-client".into())
}

fn wipe_string(s: &mut String) {
    // SAFETY: zero bytes keep the String valid UTF-8.
    unsafe { protocol::wipe_u8(s.as_bytes_mut()) };
    s.clear();
}

fn native_pointer_available() -> bool {
    #[cfg(windows)]
    {
        crate::native_cursor::available()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn update(app: &mut App, msg: Message) -> Task<Message> {
    let task = update_inner(app, msg);
    #[cfg(windows)]
    {
        app.sync_native_cursor();
        use crate::prefs::KeyCombos;
        let remote = matches!(app.screen, Screen::Remote) && app.input.is_some();
        // focus / foreground are checked per key in the hook (like mstsc)
        let capture = remote && (app.form.keys == KeyCombos::Remote || (app.form.keys == KeyCombos::FullScreen && app.fullscreen));
        crate::keyhook::configure(app.input.clone(), capture);
        crate::maximize::set_active(remote && !app.fullscreen);
    }
    task
}

fn update_inner(app: &mut App, msg: Message) -> Task<Message> {
    match msg {
        Message::Server(s) => {
            // remembered settings of that server (experience, resolution)
            if let Some(p) = crate::prefs::get(&s) {
                app.form.performance_flags = p.performance_flags;
                app.form.display = p.display;
                app.form.dynamic = p.dynamic;
                app.form.clipboard = p.clipboard;
                app.form.keys = p.keys;
            }
            app.form.server = s;
        }
        Message::ExperiencePreset(f) => app.form.performance_flags = f,
        Message::Effect(i, on) => app.form.performance_flags = protocol::perf::set_effect(app.form.performance_flags, i, on),
        Message::ShowEffects(b) => app.show_effects = b,
        Message::Clipboard(b) => app.form.clipboard = b,
        Message::Keys(k) => app.form.keys = k,
        Message::User(s) => app.form.user = s,
        Message::Password(s) => app.form.password = s,
        Message::Connect => return app.connect(),
        Message::Monitor(id, size, scale) => {
            app.window = id;
            app.scale = if scale > 0.0 { scale } else { 1.0 };
            app.monitor = size.map(|logical| Monitor { logical, scale: app.scale });
            if let Some(id) = id {
                let hwnd = window::run(id, |w| {
                    use raw_window_handle::RawWindowHandle;
                    match w.window_handle().map(|h| h.as_raw()) {
                        #[cfg(windows)]
                        Ok(RawWindowHandle::Win32(h)) => Some(h.hwnd.get()),
                        _ => None,
                    }
                })
                .map(Message::Hwnd);
                if app.connect_when_ready {
                    app.connect_when_ready = false;
                    return Task::batch([hwnd, app.connect()]);
                }
                return hwnd;
            }
            if app.connect_when_ready {
                app.connect_when_ready = false;
                return app.connect();
            }
        }
        Message::Resolution(i) => app.form.display = Some(i),
        Message::Dynamic(b) => app.form.dynamic = b,
        Message::WindowResized(id, size) => {
            app.window.get_or_insert(id);
            app.window_size = size;
            let resize = app.schedule_resize();
            if matches!(app.screen, Screen::Remote) && app.input.is_some() && !app.fullscreen {
                // maximized some other way (e.g. dragged to the top edge): full screen instead
                return Task::batch([resize, window::is_maximized(id).map(Message::MaximizedCheck)]);
            }
            return resize;
        }
        Message::MaximizeRequested | Message::MaximizedCheck(true) => {
            if matches!(app.screen, Screen::Remote) && app.input.is_some() && !app.fullscreen {
                return app.set_fullscreen(true);
            }
        }
        Message::MaximizedCheck(false) => {}
        Message::Minimize => {
            if let Some(id) = app.window {
                return window::minimize(id, true);
            }
        }
        Message::BarPin => {
            app.bar.pinned = !app.bar.pinned;
            app.bar.visible = true;
            return app.schedule_bar_hide(1500);
        }
        Message::BarHover(on) => {
            app.bar.hover = on;
            if on {
                app.bar.visible = true;
                app.bar.generation += 1;
            } else {
                return app.schedule_bar_hide(1000);
            }
        }
        Message::BarReveal => {
            app.bar.visible = true;
            // shown under the mouse: hidden again unless the mouse goes onto it
            return app.schedule_bar_hide(2000);
        }
        Message::BarHide(g) => {
            if g == app.bar.generation && !app.bar.pinned && !app.bar.hover {
                app.bar.visible = false;
            }
        }
        Message::Rescaled(s) => {
            app.scale = s;
            if let Some(m) = app.monitor.as_mut() {
                m.scale = s;
            }
            return app.schedule_resize();
        }
        Message::ApplyResize(g) => {
            if g == app.resize_gen {
                app.apply_resize();
            }
        }
        Message::Unfocused => {
            crate::log::line("window lost focus");
            app.focused = false;
        }
        Message::UpdateFound(r) => {
            if let Some(r) = &r {
                crate::log::line(&format!("FastRDP {} is available", r.version));
            }
            app.update = r;
        }
        Message::UpdateClient => {
            if let Some(r) = app.update.clone() {
                app.updating = true;
                app.status = format!("downloading FastRDP {} ...", r.version);
                return Task::perform(
                    async move { tokio::task::spawn_blocking(move || crate::selfupdate::install(&r)).await.unwrap_or_else(|e| Err(e.to_string())) },
                    Message::UpdateDone,
                );
            }
        }
        Message::UpdateDone(Ok(())) => return iced::exit(),
        Message::UpdateDone(Err(e)) => {
            app.updating = false;
            app.status = format!("update failed: {e}");
        }
        Message::Focused => {
            crate::log::line("window got focus");
            app.focused = true;
            app.sync_leds();
        }
        Message::Hwnd(h) => {
            #[cfg(windows)]
            if let Some(h) = h {
                crate::native_cursor::install(h);
                crate::keyhook::install(h);
                crate::maximize::install(h);
            }
            let _ = h;
        }
        Message::ToggleFullscreen => return app.set_fullscreen(!app.fullscreen),
        Message::Disconnect => {
            if let Some(i) = app.input.take() {
                let _ = i.send(InputCmd::Disconnect);
            }
        }
        Message::TrustRemember(b) => {
            if let Screen::Trust { remember, .. } = &mut app.screen {
                *remember = b;
            }
        }
        Message::TrustCancel => {
            app.screen = Screen::Form;
            app.status = "connection cancelled: server certificate not trusted".into();
        }
        Message::TrustAccept => {
            if let Screen::Trust { cert, remember } = &app.screen {
                let server = app.form.server.trim().to_string();
                if *remember && let Err(e) = known::save(&server, &cert.fingerprint) {
                    app.status = format!("could not save the certificate: {e}");
                }
                app.form.pin.clear(); // an explicit --pin no longer applies after the user decided
                app.accepted = Some((server, cert.fingerprint.clone()));
                return app.connect();
            }
        }
        Message::Session(u) => match u {
            Update::Connected { .. } => {
                app.status = "connected, signing in ...".into();
                wipe_string(&mut app.form.password); // the session holds its own copy
            }
            Update::Untrusted(cert) => {
                app.input = None;
                app.screen = Screen::Trust { cert, remember: false };
            }
            Update::HelloAck(_) => {}
            Update::ServerEvent(e) => match Event::from_u32(e.event) {
                Some(Event::SessionId) => app.status = format!("session {}", e.value),
                Some(Event::Logon) => app.status = format!("signed in as {}", e.text),
                Some(Event::InputStatus) => {
                    app.input_path = Some(e.value);
                    app.sync_leds();
                }
                Some(Event::EncoderFallback) => app.status = format!("encoder fallback: {}", e.text),
                Some(Event::Error) | Some(Event::Rejected) => app.status = format!("error: {}", e.text),
                _ => {}
            },
            Update::Stream(s) => app.stream = Some(s),
            Update::FrameReady => app.frame = app.frames.take(),
            Update::CursorShape(c) => app.cursor_shape = Some(c),
            Update::CursorPos(p) => app.cursor_pos = Some(p),
            Update::Clipboard(_) => {}
            Update::Stats(s) => {
                app.quality = Quality::from_interval(s.lost.saturating_sub(app.prev_loss.0), s.recovered.saturating_sub(app.prev_loss.1));
                app.prev_loss = (s.lost, s.recovered);
                app.stats = s;
            }
            Update::Ended(reason) => {
                crate::log::line(&format!("session ended: {reason}"));
                app.status = if reason.starts_with("sign-in failed") { reason } else { format!("disconnected: {reason}") };
                app.input = None;
                app.screen = Screen::Form;
                if app.fullscreen {
                    app.fullscreen = false;
                    if let Some(id) = app.window {
                        return window::set_mode(id, window::Mode::Windowed);
                    }
                }
            }
        },
    }
    Task::none()
}

/// "FastRDP x.y.z is available" + update button (empty when up to date).
fn update_notice(app: &App) -> Element<'_, Message> {
    let Some(r) = &app.update else { return row![].into() };
    let label = text(format!("FastRDP {} is available (this is {}).", r.version, update::CURRENT)).size(13).color(Color::from_rgb(0.55, 0.8, 1.0));
    let action: Element<'_, Message> = if app.updating {
        text("Updating ...").size(13).into()
    } else {
        button(text("Update and restart").size(13)).on_press(Message::UpdateClient).style(button::secondary).into()
    };
    row![label, action].spacing(10).align_y(Alignment::Center).into()
}

/// Session experience: presets + individual effects (applied to the remote session only).
fn experience(app: &App) -> Element<'_, Message> {
    use protocol::perf;
    let flags = app.form.performance_flags;
    let preset = |label: &'static str, f: u32| {
        let b = button(text(label).size(13)).on_press(Message::ExperiencePreset(f));
        if flags == f { b.style(button::primary) } else { b.style(button::secondary) }
    };
    let summary = match perf::preset_name(flags) {
        Some(n) => n.to_string(),
        None => format!("custom ({} of {} effects)", (0..perf::ITEMS.len()).filter(|&i| perf::effect_on(flags, i)).count(), perf::ITEMS.len()),
    };
    let mut c = column![
        row![
            text("Experience").width(140),
            row![preset("Best quality", perf::BEST_QUALITY), preset("Balanced", perf::BALANCED), preset("Best performance", perf::BEST_PERFORMANCE)].spacing(6),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
        row![
            text("").width(140),
            checkbox(app.show_effects).label(format!("Show effects - {summary}")).on_toggle(Message::ShowEffects).size(14).text_size(13),
        ]
        .spacing(8),
    ]
    .spacing(6);
    if app.show_effects {
        let items: Vec<Element<'_, Message>> = perf::ITEMS
            .iter()
            .enumerate()
            .map(|(i, (_, _, label))| {
                checkbox(perf::effect_on(flags, i)).label(*label).on_toggle(move |on| Message::Effect(i, on)).size(14).text_size(13).into()
            })
            .collect();
        c = c.push(row![text("").width(140), column(items).spacing(4)].spacing(8));
        c = c.push(
            row![
                text("").width(140),
                text("Applies to this remote session only; the remote computer's own settings stay unchanged. Takes effect when connecting.")
                    .size(12)
                    .color(Color::from_rgb(0.6, 0.6, 0.6))
            ]
            .spacing(8),
        );
    }
    c.into()
}

fn view(app: &App) -> Element<'_, Message> {
    match &app.screen {
        Screen::Form => {
            let field = |label: &'static str, value: &str, f: fn(String) -> Message| {
                row![text(label).width(140), text_input("", value).on_input(f).on_submit(Message::Connect)].spacing(8).align_y(Alignment::Center)
            };
            let form = column![
                text("FastRDP").size(28),
                field("Server (host:port)", &app.form.server, Message::Server),
                field("User name", &app.form.user, Message::User),
                text("Local account: name only. Domain: DOMAIN\\name or name@domain.").size(12).color(Color::from_rgb(0.6, 0.6, 0.6)),
                row![text("Password").width(140), text_input("", &app.form.password).secure(true).on_input(Message::Password).on_submit(Message::Connect)]
                    .spacing(8)
                    .align_y(Alignment::Center),
                {
                    let choices = display::choices(app.monitor);
                    let idx = app.form.display.map(|i| i.min(choices.len() as u16 - 1)).unwrap_or(choices.len() as u16 - 1);
                    row![
                        text("Display").width(140),
                        column![
                            slider(0..=(choices.len() as u16 - 1), idx, Message::Resolution),
                            text(format!(
                                "{}{}",
                                display::label(choices[idx as usize], app.monitor),
                                if app.scale != 1.0 { format!("  ·  scale {:.0}%", app.scale * 100.0) } else { String::new() }
                            ))
                            .size(13),
                        ]
                        .spacing(4)
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center)
                },
                checkbox(app.form.dynamic).label("Resize the remote display with the window").on_toggle(Message::Dynamic),
                checkbox(app.form.clipboard).label("Share the clipboard (text, images, files)").on_toggle(Message::Clipboard),
                row![
                    text("Windows key combinations").width(140),
                    pick_list(crate::prefs::KeyCombos::ALL, Some(app.form.keys), Message::Keys).text_size(14),
                    text("e.g. Alt+Tab, Win. Ctrl+Alt+End = Ctrl+Alt+Del").size(12).color(Color::from_rgb(0.6, 0.6, 0.6)),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
                experience(app),
                button("Connect").on_press(Message::Connect),
                update_notice(app),
                text(&app.status).size(14),
            ]
            .spacing(12)
            .max_width(640);
            container(form).center(Length::Fill).padding(24).into()
        }
        Screen::Trust { cert, remember } => {
            let changed = cert.expected.is_some();
            let mut c = column![
                text(if changed { "WARNING: the server certificate has changed" } else { "The identity of the remote computer cannot be verified" })
                    .size(22)
                    .color(if changed { Color::from_rgb(1.0, 0.35, 0.3) } else { Color::WHITE }),
                text(format!("Server: {}", app.form.server.trim())),
                text("Certificate SHA-256 fingerprint:"),
                container(text(session::format_fingerprint(&cert.fingerprint)).size(13).font(iced::Font::MONOSPACE)).padding(8),
            ]
            .spacing(10)
            .max_width(720);
            if let Some(old) = &cert.expected {
                c = c.push(text("Previously trusted fingerprint:")).push(container(text(session::format_fingerprint(old)).size(13).font(iced::Font::MONOSPACE)).padding(8)).push(
                    text("This happens when the server was reinstalled or its key was regenerated - or when someone is intercepting the connection. Connect only if you know why it changed.")
                        .color(Color::from_rgb(1.0, 0.6, 0.5)),
                );
            } else {
                c = c.push(text("Compare it with the fingerprint shown on the server (installer output, or the file cert.sha256 in the server's FastRDP data directory). No credentials have been sent yet."));
            }
            c = c
                .push(text("Do you want to connect anyway?"))
                .push(
                    checkbox(*remember)
                        .label(if changed { "Trust the new certificate from now on" } else { "Don't ask me again for connections to this computer" })
                        .on_toggle(Message::TrustRemember),
                )
                .push(
                    row![button("Connect").on_press(Message::TrustAccept), button("Cancel").on_press(Message::TrustCancel).style(button::secondary)]
                        .spacing(10),
                );
            container(c).center(Length::Fill).padding(24).into()
        }
        Screen::Remote => {
            let video = shader(VideoView {
                frame: app.frame.clone(),
                input: app.input.clone(),
                cursor_shape: app.cursor_shape.clone(),
                cursor_pos: app.cursor_pos,
                on_toggle_fullscreen: Some(Message::ToggleFullscreen),
                native_pointer: native_pointer_available(),
            })
            .width(Length::Fill)
            .height(Length::Fill);
            let s = &app.stats;
            let info = format!(
                "{} | {} | {:.0} fps {:.1} Mbps | lost {} fec {} | {}{}",
                app.status,
                app.stream.as_ref().map(|s| format!("{}x{} {}", s.width, s.height, s.description.split(' ').next().unwrap_or(""))).unwrap_or_default(),
                s.fps,
                s.mbps,
                s.lost,
                s.recovered,
                match app.input_path {
                    Some(protocol::input_path::DEVICES) => "input: devices",
                    Some(protocol::input_path::AGENT) => "input: agent (no lock screen/UAC)",
                    Some(_) => "input: none",
                    None => "input ?",
                },
                s.latency_ms.map(|(p50, _)| format!(" | {p50:.0} ms")).unwrap_or_default()
            );
            let black = |_: &Theme| container::Style { background: Some(Color::BLACK.into()), ..Default::default() };
            if app.fullscreen {
                // connection bar over the picture (like mstsc's)
                let top: Element<'_, Message> = if app.bar.visible || app.bar.pinned {
                    let actions =
                        connbar::Actions { pin: Message::BarPin, minimize: Message::Minimize, restore: Message::ToggleFullscreen, close: Message::Disconnect };
                    let bar = connbar::view(&connbar::display_name(&app.form.server), app.quality, app.bar.pinned, actions);
                    let bar = mouse_area(bar).on_enter(Message::BarHover(true)).on_exit(Message::BarHover(false));
                    container(bar).width(Length::Fill).align_x(Alignment::Center).into()
                } else {
                    // hidden: the top edge brings it back
                    mouse_area(container(text("")).width(Length::Fill).height(3)).on_enter(Message::BarReveal).into()
                };
                return container(stack![video, top]).style(black).into();
            }
            let bar = row![
                text(info).size(13).color(Color::from_rgb(0.85, 0.85, 0.85)).width(Length::Fill),
                button(text("Disconnect").size(13)).on_press(Message::Disconnect)
            ]
            .spacing(8)
            .padding([2, 6])
            .height(display::BAR_HEIGHT)
            .align_y(Alignment::Center);
            container(column![video, bar]).style(black).into()
        }
    }
}

pub fn run(l: Launch) -> iced::Result {
    let Launch { form, auto_connect, vsync, update_check } = l;
    crate::selfupdate::cleanup();
    iced::application(
        move || {
            let mut app = App::new(form.clone());
            app.connect_when_ready = auto_connect;
            // window id, monitor size and DPI scale (resolution choices, 1:1 sizing)
            let task = window::latest().then(|id| match id {
                Some(id) => window::scale_factor(id).then(move |s| window::monitor_size(id).map(move |m| Message::Monitor(Some(id), m, s))),
                None => Task::done(Message::Monitor(None, None, 1.0)),
            });
            let check = if update_check {
                Task::perform(async { tokio::task::spawn_blocking(crate::selfupdate::check).await.ok().flatten() }, Message::UpdateFound)
            } else {
                Task::none()
            };
            (app, Task::batch([task, check]))
        },
        update,
        view,
    )
    .title(|app: &App| match app.screen {
        Screen::Form | Screen::Trust { .. } => "FastRDP".to_string(),
        Screen::Remote => format!("FastRDP - {}", app.form.server),
    })
    .subscription(|_: &App| {
        let events = iced::event::listen_with(|e, _status, id| match e {
            iced::Event::Window(window::Event::Resized(s)) => Some(Message::WindowResized(id, s)),
            iced::Event::Window(window::Event::Rescaled(f)) => Some(Message::Rescaled(f)),
            iced::Event::Window(window::Event::Focused) => Some(Message::Focused),
            iced::Event::Window(window::Event::Unfocused) => Some(Message::Unfocused),
            _ => None,
        });
        #[cfg(windows)]
        {
            iced::Subscription::batch([events, iced::Subscription::run(maximize_requests)])
        }
        #[cfg(not(windows))]
        {
            events
        }
    })
    .theme(|_: &App| Theme::Dark)
    .settings(iced::Settings { vsync, antialiasing: false, ..Default::default() })
    .window_size((1280.0, 760.0))
    .run()
}

/// Windows: SC_MAXIMIZE swallowed by the window subclass (`maximize.rs`).
#[cfg(windows)]
fn maximize_requests() -> impl iced::futures::Stream<Item = Message> {
    use iced::futures::StreamExt;
    iced::stream::channel(4, async |tx| {
        crate::maximize::set_sender(tx);
        std::future::pending::<()>().await;
    })
    .map(|()| Message::MaximizeRequested)
}

/// UI logic tests (iced_test Simulator).
#[cfg(test)]
mod tests {
    use super::*;
    use iced_test::simulator;

    fn form() -> Form {
        Form { server: "pc.example:33890".into(), user: "alice".into(), ..Default::default() }
    }

    #[test]
    fn connect_form() {
        let app = App::new(form());
        let mut ui = simulator(view(&app));
        assert!(ui.find("User name").is_ok());
        assert!(ui.find("Server (host:port)").is_ok());
        assert!(ui.find("Domain").is_err(), "no separate domain field (DOMAIN\\name in the user field)");
        assert!(ui.find("Certificate SHA-256").is_err(), "no pin field: certificates are confirmed in a dialog");
        ui.click("Connect").unwrap();
        assert!(ui.into_messages().any(|m| matches!(m, Message::Connect)));
    }

    #[test]
    fn connect_form_offers_resolutions() {
        let mut app = App::new(form());
        let _ = update(&mut app, Message::Monitor(Some(window::Id::unique()), Some(Size::new(1706.67, 960.0)), 1.5));
        app.form.performance_flags = protocol::perf::BEST_QUALITY;
        {
            let mut ui = simulator(view(&app));
            assert!(ui.find("Full screen (2560 × 1440)  ·  scale 150%").is_ok(), "default = full screen of the monitor");
            assert!(ui.find("Resize the remote display with the window").is_ok());
        }
        let choices = display::choices(app.monitor);
        let i = choices.iter().position(|c| *c == Choice::Window(1920, 1080)).unwrap() as u16;
        let _ = update(&mut app, Message::Resolution(i));
        {
            let mut ui = simulator(view(&app));
            assert!(ui.find("1920 × 1080  ·  scale 150%").is_ok());
        }
    }

    #[test]
    fn experience_presets_and_effects() {
        use protocol::perf;
        let mut app = App::new(Form { performance_flags: perf::BEST_QUALITY, ..form() });
        {
            let mut ui = simulator(view(&app));
            assert!(ui.find("Show effects - best quality").is_ok());
            ui.click("Best performance").unwrap();
            ui.click("Show effects - best quality").unwrap();
            for m in ui.into_messages() {
                let _ = update(&mut app, m);
            }
        }
        assert_eq!(app.form.performance_flags, perf::BEST_PERFORMANCE);
        assert!(app.show_effects);
        {
            let mut ui = simulator(view(&app));
            ui.click("Font smoothing (ClearType)").unwrap();
            for m in ui.into_messages() {
                let _ = update(&mut app, m);
            }
        }
        assert_eq!(app.form.performance_flags, perf::BEST_PERFORMANCE | perf::ENABLE_FONT_SMOOTHING);
        let mut ui = simulator(view(&app));
        assert!(ui.find("Show effects - custom (1 of 8 effects)").is_ok());
    }

    #[test]
    fn window_resize_requests_a_new_remote_size() {
        let mut app = App::new(Form { dynamic: true, ..form() });
        let id = window::Id::unique();
        let _ = update(&mut app, Message::Monitor(Some(id), Some(Size::new(1706.67, 960.0)), 1.5));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        app.input = Some(tx);
        app.screen = Screen::Remote;
        // window 1000x630 logical @150% -> picture 1000 x (630-30) -> 1500x900 physical
        let _ = update(&mut app, Message::WindowResized(id, Size::new(1000.0, 630.0)));
        let _ = update(&mut app, Message::WindowResized(id, Size::new(1001.0, 630.0)));
        let stale = app.resize_gen - 1;
        let _ = update(&mut app, Message::ApplyResize(stale));
        assert!(rx.try_recv().is_err(), "only the last resize of a burst is applied");
        let g = app.resize_gen;
        let _ = update(&mut app, Message::ApplyResize(g));
        match rx.try_recv() {
            Ok(InputCmd::Display(d)) => assert_eq!((d.width, d.height, d.scale_percent), (1496, 900, 150), "1501 -> multiple of 8"),
            other => panic!("expected a display request, got {other:?}"),
        }
        let g = app.resize_gen;
        let _ = update(&mut app, Message::ApplyResize(g));
        assert!(rx.try_recv().is_err(), "unchanged size is not re-sent");
        // full screen: the whole window is picture
        app.fullscreen = true;
        let _ = update(&mut app, Message::WindowResized(id, Size::new(1706.67, 960.0)));
        let g = app.resize_gen;
        let _ = update(&mut app, Message::ApplyResize(g));
        assert!(matches!(rx.try_recv(), Ok(InputCmd::Display(d)) if (d.width, d.height) == (2560, 1440)));
        // dynamic off: nothing
        app.form.dynamic = false;
        let _ = update(&mut app, Message::WindowResized(id, Size::new(800.0, 600.0)));
        let g = app.resize_gen;
        let _ = update(&mut app, Message::ApplyResize(g));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn trust_prompt_first_contact() {
        let mut app = App::new(form());
        let fp = "ab".repeat(32);
        app.screen = Screen::Trust { cert: UntrustedCert { fingerprint: fp.clone(), expected: None }, remember: false };
        let mut ui = simulator(view(&app));
        assert!(ui.find("The identity of the remote computer cannot be verified").is_ok());
        assert!(ui.find(session::format_fingerprint(&fp).as_str()).is_ok());
        assert!(ui.find("WARNING: the server certificate has changed").is_err());
        ui.click("Don't ask me again for connections to this computer").unwrap();
        ui.click("Connect").unwrap();
        let msgs: Vec<_> = ui.into_messages().collect();
        assert!(msgs.iter().any(|m| matches!(m, Message::TrustRemember(true))), "{msgs:?}");
        assert!(msgs.iter().any(|m| matches!(m, Message::TrustAccept)), "{msgs:?}");
    }

    #[test]
    fn trust_prompt_changed_certificate() {
        let mut app = App::new(form());
        app.screen = Screen::Trust { cert: UntrustedCert { fingerprint: "cd".repeat(32), expected: Some("ab".repeat(32)) }, remember: false };
        let mut ui = simulator(view(&app));
        assert!(ui.find("WARNING: the server certificate has changed").is_ok());
        assert!(ui.find(session::format_fingerprint(&"ab".repeat(32)).as_str()).is_ok(), "old fingerprint shown");
        assert!(ui.find("Trust the new certificate from now on").is_ok());
        ui.click("Cancel").unwrap();
        assert!(ui.into_messages().any(|m| matches!(m, Message::TrustCancel)));
    }

    #[test]
    fn update_notice() {
        let mut app = App::new(form());
        assert!(simulator(view(&app)).find("Update and restart").is_err(), "nothing while up to date");
        let r = update::Release { version: update::Version(99, 0, 0), tag: "v99.0.0".into(), files: Default::default() };
        let _ = update(&mut app, Message::UpdateFound(Some(r)));
        let mut ui = simulator(view(&app));
        assert!(ui.find(format!("FastRDP 99.0.0 is available (this is {}).", update::CURRENT).as_str()).is_ok());
        ui.click("Update and restart").unwrap();
        assert!(ui.into_messages().any(|m| matches!(m, Message::UpdateClient)));
    }

    #[test]
    fn trust_flow_updates_state() {
        let mut app = App::new(form());
        app.screen = Screen::Trust { cert: UntrustedCert { fingerprint: "ef".repeat(32), expected: None }, remember: false };
        let _ = update(&mut app, Message::TrustCancel);
        assert!(matches!(app.screen, Screen::Form));
        assert!(app.status.contains("not trusted"));
        // a session reporting an untrusted certificate switches to the prompt and keeps the password
        app.form.password = "secret".into();
        let _ = update(&mut app, Message::Session(Update::Untrusted(UntrustedCert { fingerprint: "ef".repeat(32), expected: None })));
        assert!(matches!(app.screen, Screen::Trust { .. }));
        assert_eq!(app.form.password, "secret");
        // the password is wiped once the TLS handshake succeeded
        let _ = update(&mut app, Message::Session(Update::Connected { max_datagram: Some(1200) }));
        assert!(app.form.password.is_empty());
    }
}
