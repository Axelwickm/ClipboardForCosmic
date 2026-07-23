// SPDX-License-Identifier: MIT

mod bin_management;
mod clipboard_watcher;

use cosmic::Element;
use cosmic::app::{Core, Task};
use cosmic::iced::alignment::Horizontal;
use cosmic::iced::core::{text::EllipsizeHeightLimit, window};
use cosmic::iced::futures::SinkExt;
use cosmic::iced::widget::text;
use cosmic::iced::window::Id;
use cosmic::iced::{Alignment, Length, Size, Subscription};
use cosmic::surface::action::{app_window, destroy_window};
use cosmic::widget;
use fs2::FileExt;
use ksni::blocking::TrayMethods;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;
use std::time::SystemTime;
use tokio::sync::broadcast;

pub(crate) const APP_ID: &str = "com.github.clipboardforcosmic.ClipboardForCosmic";
pub(crate) const APP_NAME: &str = "ClipboardForCosmic";
const REPOSITORY_URL: &str = "https://github.com/Axelwickm/ClipboardForCosmic";
pub(crate) const ICON: &[u8] = include_bytes!("../resources/clipboard-history-symbolic.svg");
const DELETE_ICON: &[u8] = include_bytes!("../resources/delete-symbolic.svg");
const TO_FILE_ICON: &[u8] = include_bytes!("../resources/to-file-symbolic.svg");
const TO_DATA_ICON: &[u8] = include_bytes!("../resources/to-data-symbolic.svg");
const DEFAULT_HISTORY_LIMIT: usize = 255;
const HISTORY_PREVIEW_CHARS: usize = 100;
const HISTORY_ROW_HEIGHT: f32 = 36.0;
const HISTORY_ACTION_WIDTH: f32 = 40.0;
const TO_FILE_ACTION_WIDTH: f32 = 36.0;
const TO_DATA_ACTION_WIDTH: f32 = 36.0;

fn main() {
    let command = std::env::args().nth(1);
    match command.as_deref() {
        None => launch(),
        Some("install") => run_management(bin_management::install()),
        Some("uninstall") => run_management(bin_management::uninstall()),
        Some("show") => run_management(show_running_instance()),
        Some("help" | "--help" | "-h") => print_usage(),
        Some(command) => {
            eprintln!("unknown command: {command}\n");
            print_usage();
            std::process::exit(2);
        }
    }
}

fn run_management(result: Result<(), Box<dyn std::error::Error>>) {
    if let Err(error) = result {
        eprintln!("{APP_NAME}: {error}");
        std::process::exit(1);
    }
}

fn print_usage() {
    println!("Usage: clipboard-for-cosmic [install|uninstall|show]");
}

fn launch() {
    let Some(instance_guard) = InstanceGuard::acquire().unwrap_or_else(|error| {
        eprintln!("{APP_NAME}: could not establish the instance lock: {error}");
        std::process::exit(1);
    }) else {
        eprintln!("{APP_NAME} is already running.");
        return;
    };
    if let Err(error) = cleanup_stale_temp_files() {
        eprintln!("{APP_NAME}: could not clean stale temporary files: {error}");
    }

    start_tray();
    start_control_socket();
    let settings = cosmic::app::Settings::default()
        .no_main_window(true)
        .is_daemon(true);
    let result = cosmic::app::run::<ClipboardWindow>(settings, instance_guard);

    if let Err(error) = result {
        eprintln!("{APP_NAME}: {error}");
    }
}

static TRAY_EVENTS: std::sync::OnceLock<broadcast::Sender<TrayEvent>> = std::sync::OnceLock::new();
static TRAY_HANDLE: std::sync::OnceLock<ksni::blocking::Handle<TrayIcon>> =
    std::sync::OnceLock::new();

const CONTROL_MESSAGE_SHOW: &[u8] = b"show";

fn control_socket_path() -> io::Result<std::path::PathBuf> {
    dirs::runtime_dir()
        .map(|directory| directory.join("clipboard-for-cosmic.sock"))
        .ok_or_else(|| io::Error::other("could not determine the runtime directory"))
}

fn show_running_instance() -> Result<(), Box<dyn std::error::Error>> {
    let socket = std::os::unix::net::UnixDatagram::unbound()?;
    socket
        .send_to(CONTROL_MESSAGE_SHOW, control_socket_path()?)
        .map_err(|error| format!("could not contact the running service: {error}"))?;
    Ok(())
}

fn start_control_socket() {
    let path = control_socket_path().unwrap_or_else(|error| {
        eprintln!("{APP_NAME}: could not create control socket: {error}");
        std::process::exit(1);
    });
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            eprintln!("{APP_NAME}: could not replace control socket: {error}");
            std::process::exit(1);
        }
    }
    let socket = std::os::unix::net::UnixDatagram::bind(&path).unwrap_or_else(|error| {
        eprintln!("{APP_NAME}: could not bind control socket: {error}");
        std::process::exit(1);
    });
    std::thread::spawn(move || {
        let mut message = [0; 16];
        while let Ok(length) = socket.recv(&mut message) {
            if &message[..length] == CONTROL_MESSAGE_SHOW {
                let _ = TRAY_EVENTS
                    .get()
                    .expect("tray sender")
                    .send(TrayEvent::Activate);
            }
        }
    });
}

#[derive(Clone, Copy)]
enum TrayEvent {
    Activate,
    OpenRepository,
    Shutdown,
    ClearHistory,
    ConfigureShortcut,
    SetHistoryLimit(usize),
}

