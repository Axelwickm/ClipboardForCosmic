use std::collections::HashMap;
use std::io::Write;
use std::os::fd::AsFd;
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
    pub captured_at: SystemTime,
}

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
    let generation = WRITE_GENERATION
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_add(1);
    std::thread::spawn(move || {
        if let Err(error) = provide_clipboard(text, generation) {
            eprintln!("Clipboard History could not set the clipboard: {error}");
        }
    });
}

fn provide_clipboard(text: String, generation: u64) -> Result<(), Box<dyn std::error::Error>> {
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
    source.offer("text/plain;charset=utf-8".into());
    source.offer("text/plain".into());
    device.set_selection(Some(&source));
    connection.flush()?;
    drop(setup);

    let mut state = CopyState {
        text,
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
        let Some(mime) = preferred_text_mime(&mime_types).map(str::to_owned) else {
            offer.destroy();
            continue;
        };

        let (write, mut read) = tokio::net::unix::pipe::pipe()?;
        offer.receive(mime.clone(), write.as_fd());
        connection.flush()?;
        drop(write);

        let mut bytes = Vec::new();
        read.read_to_end(&mut bytes).await?;
        offer.destroy();
        if let Ok(text) = String::from_utf8(bytes) {
            let _ = sender.send(ClipboardUpdate {
                text,
                mime_type: mime,
                available_mime_types: mime_types,
                captured_at: SystemTime::now(),
            });
        }
    }
}

fn preferred_text_mime(mime_types: &[String]) -> Option<&str> {
    ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"]
        .into_iter()
        .find(|preferred| mime_types.iter().any(|mime| mime == preferred))
        .or_else(|| {
            mime_types
                .iter()
                .find(|mime| mime.starts_with("text/"))
                .map(String::as_str)
        })
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
            ext_data_control_device_v1::Event::PrimarySelection { id } => {
                if let Some(offer) = id {
                    state.offers.remove(&offer);
                    offer.destroy();
                }
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
    text: String,
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
            ext_data_control_device_v1::Event::PrimarySelection { id } => {
                if let Some(offer) = id {
                    offer.destroy();
                }
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
            ext_data_control_source_v1::Event::Send { fd, .. } => {
                let mut file = std::fs::File::from(fd);
                if let Err(error) = file.write_all(state.text.as_bytes()) {
                    eprintln!("Clipboard History could not serve clipboard text: {error}");
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
