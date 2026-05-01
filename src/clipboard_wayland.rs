//! Native Wayland clipboard reads via `wl_data_device` (and primary
//! via `zwp_primary_selection_device_v1`). Replaces shelling out to
//! `wl-paste`, which crashes on some `wl-clipboard 2.3.0` builds.
//!
//! Each call opens a fresh Wayland connection, binds a seat and the
//! data-device manager, creates a data device, dispatches until the
//! compositor sends an offer + selection, then asks the source to write
//! into a pipe and reads the bytes off the pipe.
//!
//! Requires the compositor to send the current selection to a
//! freshly-created data_device — most do (sway, mutter, kwin, sol).
//! On a strictly-spec-compliant compositor that only sends `selection`
//! on focus change, this approach won't see anything; we'd need to share
//! the application's wl_seat (which has focus) instead. Worth revisiting
//! if a user reports it.

use std::io::ErrorKind;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use wayland_client::protocol::wl_data_device::{self, WlDataDevice};
use wayland_client::protocol::wl_data_device_manager::WlDataDeviceManager;
use wayland_client::protocol::wl_data_offer::{self, WlDataOffer};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{event_created_child, Connection, Dispatch, QueueHandle};

use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1;
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_device_v1::{
    self as primary_device, ZwpPrimarySelectionDeviceV1,
};
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_offer_v1::{
    self as primary_offer, ZwpPrimarySelectionOfferV1,
};

const READ_TIMEOUT: Duration = Duration::from_millis(500);

pub fn read_clipboard() -> Option<String> {
    read_via_wl_data_device()
}

pub fn read_primary() -> Option<String> {
    read_via_primary_selection()
}

// ---------- regular clipboard (wl_data_device) ----------

#[derive(Default)]
struct ClipState {
    seat: Option<WlSeat>,
    manager: Option<WlDataDeviceManager>,
    pending_mimes: Vec<String>,
    pending_offer: Option<WlDataOffer>,
    selected_offer: Option<WlDataOffer>,
    /// `selection` event has fired. May or may not carry an offer (None
    /// means the clipboard is empty).
    selection_seen: bool,
}

fn read_via_wl_data_device() -> Option<String> {
    let conn = Connection::connect_to_env().ok()?;
    let display = conn.display();
    let mut queue = conn.new_event_queue::<ClipState>();
    let qh = queue.handle();
    let mut state = ClipState::default();

    let _registry = display.get_registry(&qh, ());
    queue.roundtrip(&mut state).ok()?; // globals

    let seat = state.seat.clone()?;
    let manager = state.manager.clone()?;
    let _device = manager.get_data_device(&seat, &qh, ());
    queue.roundtrip(&mut state).ok()?; // data_offer + selection

    if !state.selection_seen {
        // Some compositors send the selection only after another
        // roundtrip; give it one more chance.
        queue.roundtrip(&mut state).ok()?;
    }

    let offer = state.selected_offer.clone()?;
    let mime = pick_text_mime(&state.pending_mimes)?;

    let (read_fd, write_fd) = pipe2()?;
    // Send the receive request before dropping write_fd — the
    // BorrowedFd must outlive the actual sendmsg, which happens on
    // flush(). Closing too early risks the kernel reusing the fd
    // number for something else by the time wayland-client serializes.
    offer.receive(mime, write_fd.as_fd());
    queue.flush().ok()?;
    drop(write_fd);

    read_pipe(read_fd)
}