struct TrayIcon {
    installed: bool,
    autostart: bool,
    flash_generation: u64,
    flashing: bool,
    max_history_items: usize,
}
impl ksni::Tray for TrayIcon {
    fn id(&self) -> String {
        APP_ID.into()
    }
    fn title(&self) -> String {
        APP_NAME.into()
    }
    fn icon_name(&self) -> String {
        if self.flashing {
            return String::new();
        }
        dirs::data_dir()
            .map(|data| {
                data.join("icons/hicolor/scalable/apps")
                    .join(format!("{APP_ID}-symbolic.svg"))
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_else(|| format!("{APP_ID}-symbolic"))
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        if self.flashing {
            vec![flash_icon_pixmap(24), flash_icon_pixmap(48)]
        } else {
            Vec::new()
        }
    }
    fn activate(&mut self, _: i32, _: i32) {
        let _ = TRAY_EVENTS
            .get()
            .expect("tray sender")
            .send(TrayEvent::Activate);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        vec![
            ksni::menu::StandardItem {
                label: "Shutdown".into(),
                activate: Box::new(|_| {
                    let _ = TRAY_EVENTS
                        .get()
                        .expect("tray sender")
                        .send(TrayEvent::Shutdown);
                }),
                ..Default::default()
            }
            .into(),
            ksni::menu::StandardItem {
                label: "Clear history".into(),
                activate: Box::new(|_| {
                    let _ = TRAY_EVENTS
                        .get()
                        .expect("tray sender")
                        .send(TrayEvent::ClearHistory);
                }),
                ..Default::default()
            }
            .into(),
            ksni::menu::StandardItem {
                label: "Configure shortcut".into(),
                activate: Box::new(|_| {
                    let _ = TRAY_EVENTS
                        .get()
                        .expect("tray sender")
                        .send(TrayEvent::ConfigureShortcut);
                }),
                ..Default::default()
            }
            .into(),
            ksni::menu::SubMenu {
                label: format!("Max items: {}", self.max_history_items),
                submenu: HISTORY_LIMIT_OPTIONS
                    .into_iter()
                    .map(|limit| {
                        ksni::menu::CheckmarkItem {
                            label: limit.to_string(),
                            checked: limit == self.max_history_items,
                            activate: Box::new(move |tray: &mut TrayIcon| {
                                tray.max_history_items = limit;
                                let _ = TRAY_EVENTS
                                    .get()
                                    .expect("tray sender")
                                    .send(TrayEvent::SetHistoryLimit(limit));
                            }),
                            ..Default::default()
                        }
                        .into()
                    })
                    .collect(),
                ..Default::default()
            }
            .into(),
            ksni::menu::CheckmarkItem {
                label: "Autostart".into(),
                enabled: self.installed,
                checked: self.autostart,
                activate: Box::new(|tray: &mut TrayIcon| {
                    let enabled = !tray.autostart;
                    match bin_management::set_autostart(enabled) {
                        Ok(()) => tray.autostart = enabled,
                        Err(error) => eprintln!("{APP_NAME}: could not change autostart: {error}"),
                    }
                }),
                ..Default::default()
            }
            .into(),
            ksni::menu::StandardItem {
                label: "Project page".into(),
                activate: Box::new(|_| {
                    let _ = TRAY_EVENTS
                        .get()
                        .expect("tray sender")
                        .send(TrayEvent::OpenRepository);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

fn flash_icon_pixmap(size: i32) -> ksni::Icon {
    let mut data = vec![0; (size * size * 4) as usize];
    let scale = size as f32 / 24.0;
    let normal = [255, 232, 232, 232];
    let turquoise = [255, 0, 199, 183];
    let width = (1.5 * scale).round().max(1.0) as i32;
    let point = |x: f32, y: f32| ((x * scale).round() as i32, (y * scale).round() as i32);

    for (start, end) in [
        ((3.0, 4.5), (17.25, 4.5)),
        ((3.0, 9.0), (12.75, 9.0)),
        ((3.0, 13.5), (12.75, 13.5)),
    ] {
        draw_line(
            &mut data,
            size,
            point(start.0, start.1),
            point(end.0, end.1),
            width,
            normal,
        );
    }
    for (start, end) in [
        ((17.25, 9.0), (17.25, 21.0)),
        ((17.25, 21.0), (13.5, 17.25)),
        ((17.25, 21.0), (21.0, 17.25)),
    ] {
        draw_line(
            &mut data,
            size,
            point(start.0, start.1),
            point(end.0, end.1),
            width,
            turquoise,
        );
    }
    ksni::Icon {
        width: size,
        height: size,
        data,
    }
}

fn draw_line(
    pixels: &mut [u8],
    size: i32,
    (mut x, mut y): (i32, i32),
    (end_x, end_y): (i32, i32),
    width: i32,
    color: [u8; 4],
) {
    let dx = (end_x - x).abs();
    let sx = if x < end_x { 1 } else { -1 };
    let dy = -(end_y - y).abs();
    let sy = if y < end_y { 1 } else { -1 };
    let mut error = dx + dy;
    loop {
        for offset_y in -(width / 2)..=(width / 2) {
            for offset_x in -(width / 2)..=(width / 2) {
                let (pixel_x, pixel_y) = (x + offset_x, y + offset_y);
                if (0..size).contains(&pixel_x) && (0..size).contains(&pixel_y) {
                    let index = ((pixel_y * size + pixel_x) * 4) as usize;
                    pixels[index..index + 4].copy_from_slice(&color);
                }
            }
        }
        if x == end_x && y == end_y {
            break;
        }
        let doubled = 2 * error;
        if doubled >= dy {
            error += dy;
            x += sx;
        }
        if doubled <= dx {
            error += dx;
            y += sy;
        }
    }
}

fn start_tray() {
    TRAY_EVENTS.get_or_init(|| {
        let (sender, _) = broadcast::channel(16);
        sender
    });
    let handle = TrayIcon {
        installed: bin_management::is_installed_instance(),
        autostart: bin_management::autostart_enabled(),
        flash_generation: 0,
        flashing: false,
        max_history_items: DEFAULT_HISTORY_LIMIT,
    }
    .spawn()
    .expect("register Status Notifier item");
    TRAY_HANDLE.set(handle).ok().expect("set tray handle once");
}

fn flash_tray_icon() {
    let Some(handle) = TRAY_HANDLE.get() else {
        return;
    };
    let Some(generation) = handle.update(|tray| {
        tray.flash_generation = tray.flash_generation.wrapping_add(1);
        tray.flashing = true;
        tray.flash_generation
    }) else {
        return;
    };

    let handle = handle.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(350));
        handle.update(|tray| {
            if tray.flash_generation == generation {
                tray.flashing = false;
            }
        });
    });
}

/// Keeps an advisory lock alive for the complete lifetime of the process.
struct InstanceGuard {
    _file: File,
}

impl InstanceGuard {
    fn acquire() -> io::Result<Option<Self>> {
        let candidate_dirs = [
            dirs::runtime_dir(),
            dirs::cache_dir(),
            Some(std::env::temp_dir()),
        ];
        let state_dir = candidate_dirs
            .into_iter()
            .flatten()
            .find(|dir| fs::create_dir_all(dir).is_ok())
            .ok_or_else(|| io::Error::other("could not create an instance-lock directory"))?;

        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(state_dir.join("clipboard-for-cosmic.lock"))?;

        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }
}

struct ClipboardWindow {
    core: Core,
    _instance_guard: InstanceGuard,
    history: Vec<HistoryItem>,
    max_history_items: usize,
    content_window: Option<Id>,
    closing_window: Option<Id>,
    reopen_after_close: bool,
    content_window_focused: bool,
    selected_item: Option<usize>,
    hovered_item: Option<usize>,
    search_query: String,
    search_results: Vec<usize>,
    search_generation: u64,
    search_input_id: widget::Id,
    active_generated_file: Option<std::path::PathBuf>,
    generated_files: std::collections::HashSet<std::path::PathBuf>,
    history_scroll_id: widget::Id,
}

impl ClipboardWindow {
    fn close_content_window(&mut self) -> Task<WindowMessage> {
        let Some(id) = self.content_window.take() else {
            return Task::none();
        };
        self.closing_window = Some(id);
        self.content_window_focused = false;
        self.hovered_item = None;
        self.search_query.clear();
        self.search_results.clear();
        self.search_generation = self.search_generation.wrapping_add(1);
        cosmic::task::message(cosmic::Action::Cosmic(cosmic::app::Action::Surface(
            destroy_window(id),
        )))
    }

    fn refresh_search(&mut self) -> Task<WindowMessage> {
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        if self.search_query.is_empty() {
            self.search_results.clear();
            self.selected_item = (!self.history.is_empty()).then_some(0);
            return Task::none();
        }

        let query = self.search_query.clone();
        let searchable = self
            .history
            .iter()
            .enumerate()
            .map(|(index, item)| (index, searchable_text(item)))
            .collect::<Vec<_>>();
        self.search_results.clear();
        self.selected_item = None;
        Task::perform(
            async move {
                let query = query.to_lowercase();
                searchable
                    .into_iter()
                    .filter_map(|(index, text)| matches_search(&text, &query).then_some(index))
                    .collect::<Vec<_>>()
            },
            move |results| {
                cosmic::Action::App(WindowMessage::SearchCompleted {
                    generation,
                    results,
                })
            },
        )
    }
}

#[derive(Clone, Debug)]
enum KeyboardInput {
    Up,
    Down,
    Tab { backwards: bool },
    Enter,
    Escape,
    StartSearch(String),
}

#[derive(Clone)]
enum WindowMessage {
    ClipboardUpdated(clipboard_watcher::ClipboardUpdate),
    ActivateItem(usize),
    ConvertToData(usize),
    ConvertToFile(usize),
    DeleteItem(usize),
    HoverItem(Option<usize>),
    ClearHistory,
    Shutdown,
    ConfigureShortcut,
    Focus,
    WindowClosed(Id),
    WindowEvent(Id, window::Event),
    SetHistoryLimit(usize),
    Surface(cosmic::surface::Action),
    KeyboardInput(Id, KeyboardInput),
    SearchInput(String),
    OpenRepository,
    Ignore,
    SearchCompleted {
        generation: u64,
        results: Vec<usize>,
    },
}

impl std::fmt::Debug for WindowMessage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClipboardUpdated(_) => formatter.write_str("ClipboardUpdated"),
            Self::ActivateItem(index) => {
                formatter.debug_tuple("ActivateItem").field(index).finish()
            }
            Self::ConvertToData(index) => {
                formatter.debug_tuple("ConvertToData").field(index).finish()
            }
            Self::ConvertToFile(index) => {
                formatter.debug_tuple("ConvertToFile").field(index).finish()
            }
            Self::DeleteItem(index) => formatter.debug_tuple("DeleteItem").field(index).finish(),
            Self::HoverItem(index) => formatter.debug_tuple("HoverItem").field(index).finish(),
            Self::ClearHistory => formatter.write_str("ClearHistory"),
            Self::Shutdown => formatter.write_str("Shutdown"),
            Self::ConfigureShortcut => formatter.write_str("ConfigureShortcut"),
            Self::Focus => formatter.write_str("Focus"),
            Self::WindowClosed(id) => formatter.debug_tuple("WindowClosed").field(id).finish(),
            Self::WindowEvent(id, event) => formatter
                .debug_tuple("WindowEvent")
                .field(id)
                .field(event)
                .finish(),
            Self::SetHistoryLimit(limit) => formatter
                .debug_tuple("SetHistoryLimit")
                .field(limit)
                .finish(),
            Self::Surface(_) => formatter.write_str("Surface"),
            Self::KeyboardInput(id, key) => formatter
                .debug_tuple("KeyboardInput")
                .field(id)
                .field(key)
                .finish(),
            Self::SearchInput(_) => formatter.write_str("SearchInput"),
            Self::Ignore => formatter.write_str("Ignore"),
            Self::OpenRepository => formatter.write_str("OpenRepository"),
            Self::SearchCompleted {
                generation,
                results,
            } => formatter
                .debug_struct("SearchCompleted")
                .field("generation", generation)
                .field("result_count", &results.len())
                .finish(),
        }
    }
}

impl cosmic::Application for ClipboardWindow {
    type Executor = cosmic::executor::Default;
    type Flags = InstanceGuard;
    type Message = WindowMessage;

    const APP_ID: &'static str = APP_ID;

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, instance_guard: Self::Flags) -> (Self, Task<Self::Message>) {
        let mut app = Self {
            core,
            _instance_guard: instance_guard,
            history: Vec::new(),
            max_history_items: DEFAULT_HISTORY_LIMIT,
            content_window: None,
            closing_window: None,
            reopen_after_close: false,
            content_window_focused: false,
            selected_item: None,
            hovered_item: None,
            search_query: String::new(),
            search_results: Vec::new(),
            search_generation: 0,
            search_input_id: widget::Id::unique(),
            active_generated_file: None,
            generated_files: std::collections::HashSet::new(),
            history_scroll_id: widget::Id::unique(),
        };
        app.core.window.show_minimize = false;
        app.core.window.show_maximize = false;
        (app, Task::none())
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Self::Message> {
        (self.content_window == Some(id)).then_some(WindowMessage::WindowClosed(id))
    }

    fn view(&self) -> Element<'_, Self::Message> {
        clipboard_content(self)
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        let clipboard = Subscription::run(|| {
            let mut updates = clipboard_watcher::subscribe();
            cosmic::iced::stream::channel(
                64,
                move |mut sender: cosmic::iced::futures::channel::mpsc::Sender<WindowMessage>| async move {
                    while let Ok(update) = updates.recv().await {
                        let _ = sender.send(WindowMessage::ClipboardUpdated(update)).await;
                    }
                },
            )
        });
        let tray = Subscription::run(|| {
            let mut events = TRAY_EVENTS.get().expect("tray started").subscribe();
            cosmic::iced::stream::channel(
                8,
                move |mut sender: cosmic::iced::futures::channel::mpsc::Sender<WindowMessage>| async move {
                    while let Ok(event) = events.recv().await {
                        let message = match event {
                            TrayEvent::Activate => WindowMessage::Focus,
                            TrayEvent::OpenRepository => WindowMessage::OpenRepository,
                            TrayEvent::Shutdown => WindowMessage::Shutdown,
                            TrayEvent::ClearHistory => WindowMessage::ClearHistory,
                            TrayEvent::ConfigureShortcut => WindowMessage::ConfigureShortcut,
                            TrayEvent::SetHistoryLimit(limit) => {
                                WindowMessage::SetHistoryLimit(limit)
                            }
                        };
                        let _ = sender.send(message).await;
                    }
                },
            )
        });
        let window_events =
            cosmic::iced::window::events().map(|(id, event)| WindowMessage::WindowEvent(id, event));
        let keyboard = cosmic::iced::event::listen_with(|event, _, id| {
            let cosmic::iced::Event::Keyboard(cosmic::iced::keyboard::Event::KeyPressed {
                key,
                modifiers,
                text,
                ..
            }) = event
            else {
                return None;
            };
            use cosmic::iced::keyboard::key::Named;
            let input = match key {
                cosmic::iced::keyboard::Key::Named(Named::ArrowUp) => KeyboardInput::Up,
                cosmic::iced::keyboard::Key::Named(Named::ArrowDown) => KeyboardInput::Down,
                cosmic::iced::keyboard::Key::Named(Named::Tab) => KeyboardInput::Tab {
                    backwards: modifiers.shift(),
                },
                cosmic::iced::keyboard::Key::Named(Named::Enter) => KeyboardInput::Enter,
                cosmic::iced::keyboard::Key::Named(Named::Escape) => KeyboardInput::Escape,
                _ if !modifiers.control() && !modifiers.logo() => {
                    let text = text?.to_string();
                    if text.chars().all(char::is_control) {
                        return None;
                    }
                    KeyboardInput::StartSearch(text)
                }
                _ => return None,
            };
            Some(WindowMessage::KeyboardInput(id, input))
        });
        Subscription::batch([clipboard, tray, window_events, keyboard])
    }

    fn update(&mut self, message: Self::Message) -> Task<Self::Message> {
        match message {
            WindowMessage::ClipboardUpdated(update) => {
                if !update.text.trim().is_empty() {
                    flash_tray_icon();
                }
                self.active_generated_file = self
                    .active_generated_file
                    .take()
                    .filter(|path| clipboard_update_contains_path(&update, path));
                record_history(&mut self.history, update, self.max_history_items);
                self.selected_item = (!self.history.is_empty()).then_some(0);
                report_temp_cleanup(
                    &self.history,
                    self.active_generated_file.as_deref(),
                    &mut self.generated_files,
                );
                return self.refresh_search();
            }
            WindowMessage::ActivateItem(index) => {
                let Some(item) =
                    activate_history_item(&mut self.history, &mut self.selected_item, index)
                else {
                    return Task::none();
                };
                self.active_generated_file = activate_clipboard_content(item);
                return self.close_content_window();
            }
            WindowMessage::ConvertToFile(index) => {
                let Some(item) = self.history.get(index) else {
                    return Task::none();
                };
                if item.files.is_some() {
                    return Task::none();
                }
                let files = match materialize_history_item(item) {
                    Ok(files) => files,
                    Err(error) => {
                        eprintln!(
                            "{APP_NAME}: could not convert clipboard item to a file: {error}"
                        );
                        return Task::none();
                    }
                };
                let label = clipboard_watcher::files_label(&files);
                self.active_generated_file = generated_path(&files);
                self.generated_files
                    .extend(self.active_generated_file.iter().cloned());
                record_history(
                    &mut self.history,
                    clipboard_watcher::ClipboardUpdate {
                        text: label,
                        mime_type: "text/uri-list".into(),
                        available_mime_types: vec![
                            "text/uri-list".into(),
                            "x-special/gnome-copied-files".into(),
                        ],
                        color_rgba: None,
                        image: None,
                        files: Some(files.clone()),
                        captured_at: SystemTime::now(),
                    },
                    self.max_history_items,
                );
                self.selected_item = Some(0);
                clipboard_watcher::copy_files(files);
                return self.refresh_search();
            }
            WindowMessage::ConvertToData(index) => {
                let Some(path) = self
                    .history
                    .get(index)
                    .and_then(|item| item.files.as_ref())
                    .and_then(single_regular_file_path)
                    .map(Path::to_path_buf)
                else {
                    return Task::none();
                };
                let update = match clipboard_watcher::read_file_as_clipboard_update(&path) {
                    Ok(update) => update,
                    Err(error) => {
                        eprintln!("{APP_NAME}: could not convert clipboard file to data: {error}");
                        return Task::none();
                    }
                };
                record_history(&mut self.history, update, self.max_history_items);
                if let Some(item) =
                    activate_history_item(&mut self.history, &mut self.selected_item, 0)
                {
                    self.active_generated_file = activate_clipboard_content(item);
                }
                return self.refresh_search();
            }
            WindowMessage::DeleteItem(index) => {
                if index < self.history.len() {
                    self.history.remove(index);
                    self.selected_item = match (self.selected_item, self.history.is_empty()) {
                        (_, true) => None,
                        (Some(selected), false) if selected > index => Some(selected - 1),
                        (Some(selected), false) => Some(selected.min(self.history.len() - 1)),
                        (None, false) => None,
                    };
                }
                self.hovered_item = None;
                report_temp_cleanup(
                    &self.history,
                    self.active_generated_file.as_deref(),
                    &mut self.generated_files,
                );
                return self.refresh_search();
            }
            WindowMessage::HoverItem(index) => {
                self.hovered_item = index.filter(|index| *index < self.history.len());
            }
            WindowMessage::ClearHistory => {
                self.history.clear();
                self.selected_item = None;
                report_temp_cleanup(
                    &self.history,
                    self.active_generated_file.as_deref(),
                    &mut self.generated_files,
                );
                return self.refresh_search();
            }
            WindowMessage::Shutdown => {
                report_temp_cleanup(
                    &self.history,
                    self.active_generated_file.as_deref(),
                    &mut self.generated_files,
                );
                std::process::exit(0);
            }
            WindowMessage::ConfigureShortcut => {
                let command = match bin_management::show_command() {
                    Ok(command) => command,
                    Err(error) => {
                        eprintln!("{APP_NAME}: could not build shortcut command: {error}");
                        return Task::none();
                    }
                };
                if let Err(error) = std::process::Command::new("cosmic-settings")
                    .arg("keyboard")
                    .spawn()
                {
                    eprintln!("{APP_NAME}: could not open keyboard settings: {error}");
                }
                clipboard_watcher::copy_text(command);
            }
            WindowMessage::Focus => {
                if let Some(id) = self.content_window {
                    return cosmic::iced::window::gain_focus(id);
                }
                if self.closing_window.is_some() {
                    self.reopen_after_close = true;
                    return Task::none();
                }
                self.selected_item = (!self.history.is_empty()).then_some(0);
                self.hovered_item = None;
                let (id, action) = app_window::<ClipboardWindow>(
                    |_| Default::default(),
                    |_| window::Settings {
                        size: Size::new(310.0, 768.0),
                        min_size: Some(Size::new(310.0, 768.0)),
                        max_size: Some(Size::new(310.0, 768.0)),
                        resizable: false,
                        minimizable: false,
                        closeable: true,
                        decorations: false,
                        ..window::Settings::default()
                    },
                    Some(Box::new(|state| {
                        clipboard_content(state).map(cosmic::Action::App)
                    })),
                );
                self.content_window = Some(id);
                self.content_window_focused = false;
                return cosmic::task::message(cosmic::Action::Cosmic(
                    cosmic::app::Action::Surface(action),
                ));
            }
            WindowMessage::WindowClosed(id) => {
                if self.content_window == Some(id) {
                    return self.close_content_window();
                }
            }
            WindowMessage::WindowEvent(id, window::Event::Closed) => {
                if self.content_window == Some(id) {
                    self.content_window = None;
                    self.content_window_focused = false;
                }
                if self.closing_window == Some(id) {
                    self.closing_window = None;
                    if std::mem::take(&mut self.reopen_after_close) {
                        return cosmic::task::message(cosmic::Action::App(WindowMessage::Focus));
                    }
                }
            }
            WindowMessage::WindowEvent(id, window::Event::Focused)
                if self.content_window == Some(id) =>
            {
                self.content_window_focused = true;
                return widget::text_input::focus(self.search_input_id.clone());
            }
            WindowMessage::WindowEvent(id, window::Event::Opened { .. })
                if self.content_window == Some(id) =>
            {
                return widget::text_input::focus(self.search_input_id.clone());
            }
            WindowMessage::WindowEvent(id, window::Event::Unfocused)
                if self.content_window == Some(id) && self.content_window_focused =>
            {
                self.hovered_item = None;
                return self.close_content_window();
            }
            WindowMessage::WindowEvent(_, _) => {}
            WindowMessage::SetHistoryLimit(limit) => {
                self.max_history_items = limit;
                self.history.truncate(limit);
                self.selected_item = self
                    .selected_item
                    .filter(|index| *index < self.history.len());
                report_temp_cleanup(
                    &self.history,
                    self.active_generated_file.as_deref(),
                    &mut self.generated_files,
                );
                return self.refresh_search();
            }
            WindowMessage::Surface(action) => {
                return cosmic::task::message(cosmic::Action::Cosmic(
                    cosmic::app::Action::Surface(action),
                ));
            }
            WindowMessage::KeyboardInput(id, key) if self.content_window == Some(id) => {
                match key {
                    KeyboardInput::StartSearch(text) if self.search_query.is_empty() => {
                        self.search_query = text;
                        return Task::batch(vec![
                            self.refresh_search(),
                            widget::text_input::focus(self.search_input_id.clone()),
                        ]);
                    }
                    KeyboardInput::StartSearch(_) => return Task::none(),
                    KeyboardInput::Escape if !self.search_query.is_empty() => {
                        self.search_query.clear();
                        return self.refresh_search();
                    }
                    KeyboardInput::Escape => return self.close_content_window(),
                    _ => {}
                }

                let visible = if self.search_query.is_empty() {
                    (0..self.history.len()).collect::<Vec<_>>()
                } else {
                    self.search_results.clone()
                };
                if visible.is_empty() {
                    self.selected_item = None;
                    return Task::none();
                }
                let current_position = self
                    .selected_item
                    .and_then(|selected| visible.iter().position(|index| *index == selected))
                    .unwrap_or(0);
                match key {
                    KeyboardInput::Enter => {
                        let current = visible[current_position];
                        let item = activate_history_item(
                            &mut self.history,
                            &mut self.selected_item,
                            current,
                        )
                        .expect("validated history index");
                        self.active_generated_file = activate_clipboard_content(item);
                        return self.close_content_window();
                    }
                    KeyboardInput::Up => {
                        self.selected_item = Some(move_selection(&visible, current_position, true));
                    }
                    KeyboardInput::Down => {
                        self.selected_item =
                            Some(move_selection(&visible, current_position, false));
                    }
                    KeyboardInput::Tab { backwards } => {
                        self.selected_item =
                            Some(move_selection(&visible, current_position, backwards));
                    }
                    KeyboardInput::Escape | KeyboardInput::StartSearch(_) => {
                        unreachable!("search input handled before navigation")
                    }
                }
                let selected_position = visible
                    .iter()
                    .position(|index| Some(*index) == self.selected_item)
                    .unwrap_or(0);
                let offset =
                    selected_position as f32 / visible.len().saturating_sub(1).max(1) as f32;
                return cosmic::iced::widget::scrollable::snap_to(
                    self.history_scroll_id.clone(),
                    cosmic::iced::widget::scrollable::RelativeOffset {
                        x: None,
                        y: Some(offset),
                    },
                );
            }
            WindowMessage::KeyboardInput(_, _) => {}
            WindowMessage::SearchInput(query) => {
                self.search_query = query;
                return self.refresh_search();
            }
            WindowMessage::Ignore => {}
            WindowMessage::OpenRepository => {
                if let Err(error) = std::process::Command::new("xdg-open")
                    .arg(REPOSITORY_URL)
                    .spawn()
                {
                    eprintln!("{APP_NAME}: could not open repository link: {error}");
                }
            }
            WindowMessage::SearchCompleted {
                generation,
                results,
            } => {
                if generation == self.search_generation {
                    self.search_results = results;
                    self.selected_item = self.search_results.first().copied();
                    return cosmic::iced::widget::scrollable::snap_to(
                        self.history_scroll_id.clone(),
                        cosmic::iced::widget::scrollable::RelativeOffset {
                            x: None,
                            y: Some(0.0),
                        },
                    );
                }
            }
        }
        Task::none()
    }
}

fn clipboard_content(state: &ClipboardWindow) -> Element<'static, WindowMessage> {
    let status_bar = widget::container(widget::row::with_children([
        widget::text(APP_NAME).into(),
        widget::Space::new().width(Length::Fill).into(),
        widget::text(format!(
            "{}/{} items",
            state.history.len(),
            state.max_history_items
        ))
        .into(),
    ]))
    .height(28.0)
    .padding(cosmic::theme::spacing().space_s)
    .align_x(Horizontal::Right)
    .align_y(Alignment::Center)
    .width(Length::Fill);

