use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use tokio::io::AsyncReadExt;
use tokio::sync::broadcast;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop, event_created_child};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1, ext_data_control_manager_v1, ext_data_control_offer_v1,
    ext_data_control_source_v1,
};

type Device = ext_data_control_device_v1::ExtDataControlDeviceV1;
type Manager = ext_data_control_manager_v1::ExtDataControlManagerV1;
type Offer = ext_data_control_offer_v1::ExtDataControlOfferV1;
type Source = ext_data_control_source_v1::ExtDataControlSourceV1;

#[derive(Clone, Debug)]
pub struct ClipboardUpdate {
    pub text: String,
    pub mime_type: String,
    pub available_mime_types: Vec<String>,
    pub color_rgba: Option<[u8; 4]>,
    pub image: Option<ClipboardImage>,
    pub files: Option<ClipboardFiles>,
    pub captured_at: SystemTime,
}

#[derive(Clone, Debug)]
pub struct ClipboardImage {
    pub mime_type: String,
    pub bytes: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
    pub preview_rgba: Arc<[u8]>,
    pub preview_width: u32,
    pub preview_height: u32,
    pub thumbnail_rgba: Arc<[u8]>,
    pub thumbnail_width: u32,
    pub thumbnail_height: u32,
    pub is_svg: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardFiles {
    pub entries: Vec<ClipboardFile>,
    pub operation: FileOperation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardFile {
    pub uri: String,
    pub path: Option<std::path::PathBuf>,
    pub size: Option<u64>,
    pub is_dir: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileOperation {
    Copy,
    Cut,
}

const MAX_CLIPBOARD_BYTES: u64 = 50 * 1024 * 1024;
const THUMBNAIL_SIZE: u32 = 16;
const PREVIEW_MAX_WIDTH: u32 = 960;
const PREVIEW_MAX_HEIGHT: u32 = 540;

static UPDATES: OnceLock<broadcast::Sender<ClipboardUpdate>> = OnceLock::new();
static WRITE_GENERATION: AtomicU64 = AtomicU64::new(0);
static WRITE_SETUP: Mutex<()> = Mutex::new(());

pub fn subscribe() -> broadcast::Receiver<ClipboardUpdate> {
    UPDATES
        .get_or_init(|| {
            let (sender, _) = broadcast::channel(64);
            start(sender.clone());
            sender
        })
        .subscribe()
}

pub fn copy_text(text: String) {
    copy_content(text, None);
}

pub fn copy_text_with_color(text: String, color_rgba: Option<[u8; 4]>) {
    copy_content(text, color_rgba);
}

fn copy_content(text: String, color_rgba: Option<[u8; 4]>) {
    let generation = WRITE_GENERATION
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_add(1);
    std::thread::spawn(move || {
        if let Err(error) = provide_clipboard(Some(text), color_rgba, None, None, generation) {
            eprintln!("ClipboardForCosmic could not set the clipboard: {error}");
        }
    });
}

pub fn copy_image(image: ClipboardImage) {
    let generation = WRITE_GENERATION
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_add(1);
    std::thread::spawn(move || {
        let text = image
            .is_svg
            .then(|| String::from_utf8(image.bytes.as_ref().to_vec()).ok())
            .flatten();
        if let Err(error) = provide_clipboard(text, None, Some(image), None, generation) {
            eprintln!("ClipboardForCosmic could not set the clipboard image: {error}");
        }
    });
}

pub fn copy_files(files: ClipboardFiles) {
    let generation = WRITE_GENERATION
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_add(1);
    std::thread::spawn(move || {
        if let Err(error) = provide_clipboard(None, None, None, Some(files), generation) {
            eprintln!("ClipboardForCosmic could not set the clipboard files: {error}");
        }
    });
}

fn provide_clipboard(
    text: Option<String>,
    color_rgba: Option<[u8; 4]>,
    image: Option<ClipboardImage>,
    files: Option<ClipboardFiles>,
    generation: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    // Wayland clipboard sources must remain alive while serving their data, so
    // each request has its own connection. Serialize only connection setup and
    // discard superseded requests to preserve call order under rapid writes.
    let setup = WRITE_SETUP
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if generation != WRITE_GENERATION.load(Ordering::SeqCst) {
        return Ok(());
    }
    let connection = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<CopyState>(&connection)?;
    let qh = queue.handle();
    let manager: Manager = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| "COSMIC does not expose ext-data-control")?;
    let seat: wl_seat::WlSeat = globals
        .bind(&qh, 1..=9, ())
        .map_err(|_| "COSMIC did not advertise a Wayland seat")?;
    let device = manager.get_data_device(&seat, &qh, ());
    let source = manager.create_data_source(&qh, ());
    if text.is_some() {
        source.offer("text/plain;charset=utf-8".into());
        source.offer("text/plain".into());
    }
    if color_rgba.is_some() {
        source.offer("application/x-color".into());
    }
    if let Some(image) = &image {
        source.offer(image.mime_type.clone());
    }
    if files.is_some() {
        source.offer("text/uri-list".into());
        source.offer("x-special/gnome-copied-files".into());
    }
    device.set_selection(Some(&source));
    connection.flush()?;
    drop(setup);

    let mut state = CopyState {
        text,
        native_color: color_rgba.map(encode_native_color),
        image,
        files,
        cancelled: false,
        current_offer: None,
    };
    while !state.cancelled {
        queue.blocking_dispatch(&mut state)?;
    }
    Ok(())
}

fn start(sender: broadcast::Sender<ClipboardUpdate>) {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .expect("create clipboard watcher runtime");

        runtime.block_on(async move {
            if let Err(error) = watch(sender).await {
                eprintln!("ClipboardForCosmic watcher stopped: {error}");
            }
        });
    });
}

