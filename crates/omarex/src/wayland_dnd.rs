//! Native Wayland drag-and-drop receiver (`wl_data_device` / `wl_data_offer`) attached directly
//! to `winit`'s shared `wl_display` and window `wl_surface`.
//!
//! `winit 0.30.x` does not implement `WindowEvent::DroppedFile` or `HoveredFile` on Wayland
//! (and does not bind `wl_data_device_manager`). This module binds `wl_data_device_manager` v3
//! on an isolated `wayland-client` `EventQueue` using `Backend::from_foreign_display`, avoiding
//! any conflict with `winit` or Vulkan WSI (`vkAcquireNextImageKHR`), while supporting:
//! - Local file drops (`text/uri-list`, `application/x-kde-urilist`, `x-special/gnome-copied-files`)
//! - Flatpak / XDG Desktop Portal drops (`application/vnd.portal.filetransfer` via `zbus`)
//! - Direct binary media & document drops from browsers/apps (`image/*`, `video/*`, `audio/*`,
//!   `application/pdf`, `application/zip`) streamed in bounded `64 KiB` chunks to temp files
//! - Plain-text drops (`text/plain;charset=utf-8`, `text/plain`, `UTF8_STRING`).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::pipe::{PipeFlags, pipe_with};
use wayland_backend::client::{Backend, ObjectId};
use wayland_client::protocol::wl_data_device::{self, WlDataDevice};
use wayland_client::protocol::wl_data_device_manager::{DndAction, WlDataDeviceManager};
use wayland_client::protocol::wl_data_offer::{self, WlDataOffer};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};

/// Maximum bytes accepted for text/URI-list DnD payloads (`4 MiB`).
const MAX_TEXT_DROP_BYTES: usize = 4 * 1024 * 1024;
/// Maximum bytes accepted when streaming a direct binary media drop (e.g. browser video/image)
/// to a temporary file (`512 MiB`, streamed in bounded `64 KiB` chunks).
const MAX_BINARY_DROP_BYTES: u64 = 512 * 1024 * 1024;
/// Streaming buffer size (`64 KiB`).
const PIPE_CHUNK_BYTES: usize = 64 * 1024;

static DROP_FILE_SEQ: AtomicU64 = AtomicU64::new(1);

/// Decoded payload from a completed Wayland drag-and-drop operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaylandDropPayload {
    /// One or more local filesystem paths (including temp files saved from direct binary drops).
    Files(Vec<PathBuf>),
    /// Dropped UTF-8 plain text (when no file URI or binary media was offered).
    Text(String),
}

/// High-level Wayland DnD lifecycle event delivered to `app.rs`.
#[derive(Debug, Clone, PartialEq)]
pub enum WaylandDndEvent {
    /// Drag entered or moved across our window at surface-local logical coordinates `(x, y)`.
    Hover { x: f32, y: f32, mime: String },
    /// Drag left our window without dropping.
    Leave,
    /// User released the drop at surface-local logical coordinates `(x, y)`.
    Drop {
        x: f32,
        y: f32,
        payload: WaylandDropPayload,
    },
}

/// Owned handle keeping the native Wayland DnD thread alive until window shutdown.
pub struct WaylandDndHandle {
    shutdown: Arc<AtomicBool>,
    shutdown_tx: OwnedFd,
    thread: Option<JoinHandle<()>>,
}