    let visible_indices = if state.search_query.is_empty() {
        (0..state.history.len()).collect::<Vec<_>>()
    } else {
        state.search_results.clone()
    };
    let search = widget::text_input::inline_input("", state.search_query.clone())
        .id(state.search_input_id.clone())
        .always_active()
        .on_input(WindowMessage::SearchInput)
        .on_tab(WindowMessage::Ignore)
        .leading_icon(
            widget::icon::from_name("system-search-symbolic")
                .size(16)
                .icon()
                .into(),
        )
        .width(Length::Fill);
    let searching = !state.search_query.is_empty();
    let mut content = widget::column::with_capacity(3).height(Length::Fill).push(
        widget::container(search)
            .height(if searching { 34.0 } else { 0.0 })
            .padding(if searching { [3, 8] } else { [0, 0] })
            .width(Length::Fill),
    );
    content = content.push(history_list(
        &state.history,
        &visible_indices,
        state.content_window,
        state.selected_item,
        state.hovered_item,
        state.history_scroll_id.clone(),
    ));
    content = content.push(status_bar);

    widget::container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

#[derive(Clone, Debug)]
struct HistoryItem {
    text: String,
    mime_type: String,
    available_mime_types: Vec<String>,
    color_rgba: Option<[u8; 4]>,
    image: Option<clipboard_watcher::ClipboardImage>,
    files: Option<clipboard_watcher::ClipboardFiles>,
    image_handle: Option<cosmic::iced::widget::image::Handle>,
    image_preview_handle: Option<ImagePreviewHandle>,
    captured_at: SystemTime,
    tooltip_popup_id: Id,
    tooltip_autosize_id: widget::Id,
    preview_popup_id: Id,
    preview_autosize_id: widget::Id,
    to_file_popup_id: Id,
    to_file_autosize_id: widget::Id,
    to_data_popup_id: Id,
    to_data_autosize_id: widget::Id,
}

fn single_regular_file_path(files: &clipboard_watcher::ClipboardFiles) -> Option<&Path> {
    match files.entries.as_slice() {
        [entry] if !entry.is_dir => entry.path.as_deref(),
        _ => None,
    }
}

fn record_history(
    history: &mut Vec<HistoryItem>,
    update: clipboard_watcher::ClipboardUpdate,
    limit: usize,
) {
    if update.text.trim().is_empty() {
        return;
    }
    let existing = history
        .iter()
        .position(
            |entry| match (&entry.image, &update.image, &entry.files, &update.files) {
                (Some(existing), Some(incoming), _, _) => existing.bytes == incoming.bytes,
                (None, None, Some(existing), Some(incoming)) => existing == incoming,
                (None, None, None, None) => entry.text == update.text,
                _ => false,
            },
        )
        .map(|index| {
            let existing = history.remove(index);
            (
                existing.tooltip_popup_id,
                existing.tooltip_autosize_id,
                existing.preview_popup_id,
                existing.preview_autosize_id,
                existing.to_file_popup_id,
                existing.to_file_autosize_id,
                existing.to_data_popup_id,
                existing.to_data_autosize_id,
                existing.image_handle,
                existing.image_preview_handle,
            )
        });
    let (
        tooltip_popup_id,
        tooltip_autosize_id,
        preview_popup_id,
        preview_autosize_id,
        to_file_popup_id,
        to_file_autosize_id,
        to_data_popup_id,
        to_data_autosize_id,
        existing_image_handle,
        existing_image_preview_handle,
    ) = existing.unwrap_or_else(|| {
        (
            Id::unique(),
            widget::Id::unique(),
            Id::unique(),
            widget::Id::unique(),
            Id::unique(),
            widget::Id::unique(),
            Id::unique(),
            widget::Id::unique(),
            None,
            None,
        )
    });
    let image_handle = existing_image_handle.or_else(|| {
        update.image.as_ref().map(|image| {
            cosmic::iced::widget::image::Handle::from_rgba(
                image.thumbnail_width,
                image.thumbnail_height,
                image.thumbnail_rgba.as_ref().to_vec(),
            )
        })
    });
    let image_preview_handle = existing_image_preview_handle.or_else(|| {
        update.image.as_ref().map(|image| {
            if image.is_svg {
                ImagePreviewHandle::Svg(cosmic::iced::widget::svg::Handle::from_memory(
                    image.bytes.as_ref().to_vec(),
                ))
            } else {
                ImagePreviewHandle::Raster(cosmic::iced::widget::image::Handle::from_bytes(
                    cosmic::iced::core::Bytes::from_owner(image.bytes.clone()),
                ))
            }
        })
    });
    history.insert(
        0,
        HistoryItem {
            text: update.text,
            mime_type: update.mime_type,
            available_mime_types: update.available_mime_types,
            color_rgba: update.color_rgba,
            image: update.image,
            files: update.files,
            image_handle,
            image_preview_handle,
            captured_at: update.captured_at,
            tooltip_popup_id,
            tooltip_autosize_id,
            preview_popup_id,
            preview_autosize_id,
            to_file_popup_id,
            to_file_autosize_id,
            to_data_popup_id,
            to_data_autosize_id,
        },
    );
    history.truncate(limit);
}

/// Makes a history item the active clipboard item in one state transition.
/// The subsequent watcher event refreshes its metadata without duplicating it.
fn activate_history_item(
    history: &mut Vec<HistoryItem>,
    selected_item: &mut Option<usize>,
    index: usize,
) -> Option<ActivatedClipboardItem> {
    if index >= history.len() {
        return None;
    }
    let item = history.remove(index);
    let activated = ActivatedClipboardItem {
        text: item.text.clone(),
        color_rgba: item.color_rgba,
        image: item.image.clone(),
        files: item.files.clone(),
    };
    history.insert(0, item);
    *selected_item = Some(0);
    Some(activated)
}

struct ActivatedClipboardItem {
    text: String,
    color_rgba: Option<[u8; 4]>,
    image: Option<clipboard_watcher::ClipboardImage>,
    files: Option<clipboard_watcher::ClipboardFiles>,
}

fn activate_clipboard_content(item: ActivatedClipboardItem) -> Option<std::path::PathBuf> {
    if let Some(image) = item.image {
        clipboard_watcher::copy_image(image);
        None
    } else if let Some(files) = item.files {
        let generated = generated_path(&files);
        clipboard_watcher::copy_files(files);
        generated
    } else {
        clipboard_watcher::copy_text_with_color(item.text, item.color_rgba);
        None
    }
}

const TEMP_FILE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

fn temp_storage_dir() -> io::Result<std::path::PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    let path = std::path::PathBuf::from("/tmp/clipboard-for-cosmic");
    match fs::DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::other(
            "temporary storage path is not a real directory",
        ));
    }
    // SAFETY: `geteuid` has no preconditions and does not retain state.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "temporary storage directory belongs to another user",
        ));
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