async fn watch(
    sender: broadcast::Sender<ClipboardUpdate>,
) -> Result<(), Box<dyn std::error::Error>> {
    let connection = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&connection)?;
    let qh = queue.handle();

    let manager: Manager = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| "COSMIC does not expose ext-data-control")?;
    let seat: wl_seat::WlSeat = globals
        .bind(&qh, 1..=9, ())
        .map_err(|_| "COSMIC did not advertise a Wayland seat")?;
    let _device = manager.get_data_device(&seat, &qh, ());
    let mut state = State::default();

    loop {
        queue.blocking_dispatch(&mut state)?;
        let Some(offer) = state.selection.take() else {
            continue;
        };
        let Some(mime_types) = state.offers.remove(&offer) else {
            continue;
        };
        let Some(mime) = preferred_mime(&mime_types).map(str::to_owned) else {
            offer.destroy();
            continue;
        };

        let (write, read) = tokio::net::unix::pipe::pipe()?;
        offer.receive(mime.clone(), write.as_fd());
        connection.flush()?;
        drop(write);

        let mut bytes = Vec::new();
        read.take(MAX_CLIPBOARD_BYTES + 1)
            .read_to_end(&mut bytes)
            .await?;
        offer.destroy();
        if bytes.len() as u64 > MAX_CLIPBOARD_BYTES {
            eprintln!("ClipboardForCosmic ignored clipboard data larger than 50 MiB");
            continue;
        }
        let content = if mime == "application/x-color" {
            decode_native_color(&bytes).map(|rgba| (format_color(rgba), Some(rgba), None, None))
        } else if is_file_mime(&mime) {
            decode_files(&mime, &bytes).map(|files| (files_label(&files), None, None, Some(files)))
        } else if is_image_mime(&mime) {
            decode_image(&mime, bytes).map(|image| (image_label(&image), None, Some(image), None))
        } else {
            String::from_utf8(bytes).ok().map(|text| {
                if let Some(image) = detect_svg_text(&text) {
                    (image_label(&image), None, Some(image), None)
                } else {
                    let color = parse_color_expression(&text);
                    (text, color, None, None)
                }
            })
        };
        if let Some((text, color_rgba, image, files)) = content {
            let recorded_mime = if image.is_some() {
                image
                    .as_ref()
                    .map(|image| image.mime_type.clone())
                    .unwrap_or(mime)
            } else if files.is_some() {
                "text/uri-list".into()
            } else {
                mime
            };
            let _ = sender.send(ClipboardUpdate {
                text,
                mime_type: recorded_mime,
                available_mime_types: mime_types,
                color_rgba,
                image,
                files,
                captured_at: SystemTime::now(),
            });
        }
    }
}