impl WaylandDndHandle {
    /// Attaches a native Wayland `wl_data_device` DnD listener to a `winit` window.
    ///
    /// Returns `Ok(None)` cleanly on X11 or if the compositor does not advertise
    /// `wl_data_device_manager`.
    pub fn attach<W, F>(window: &W, on_event: F) -> Option<Self>
    where
        W: HasDisplayHandle + HasWindowHandle,
        F: Fn(WaylandDndEvent) + Send + Sync + 'static,
    {
        let display_ptr = match window.display_handle().ok()?.as_raw() {
            RawDisplayHandle::Wayland(h) => h.display.as_ptr(),
            _ => return None,
        };
        let surface_ptr = match window.window_handle().ok()?.as_raw() {
            RawWindowHandle::Wayland(h) => h.surface.as_ptr() as usize,
            _ => return None,
        };

        // SAFETY: `display_ptr` comes from `winit`'s live Wayland display and remains valid until
        // after `WaylandDndHandle::drop` joins the thread and drops `Connection` in `shutdown_gracefully`.
        let backend = unsafe { Backend::from_foreign_display(display_ptr.cast()) };
        let conn = Connection::from_backend(backend);
        let mut event_queue: EventQueue<DndState> = conn.new_event_queue();
        let qh = event_queue.handle();

        let callback: Arc<dyn Fn(WaylandDndEvent) + Send + Sync> = Arc::new(on_event);
        let mut state = DndState::new(surface_ptr, conn.clone(), callback);

        let registry = conn.display().get_registry(&qh, ());
        state.registry = Some(registry);

        // One-time synchronous roundtrip during `resumed()` (before the event loop polls) so
        // `wl_data_device_manager` and existing `wl_seat` globals are bound immediately.
        if let Err(err) = event_queue.roundtrip(&mut state) {
            tracing::warn!(error = %err, "Wayland DnD initial registry roundtrip failed");
            state.cleanup();
            return None;
        }
        if state.data_device_manager.is_none() {
            tracing::debug!("Wayland compositor does not advertise wl_data_device_manager");
            state.cleanup();
            return None;
        }
        state.bind_missing_data_devices(&qh);
        let _ = conn.flush();

        let (shutdown_rx, shutdown_tx) = match pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK) {
            Ok(pair) => pair,
            Err(err) => {
                tracing::warn!(error = %err, "failed to create Wayland DnD wakeup pipe");
                state.cleanup();
                return None;
            }
        };

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_thread = Arc::clone(&shutdown);

        tracing::info!(
            seats = state.data_devices.len(),
            "attached native Wayland wl_data_device drag-and-drop listener"
        );

        let thread = thread::Builder::new()
            .name("omarex-wl-dnd".to_owned())
            .spawn(move || {
                run_dnd_loop(conn, event_queue, state, shutdown_rx, shutdown_thread);
            })
            .ok()?;

        Some(Self {
            shutdown,
            shutdown_tx,
            thread: Some(thread),
        })
    }
}