fn cleanup_stale_temp_files() -> io::Result<()> {
    let directory = temp_storage_dir()?;
    let now = SystemTime::now();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        let stale = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= TEMP_FILE_MAX_AGE);
        if stale && (metadata.is_file() || metadata.file_type().is_symlink()) {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn generated_path(files: &clipboard_watcher::ClipboardFiles) -> Option<std::path::PathBuf> {
    let directory = std::path::Path::new("/tmp/clipboard-for-cosmic");
    files.entries.iter().find_map(|entry| {
        entry
            .path
            .as_ref()
            .filter(|path| path.parent() == Some(directory))
            .cloned()
    })
}

fn clipboard_update_contains_path(
    update: &clipboard_watcher::ClipboardUpdate,
    path: &std::path::Path,
) -> bool {
    update.files.as_ref().is_some_and(|files| {
        files
            .entries
            .iter()
            .any(|entry| entry.path.as_deref() == Some(path))
    })
}

fn report_temp_cleanup(
    history: &[HistoryItem],
    active: Option<&std::path::Path>,
    generated: &mut std::collections::HashSet<std::path::PathBuf>,
) {
    if let Err(error) = cleanup_unreferenced_temp_files(history, active, generated) {
        eprintln!("{APP_NAME}: could not clean temporary files: {error}");
    }
}

fn cleanup_unreferenced_temp_files(
    history: &[HistoryItem],
    active: Option<&std::path::Path>,
    generated: &mut std::collections::HashSet<std::path::PathBuf>,
) -> io::Result<()> {
    let mut referenced = history
        .iter()
        .filter_map(|item| item.files.as_ref())
        .flat_map(|files| files.entries.iter())
        .filter_map(|entry| entry.path.clone())
        .collect::<std::collections::HashSet<_>>();
    referenced.extend(active.map(std::path::Path::to_path_buf));
    let unreferenced = generated
        .iter()
        .filter(|path| !referenced.contains(*path))
        .cloned()
        .collect::<Vec<_>>();
    for path in unreferenced {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        generated.remove(&path);
    }
    Ok(())
}

fn materialize_history_item(
    item: &HistoryItem,
) -> Result<clipboard_watcher::ClipboardFiles, Box<dyn std::error::Error>> {
    let (bytes, suffix): (&[u8], &str) = if let Some(image) = &item.image {
        let suffix = match image.mime_type.as_str() {
            "image/png" => ".png",
            "image/jpeg" | "image/jpg" => ".jpg",
            "image/webp" => ".webp",
            "image/gif" => ".gif",
            "image/bmp" | "image/x-bmp" => ".bmp",
            "image/tiff" => ".tiff",
            "image/svg+xml" => ".svg",
            _ => ".img",
        };
        (image.bytes.as_ref(), suffix)
    } else {
        (item.text.as_bytes(), ".txt")
    };
    let mut temporary = tempfile::Builder::new()
        .prefix("jax-clipboard-")
        .suffix(suffix)
        .tempfile_in(temp_storage_dir()?)?;
    temporary.write_all(bytes)?;
    temporary.flush()?;
    let (_, path) = temporary.keep().map_err(|error| error.error)?;
    Ok(clipboard_watcher::file_reference(&path)?)
}

fn history_list(
    history: &[HistoryItem],
    visible_indices: &[usize],
    parent_window: Option<Id>,
    selected_item: Option<usize>,
    hovered_item: Option<usize>,
    history_scroll_id: widget::Id,
) -> Element<'static, WindowMessage> {
    let mut entries = widget::column::with_capacity(0).spacing(2);
    if visible_indices.is_empty() {
        let empty_message = if history.is_empty() {
            "No clipboard items yet."
        } else {
            "No matches."
        };
        return widget::container(widget::text(empty_message))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Horizontal::Center)
            .align_y(Alignment::Center)
            .into();
    } else {
        for &index in visible_indices {
            let Some(item) = history.get(index) else {
                continue;
            };
            let preview = history_preview(&item.text);
            let tooltip = history_tooltip(item);
            let selected = selected_item == Some(index);
            let row_hovered = hovered_item == Some(index);
            let can_convert_to_data = item
                .files
                .as_ref()
                .and_then(single_regular_file_path)
                .is_some();
            let action_width = HISTORY_ACTION_WIDTH
                + if item.files.is_none() {
                    TO_FILE_ACTION_WIDTH
                } else if can_convert_to_data {
                    TO_DATA_ACTION_WIDTH
                } else {
                    0.0
                };
            let mut item_content: Vec<Element<'static, WindowMessage>> = vec![
                widget::text(if selected { "›" } else { " " })
                    .width(14.0)
                    .into(),
            ];
            if let Some(handle) = &item.image_handle {
                let thumbnail: Element<'static, WindowMessage> = widget::image(handle.clone())
                    .width(16.0)
                    .height(16.0)
                    .content_fit(cosmic::iced::ContentFit::Cover)
                    .border_radius(4.0)
                    .into();
                let thumbnail = if let Some(preview_handle) = item.image_preview_handle.clone() {
                    wayland_tooltip(
                        thumbnail,
                        HistoryTooltip {
                            text: String::new(),
                            image: Some(preview_handle),
                        },
                        parent_window.unwrap_or(Id::RESERVED),
                        item.preview_popup_id,
                        item.preview_autosize_id.clone(),
                        false,
                    )
                } else {
                    thumbnail
                };
                item_content.push(thumbnail);
                item_content.push(widget::Space::new().width(6.0).into());
            } else if let Some(files) = &item.files {
                let icon_name = if files.entries.iter().all(|entry| entry.is_dir) {
                    "folder-symbolic"
                } else {
                    "text-x-generic-symbolic"
                };
                item_content.push(widget::icon::from_name(icon_name).size(16).icon().into());
                item_content.push(widget::Space::new().width(6.0).into());
            } else if let Some(rgba) = item.color_rgba {
                let [red, green, blue, alpha] = rgba;
                item_content.push(
                    widget::container(widget::Space::new().width(16.0).height(16.0))
                        .width(16.0)
                        .height(16.0)
                        .class(cosmic::theme::Container::custom(move |_| {
                            widget::container::Style {
                                background: Some(cosmic::iced::Background::Color(
                                    cosmic::iced::Color::from_rgba8(
                                        red,
                                        green,
                                        blue,
                                        alpha as f32 / 255.0,
                                    ),
                                )),
                                border: cosmic::iced::Border {
                                    color: cosmic::iced::Color::from_rgba(0.5, 0.5, 0.5, 0.5),
                                    width: 1.0,
                                    radius: 4.0.into(),
                                },
                                ..Default::default()
                            }
                        }))
                        .into(),
                );
                item_content.push(widget::Space::new().width(6.0).into());
            }
            item_content.extend([
                widget::text(preview)
                    .width(Length::Fill)
                    .wrapping(text::Wrapping::None)
                    .ellipsize(text::Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
                    .into(),
                widget::Space::new().width(action_width + 4.0).into(),
            ]);
            let item_surface = widget::container(
                widget::row::with_children(item_content).align_y(Alignment::Center),
            )
            .height(HISTORY_ROW_HEIGHT)
            .padding(cosmic::theme::spacing().space_xs)
            .width(Length::Fill)
            .class(cosmic::theme::Container::custom(move |_| {
                widget::container::Style {
                    background: if row_hovered {
                        Some(cosmic::iced::Background::Color(
                            cosmic::iced::Color::from_rgba(1.0, 1.0, 1.0, 0.04),
                        ))
                    } else {
                        None
                    },
                    border: cosmic::iced::Border::default(),
                    ..Default::default()
                }
            }));
            let item_target =
                widget::mouse_area(item_surface).on_press(WindowMessage::ActivateItem(index));

            let mut actions = Vec::with_capacity(2);
            if item.files.is_none() {
                let to_file = hover_action(
                    TO_FILE_ICON,
                    WindowMessage::ConvertToFile(index),
                    TO_FILE_ACTION_WIDTH,
                    row_hovered,
                );
                let to_file = wayland_tooltip(
                    to_file,
                    HistoryTooltip {
                        text: "To file".into(),
                        image: None,
                    },
                    parent_window.unwrap_or(Id::RESERVED),
                    item.to_file_popup_id,
                    item.to_file_autosize_id.clone(),
                    false,
                );
                actions.push(
                    widget::container(to_file)
                        .width(TO_FILE_ACTION_WIDTH)
                        .height(Length::Fill)
                        .align_x(Horizontal::Center)
                        .align_y(Alignment::Center)
                        .into(),
                );
            } else if can_convert_to_data {
                let to_data = hover_action(
                    TO_DATA_ICON,
                    WindowMessage::ConvertToData(index),
                    TO_DATA_ACTION_WIDTH,
                    row_hovered,
                );
                let to_data = wayland_tooltip(
                    to_data,
                    HistoryTooltip {
                        text: "To data".into(),
                        image: None,
                    },
                    parent_window.unwrap_or(Id::RESERVED),
                    item.to_data_popup_id,
                    item.to_data_autosize_id.clone(),
                    false,
                );
                actions.push(
                    widget::container(to_data)
                        .width(TO_DATA_ACTION_WIDTH)
                        .height(Length::Fill)
                        .align_x(Horizontal::Center)
                        .align_y(Alignment::Center)
                        .into(),
                );
            }
            actions.push(hover_action(
                DELETE_ICON,
                WindowMessage::DeleteItem(index),
                HISTORY_ACTION_WIDTH,
                row_hovered,
            ));
            let action_panel = widget::container(
                widget::row::with_children(actions)
                    .width(action_width)
                    .height(Length::Fill),
            )
            .width(action_width)
            .height(Length::Fill)
            .class(cosmic::theme::Container::custom(move |_| {
                widget::container::Style {
                    background: row_hovered.then_some(cosmic::iced::Background::Color(
                        cosmic::iced::Color::from_rgba(0.06, 0.07, 0.08, 0.42),
                    )),
                    border: cosmic::iced::Border {
                        radius: 8.0.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }
            }));
            let menu_overlay = widget::container(action_panel)
                .width(Length::Fill)
                .height(Length::Fill)
                .padding([3, 0])
                .align_x(Horizontal::Right);
            let row: Element<'static, WindowMessage> =
                cosmic::iced::widget::stack([item_target.into(), menu_overlay.into()])
                    .width(Length::Fill)
                    .height(HISTORY_ROW_HEIGHT)
                    .into();
            let hoverable = widget::mouse_area(row)
                .on_enter(WindowMessage::HoverItem(Some(index)))
                .on_exit(WindowMessage::HoverItem(None));

            entries = entries.push(wayland_tooltip(
                hoverable,
                tooltip,
                parent_window.unwrap_or(Id::RESERVED),
                item.tooltip_popup_id,
                item.tooltip_autosize_id.clone(),
                true,
            ));
        }
    }
    widget::scrollable(entries)
        .id(history_scroll_id)
        .height(Length::Fill)
        .into()
}