fn preferred_mime(mime_types: &[String]) -> Option<&str> {
    [
        "x-special/gnome-copied-files",
        "text/uri-list",
        "image/png",
        "image/jpeg",
        "image/jpg",
        "image/webp",
        "image/gif",
        "image/bmp",
        "image/x-bmp",
        "image/tiff",
        "image/svg+xml",
        "application/x-color",
        "text/plain;charset=utf-8",
        "text/plain",
        "UTF8_STRING",
    ]
    .into_iter()
    .find(|preferred| mime_types.iter().any(|mime| mime == preferred))
    .or_else(|| {
        mime_types
            .iter()
            .find(|mime| mime.starts_with("text/"))
            .map(String::as_str)
    })
}

fn is_file_mime(mime: &str) -> bool {
    matches!(mime, "text/uri-list" | "x-special/gnome-copied-files")
}

fn decode_files(mime: &str, bytes: &[u8]) -> Option<ClipboardFiles> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let operation = if mime == "x-special/gnome-copied-files" {
        match lines.next()? {
            "copy" => FileOperation::Copy,
            "cut" => FileOperation::Cut,
            _ => return None,
        }
    } else {
        FileOperation::Copy
    };
    let entries = lines
        .filter(|line| !line.starts_with('#'))
        .filter_map(file_entry)
        .collect::<Vec<_>>();
    (!entries.is_empty()).then_some(ClipboardFiles { entries, operation })
}

fn file_entry(uri: &str) -> Option<ClipboardFile> {
    let parsed = url::Url::parse(uri).ok()?;
    let path = parsed.to_file_path().ok();
    let metadata = path
        .as_ref()
        .and_then(|path| std::fs::symlink_metadata(path).ok());
    Some(ClipboardFile {
        uri: uri.to_owned(),
        path,
        size: metadata
            .as_ref()
            .filter(|metadata| metadata.is_file())
            .map(std::fs::Metadata::len),
        is_dir: metadata.is_some_and(|metadata| metadata.is_dir()),
    })
}

pub fn files_label(files: &ClipboardFiles) -> String {
    let total_size = files
        .entries
        .iter()
        .filter_map(|entry| entry.size)
        .sum::<u64>();
    let size = (total_size > 0).then(|| format!(" · {}", format_bytes(total_size)));
    if let [entry] = files.entries.as_slice() {
        format!("{}{}", file_name(entry), size.unwrap_or_default())
    } else {
        let directories = files.entries.iter().filter(|entry| entry.is_dir).count();
        let noun = if directories == files.entries.len() {
            "folders"
        } else if directories == 0 {
            "files"
        } else {
            "items"
        };
        format!("{} {noun}{}", files.entries.len(), size.unwrap_or_default())
    }
}

pub fn file_name(file: &ClipboardFile) -> String {
    file.path
        .as_ref()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .or_else(|| {
            url::Url::parse(&file.uri)
                .ok()?
                .path_segments()?
                .next_back()
                .map(str::to_owned)
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| file.uri.clone())
}

pub fn file_reference(path: &std::path::Path) -> std::io::Result<ClipboardFiles> {
    let uri = url::Url::from_file_path(path)
        .map_err(|()| std::io::Error::other("temporary file path is not an absolute file path"))?
        .to_string();
    let entry =
        file_entry(&uri).ok_or_else(|| std::io::Error::other("could not create a file URI"))?;
    Ok(ClipboardFiles {
        entries: vec![entry],
        operation: FileOperation::Copy,
    })
}

pub fn read_file_as_clipboard_update(path: &std::path::Path) -> std::io::Result<ClipboardUpdate> {
    let metadata = fs_metadata_file(path)?;
    if metadata.len() > MAX_CLIPBOARD_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file is larger than 50 MiB",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    std::fs::File::open(path)?
        .take(MAX_CLIPBOARD_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CLIPBOARD_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file is larger than 50 MiB",
        ));
    }

    if let Ok(text) = std::str::from_utf8(&bytes)
        && let Some(image) = detect_svg_text(text)
    {
        return Ok(ClipboardUpdate {
            text: image_label(&image),
            mime_type: "image/svg+xml".into(),
            available_mime_types: vec!["image/svg+xml".into(), "text/plain".into()],
            color_rgba: None,
            image: Some(image),
            files: None,
            captured_at: SystemTime::now(),
        });
    }
    if let Ok(format) = image::guess_format(&bytes)
        && let Some(mime) = mime_for_image_format(format)
        && let Some(image) = decode_image(mime, bytes.clone())
    {
        return Ok(ClipboardUpdate {
            text: image_label(&image),
            mime_type: mime.into(),
            available_mime_types: vec![mime.into()],
            color_rgba: None,
            image: Some(image),
            files: None,
            captured_at: SystemTime::now(),
        });
    }
    let text = String::from_utf8(bytes).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file is neither a supported image nor UTF-8 text",
        )
    })?;
    Ok(ClipboardUpdate {
        color_rgba: parse_color_expression(&text),
        text,
        mime_type: "text/plain;charset=utf-8".into(),
        available_mime_types: vec!["text/plain;charset=utf-8".into(), "text/plain".into()],
        image: None,
        files: None,
        captured_at: SystemTime::now(),
    })
}