impl Dispatch<WlRegistry, ()> for ClipState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            // Bind the first seat we see (most setups have one) and the
            // data_device_manager. Versions are clamped to what the
            // generated stubs require — anything above that the
            // compositor offers we just ignore.
            match interface.as_str() {
                "wl_seat" if state.seat.is_none() => {
                    state.seat =
                        Some(registry.bind::<WlSeat, _, _>(name, version.min(7), qh, ()));
                }
                "wl_data_device_manager" if state.manager.is_none() => {
                    state.manager = Some(registry.bind::<WlDataDeviceManager, _, _>(
                        name,
                        version.min(3),
                        qh,
                        (),
                    ));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for ClipState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        _event: wayland_client::protocol::wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataDeviceManager, ()> for ClipState {
    fn event(
        _state: &mut Self,
        _proxy: &WlDataDeviceManager,
        _event: wayland_client::protocol::wl_data_device_manager::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataDevice, ()> for ClipState {
    fn event(
        state: &mut Self,
        _proxy: &WlDataDevice,
        event: wl_data_device::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                // Fresh offer is being announced. Mimes will arrive on
                // it via `wl_data_offer.offer` events; remember it as
                // "pending" until selection() picks it up.
                state.pending_offer = Some(id);
                state.pending_mimes.clear();
            }
            wl_data_device::Event::Selection { id } => {
                state.selected_offer = id;
                state.selection_seen = true;
            }
            _ => {}
        }
    }

    event_created_child!(ClipState, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

impl Dispatch<WlDataOffer, ()> for ClipState {
    fn event(
        state: &mut Self,
        _proxy: &WlDataOffer,
        event: wl_data_offer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            state.pending_mimes.push(mime_type);
        }
    }
}

// ---------- primary selection (zwp_primary_selection_device_v1) ----------

#[derive(Default)]
struct PrimaryState {
    seat: Option<WlSeat>,
    manager: Option<ZwpPrimarySelectionDeviceManagerV1>,
    pending_mimes: Vec<String>,
    selected_offer: Option<ZwpPrimarySelectionOfferV1>,
    selection_seen: bool,
}

fn read_via_primary_selection() -> Option<String> {
    let conn = Connection::connect_to_env().ok()?;
    let display = conn.display();
    let mut queue = conn.new_event_queue::<PrimaryState>();
    let qh = queue.handle();
    let mut state = PrimaryState::default();

    let _registry = display.get_registry(&qh, ());
    queue.roundtrip(&mut state).ok()?;

    let seat = state.seat.clone()?;
    let manager = state.manager.clone()?;
    let _device = manager.get_device(&seat, &qh, ());
    queue.roundtrip(&mut state).ok()?;
    if !state.selection_seen {
        queue.roundtrip(&mut state).ok()?;
    }

    let offer = state.selected_offer.clone()?;
    let mime = pick_text_mime(&state.pending_mimes)?;

    let (read_fd, write_fd) = pipe2()?;
    // Send the receive request before dropping write_fd — the
    // BorrowedFd must outlive the actual sendmsg, which happens on
    // flush(). Closing too early risks the kernel reusing the fd
    // number for something else by the time wayland-client serializes.
    offer.receive(mime, write_fd.as_fd());
    queue.flush().ok()?;
    drop(write_fd);

    read_pipe(read_fd)
}

impl Dispatch<WlRegistry, ()> for PrimaryState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_seat" if state.seat.is_none() => {
                    state.seat =
                        Some(registry.bind::<WlSeat, _, _>(name, version.min(7), qh, ()));
                }
                "zwp_primary_selection_device_manager_v1" if state.manager.is_none() => {
                    state.manager = Some(registry.bind::<ZwpPrimarySelectionDeviceManagerV1, _, _>(
                        name,
                        version.min(1),
                        qh,
                        (),
                    ));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for PrimaryState {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: wayland_client::protocol::wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpPrimarySelectionDeviceManagerV1, ()> for PrimaryState {
    fn event(
        _: &mut Self,
        _: &ZwpPrimarySelectionDeviceManagerV1,
        _: wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_device_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpPrimarySelectionDeviceV1, ()> for PrimaryState {
    fn event(
        state: &mut Self,
        _: &ZwpPrimarySelectionDeviceV1,
        event: primary_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            primary_device::Event::DataOffer { offer: _ } => {
                state.pending_mimes.clear();
            }
            primary_device::Event::Selection { id } => {
                state.selected_offer = id;
                state.selection_seen = true;
            }
            _ => {}
        }
    }

    event_created_child!(PrimaryState, ZwpPrimarySelectionDeviceV1, [
        primary_device::EVT_DATA_OFFER_OPCODE => (ZwpPrimarySelectionOfferV1, ()),
    ]);
}

impl Dispatch<ZwpPrimarySelectionOfferV1, ()> for PrimaryState {
    fn event(
        state: &mut Self,
        _: &ZwpPrimarySelectionOfferV1,
        event: primary_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let primary_offer::Event::Offer { mime_type } = event {
            state.pending_mimes.push(mime_type);
        }
    }
}

// ---------- shared helpers ----------

/// Pick the most-preferred text MIME we know how to decode. Order
/// matches what xterm-style apps advertise; UTF-8 first.
fn pick_text_mime(mimes: &[String]) -> Option<String> {
    const PREFERRED: &[&str] = &[
        "text/plain;charset=utf-8",
        "text/plain",
        "UTF8_STRING",
        "TEXT",
        "STRING",
    ];
    for want in PREFERRED {
        if let Some(m) = mimes.iter().find(|m| m.eq_ignore_ascii_case(want)) {
            return Some(m.clone());
        }
    }
    None
}

fn pipe2() -> Option<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    let r = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if r != 0 {
        return None;
    }
    Some(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Drain `fd` to a String with a hard timeout. Sets the fd non-blocking
/// and polls so a hung source can't lock up the paste path.
fn read_pipe(fd: OwnedFd) -> Option<String> {
    let raw = fd.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFL);
        if flags < 0 {
            return None;
        }
        if libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return None;
        }
    }

    let mut out: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + READ_TIMEOUT;
    loop {
        let remaining = match deadline.checked_duration_since(Instant::now()) {
            Some(d) => d,
            None => return None,
        };
        let ms = remaining.as_millis().min(60_000) as i32;
        let mut pfd = libc::pollfd {
            fd: raw,
            events: libc::POLLIN,
            revents: 0,
        };
        let r = unsafe { libc::poll(&mut pfd, 1, ms) };
        if r == 0 {
            return None; // timeout
        }
        if r < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        let n = unsafe { libc::read(raw, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n == 0 {
            break;
        }
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    drop(fd); // explicit close — OwnedFd's Drop closes the read pipe
    String::from_utf8(out).ok()
}