fn hover_action(
    icon: &'static [u8],
    message: WindowMessage,
    width: f32,
    row_hovered: bool,
) -> Element<'static, WindowMessage> {
    let icon = widget::svg(cosmic::iced::widget::svg::Handle::from_memory(icon))
        .width(20.0)
        .height(20.0)
        .opacity(if row_hovered { 1.0 } else { 0.0 })
        .symbolic(true);
    let centered_icon = widget::container(icon)
        .width(width)
        .height(Length::Fill)
        .align_x(Horizontal::Center)
        .align_y(Alignment::Center);
    widget::mouse_area(centered_icon).on_press(message).into()
}

#[derive(Clone)]
struct HistoryTooltip {
    text: String,
    image: Option<ImagePreviewHandle>,
}

#[derive(Clone, Debug)]
enum ImagePreviewHandle {
    Raster(cosmic::iced::widget::image::Handle),
    Svg(cosmic::iced::widget::svg::Handle),
}

fn wayland_tooltip(
    content: impl Into<Element<'static, WindowMessage>>,
    tooltip: HistoryTooltip,
    parent: Id,
    popup_id: Id,
    autosize_id: widget::Id,
    fill_width: bool,
) -> Element<'static, WindowMessage> {
    use cosmic::cctk::sctk::reexports::protocols::xdg::shell::client::xdg_positioner::{
        Anchor, Gravity,
    };
    use cosmic::iced::runtime::platform_specific::wayland::popup::{
        SctkPopupSettings, SctkPositioner,
    };

    let is_image = tooltip.image.is_some();
    let tooltip_widget =
        cosmic::widget::wayland::tooltip::widget::Tooltip::<WindowMessage, WindowMessage>::new(
            content,
            Some(move |bounds: cosmic::iced::Rectangle| SctkPopupSettings {
                parent,
                id: popup_id,
                grab: false,
                input_zone: is_image.then(Vec::new),
                positioner: SctkPositioner {
                    size: None,
                    size_limits: cosmic::iced::Limits::NONE.min_width(1.0).min_height(1.0),
                    anchor_rect: cosmic::iced::Rectangle {
                        x: bounds.x.round() as i32,
                        y: bounds.y.round() as i32,
                        width: bounds.width.round() as i32,
                        height: bounds.height.round() as i32,
                    },
                    anchor: Anchor::BottomRight,
                    gravity: Gravity::BottomRight,
                    constraint_adjustment: 15,
                    offset: (8, 8),
                    reactive: true,
                },
                parent_size: None,
                close_with_children: true,
            }),
            move || {
                let tooltip_content = if let Some(handle) = tooltip.image.clone() {
                    let preview: Element<'static, cosmic::Action<WindowMessage>> = match handle {
                        ImagePreviewHandle::Raster(handle) => widget::image(handle)
                            .width(1920.0)
                            .height(1080.0)
                            .content_fit(cosmic::iced::ContentFit::Contain)
                            .border_radius(8.0)
                            .into(),
                        ImagePreviewHandle::Svg(handle) => widget::svg(handle)
                            .width(1920.0)
                            .height(1080.0)
                            .content_fit(cosmic::iced::ContentFit::Contain)
                            .into(),
                    };
                    widget::column::with_children([preview])
                } else {
                    widget::column::with_children([widget::text(tooltip.text.clone()).into()])
                };
                widget::autosize::autosize(
                    widget::layer_container(tooltip_content).padding(6),
                    autosize_id.clone(),
                )
                .into()
            },
            WindowMessage::Surface(cosmic::surface::action::destroy_popup(popup_id)),
            WindowMessage::Surface,
        )
        .delay(Duration::from_millis(if is_image { 1_200 } else { 350 }));
    if fill_width {
        tooltip_widget.width(Length::Fill).into()
    } else {
        tooltip_widget.into()
    }
}