fn fs_metadata_file(path: &std::path::Path) -> std::io::Result<std::fs::Metadata> {
    let metadata = std::fs::metadata(path)?;
    if metadata.is_file() {
        Ok(metadata)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "clipboard URI does not refer to a regular file",
        ))
    }
}

fn uri_list_payload(files: &ClipboardFiles) -> Vec<u8> {
    let mut payload = files
        .entries
        .iter()
        .map(|entry| entry.uri.as_str())
        .collect::<Vec<_>>()
        .join("\r\n");
    payload.push_str("\r\n");
    payload.into_bytes()
}

fn gnome_files_payload(files: &ClipboardFiles) -> Vec<u8> {
    let operation = match files.operation {
        FileOperation::Copy => "copy",
        FileOperation::Cut => "cut",
    };
    let mut payload = format!("{operation}\n");
    payload.push_str(
        &files
            .entries
            .iter()
            .map(|entry| entry.uri.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    );
    payload.push('\n');
    payload.into_bytes()
}

fn is_image_mime(mime: &str) -> bool {
    mime == "image/svg+xml" || image_format(mime).is_some()
}

fn image_format(mime: &str) -> Option<image::ImageFormat> {
    match mime {
        "image/png" => Some(image::ImageFormat::Png),
        "image/jpeg" | "image/jpg" => Some(image::ImageFormat::Jpeg),
        "image/webp" => Some(image::ImageFormat::WebP),
        "image/gif" => Some(image::ImageFormat::Gif),
        "image/bmp" | "image/x-bmp" => Some(image::ImageFormat::Bmp),
        "image/tiff" => Some(image::ImageFormat::Tiff),
        _ => None,
    }
}

fn mime_for_image_format(format: image::ImageFormat) -> Option<&'static str> {
    match format {
        image::ImageFormat::Png => Some("image/png"),
        image::ImageFormat::Jpeg => Some("image/jpeg"),
        image::ImageFormat::WebP => Some("image/webp"),
        image::ImageFormat::Gif => Some("image/gif"),
        image::ImageFormat::Bmp => Some("image/bmp"),
        image::ImageFormat::Tiff => Some("image/tiff"),
        _ => None,
    }
}

fn decode_image(mime_type: &str, bytes: Vec<u8>) -> Option<ClipboardImage> {
    if mime_type == "image/svg+xml" {
        return decode_svg(bytes);
    }
    use image::GenericImageView;

    let decoded = image::load_from_memory_with_format(&bytes, image_format(mime_type)?).ok()?;
    let (width, height) = decoded.dimensions();
    let preview = decoded
        .thumbnail(PREVIEW_MAX_WIDTH.min(width), PREVIEW_MAX_HEIGHT.min(height))
        .to_rgba8();
    let (preview_width, preview_height) = preview.dimensions();
    let thumbnail = decoded
        .thumbnail(THUMBNAIL_SIZE.min(width), THUMBNAIL_SIZE.min(height))
        .to_rgba8();
    let (thumbnail_width, thumbnail_height) = thumbnail.dimensions();
    (width > 0 && height > 0).then(|| ClipboardImage {
        mime_type: mime_type.to_owned(),
        bytes: Arc::from(bytes),
        width,
        height,
        preview_rgba: Arc::from(preview.into_raw()),
        preview_width,
        preview_height,
        thumbnail_rgba: Arc::from(thumbnail.into_raw()),
        thumbnail_width,
        thumbnail_height,
        is_svg: false,
    })
}