impl Drop for WaylandDndHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = rustix::io::write(&self.shutdown_tx, &[1u8]);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// Runs the deadlock-free Wayland DnD event dispatcher loop.
///
/// Crucially, this thread **never** calls `prepare_read()` / `read_events()` on the shared
/// `wl_display` so it can never block `winit`'s `WaylandSource` or Vulkan WSI
/// (`vkAcquireNextImageKHR`). Instead, `winit` reads incoming Wayland packets into libwayland's
/// per-queue ring buffers, and this thread wakes via `poll(display_fd)` (0% idle CPU) to drain
/// our dedicated `wl_event_queue` via `dispatch_pending()`.
fn run_dnd_loop(
    conn: Connection,
    mut event_queue: EventQueue<DndState>,
    mut state: DndState,
    shutdown_rx: OwnedFd,
    shutdown: Arc<AtomicBool>,
) {
    let backend = conn.backend();
    let display_fd = backend.poll_fd();

    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }

        // Drain any events already routed into our queue before sleeping.
        match event_queue.dispatch_pending(&mut state) {
            Ok(n) if n > 0 => {
                let _ = conn.flush();
            }
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(error = %err, "Wayland DnD queue dispatch failed");
                break;
            }
        }

        // Park in the kernel until either the Wayland socket receives data or shutdown fires.
        let mut fds = [
            PollFd::new(&display_fd, PollFlags::IN),
            PollFd::new(&shutdown_rx, PollFlags::IN),
        ];
        if let Err(err) = poll(&mut fds, None) {
            if err == rustix::io::Errno::INTR {
                continue;
            }
            tracing::warn!(error = %err, "Wayland DnD poll error");
            break;
        }

        if shutdown.load(Ordering::Acquire) || fds[1].revents().contains(PollFlags::IN) {
            break;
        }
        if fds[0].revents().intersects(PollFlags::ERR | PollFlags::HUP) {
            break;
        }

        // `winit`'s `WaylandSource` wakes simultaneously on `display_fd` and reads bytes into
        // libwayland's queues. Poll our own queue briefly until `display_fd` has been drained by
        // `winit` and all DnD events routed to our queue are dispatched.
        for attempt in 0..16 {
            if shutdown.load(Ordering::Acquire) {
                break;
            }
            match event_queue.dispatch_pending(&mut state) {
                Ok(n) if n > 0 => {
                    let _ = conn.flush();
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(error = %err, "Wayland DnD queue dispatch error");
                    state.cleanup();
                    return;
                }
            }

            // Check non-blocking if `display_fd` is still readable (meaning winit hasn't read it yet).
            let mut check = [PollFd::new(&display_fd, PollFlags::IN)];
            let zero = Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            let still_readable = poll(&mut check, Some(&zero))
                .is_ok_and(|_| check[0].revents().contains(PollFlags::IN));

            if !still_readable {
                // One short grace pass on the first check in case winit just finished reading into
                // our queue concurrently.
                if attempt == 0 {
                    thread::sleep(Duration::from_millis(1));
                    if let Ok(n) = event_queue.dispatch_pending(&mut state)
                        && n > 0
                    {
                        let _ = conn.flush();
                    }
                }
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    state.cleanup();
    let _ = conn.flush();
}

// ============================================================================
// Wayland Protocol State & Dispatch Handlers
// ============================================================================

struct ActiveDndOffer {
    offer: WlDataOffer,
    serial: u32,
    x: f32,
    y: f32,
    accepted_mime: Option<String>,
}

struct DndState {
    target_surface_ptr: usize,
    conn: Connection,
    callback: Arc<dyn Fn(WaylandDndEvent) + Send + Sync>,
    registry: Option<WlRegistry>,
    data_device_manager: Option<WlDataDeviceManager>,
    seats: Vec<(u32, WlSeat)>,
    data_devices: HashMap<u32, WlDataDevice>,
    /// Offered MIME types indexed by `WlDataOffer` `ObjectId`.
    offer_mimes: HashMap<ObjectId, Vec<String>>,
    /// Currently hovered DnD offer over our window's `wl_surface`.
    active_dnd: Option<ActiveDndOffer>,
}

impl DndState {
    fn new(
        target_surface_ptr: usize,
        conn: Connection,
        callback: Arc<dyn Fn(WaylandDndEvent) + Send + Sync>,
    ) -> Self {
        Self {
            target_surface_ptr,
            conn,
            callback,
            registry: None,
            data_device_manager: None,
            seats: Vec::new(),
            data_devices: HashMap::new(),
            offer_mimes: HashMap::new(),
            active_dnd: None,
        }
    }

    fn bind_missing_data_devices(&mut self, qh: &QueueHandle<Self>) {
        let Some(mgr) = self.data_device_manager.as_ref() else {
            return;
        };
        for (seat_name, seat) in &self.seats {
            if !self.data_devices.contains_key(seat_name) {
                let dev = mgr.get_data_device(seat, qh, *seat_name);
                self.data_devices.insert(*seat_name, dev);
            }
        }
    }

    fn negotiate_active_offer(&mut self) {
        let Some(active) = self.active_dnd.as_mut() else {
            return;
        };
        let offer_id = active.offer.id();
        let best = self
            .offer_mimes
            .get(&offer_id)
            .and_then(|mimes| choose_best_mime(mimes));

        if best != active.accepted_mime {
            active.offer.accept(active.serial, best.clone());
            if active.offer.version() >= 3 {
                if best.is_some() {
                    active
                        .offer
                        .set_actions(DndAction::Copy | DndAction::Move, DndAction::Copy);
                } else {
                    active
                        .offer
                        .set_actions(DndAction::empty(), DndAction::empty());
                }
            }
            active.accepted_mime = best;
            let _ = self.conn.flush();
        }
    }

    fn cleanup(&mut self) {
        if let Some(active) = self.active_dnd.take() {
            self.offer_mimes.remove(&active.offer.id());
            active.offer.destroy();
        }
        for (_, dev) in self.data_devices.drain() {
            if dev.version() >= 2 {
                dev.release();
            }
        }
        for (_, seat) in self.seats.drain(..) {
            if seat.version() >= 5 {
                seat.release();
            }
        }
        self.data_device_manager = None;
        self.registry = None;
        self.offer_mimes.clear();
    }
}

impl Dispatch<WlRegistry, ()> for DndState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_data_device_manager" if state.data_device_manager.is_none() => {
                    let mgr: WlDataDeviceManager = registry.bind(name, version.min(3), qh, ());
                    state.data_device_manager = Some(mgr);
                    state.bind_missing_data_devices(qh);
                }
                "wl_seat" => {
                    let seat: WlSeat = registry.bind(name, version.min(5), qh, name);
                    state.seats.push((name, seat));
                    state.bind_missing_data_devices(qh);
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(dev) = state.data_devices.remove(&name)
                    && dev.version() >= 2
                {
                    dev.release();
                }
                state.seats.retain(|(n, seat)| {
                    if *n == name {
                        if seat.version() >= 5 {
                            seat.release();
                        }
                        false
                    } else {
                        true
                    }
                });
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataDeviceManager, ()> for DndState {
    fn event(
        _state: &mut Self,
        _proxy: &WlDataDeviceManager,
        _event: <WlDataDeviceManager as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, u32> for DndState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        _event: wl_seat::Event,
        _data: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataOffer, ()> for DndState {
    fn event(
        state: &mut Self,
        offer: &WlDataOffer,
        event: wl_data_offer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_offer::Event::Offer { mime_type } => {
                let entry = state.offer_mimes.entry(offer.id()).or_default();
                if !entry.iter().any(|m| m == &mime_type) {
                    entry.push(mime_type);
                }
                if state
                    .active_dnd
                    .as_ref()
                    .is_some_and(|a| a.offer.id() == offer.id())
                {
                    state.negotiate_active_offer();
                    if let Some(active) = &state.active_dnd
                        && let Some(mime) = &active.accepted_mime
                    {
                        (state.callback)(WaylandDndEvent::Hover {
                            x: active.x,
                            y: active.y,
                            mime: mime.clone(),
                        });
                    }
                }
            }
            wl_data_offer::Event::SourceActions { .. } | wl_data_offer::Event::Action { .. } => {}
            _ => {}
        }
    }
}

impl Dispatch<WlDataDevice, u32> for DndState {
    fn event(
        state: &mut Self,
        _proxy: &WlDataDevice,
        event: wl_data_device::Event,
        _seat_name: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id: offer } => {
                state.offer_mimes.entry(offer.id()).or_default();
            }
            wl_data_device::Event::Enter {
                serial,
                surface,
                x,
                y,
                id,
            } => {
                // Clean up any stale prior active offer.
                if let Some(prev) = state.active_dnd.take() {
                    state.offer_mimes.remove(&prev.offer.id());
                    prev.offer.destroy();
                }
                let Some(offer) = id else {
                    return;
                };
                let entered_ptr = surface.id().as_ptr() as usize;
                if entered_ptr != state.target_surface_ptr {
                    state.offer_mimes.remove(&offer.id());
                    offer.destroy();
                    return;
                }

                let mimes = state
                    .offer_mimes
                    .get(&offer.id())
                    .cloned()
                    .unwrap_or_default();
                let best = choose_best_mime(&mimes);
                offer.accept(serial, best.clone());
                if offer.version() >= 3 {
                    if best.is_some() {
                        offer.set_actions(DndAction::Copy | DndAction::Move, DndAction::Copy);
                    } else {
                        offer.set_actions(DndAction::empty(), DndAction::empty());
                    }
                }
                let _ = state.conn.flush();

                let fx = x as f32;
                let fy = y as f32;
                if let Some(mime) = &best {
                    (state.callback)(WaylandDndEvent::Hover {
                        x: fx,
                        y: fy,
                        mime: mime.clone(),
                    });
                }
                state.active_dnd = Some(ActiveDndOffer {
                    offer,
                    serial,
                    x: fx,
                    y: fy,
                    accepted_mime: best,
                });
            }
            wl_data_device::Event::Motion { x, y, .. } => {
                if let Some(active) = state.active_dnd.as_mut() {
                    active.x = x as f32;
                    active.y = y as f32;
                    let fx = active.x;
                    let fy = active.y;
                    let mime = active.accepted_mime.clone();
                    if let Some(mime) = mime {
                        (state.callback)(WaylandDndEvent::Hover { x: fx, y: fy, mime });
                    }
                }
            }
            wl_data_device::Event::Leave => {
                if let Some(prev) = state.active_dnd.take() {
                    state.offer_mimes.remove(&prev.offer.id());
                    prev.offer.destroy();
                }
                (state.callback)(WaylandDndEvent::Leave);
            }
            wl_data_device::Event::Drop => {
                let Some(active) = state.active_dnd.take() else {
                    (state.callback)(WaylandDndEvent::Leave);
                    return;
                };
                let offer_id = active.offer.id();
                let mimes = state.offer_mimes.remove(&offer_id).unwrap_or_default();
                let mime = active.accepted_mime.or_else(|| choose_best_mime(&mimes));

                let Some(mime) = mime else {
                    active.offer.destroy();
                    let _ = state.conn.flush();
                    (state.callback)(WaylandDndEvent::Leave);
                    return;
                };

                let Ok((read_fd, write_fd)) = pipe_with(PipeFlags::CLOEXEC) else {
                    active.offer.destroy();
                    let _ = state.conn.flush();
                    (state.callback)(WaylandDndEvent::Leave);
                    return;
                };

                active.offer.receive(mime.clone(), write_fd.as_fd());
                // Close our copy of the write end immediately so the reader hits EOF once the
                // drag source finishes writing.
                drop(write_fd);
                let _ = state.conn.flush();

                let offer = active.offer;
                let conn = state.conn.clone();
                let cb = Arc::clone(&state.callback);
                let x = active.x;
                let y = active.y;

                // Stream the drop payload off-thread so large images/videos never stall Wayland.
                let _ = thread::Builder::new()
                    .name("omarex-dnd-read".to_owned())
                    .spawn(move || {
                        let mut payload = read_drop_payload(read_fd, &mime);

                        // Browser drag fallback: browsers often offer `text/uri-list` containing an
                        // `https://` URL alongside direct `image/*`/`video/*` bytes. If `text/uri-list`
                        // had no local `file://` path, try receiving any offered binary media MIME
                        // on the same `wl_data_offer` before finishing!
                        if matches!(payload, None | Some(WaylandDropPayload::Text(_)))
                            && mime.eq_ignore_ascii_case("text/uri-list")
                            && let Some(media_mime) = choose_best_binary_media_mime(&mimes)
                            && let Ok((media_rx, media_tx)) = pipe_with(PipeFlags::CLOEXEC)
                        {
                            offer.receive(media_mime.clone(), media_tx.as_fd());
                            drop(media_tx);
                            let _ = conn.flush();
                            if let Some(media_payload) = read_drop_payload(media_rx, &media_mime) {
                                payload = Some(media_payload);
                            }
                        }

                        if offer.version() >= 3 {
                            offer.finish();
                        }
                        offer.destroy();
                        let _ = conn.flush();

                        match payload {
                            Some(p) => cb(WaylandDndEvent::Drop { x, y, payload: p }),
                            None => cb(WaylandDndEvent::Leave),
                        }
                    });
            }
            wl_data_device::Event::Selection { id: Some(offer) } => {
                // We use `arboard` / clipboard helpers for copy-paste; immediately destroy unused
                // clipboard selection offers so we never leak Wayland proxy IDs.
                state.offer_mimes.remove(&offer.id());
                offer.destroy();
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(DndState, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

// ============================================================================
// MIME Selection & Payload Decoding (Files, Portal, Images, Videos, Text)
// ============================================================================

/// Selects the highest-priority supported MIME type from a Wayland `wl_data_offer`'s offers.
///
/// Priority:
/// 1. Local file URI lists (`text/uri-list`, KDE/GNOME variants)
/// 2. XDG Desktop Portal file transfer (`application/vnd.portal.filetransfer`)
/// 3. Direct binary media & document payloads (`image/*`, `video/*`, `audio/*`, PDF, ZIP)
/// 4. UTF-8 plain text fallback (`text/plain;charset=utf-8`, `text/plain`, `UTF8_STRING`).
pub fn choose_best_mime(offered: &[String]) -> Option<String> {
    const PRIORITY_LIST: &[&str] = &[
        "text/uri-list",
        "application/x-kde-urilist",
        "x-special/gnome-copied-files",
        "application/vnd.portal.filetransfer",
        // Images (browser / screenshot / editor drags)
        "image/png",
        "image/webp",
        "image/jpeg",
        "image/gif",
        "image/avif",
        "image/bmp",
        "image/tiff",
        "image/svg+xml",
        // Videos
        "video/mp4",
        "video/webm",
        "video/quicktime",
        "video/x-matroska",
        "video/ogg",
        "video/x-msvideo",
        // Audio & documents
        "audio/mpeg",
        "audio/wav",
        "audio/ogg",
        "audio/flac",
        "audio/mp4",
        "audio/webm",
        "application/pdf",
        "application/zip",
        // Plain text fallback
        "text/plain;charset=utf-8",
        "text/plain",
        "UTF8_STRING",
    ];

    for preferred in PRIORITY_LIST {
        if let Some(found) = offered.iter().find(|m| m.eq_ignore_ascii_case(preferred)) {
            return Some(found.clone());
        }
    }

    // Accept any other concrete `image/*`, `video/*`, or `audio/*` MIME offered by exotic apps.
    for m in offered {
        let lower = m.to_ascii_lowercase();
        let base = lower.split(';').next().unwrap_or("").trim();
        if (base.starts_with("image/") || base.starts_with("video/") || base.starts_with("audio/"))
            && !base.ends_with("/*")
        {
            return Some(m.clone());
        }
    }

    None
}

/// Picks the best direct binary media MIME (`image/*`, `video/*`, `audio/*`, PDF, ZIP) offered
/// alongside a non-local `text/uri-list` (e.g. dragging a web image from Chromium/Firefox).
pub fn choose_best_binary_media_mime(offered: &[String]) -> Option<String> {
    let media_only: Vec<String> = offered
        .iter()
        .filter(|m| {
            let base = m
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            ((base.starts_with("image/")
                || base.starts_with("video/")
                || base.starts_with("audio/"))
                && !base.ends_with("/*"))
                || base == "application/pdf"
                || base == "application/zip"
        })
        .cloned()
        .collect();
    choose_best_mime(&media_only)
}

/// Extracts non-comment, non-empty `http://` / `https://` URLs from a `text/uri-list` payload.
pub fn parse_web_uri_list(raw: &str) -> Vec<String> {
    raw.lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty()
                && !l.starts_with('#')
                && (l.starts_with("https://") || l.starts_with("http://"))
        })
        .map(str::to_owned)
        .collect()
}

/// Maps a binary media/document MIME type to a clean lowercase filename extension.
pub fn mime_to_extension(mime: &str) -> Option<&'static str> {
    let base = mime.split(';').next().unwrap_or(mime).trim();
    match base.to_ascii_lowercase().as_str() {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        "image/avif" => Some("avif"),
        "image/bmp" | "image/x-ms-bmp" => Some("bmp"),
        "image/tiff" => Some("tiff"),
        "image/svg+xml" => Some("svg"),
        "image/heic" => Some("heic"),
        "image/heif" => Some("heif"),
        "video/mp4" => Some("mp4"),
        "video/webm" => Some("webm"),
        "video/quicktime" => Some("mov"),
        "video/x-matroska" => Some("mkv"),
        "video/ogg" => Some("ogv"),
        "video/x-msvideo" => Some("avi"),
        "audio/mpeg" => Some("mp3"),
        "audio/wav" | "audio/x-wav" => Some("wav"),
        "audio/ogg" => Some("ogg"),
        "audio/flac" => Some("flac"),
        "audio/mp4" => Some("m4a"),
        "audio/webm" => Some("weba"),
        "application/pdf" => Some("pdf"),
        "application/zip" => Some("zip"),
        _ => None,
    }
}

fn read_drop_payload(read_fd: OwnedFd, mime: &str) -> Option<WaylandDropPayload> {
    let mut reader = std::fs::File::from(read_fd);
    let lower = mime.to_ascii_lowercase();
    let base = lower.split(';').next().unwrap_or(&lower).trim();

    match base {
        "text/uri-list" | "application/x-kde-urilist" => {
            let bytes = read_capped_bytes(&mut reader, MAX_TEXT_DROP_BYTES)?;
            let text = String::from_utf8_lossy(&bytes);
            let paths = parse_uri_list(&text);
            if !paths.is_empty() {
                Some(WaylandDropPayload::Files(paths))
            } else {
                let web_urls = parse_web_uri_list(&text);
                if web_urls.is_empty() {
                    None
                } else {
                    Some(WaylandDropPayload::Text(web_urls.join(" ")))
                }
            }
        }
        "x-special/gnome-copied-files" => {
            let bytes = read_capped_bytes(&mut reader, MAX_TEXT_DROP_BYTES)?;
            let text = String::from_utf8_lossy(&bytes);
            let paths = parse_gnome_copied_files(&text);
            if paths.is_empty() {
                None
            } else {
                Some(WaylandDropPayload::Files(paths))
            }
        }
        "application/vnd.portal.filetransfer" => {
            let bytes = read_capped_bytes(&mut reader, 64 * 1024)?;
            let key = String::from_utf8_lossy(&bytes)
                .trim_matches(['\0', '\r', '\n', ' '])
                .to_owned();
            if key.is_empty() {
                return None;
            }
            let paths = retrieve_portal_filetransfer(&key);
            if paths.is_empty() {
                None
            } else {
                Some(WaylandDropPayload::Files(paths))
            }
        }
        "text/plain" | "utf8_string" => {
            let bytes = read_capped_bytes(&mut reader, MAX_TEXT_DROP_BYTES)?;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if text.is_empty() {
                return None;
            }
            // If a drag source passed `file://...` lines or existing local paths as `text/plain`,
            // treat them as real file drops so images/videos get full file/upload handling!
            let uri_paths = parse_uri_list(&text);
            if !uri_paths.is_empty() {
                return Some(WaylandDropPayload::Files(uri_paths));
            }
            let plain_paths = parse_existing_local_paths(&text);
            if !plain_paths.is_empty() {
                return Some(WaylandDropPayload::Files(plain_paths));
            }
            Some(WaylandDropPayload::Text(text))
        }
        _ => {
            let ext = mime_to_extension(base).unwrap_or("bin");
            let path = stream_pipe_to_temp_file(&mut reader, ext, MAX_BINARY_DROP_BYTES)?;
            Some(WaylandDropPayload::Files(vec![path]))
        }
    }
}

fn read_capped_bytes(reader: &mut impl Read, max_bytes: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let remaining = max_bytes.saturating_sub(out.len());
                if remaining == 0 {
                    break;
                }
                let take = n.min(remaining);
                out.extend_from_slice(&buf[..take]);
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                tracing::warn!(error = %err, "failed reading Wayland DnD pipe");
                return None;
            }
        }
    }
    Some(out)
}