fn history_tooltip(item: &HistoryItem) -> HistoryTooltip {
    let characters = item.text.chars().count();
    let bytes = item.text.len();
    let lines = item.text.lines().count().max(1);
    let captured = SystemTime::now()
        .duration_since(item.captured_at)
        .map(|duration| format_capture_time(item.captured_at, duration.as_secs()))
        .unwrap_or_else(|_| format_local_timestamp(item.captured_at));
    let offered = item.available_mime_types.join(", ");
    let color = item
        .color_rgba
        .map(clipboard_watcher::format_color)
        .map(|color| format!("\nColor: {color}"))
        .unwrap_or_default();
    let text = if let Some(image) = &item.image {
        format!(
            "{}\nMIME: {}\nAvailable types: {offered}\nCaptured: {captured}",
            clipboard_watcher::image_label(image),
            item.mime_type
        )
    } else if let Some(files) = &item.files {
        files_tooltip(files, &offered, &captured)
    } else {
        format!(
            "{characters} characters · {bytes} UTF-8 bytes · {lines} lines\nMIME: {}\nAvailable types: {offered}\nCaptured: {captured}",
            item.mime_type
        ) + &color
    };
    HistoryTooltip { text, image: None }
}

fn files_tooltip(
    files: &clipboard_watcher::ClipboardFiles,
    offered: &str,
    captured: &str,
) -> String {
    let operation = match files.operation {
        clipboard_watcher::FileOperation::Copy => "Copy",
        clipboard_watcher::FileOperation::Cut => "Cut",
    };
    let mut paths = files
        .entries
        .iter()
        .take(20)
        .map(|entry| {
            entry
                .path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|| entry.uri.clone())
        })
        .collect::<Vec<_>>();
    if files.entries.len() > paths.len() {
        paths.push(format!("… and {} more", files.entries.len() - paths.len()));
    }
    format!(
        "{}\nOperation: {operation}\n{}\nMIME: text/uri-list\nAvailable types: {offered}\nCaptured: {captured}",
        clipboard_watcher::files_label(files),
        paths.join("\n")
    )
}