fn decode_svg(bytes: Vec<u8>) -> Option<ClipboardImage> {
    let tree = resvg::usvg::Tree::from_data(&bytes, &resvg::usvg::Options::default()).ok()?;
    let size = tree.size();
    let width = size.width().round().max(1.0) as u32;
    let height = size.height().round().max(1.0) as u32;
    let preview_scale = (PREVIEW_MAX_WIDTH as f32 / size.width())
        .min(PREVIEW_MAX_HEIGHT as f32 / size.height())
        .min(1.0);
    let preview_width = (size.width() * preview_scale).round().max(1.0) as u32;
    let preview_height = (size.height() * preview_scale).round().max(1.0) as u32;
    let preview_rgba = render_svg_rgba(&tree, preview_width, preview_height, preview_scale)?;
    let scale = (THUMBNAIL_SIZE as f32 / size.width())
        .min(THUMBNAIL_SIZE as f32 / size.height())
        .min(1.0);
    let thumbnail_width = (size.width() * scale).round().max(1.0) as u32;
    let thumbnail_height = (size.height() * scale).round().max(1.0) as u32;
    let thumbnail_rgba = render_svg_rgba(&tree, thumbnail_width, thumbnail_height, scale)?;
    Some(ClipboardImage {
        mime_type: "image/svg+xml".into(),
        bytes: Arc::from(bytes),
        width,
        height,
        preview_rgba: Arc::from(preview_rgba),
        preview_width,
        preview_height,
        thumbnail_rgba: Arc::from(thumbnail_rgba),
        thumbnail_width,
        thumbnail_height,
        is_svg: true,
    })
}

fn render_svg_rgba(
    tree: &resvg::usvg::Tree,
    width: u32,
    height: u32,
    scale: f32,
) -> Option<Vec<u8>> {
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)?;
    resvg::render(
        tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let mut rgba = pixmap.take();
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        for channel in &mut pixel[..3] {
            *channel = (u16::from(*channel) * 255)
                .checked_div(alpha)
                .unwrap_or(0)
                .min(255) as u8;
        }
    }
    Some(rgba)
}

fn detect_svg_text(text: &str) -> Option<ClipboardImage> {
    let trimmed = text.trim();
    let plausible_svg =
        trimmed.starts_with("<svg") || (trimmed.starts_with("<?xml") && trimmed.contains("<svg"));
    plausible_svg
        .then(|| decode_svg(text.as_bytes().to_vec()))
        .flatten()
}

pub fn image_label(image: &ClipboardImage) -> String {
    format!(
        "{} × {} · {}",
        image.width,
        image.height,
        format_bytes(image.bytes.len() as u64)
    )
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else {
        format!("{} B", bytes as u64)
    }
}

fn decode_native_color(bytes: &[u8]) -> Option<[u8; 4]> {
    let channels: [u16; 4] = bytes
        .get(..8)?
        .chunks_exact(2)
        .map(|bytes| u16::from_ne_bytes([bytes[0], bytes[1]]))
        .collect::<Vec<_>>()
        .try_into()
        .ok()?;
    Some(channels.map(|channel| ((u32::from(channel) + 128) / 257) as u8))
}

fn encode_native_color(rgba: [u8; 4]) -> Vec<u8> {
    rgba.into_iter()
        .flat_map(|channel| (u16::from(channel) * 257).to_ne_bytes())
        .collect()
}

pub fn format_color([red, green, blue, alpha]: [u8; 4]) -> String {
    if alpha == u8::MAX {
        format!("#{red:02X}{green:02X}{blue:02X}")
    } else {
        format!("#{red:02X}{green:02X}{blue:02X}{alpha:02X}")
    }
}