fn stream_pipe_to_temp_file(reader: &mut impl Read, ext: &str, max_bytes: u64) -> Option<PathBuf> {
    let seq = DROP_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("omarex-drop-{}-{seq}.{ext}", std::process::id()));
    let mut file = std::fs::File::create(&path).ok()?;
    let mut buf = vec![0u8; PIPE_CHUNK_BYTES];
    let mut written: u64 = 0;

    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let next = written.saturating_add(n as u64);
                if next > max_bytes {
                    tracing::warn!(
                        max_bytes,
                        path = %path.display(),
                        "Wayland binary drop exceeded size limit; aborting"
                    );
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                    return None;
                }
                if file.write_all(&buf[..n]).is_err() {
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                    return None;
                }
                written = next;
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                tracing::warn!(error = %err, "failed streaming Wayland binary drop");
                drop(file);
                let _ = std::fs::remove_file(&path);
                return None;
            }
        }
    }

    if written == 0 {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let _ = file.flush();
    Some(path)
}

/// Retrieves file paths from `org.freedesktop.portal.FileTransfer.RetrieveFiles` over session D-Bus
/// (used by Flatpak / sandboxed browsers & file managers on Wayland).
fn retrieve_portal_filetransfer(key: &str) -> Vec<PathBuf> {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return Vec::new();
    };
    rt.block_on(async {
        let Ok(conn) = zbus::Connection::session().await else {
            return Vec::new();
        };
        let options: HashMap<&str, zbus::Variant<'_>> = HashMap::new();
        let reply = conn
            .call_method(
                Some("org.freedesktop.portal.Documents"),
                "/org/freedesktop/portal/documents",
                Some("org.freedesktop.portal.FileTransfer"),
                "RetrieveFiles",
                &(key, options),
            )
            .await;
        match reply {
            Ok(msg) => msg
                .body()
                .deserialize::<Vec<String>>()
                .unwrap_or_default()
                .into_iter()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect(),
            Err(err) => {
                tracing::debug!(error = %err, "XDG portal FileTransfer.RetrieveFiles failed");
                Vec::new()
            }
        }
    })
}