fn format_capture_time(captured_at: SystemTime, seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds}s ago"),
        60..=3_599 => format!("{}m ago", seconds / 60),
        _ => format_local_timestamp(captured_at),
    }
}

fn format_local_timestamp(time: SystemTime) -> String {
    let Ok(duration) = time.duration_since(std::time::UNIX_EPOCH) else {
        return "unknown".into();
    };
    let unix_time = duration.as_secs() as libc::time_t;
    // SAFETY: `localtime_r` writes to the provided valid `tm` pointer and does
    // not retain either pointer after returning.
    let local = unsafe {
        let mut local = std::mem::zeroed::<libc::tm>();
        if libc::localtime_r(&unix_time, &mut local).is_null() {
            return "unknown".into();
        }
        local
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        local.tm_year + 1900,
        local.tm_mon + 1,
        local.tm_mday,
        local.tm_hour,
        local.tm_min,
        local.tm_sec
    )
}

/// Produces a compact, single-line preview without retaining a large rendered layout.
fn history_preview(value: &str) -> String {
    let mut preview = String::with_capacity(HISTORY_PREVIEW_CHARS + 1);
    let mut previous_was_space = true;
    let mut character_count = 0;
    let mut truncated = false;

    for character in value.chars() {
        let character = if character.is_whitespace() {
            ' '
        } else {
            character
        };
        if character == ' ' && previous_was_space {
            continue;
        }
        if character_count == HISTORY_PREVIEW_CHARS {
            truncated = true;
            break;
        }
        preview.push(character);
        character_count += 1;
        previous_was_space = character == ' ';
    }

    let trimmed_len = preview.trim_end().len();
    preview.truncate(trimmed_len);
    if truncated {
        preview.push('…');
    }
    preview
}