pub fn parse_color_expression(value: &str) -> Option<[u8; 4]> {
    let value = value.trim();
    parse_hex_color(value).or_else(|| parse_rgb_color(value))
}

fn parse_hex_color(value: &str) -> Option<[u8; 4]> {
    let hex = value.strip_prefix('#')?;
    let expand = |digit: u8| (digit << 4) | digit;
    match hex.len() {
        3 | 4 => {
            let mut digits = hex.bytes().map(hex_digit);
            let red = expand(digits.next()??);
            let green = expand(digits.next()??);
            let blue = expand(digits.next()??);
            let alpha = if hex.len() == 4 {
                expand(digits.next()??)
            } else {
                u8::MAX
            };
            Some([red, green, blue, alpha])
        }
        6 | 8 => {
            let channel = |offset| u8::from_str_radix(&hex[offset..offset + 2], 16).ok();
            Some([
                channel(0)?,
                channel(2)?,
                channel(4)?,
                if hex.len() == 8 { channel(6)? } else { u8::MAX },
            ])
        }
        _ => None,
    }
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_rgb_color(value: &str) -> Option<[u8; 4]> {
    let (name, arguments) = value.split_once('(')?;
    let arguments = arguments.strip_suffix(')')?;
    let parts = arguments.split(',').map(str::trim).collect::<Vec<_>>();
    match (name.trim().to_ascii_lowercase().as_str(), parts.as_slice()) {
        ("rgb", [red, green, blue]) => Some([
            parse_rgb_channel(red)?,
            parse_rgb_channel(green)?,
            parse_rgb_channel(blue)?,
            u8::MAX,
        ]),
        ("rgba", [red, green, blue, alpha]) => Some([
            parse_rgb_channel(red)?,
            parse_rgb_channel(green)?,
            parse_rgb_channel(blue)?,
            parse_alpha_channel(alpha)?,
        ]),
        _ => None,
    }
}

fn parse_rgb_channel(value: &str) -> Option<u8> {
    if let Some(percent) = value.strip_suffix('%') {
        let percent: f32 = percent.trim().parse().ok()?;
        (percent.is_finite() && (0.0..=100.0).contains(&percent))
            .then(|| (percent * 2.55).round() as u8)
    } else {
        value.parse().ok()
    }
}

fn parse_alpha_channel(value: &str) -> Option<u8> {
    if let Some(percent) = value.strip_suffix('%') {
        parse_rgb_channel(&format!("{percent}%"))
    } else {
        let alpha: f32 = value.parse().ok()?;
        (alpha.is_finite() && (0.0..=1.0).contains(&alpha)).then(|| (alpha * 255.0).round() as u8)
    }
}

#[derive(Default)]
struct State {
    offers: HashMap<Offer, Vec<String>>,
    selection: Option<Offer>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore Manager);
delegate_noop!(State: ignore wl_seat::WlSeat);

impl Dispatch<Device, ()> for State {
    fn event(
        state: &mut Self,
        _: &Device,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::DataOffer { id } => {
                state.offers.insert(id, Vec::new());
            }
            ext_data_control_device_v1::Event::Selection { id } => {
                if let Some(previous) = state.selection.take() {
                    state.offers.remove(&previous);
                    previous.destroy();
                }
                state.selection = id;
            }
            ext_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => {
                state.offers.remove(&offer);
                offer.destroy();
            }
            _ => {}
        }
    }

    event_created_child!(State, Device, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (Offer, ()),
    ]);
}

impl Dispatch<Offer, ()> for State {
    fn event(
        state: &mut Self,
        offer: &Offer,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event
            && let Some(mime_types) = state.offers.get_mut(offer)
        {
            mime_types.push(mime_type);
        }
    }
}

struct CopyState {
    text: Option<String>,
    native_color: Option<Vec<u8>>,
    image: Option<ClipboardImage>,
    files: Option<ClipboardFiles>,
    cancelled: bool,
    current_offer: Option<Offer>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for CopyState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(CopyState: ignore Manager);
delegate_noop!(CopyState: ignore wl_seat::WlSeat);
delegate_noop!(CopyState: ignore Offer);

impl Dispatch<Device, ()> for CopyState {
    fn event(
        state: &mut Self,
        _: &Device,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::Selection { id } => {
                if let Some(previous) = state.current_offer.take() {
                    previous.destroy();
                }
                state.current_offer = id;
            }
            ext_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => {
                offer.destroy();
            }
            _ => {}
        }
    }