/// Parses an RFC 2483 `text/uri-list` payload into local filesystem paths (decoding `file://`
/// authorities and `%XX` UTF-8 escapes, ignoring `#` comment lines and blank lines).
pub fn parse_uri_list(raw: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(path) = file_uri_to_path(trimmed) {
            paths.push(path);
        }
    }
    paths
}

/// Parses an `x-special/gnome-copied-files` payload (`copy\nfile:///...` or `cut\nfile:///...`).
pub fn parse_gnome_copied_files(raw: &str) -> Vec<PathBuf> {
    let mut lines = raw.lines();
    if let Some(first) = lines.next() {
        let trimmed = first.trim();
        if trimmed.eq_ignore_ascii_case("copy") || trimmed.eq_ignore_ascii_case("cut") {
            let rest: String = lines.collect::<Vec<_>>().join("\n");
            return parse_uri_list(&rest);
        }
    }
    parse_uri_list(raw)
}

/// Parses newline-separated absolute local paths (if EVERY non-empty line is an existing path).
fn parse_existing_local_paths(raw: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let unquoted = trimmed
            .strip_prefix('\'')
            .and_then(|s| s.strip_suffix('\''))
            .or_else(|| trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
            .unwrap_or(trimmed);
        if !unquoted.starts_with('/') {
            return Vec::new();
        }
        let p = Path::new(unquoted);
        if !p.exists() {
            return Vec::new();
        }
        paths.push(p.to_path_buf());
    }
    paths
}

