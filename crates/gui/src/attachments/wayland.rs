//! Core wl_data_device on eframe's focused client, without privileged
//! data-control extensions or a second, unfocused Wayland connection.
use std::{
    collections::HashMap,
    io::{Cursor, Read},
    os::fd::AsFd,
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    backend::ObjectId,
    delegate_noop,
    protocol::{wl_data_device, wl_data_device_manager, wl_data_offer, wl_registry, wl_seat},
};

use super::{ClipboardRgba, FILE_BYTES, MAX_RGBA_BYTES};

#[derive(Default)]
struct State {
    manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    seats: Vec<wl_seat::WlSeat>,
    offers: HashMap<ObjectId, (wl_data_offer::WlDataOffer, Vec<String>)>,
    selections: HashMap<ObjectId, wl_data_offer::WlDataOffer>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_data_device_manager" if version >= 2 => {
                    state.manager = Some(registry.bind(name, version.min(3), queue, ()));
                }
                "wl_seat" if version >= 5 => state.seats.push(registry.bind(name, 5, queue, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn event(
        state: &mut Self,
        device: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                state.offers.insert(id.id(), (id, Vec::new()));
            }
            wl_data_device::Event::Selection { id } => {
                let old = if let Some(offer) = id {
                    state.selections.insert(device.id(), offer)
                } else {
                    state.selections.remove(&device.id())
                };
                if let Some(old) = old
                    && !state
                        .selections
                        .values()
                        .any(|selected| selected.id() == old.id())
                    && let Some((offer, _)) = state.offers.remove(&old.id())
                {
                    offer.destroy();
                }
            }
            wl_data_device::Event::Leave => {
                state.offers.retain(|id, (offer, _)| {
                    if state
                        .selections
                        .values()
                        .any(|selected| selected.id() == *id)
                    {
                        true
                    } else {
                        offer.destroy();
                        false
                    }
                });
            }
            _ => {}
        }
    }
    wayland_client::event_created_child!(State, wl_data_device::WlDataDevice, [0 => (wl_data_offer::WlDataOffer, ())]);
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event
            && let Some((_, mimes)) = state.offers.get_mut(&proxy.id())
            && mimes.len() < 128
        {
            mimes.push(mime_type);
        }
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore wl_data_device_manager::WlDataDeviceManager);

pub(super) struct CoreClipboard {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    devices: Vec<wl_data_device::WlDataDevice>,
}

impl CoreClipboard {
    /// # Safety
    /// The caller must keep the eframe wl_display alive until this value is
    /// dropped, and join this worker before destroying that display.
    pub(super) unsafe fn new(display: usize) -> Result<Self, String> {
        // SAFETY: the caller owns the lifetime of the foreign display.
        let backend =
            unsafe { wayland_client::backend::Backend::from_foreign_display(display as *mut _) };
        let connection = Connection::from_backend(backend);
        let mut queue = connection.new_event_queue();
        let handle = queue.handle();
        let _registry = connection.display().get_registry(&handle, ());
        let mut state = State::default();
        queue
            .roundtrip(&mut state)
            .map_err(|e| format!("Wayland clipboard globals: {e}"))?;
        let manager = state
            .manager
            .as_ref()
            .ok_or("Wayland core data device is unavailable")?;
        if state.seats.is_empty() {
            return Err("Wayland clipboard has no seat".into());
        }
        let devices = state
            .seats
            .iter()
            .map(|seat| manager.get_data_device(seat, &handle, ()))
            .collect();
        queue
            .roundtrip(&mut state)
            .map_err(|e| format!("Wayland clipboard selection: {e}"))?;
        Ok(Self {
            connection,
            queue,
            state,
            devices,
        })
    }

    pub(super) fn image(&mut self) -> Result<ClipboardRgba, String> {
        self.queue
            .roundtrip(&mut self.state)
            .map_err(|e| format!("Wayland clipboard: {e}"))?;
        let offer = unique_selection(self.state.selections.values().cloned())?
            .ok_or("Clipboard contains no image selection (focus the composer)")?;
        let (_, mimes) = self
            .state
            .offers
            .get(&offer.id())
            .ok_or("Clipboard image offer disappeared")?;
        // A selection change between egui's text read and this worker must not
        // turn a newly available text paste into an image-only paste.
        if mimes
            .iter()
            .any(|mime| mime.starts_with("text/plain") || mime == "UTF8_STRING")
        {
            return Err("Clipboard now contains text; paste again to insert the text".into());
        }
        let mime = [
            "image/png",
            "image/jpeg",
            "image/webp",
            "image/gif",
            "image/bmp",
        ]
        .into_iter()
        .find(|wanted| mimes.iter().any(|mime| mime == wanted))
        .ok_or("Clipboard contains no supported image")?;
        let (read, write) = rustix::pipe::pipe_with(
            rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
        )
        .map_err(|e| e.to_string())?;
        offer.receive(mime.to_owned(), write.as_fd());
        self.connection.flush().map_err(|e| e.to_string())?;
        drop(write);
        let mut file = std::fs::File::from(read);
        let mut bytes = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err("Clipboard image transfer timed out".into());
            }
            let mut polls = [rustix::event::PollFd::new(
                &file,
                rustix::event::PollFlags::IN,
            )];
            let timeout =
                rustix::event::Timespec::try_from(remaining).map_err(|e| e.to_string())?;
            if rustix::event::poll(&mut polls, Some(&timeout)).map_err(|e| e.to_string())? == 0 {
                return Err("Clipboard image transfer timed out".into());
            }
            let mut buffer = [0_u8; 16 * 1024];
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(length) => {
                    if bytes.len() + length > FILE_BYTES as usize {
                        return Err("Clipboard encoded image exceeds 8 MiB".into());
                    }
                    bytes.extend_from_slice(&buffer[..length]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(format!("Clipboard image transfer: {error}")),
            }
        }
        let mut reader = image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| e.to_string())?;
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(MAX_RGBA_BYTES as u64);
        reader.limits(limits);
        let image = reader
            .decode()
            .map_err(|e| format!("Clipboard image decode: {e}"))?
            .into_rgba8();
        Ok(ClipboardRgba {
            width: image.width() as usize,
            height: image.height() as usize,
            bytes: image.into_raw(),
        })
    }
}

impl Drop for CoreClipboard {
    fn drop(&mut self) {
        for (_, (offer, _)) in self.state.offers.drain() {
            offer.destroy();
        }
        for device in self.devices.drain(..) {
            device.release();
        }
        for seat in self.state.seats.drain(..) {
            seat.release();
        }
        let _ = self.connection.flush();
    }
}

// A foreign display does not tell us which of several seats eframe focused.
// Conflicting selections must not paste another seat's clipboard.
fn unique_selection<T: Eq>(offers: impl IntoIterator<Item = T>) -> Result<Option<T>, String> {
    let mut selected = None;
    for offer in offers {
        if selected.as_ref().is_some_and(|previous| previous != &offer) {
            return Err(
                "Multiple Wayland seats have different clipboard selections; use Attach file"
                    .into(),
            );
        }
        selected = Some(offer);
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    #[test]
    fn multiple_seat_selections_are_never_chosen_arbitrarily() {
        assert_eq!(super::unique_selection([7, 7]).unwrap(), Some(7));
        assert_eq!(super::unique_selection::<u8>([]).unwrap(), None);
        assert!(super::unique_selection([7, 8]).is_err());
    }
}