    event_created_child!(CopyState, Device, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (Offer, ()),
    ]);
}

impl Dispatch<Source, ()> for CopyState {
    fn event(
        state: &mut Self,
        source: &Source,
        event: ext_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                let mut file = std::fs::File::from(fd);
                if let Err(error) = make_blocking(&file) {
                    eprintln!("ClipboardForCosmic could not prepare clipboard transfer: {error}");
                    return;
                }
                let data = if mime_type == "application/x-color" {
                    state.native_color.as_deref().unwrap_or_default()
                } else if let Some(image) = &state.image
                    && mime_type == image.mime_type
                {
                    &image.bytes
                } else if let Some(files) = &state.files
                    && mime_type == "text/uri-list"
                {
                    let payload = uri_list_payload(files);
                    if let Err(error) = file.write_all(&payload) {
                        eprintln!("ClipboardForCosmic could not serve clipboard data: {error}");
                    }
                    return;
                } else if let Some(files) = &state.files
                    && mime_type == "x-special/gnome-copied-files"
                {
                    let payload = gnome_files_payload(files);
                    if let Err(error) = file.write_all(&payload) {
                        eprintln!("ClipboardForCosmic could not serve clipboard data: {error}");
                    }
                    return;
                } else {
                    state.text.as_deref().unwrap_or_default().as_bytes()
                };
                if let Err(error) = file.write_all(data) {
                    eprintln!("ClipboardForCosmic could not serve clipboard data: {error}");
                }
            }
            ext_data_control_source_v1::Event::Cancelled => {
                source.destroy();
                state.cancelled = true;
            }
            _ => {}
        }
    }
}

