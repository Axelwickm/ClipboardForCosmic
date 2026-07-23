use std::collections::HashMap;
use std::io::Write;
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
    pub captured_at: SystemTime,
}

#[derive(Clone, Debug)]
pub struct ClipboardImage {
    pub mime_type: String,
    pub bytes: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
    pub thumbnail_rgba: Arc<[u8]>,
    pub thumbnail_width: u32,
    pub thumbnail_height: u32,
}

const MAX_IMAGE_BYTES: u64 = 50 * 1024 * 1024;
const THUMBNAIL_SIZE: u32 = 16;

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
        if let Err(error) = provide_clipboard(Some(text), color_rgba, None, generation) {
            eprintln!("Clipboard History could not set the clipboard: {error}");
        }
    });
}

pub fn copy_image(image: ClipboardImage) {
    let generation = WRITE_GENERATION
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_add(1);
    std::thread::spawn(move || {
        if let Err(error) = provide_clipboard(None, None, Some(image), generation) {
            eprintln!("Clipboard History could not set the clipboard image: {error}");
        }
    });
}

fn provide_clipboard(
    text: Option<String>,
    color_rgba: Option<[u8; 4]>,
    image: Option<ClipboardImage>,
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
    device.set_selection(Some(&source));
    connection.flush()?;
    drop(setup);

    let mut state = CopyState {
        text,
        native_color: color_rgba.map(encode_native_color),
        image,
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
                eprintln!("Clipboard History watcher stopped: {error}");
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
        read.take(MAX_IMAGE_BYTES + 1)
            .read_to_end(&mut bytes)
            .await?;
        offer.destroy();
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            eprintln!("Clipboard History ignored an image larger than 50 MiB");
            continue;
        }
        let content = if mime == "application/x-color" {
            decode_native_color(&bytes).map(|rgba| (format_color(rgba), Some(rgba), None))
        } else if is_image_mime(&mime) {
            decode_image(&mime, bytes).map(|image| (image_label(&image), None, Some(image)))
        } else {
            String::from_utf8(bytes).ok().map(|text| {
                let color = parse_color_expression(&text);
                (text, color, None)
            })
        };
        if let Some((text, color_rgba, image)) = content {
            let _ = sender.send(ClipboardUpdate {
                text,
                mime_type: mime,
                available_mime_types: mime_types,
                color_rgba,
                image,
                captured_at: SystemTime::now(),
            });
        }
    }
}

fn preferred_mime(mime_types: &[String]) -> Option<&str> {
    [
        "image/png",
        "image/jpeg",
        "image/jpg",
        "image/webp",
        "image/gif",
        "image/bmp",
        "image/x-bmp",
        "image/tiff",
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

fn is_image_mime(mime: &str) -> bool {
    image_format(mime).is_some()
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

fn decode_image(mime_type: &str, bytes: Vec<u8>) -> Option<ClipboardImage> {
    use image::GenericImageView;

    let decoded = image::load_from_memory_with_format(&bytes, image_format(mime_type)?).ok()?;
    let (width, height) = decoded.dimensions();
    let thumbnail = decoded
        .thumbnail(THUMBNAIL_SIZE.min(width), THUMBNAIL_SIZE.min(height))
        .to_rgba8();
    let (thumbnail_width, thumbnail_height) = thumbnail.dimensions();
    (width > 0 && height > 0).then(|| ClipboardImage {
        mime_type: mime_type.to_owned(),
        bytes: Arc::from(bytes),
        width,
        height,
        thumbnail_rgba: Arc::from(thumbnail.into_raw()),
        thumbnail_width,
        thumbnail_height,
    })
}

pub fn image_label(image: &ClipboardImage) -> String {
    format!(
        "{} × {} · {}",
        image.width,
        image.height,
        format_bytes(image.bytes.len())
    )
}

fn format_bytes(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else {
        format!("{} B", bytes as usize)
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
                    eprintln!("Clipboard History could not prepare clipboard transfer: {error}");
                    return;
                }
                let data = if mime_type == "application/x-color" {
                    state.native_color.as_deref().unwrap_or_default()
                } else if let Some(image) = &state.image
                    && mime_type == image.mime_type
                {
                    &image.bytes
                } else {
                    state.text.as_deref().unwrap_or_default().as_bytes()
                };
                if let Err(error) = file.write_all(data) {
                    eprintln!("Clipboard History could not serve clipboard data: {error}");
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
        assert_eq!((image.thumbnail_width, image.thumbnail_height), (16, 8));
        assert_eq!(image.thumbnail_rgba.len(), 16 * 8 * 4);
    }
}