fn searchable_text(item: &HistoryItem) -> String {
    let mut searchable = String::with_capacity(item.text.len() + 96);
    searchable.push_str(&item.text);
    searchable.push(' ');
    searchable.push_str(&item.mime_type);
    for mime_type in &item.available_mime_types {
        searchable.push(' ');
        searchable.push_str(mime_type);
    }
    if let Some(image) = &item.image {
        searchable.push_str(&format!(" {}x{} image", image.width, image.height));
    }
    if let Some(files) = &item.files {
        for entry in &files.entries {
            searchable.push(' ');
            searchable.push_str(&entry.uri);
            if let Some(path) = &entry.path {
                searchable.push(' ');
                searchable.push_str(&path.to_string_lossy());
            }
        }
    }
    searchable
}

fn matches_search(searchable: &str, lowercase_query: &str) -> bool {
    searchable.to_lowercase().contains(lowercase_query)
}

fn move_selection(visible: &[usize], current_position: usize, backwards: bool) -> usize {
    let next_position = if backwards {
        current_position.checked_sub(1).unwrap_or(visible.len() - 1)
    } else {
        (current_position + 1) % visible.len()
    };
    visible[next_position]
}

const HISTORY_LIMIT_OPTIONS: [usize; 5] = [50, 100, DEFAULT_HISTORY_LIMIT, 500, 1_000];

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str) -> HistoryItem {
        HistoryItem {
            text: text.into(),
            mime_type: "text/plain".into(),
            available_mime_types: vec!["text/plain".into()],
            color_rgba: clipboard_watcher::parse_color_expression(text),
            image: None,
            files: None,
            image_handle: None,
            image_preview_handle: None,
            captured_at: SystemTime::now(),
            tooltip_popup_id: Id::unique(),
            tooltip_autosize_id: widget::Id::unique(),
            preview_popup_id: Id::unique(),
            preview_autosize_id: widget::Id::unique(),
            to_file_popup_id: Id::unique(),
            to_file_autosize_id: widget::Id::unique(),
            to_data_popup_id: Id::unique(),
            to_data_autosize_id: widget::Id::unique(),
        }
    }

    #[test]
    fn activating_an_item_moves_it_to_the_top_and_selects_it() {
        let mut history = vec![item("newest"), item("middle"), item("oldest")];
        let mut selected = Some(2);

        let copied = activate_history_item(&mut history, &mut selected, 1);

        assert_eq!(
            copied.as_ref().map(|item| item.text.as_str()),
            Some("middle")
        );
        assert_eq!(selected, Some(0));
        assert_eq!(
            history
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            ["middle", "newest", "oldest"]
        );
    }

    #[test]
    fn invalid_activation_does_not_change_history_or_selection() {
        let mut history = vec![item("only")];
        let mut selected = Some(0);

        assert!(activate_history_item(&mut history, &mut selected, 4).is_none());
        assert_eq!(selected, Some(0));
        assert_eq!(history[0].text, "only");
    }

    #[test]
    fn search_matches_text_and_metadata_without_case_sensitivity() {
        let mut entry = item("A Useful Snippet");
        entry.mime_type = "application/example".into();
        let searchable = searchable_text(&entry);

        assert!(matches_search(&searchable, "useful"));
        assert!(matches_search(&searchable, "application/example"));
        assert!(!matches_search(&searchable, "missing"));
    }

    #[test]
    fn unified_selection_navigation_loops_over_visible_results() {
        let visible = [1, 4, 7];

        assert_eq!(move_selection(&visible, 0, false), 4);
        assert_eq!(move_selection(&visible, 2, false), 1);
        assert_eq!(move_selection(&visible, 2, true), 4);
        assert_eq!(move_selection(&visible, 0, true), 7);
    }
}