fn make_blocking(file: &std::fs::File) -> std::io::Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is valid for the duration of both calls. `fcntl` does not
    // retain the descriptor, and `F_SETFL` only changes its status flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if flags & libc::O_NONBLOCK != 0 {
        // SAFETY: As above; the flags came from `F_GETFL` for this descriptor.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_text_color_expressions() {
        assert_eq!(
            parse_color_expression("#3ad"),
            Some([0x33, 0xaa, 0xdd, 0xff])
        );
        assert_eq!(
            parse_color_expression("#33AADD80"),
            Some([0x33, 0xaa, 0xdd, 0x80])
        );
        assert_eq!(
            parse_color_expression("rgb(51, 170, 221)"),
            Some([51, 170, 221, 255])
        );
        assert_eq!(
            parse_color_expression("rgba(20%, 40%, 60%, 0.5)"),
            Some([51, 102, 153, 128])
        );
    }

    #[test]
    fn rejects_text_that_only_contains_a_color() {
        assert_eq!(parse_color_expression("color: #33AADD"), None);
        assert_eq!(parse_color_expression("#12"), None);
        assert_eq!(parse_color_expression("rgb(300, 0, 0)"), None);
    }

    #[test]
    fn native_color_round_trips() {
        let rgba = [0x33, 0xaa, 0xdd, 0x80];
        assert_eq!(decode_native_color(&encode_native_color(rgba)), Some(rgba));
    }

    #[test]
    fn image_capture_generates_a_tiny_cached_thumbnail() {
        use image::ImageEncoder;

        let width = 64;
        let height = 32;
        let pixels = vec![0x80; width * height * 4];
        let mut encoded = Vec::new();
        image::codecs::png::PngEncoder::new(&mut encoded)
            .write_image(
                &pixels,
                width as u32,
                height as u32,
                image::ExtendedColorType::Rgba8,
            )
            .expect("encode test PNG");

        let image = decode_image("image/png", encoded).expect("decode test PNG");

        assert_eq!((image.width, image.height), (64, 32));
        assert_eq!((image.preview_width, image.preview_height), (64, 32));
        assert_eq!(image.preview_rgba.len(), 64 * 32 * 4);
        assert_eq!((image.thumbnail_width, image.thumbnail_height), (16, 8));
        assert_eq!(image.thumbnail_rgba.len(), 16 * 8 * 4);
    }

    #[test]
    fn svg_capture_behaves_like_a_raster_image() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 200 100">
            <rect width="200" height="100" fill="#33aadd"/>
        </svg>"##;

        let image = decode_image("image/svg+xml", svg.to_vec()).expect("decode test SVG");

        assert!(image.is_svg);
        assert_eq!((image.width, image.height), (200, 100));
        assert_eq!((image.preview_width, image.preview_height), (200, 100));
        assert_eq!(image.preview_rgba.len(), 200 * 100 * 4);
        assert_eq!((image.thumbnail_width, image.thumbnail_height), (16, 8));
        assert_eq!(image.thumbnail_rgba.len(), 16 * 8 * 4);
        assert_eq!(image.bytes.as_ref(), svg);
    }

    #[test]
    fn detects_a_complete_svg_copied_as_plain_text() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1em" height="1em"
            viewBox="0 0 24 24"><title xmlns="">a-arrow-up</title>
            <path fill="none" stroke="currentColor" d="m14 11 4-4 4 4"/></svg>"#;

        let image = detect_svg_text(svg).expect("detect SVG text");

        assert!(image.is_svg);
        assert_eq!(image.mime_type, "image/svg+xml");
        assert_eq!(image.bytes.as_ref(), svg.as_bytes());
    }

    #[test]
    fn does_not_treat_an_svg_fragment_as_an_image() {
        assert!(detect_svg_text(r#"Use <svg viewBox="0 0 10 10"> here"#).is_none());
    }

    #[test]
    fn parses_and_restores_standard_file_uri_lists() {
        let payload =
            b"# copied files\r\nfile:///tmp/first%20file.txt\r\nfile:///tmp/second.txt\r\n";
        let files = decode_files("text/uri-list", payload).expect("decode URI list");

        assert_eq!(files.operation, FileOperation::Copy);
        assert_eq!(files.entries.len(), 2);
        assert_eq!(file_name(&files.entries[0]), "first file.txt");
        assert_eq!(
            uri_list_payload(&files),
            b"file:///tmp/first%20file.txt\r\nfile:///tmp/second.txt\r\n"
        );
    }

    #[test]
    fn preserves_gnome_cut_intent() {
        let payload = b"cut\nfile:///tmp/example.txt\n";
        let files =
            decode_files("x-special/gnome-copied-files", payload).expect("decode GNOME files");

        assert_eq!(files.operation, FileOperation::Cut);
        assert_eq!(gnome_files_payload(&files), payload);
    }

    #[test]
    fn reads_a_text_file_back_as_clipboard_data() {
        let mut file = tempfile::NamedTempFile::new().expect("create temporary text file");
        file.write_all(b"#33aadd")
            .expect("write temporary text file");

        let update =
            read_file_as_clipboard_update(file.path()).expect("read text as clipboard data");

        assert_eq!(update.text, "#33aadd");
        assert_eq!(update.mime_type, "text/plain;charset=utf-8");
        assert_eq!(update.color_rgba, Some([0x33, 0xaa, 0xdd, 0xff]));
        assert!(update.image.is_none());
        assert!(update.files.is_none());
    }

    #[test]
    fn reads_an_image_file_back_as_clipboard_data() {
        use image::ImageEncoder;

        let mut encoded = Vec::new();
        image::codecs::png::PngEncoder::new(&mut encoded)
            .write_image(&[0x80; 4 * 4 * 4], 4, 4, image::ExtendedColorType::Rgba8)
            .expect("encode test PNG");
        let mut file = tempfile::NamedTempFile::new().expect("create temporary image file");
        file.write_all(&encoded)
            .expect("write temporary image file");

        let update =
            read_file_as_clipboard_update(file.path()).expect("read image as clipboard data");

        assert_eq!(update.mime_type, "image/png");
        assert_eq!(
            update
                .image
                .as_ref()
                .map(|image| (image.width, image.height)),
            Some((4, 4))
        );
        assert!(update.files.is_none());
    }
}