/// Decodes a `file://` URI (`file:///path/to/file`, `file://localhost/path/to/file`) into a
/// local [`PathBuf`], handling `%XX` UTF-8 percent encoding.
pub fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let slash_idx = rest.find('/')?;
    let authority = &rest[..slash_idx];
    // Accept empty authority (`file:///...`) and local hostnames (`file://localhost/...`).
    if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
        // Still allow matching this machine's hostname (some file managers emit `file://<host>/...`).
        let host_ok = std::fs::read_to_string("/etc/hostname")
            .ok()
            .is_some_and(|h| h.trim().eq_ignore_ascii_case(authority));
        if !host_ok {
            return None;
        }
    }
    let encoded_path = &rest[slash_idx..];
    // Strip optional query/fragment if any drag source appended one.
    let clean_path = encoded_path
        .split(['?', '#'])
        .next()
        .unwrap_or(encoded_path);
    let decoded = percent_decode_bytes(clean_path);
    #[cfg(unix)]
    {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        if decoded.is_empty() || decoded[0] != b'/' {
            return None;
        }
        Some(PathBuf::from(OsString::from_vec(decoded)))
    }
    #[cfg(not(unix))]
    {
        let s = String::from_utf8(decoded).ok()?;
        s.starts_with('/').then(|| PathBuf::from(s))
    }
}

fn percent_decode_bytes(input: &str) -> Vec<u8> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

const fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_uri_list_with_images_videos_spaces_and_umlauts() {
        let raw = "# comment from file manager\r\n\
                   file:///home/user/Pictures/Screenshot%202026.png\r\n\
                   file://localhost/home/user/Videos/M%C3%BCnchen%20Clip.mp4\r\n\
                   file:///tmp/notes.pdf\r\n";
        let paths = parse_uri_list(raw);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/home/user/Pictures/Screenshot 2026.png"),
                PathBuf::from("/home/user/Videos/München Clip.mp4"),
                PathBuf::from("/tmp/notes.pdf"),
            ]
        );
    }

    #[test]
    fn parses_gnome_copied_files_payload() {
        let raw = "copy\nfile:///home/user/clip.webm\nfile:///home/user/shot.webp\n";
        let paths = parse_gnome_copied_files(raw);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/home/user/clip.webm"),
                PathBuf::from("/home/user/shot.webp"),
            ]
        );
    }

    #[test]
    fn chooses_highest_priority_mime_for_files_images_videos_and_text() {
        // File URI list wins over plain text & image previews when dragging from a file manager
        assert_eq!(
            choose_best_mime(&[
                "text/plain".into(),
                "image/png".into(),
                "text/uri-list".into(),
            ])
            .as_deref(),
            Some("text/uri-list")
        );

        // Browser direct image drag (no file URI) prefers image/png or image/webp over text/plain
        assert_eq!(
            choose_best_mime(&["text/plain".into(), "image/webp".into()]).as_deref(),
            Some("image/webp")
        );

        // Video drag prefers video/mp4
        assert_eq!(
            choose_best_mime(&["video/mp4".into(), "text/plain".into()]).as_deref(),
            Some("video/mp4")
        );

        // Generic video/* fallback works too
        assert_eq!(
            choose_best_mime(&["video/x-flv".into()]).as_deref(),
            Some("video/x-flv")
        );
    }

    #[test]
    fn maps_common_media_and_document_mimes_to_extensions() {
        assert_eq!(mime_to_extension("image/png"), Some("png"));
        assert_eq!(mime_to_extension("image/webp"), Some("webp"));
        assert_eq!(mime_to_extension("image/jpeg"), Some("jpg"));
        assert_eq!(mime_to_extension("video/mp4"), Some("mp4"));
        assert_eq!(mime_to_extension("video/webm"), Some("webm"));
        assert_eq!(mime_to_extension("video/quicktime"), Some("mov"));
        assert_eq!(mime_to_extension("video/x-matroska"), Some("mkv"));
        assert_eq!(mime_to_extension("application/pdf"), Some("pdf"));
        assert_eq!(mime_to_extension("audio/mpeg"), Some("mp3"));
    }

    #[test]
    fn browser_drag_fallback_picks_media_mime_and_web_urls() {
        let mimes = vec![
            "text/uri-list".to_owned(),
            "text/plain".to_owned(),
            "image/webp".to_owned(),
            "image/png".to_owned(),
        ];
        assert_eq!(
            choose_best_binary_media_mime(&mimes).as_deref(),
            Some("image/png")
        );
        assert_eq!(
            parse_web_uri_list("# browser link\r\nhttps://example.com/clip.mp4\r\n"),
            vec!["https://example.com/clip.mp4".to_owned()]
        );
    }
}
