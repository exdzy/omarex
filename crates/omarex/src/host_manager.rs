//! Async Tokio multi-host orchestrator driving `rex_proto::ControlClient`,
//! per-block `TerminalDataStream` tasks (with `RESUME` + `GHOSTSNP` boundary detection +
//! nonce-verified `RexApcFramer`), chunked file transfers, `omarchy-theme` live watchers,
//! and D-Bus `DesktopNotifier`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use omarchy_theme::{
    DesktopNotifier, MaterialStyle, MuteFilter, NotificationRequest, NotificationTarget,
    ThemeEvent, ThemeRuntimeHandles, start_live_watchers,
};
use rex_proto::types::{
    BlockCloseRequest, BlockEventRequest, BlockInputEvent, ClientActionRequest,
    ClientActionResponse, ClientCallRequestedEvent, ClientKeyRequest, ClientKeymapRequest,
    ClientKeymapResponse, ClientListResponse, ClientPerformRequestedEvent, ClientPromptEndedEvent,
    ClientPromptRequestedEvent, ClientReplyRequest, DataTicketSpec, HostsListResponse, OwnMode,
    ServerEvent, SessionBootstrapRequest, SessionBootstrapResponse, SessionCreateRequest,
    SessionDisownBlockRequest, SessionFloatBlockRequest, SessionNewLayerRequest,
    SessionNewSplitRequest, SessionNewWindowRequest, SessionOwnBlockRequest,
    SessionResizeLayerRequest, SessionResizePaneRequest, SessionSetLayerStateRequest, SessionView,
    SessionZoomBlockRequest, TerminalResizeArgs, TerminalSetThemeArgs,
};
use rex_proto::{
    ControlClient, ControlConfig, ControlNotification, DEFAULT_MAX_DOWNLOAD_BYTES, DataCloseAction,
    DataWelcome, DownloadResult, FramedChunk, RecoveryOutcome, RexApcFramer, RexControlMessage,
    RexEndpoint, TerminalDataStream, TerminalStreamChunk, TransferProgress, UploadBatch,
    UploadResult, download_file, upload_file,
};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::config::SavedHost;
use crate::workspace::{BlockKey, HydratedBlockMeta};

/// Host ID assigned to the default local Unix-socket Rex server.
pub const LOCAL_HOST_ID: &str = "local";

const GHOSTSNP_MAGIC: &[u8; 8] = b"GHOSTSNP";
const GHOSTSNP_TAG_FINISH: u16 = 6;

/// Startup configuration for [`HostManager`].
pub struct HostManagerConfig {
    /// Local Rex socket endpoint (defaults to `Some(RexEndpoint::discover_default())`).
    pub local_endpoint: Option<RexEndpoint>,
    /// Saved remote hosts from [`crate::config::UserSettings::saved_hosts`].
    pub saved_hosts: Vec<SavedHost>,
    /// Optional preferred session ID for initial `session.bootstrap` on the local host.
    pub preferred_local_session_id: Option<String>,
    /// Optional client label advertised in `hello.client.label`.
    pub client_label: Option<String>,
    /// Initial `client.keymap` registration payload (client actions & defaults).
    pub initial_keymap_request: ClientKeymapRequest,
    /// Start `omarchy_theme::start_live_watchers` (`inotify` + Hyprland `.socket2.sock`).
    pub start_theme_watchers: bool,
    /// Connect D-Bus `DesktopNotifier` (`org.freedesktop.Notifications`).
    pub start_notifier: bool,
    /// Window surface material passed to live theme watchers.
    pub material: MaterialStyle,
    /// Optional UI wakeup hook (e.g., `winit::event_loop::EventLoopProxy`) invoked whenever a
    /// [`RuntimeEvent`] is queued.
    pub wake_ui: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Default for HostManagerConfig {
    fn default() -> Self {
        Self {
            local_endpoint: Some(RexEndpoint::discover_default()),
            saved_hosts: Vec::new(),
            preferred_local_session_id: None,
            client_label: None,
            initial_keymap_request: ClientKeymapRequest::default(),
            start_theme_watchers: true,
            start_notifier: true,
            material: MaterialStyle::Classic,
            wake_ui: None,
        }
    }
}

/// Non-blocking commands sent from the GUI/main thread to [`HostManager`].
#[derive(Debug)]
pub enum HostCommand {
    /// Connect a new Rex host (or reconnect if disconnected).
    AddHost {
        host_id: String,
        label: String,
        endpoint: RexEndpoint,
        preferred_session_id: Option<String>,
    },
    /// Disconnect and tear down all streams belonging to `host_id`.
    RemoveHost { host_id: String },
    /// Attach to a session and fetch its authoritative `SessionView`.
    SelectSession { host_id: String, session_id: String },
    /// Fetch the latest `SessionView` (caller deduplicates via `WorkspaceState`).
    FetchView { host_id: String, session_id: String },
    /// Hydrate `program_status` (and fallback `pwd`/`process`) for a batch of blocks off-thread
    /// in parallel and emit a single coalesced [`RuntimeEvent::SessionMetadataHydrated`].
    HydrateSessionMetadata {
        host_id: String,
        session_id: String,
        block_ids: Vec<String>,
    },
    /// Claim block ownership (`OwnMode::Request` or `OwnMode::Take`).
    OwnBlock {
        host_id: String,
        session_id: String,
        block_ids: Option<Vec<String>>,
        mode: OwnMode,
    },
    /// Release block ownership.
    DisownBlock {
        host_id: String,
        session_id: String,
        block_ids: Option<Vec<String>>,
    },
    /// Raw UTF-8 write directly to PTY (`com.superlogical.terminal.write`).
    TerminalWrite {
        host_id: String,
        session_id: String,
        block_id: String,
        data: String,
    },
    /// Resize terminal block (`com.superlogical.terminal.resize`).
    TerminalResize {
        host_id: String,
        session_id: String,
        block_id: String,
        args: TerminalResizeArgs,
    },
    /// Push Omarchy terminal colors & explicit Mode 2031 scheme (`com.superlogical.terminal.set_theme`).
    SetTheme {
        host_id: String,
        session_id: String,
        block_id: String,
        args: TerminalSetThemeArgs,
    },
    /// Direct block input event (`paste`, `mouse`, `wheel`, `focus`, `key`).
    BlockEvent {
        host_id: String,
        session_id: String,
        block_id: String,
        event: BlockInputEvent,
    },
    /// Execute a server Lua or block action (`client.action`).
    ClientAction {
        host_id: String,
        request: ClientActionRequest,
    },
    /// Send a keypress giving server Lua bindings first chance (`client.key`).
    ClientKey {
        host_id: String,
        request: ClientKeyRequest,
    },
    /// **Prioritized unordered fast-path** reply (`client.reply` via `ControlClient::reply_fast`).
    ClientReply {
        host_id: String,
        request: ClientReplyRequest,
    },
    /// Re-fetch merged `client.keymap` (e.g., after `keymap_changed`).
    RefreshKeymap {
        host_id: String,
        request: ClientKeymapRequest,
    },
    /// Trigger `server.config_reload`.
    ReloadConfig { host_id: String },
    /// Split pane (`session.new_split`).
    SplitPane {
        host_id: String,
        request: SessionNewSplitRequest,
    },
    /// Close block (`block.close`).
    CloseBlock {
        host_id: String,
        session_id: String,
        block_id: String,
        undoable: bool,
    },
    /// Close window (`session.close_window`).
    CloseWindow {
        host_id: String,
        session_id: String,
        window_id: String,
        undoable: bool,
    },
    /// Create new window (`session.new_window`).
    NewWindow {
        host_id: String,
        request: SessionNewWindowRequest,
    },
    /// Create new session (`session.create`).
    NewSession {
        host_id: String,
        request: SessionCreateRequest,
    },
    /// Create a dedicated ephemeral session for `-e <cmd>` execution and attach immediately.
    CreateCommandSession {
        host_id: String,
        request: SessionCreateRequest,
    },
    /// Destroy a session (`session.destroy`), used to clean up ephemeral `-e` sessions.
    DestroySession { host_id: String, session_id: String },
    /// Focus window (`session.focus_window`).
    FocusWindow {
        host_id: String,
        session_id: String,
        window_id: String,
    },
    /// Focus block (`session.focus_block`).
    FocusBlock {
        host_id: String,
        session_id: String,
        block_id: String,
    },
    /// Undo last close (`client.undo_close`).
    UndoClose { host_id: String, session_id: String },
    /// Rename session (`session.set_label`).
    RenameSession {
        host_id: String,
        session_id: String,
        label: String,
    },
    /// Rename window (`session.set_window_label`).
    RenameWindow {
        host_id: String,
        session_id: String,
        window_id: String,
        label: String,
    },
    /// Rename block (`session.set_block_label`).
    RenameBlock {
        host_id: String,
        session_id: String,
        block_id: String,
        label: String,
    },
    /// Float block into a floating layer (`session.float_block`).
    FloatBlock {
        host_id: String,
        request: SessionFloatBlockRequest,
    },
    /// Move a block (or dock a floating block) beside an anchor block (`session.move_block`).
    MoveBlock {
        host_id: String,
        session_id: String,
        block_id: String,
        anchor_block_id: String,
        direction: String,
        side: String,
        ratio: Option<f64>,
    },
    /// Reorder a window within its session (`session.move_window`).
    MoveWindow {
        host_id: String,
        session_id: String,
        window_id: String,
        before_window_id: Option<String>,
    },
    /// Create a new floating layer (`session.new_layer`).
    NewLayer {
        host_id: String,
        request: SessionNewLayerRequest,
    },
    /// Update floating layer state (`normal`, `minimized`, `maximized`).
    SetLayerState {
        host_id: String,
        request: SessionSetLayerStateRequest,
    },
    /// Resize tiled split ratio (`session.resize_pane`).
    ResizePane {
        host_id: String,
        request: SessionResizePaneRequest,
    },
    /// Move/resize floating layer (`session.resize_layer`).
    ResizeLayer {
        host_id: String,
        request: SessionResizeLayerRequest,
    },
    /// Toggle or set pane zoom (`session.zoom_block`).
    ZoomBlock {
        host_id: String,
        request: SessionZoomBlockRequest,
    },
    /// Stream-upload a local file (`upload_begin` + `rex.data.bytes.v1`).
    UploadFile {
        host_id: String,
        session_id: String,
        block_id: String,
        transfer_id: String,
        local_path: PathBuf,
        batch: Option<UploadBatch>,
    },
    /// Stream-download a remote file into `cache_dir` (`download_begin` + `rex.data.bytes.v1`).
    DownloadFile {
        host_id: String,
        session_id: String,
        block_id: String,
        transfer_id: String,
        remote_path: String,
        cache_dir: PathBuf,
        if_none_match: Option<String>,
        max_bytes: Option<u64>,
    },
    /// Open (or reuse) a `/api/data` `rex.data.terminal.v1` stream for a block.
    OpenBlockStream {
        host_id: String,
        session_id: String,
        block_id: String,
        /// Optional pre-issued ticket from `session.bootstrap.data_tickets`.
        ticket: Option<String>,
    },
    /// Cleanly close a single block's `/api/data` stream (WS status `1000`).
    CloseBlockStream {
        host_id: String,
        session_id: String,
        block_id: String,
    },
    /// Cleanly close all `/api/data` streams belonging to an LRU-evicted cold session
    /// (and optionally call `session.disown_block`).
    EvictSessionStreams {
        host_id: String,
        session_id: String,
        disown: bool,
    },
    /// Send an interactive desktop notification via D-Bus (`org.freedesktop.Notifications`).
    SendNotification(NotificationRequest),
    /// Shut down all host connections, data streams, and background tasks cleanly.
    Shutdown,
}

/// Typed events emitted from [`HostManager`] to the GUI/main thread.
#[derive(Debug)]
pub enum RuntimeEvent {
    HostConnected {
        host_id: String,
        client_id: String,
        resumed: bool,
        api_revision: u32,
    },
    HostDisconnected {
        host_id: String,
        will_reconnect: bool,
        reason: String,
    },
    HostError {
        host_id: String,
        context: String,
        message: String,
    },
    BootstrapReady {
        host_id: String,
        bootstrap: Box<SessionBootstrapResponse>,
        seq: u64,
    },
    /// Connected clients snapshot (emitted during bootstrap so multi-window launches can see
    /// which sessions are already attached by another live app window).
    ClientsListed {
        host_id: String,
        clients: ClientListResponse,
    },
    SessionViewUpdated {
        host_id: String,
        session_id: String,
        view: Box<SessionView>,
        seq: u64,
    },
    /// A `session.view` or `session.attach` fetch failed so the controller can reset
    /// `view_fetch_in_flight` (and evict the session cleanly when `session_not_found` is true).
    SessionViewFetchFailed {
        host_id: String,
        session_id: String,
        session_not_found: bool,
    },
    /// Interactive session created by this client and immediately attached with its initial view.
    SessionCreatedAndAttached {
        host_id: String,
        session_id: String,
        view: Box<SessionView>,
        seq: u64,
    },
    /// New window/tab created by this client inside `session_id` (with fresh view if fetched).
    WindowCreated {
        host_id: String,
        session_id: String,
        window_id: String,
        block_ids: Vec<String>,
        view: Option<Box<SessionView>>,
        seq: u64,
    },
    /// Dedicated ephemeral command session created & attached for `-e <cmd>` launches.
    CommandSessionReady {
        host_id: String,
        session_id: String,
        view: Box<SessionView>,
    },
    /// Single coalesced batch of initial per-block metadata (`program_status`, `pwd`, `process`)
    /// fetched concurrently off the UI thread.
    SessionMetadataHydrated {
        host_id: String,
        session_id: String,
        blocks: Vec<HydratedBlockMeta>,
    },
    KeymapUpdated {
        host_id: String,
        keymap: Box<ClientKeymapResponse>,
        seq: u64,
    },
    HostsListUpdated {
        host_id: String,
        hosts: HostsListResponse,
    },
    /// General Rex control/session/block event (preserving monotonic `seq`).
    ControlEvent {
        host_id: String,
        seq: u64,
        event: Box<ServerEvent>,
    },
    /// First-class `rex.ui` interactive prompt request (`input` or `pick`).
    PromptRequested {
        host_id: String,
        seq: u64,
        request: Box<ClientPromptRequestedEvent>,
    },
    /// First-class `rex.ui` prompt cancellation/completion notice.
    PromptEnded {
        host_id: String,
        seq: u64,
        event: ClientPromptEndedEvent,
    },
    /// Server-pushed client action request (`session.select`, `pane.paste`, etc.).
    PerformRequested {
        host_id: String,
        seq: u64,
        request: Box<ClientPerformRequestedEvent>,
    },
    /// Multi-server Lua action proxy request (must reply within 10s via `ClientReply`).
    CallRequested {
        host_id: String,
        seq: u64,
        request: ClientCallRequestedEvent,
    },
    /// Result of a `client.action` RPC (including inline `client_actions` queue).
    ClientActionCompleted {
        host_id: String,
        action: String,
        response: ClientActionResponse,
        seq: u64,
    },
    /// Per-block `/api/data` terminal stream event (snapshot chunks, live VT, verified `rex1;` APCs).
    DataStream(DataStreamEvent),
    /// Upload/download progress tick.
    TransferProgress {
        host_id: String,
        session_id: String,
        block_id: String,
        transfer_id: String,
        progress: TransferProgress,
    },
    /// Upload completed or failed.
    UploadFinished {
        host_id: String,
        session_id: String,
        block_id: String,
        transfer_id: String,
        result: Result<UploadResult, String>,
    },
    /// Download completed or failed.
    DownloadFinished {
        host_id: String,
        session_id: String,
        block_id: String,
        transfer_id: String,
        result: Result<DownloadResult, String>,
    },
    /// Live Omarchy theme or Hyprland rounding change.
    ThemeChanged(ThemeEvent),
    /// User clicked a desktop notification or its `"Open"` action button.
    NotificationClicked(NotificationTarget),
}

/// Payload carried by [`RuntimeEvent::DataStream`].
#[derive(Debug, Clone)]
pub struct DataStreamEvent {
    pub host_id: String,
    pub session_id: String,
    pub block_id: String,
    pub kind: DataStreamEventKind,
}

/// Lifecycle & payload events for a single block's `rex.data.terminal.v1` stream.
#[derive(Debug, Clone)]
pub enum DataStreamEventKind {
    /// Initial `HELLO` -> `WELCOME` succeeded (`channel_nonce` established).
    Opened { welcome: DataWelcome },
    /// Phase 1 raw `GHOSTSNP` v1 binary snapshot chunk (untouched; feed directly to `vt_engine::SnapshotDecoder`).
    SnapshotChunk { bytes: Bytes, bytes_received: u64 },
    /// Exact `FINISH` record of `GHOSTSNP` v1 reached; all subsequent chunks are Phase 2 live VT + `rex1;` APCs.
    SnapshotComplete { snapshot_bytes: u64 },
    /// Phase 2 raw live VT bytes (with authenticated `rex1;` APCs extracted and Kitty Graphics untouched).
    LiveVtChunk { bytes: Bytes, bytes_received: u64 },
    /// Nonce-verified in-band Rex control APC (`resize`, `theme`, `child_exited`).
    RexApc(RexControlMessage),
    /// Stream seamlessly resumed via `RESUME` at exact `bytes_received` (no snapshot reset needed).
    Resumed {
        bytes_received: u64,
        welcome: DataWelcome,
    },
    /// Stream reopened with a fresh ticket; caller MUST reset its `SnapshotDecoder` and `channel_nonce`.
    ReopenedFreshSnapshot { welcome: DataWelcome },
    /// Stream ended permanently (`1000`, `4000`, `1008`, or explicit close).
    Closed { action: DataCloseAction },
    /// Fatal stream error.
    Error { message: String },
}

/// Cloneable UI-facing handle to the background [`HostManager`] Tokio actor.
#[derive(Clone)]
pub struct HostManager {
    cmd_tx: mpsc::UnboundedSender<HostCommand>,
}

impl std::fmt::Debug for HostManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostManager").finish_non_exhaustive()
    }
}

impl HostManager {
    /// Spawn the multi-host background runtime onto the current Tokio runtime.
    pub fn spawn(config: HostManagerConfig) -> (Self, mpsc::UnboundedReceiver<RuntimeEvent>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();

        let emitter = EventEmitter {
            tx: ev_tx,
            wake_ui: config.wake_ui.clone(),
        };

        tokio::spawn(async move {
            run_host_manager(config, cmd_rx, emitter).await;
        });

        (Self { cmd_tx }, ev_rx)
    }

    /// Enqueue a non-blocking command for the background orchestrator.
    pub fn send(&self, cmd: HostCommand) -> Result<(), Box<HostCommand>> {
        self.cmd_tx.send(cmd).map_err(|e| Box::new(e.0))
    }

    /// Request a clean shutdown of all hosts, streams, and watchers.
    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(HostCommand::Shutdown);
    }
}

#[derive(Clone)]
struct EventEmitter {
    tx: mpsc::UnboundedSender<RuntimeEvent>,
    wake_ui: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl EventEmitter {
    fn emit(&self, event: RuntimeEvent) {
        if self.tx.send(event).is_ok()
            && let Some(wake) = &self.wake_ui
        {
            wake();
        }
    }
}

/// Exact `O(1)` streaming boundary detector for Ghostty `GHOSTSNP` v1 snapshots.
///
/// Format (`docs/protocol-spec.md §4.2`):
/// - Header (10 bytes): `b"GHOSTSNP"` (8 B) + `version: u16 LE` (2 B)
/// - Records: `[tag: u16 LE][payload_len: u32 LE][crc32c: u32 LE][payload (payload_len B)]`
/// - Snapshot terminates immediately after the payload of record `tag == 6 (FINISH)`.
#[derive(Debug, Clone)]
pub struct GhostSnpBoundaryScanner {
    phase: BoundaryPhase,
    total_snapshot_bytes: u64,
}

#[derive(Debug, Clone)]
enum BoundaryPhase {
    Header {
        buf: [u8; 10],
        filled: usize,
    },
    RecordHeader {
        buf: [u8; 10],
        filled: usize,
    },
    RecordPayload {
        tag: u16,
        remaining: u64,
    },
    Finished,
    /// Fallback if stream did not start with `GHOSTSNP` magic.
    PassthroughLive,
}

impl Default for GhostSnpBoundaryScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl GhostSnpBoundaryScanner {
    pub fn new() -> Self {
        Self {
            phase: BoundaryPhase::Header {
                buf: [0u8; 10],
                filled: 0,
            },
            total_snapshot_bytes: 0,
        }
    }

    pub fn is_finished(&self) -> bool {
        matches!(
            self.phase,
            BoundaryPhase::Finished | BoundaryPhase::PassthroughLive
        )
    }

    /// Split `chunk` at the exact `GHOSTSNP` `FINISH` record boundary.
    ///
    /// Returns `(snapshot_part, live_vt_part, finished_snapshot_byte_count)`.
    pub fn scan_split<'a>(&mut self, chunk: &'a [u8]) -> (&'a [u8], &'a [u8], Option<u64>) {
        if self.is_finished() {
            return (&[], chunk, None);
        }

        let mut offset = 0usize;
        while offset < chunk.len() {
            match &mut self.phase {
                BoundaryPhase::Header { buf, filled } => {
                    let take = (10 - *filled).min(chunk.len() - offset);
                    buf[*filled..*filled + take].copy_from_slice(&chunk[offset..offset + take]);
                    *filled += take;
                    offset += take;
                    if *filled == 10 {
                        let version = u16::from_le_bytes([buf[8], buf[9]]);
                        if &buf[0..8] == GHOSTSNP_MAGIC && version == 1 {
                            self.total_snapshot_bytes += 10;
                            self.phase = BoundaryPhase::RecordHeader {
                                buf: [0u8; 10],
                                filled: 0,
                            };
                        } else {
                            // Not a GHOSTSNP v1 stream; treat entire chunk as live VT.
                            self.phase = BoundaryPhase::PassthroughLive;
                            return (&[], chunk, None);
                        }
                    }
                }
                BoundaryPhase::RecordHeader { buf, filled } => {
                    let take = (10 - *filled).min(chunk.len() - offset);
                    buf[*filled..*filled + take].copy_from_slice(&chunk[offset..offset + take]);
                    *filled += take;
                    offset += take;
                    if *filled == 10 {
                        let tag = u16::from_le_bytes([buf[0], buf[1]]);
                        let payload_len =
                            u64::from(u32::from_le_bytes([buf[2], buf[3], buf[4], buf[5]]));
                        self.total_snapshot_bytes += 10;
                        if payload_len == 0 {
                            if tag == GHOSTSNP_TAG_FINISH {
                                self.phase = BoundaryPhase::Finished;
                                return (
                                    &chunk[..offset],
                                    &chunk[offset..],
                                    Some(self.total_snapshot_bytes),
                                );
                            }
                            self.phase = BoundaryPhase::RecordHeader {
                                buf: [0u8; 10],
                                filled: 0,
                            };
                        } else {
                            self.phase = BoundaryPhase::RecordPayload {
                                tag,
                                remaining: payload_len,
                            };
                        }
                    }
                }
                BoundaryPhase::RecordPayload { tag, remaining } => {
                    let avail = (chunk.len() - offset) as u64;
                    let take = (*remaining).min(avail);
                    *remaining -= take;
                    self.total_snapshot_bytes += take;
                    offset += take as usize;
                    if *remaining == 0 {
                        let finished_tag = *tag;
                        if finished_tag == GHOSTSNP_TAG_FINISH {
                            self.phase = BoundaryPhase::Finished;
                            return (
                                &chunk[..offset],
                                &chunk[offset..],
                                Some(self.total_snapshot_bytes),
                            );
                        }
                        self.phase = BoundaryPhase::RecordHeader {
                            buf: [0u8; 10],
                            filled: 0,
                        };
                    }
                }
                BoundaryPhase::Finished | BoundaryPhase::PassthroughLive => {
                    return (&chunk[..offset], &chunk[offset..], None);
                }
            }
        }

        (chunk, &[], None)
    }
}

/// Coordinates per-host `session.attach` state so concurrent `data.connect` (`OpenBlockStream`)
/// and `session.own_block` / `session.disown_block` RPCs never race ahead of `session.attach`
/// and trigger Rex `409 conflict` or `400 bad_request` errors.
#[derive(Default)]
struct HostAttachState {
    inner: Mutex<HostAttachInner>,
}

#[derive(Default)]
struct HostAttachInner {
    attached: HashSet<String>,
    in_flight: HashMap<String, Arc<Notify>>,
}

struct AttachInFlightGuard<'a> {
    state: &'a HostAttachState,
    session_id: String,
    armed: bool,
}

impl Drop for AttachInFlightGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut g = self.state.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(notify) = g.in_flight.remove(&self.session_id) {
                notify.notify_waiters();
            }
        }
    }
}

impl HostAttachState {
    fn mark_attached(&self, session_id: &str) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.attached.insert(session_id.to_owned());
        if let Some(notify) = g.in_flight.remove(session_id) {
            notify.notify_waiters();
        }
    }

    fn clear_session(&self, session_id: &str) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.attached.remove(session_id);
        if let Some(notify) = g.in_flight.remove(session_id) {
            notify.notify_waiters();
        }
    }

    fn clear_all(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.attached.clear();
        for (_, notify) in g.in_flight.drain() {
            notify.notify_waiters();
        }
    }

    fn is_attached(&self, session_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .attached
            .contains(session_id)
    }

    async fn wait_if_attaching(&self, session_id: &str) {
        loop {
            let notified = {
                let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                g.in_flight
                    .get(session_id)
                    .map(|n| Arc::clone(n).notified_owned())
            };
            match notified {
                Some(fut) => fut.await,
                None => return,
            }
        }
    }

    /// Ensures `session_id` is attached on `client`, deduplicating concurrent callers.
    ///
    /// When `force` is `false` and the session is already marked attached (or finishes attaching
    /// via another in-flight caller), returns `Ok(None)` without issuing a duplicate RPC.
    /// When `force` is `true`, always issues `session.attach` once any earlier attach settles
    /// and returns `Ok(Some((view, seq)))`.
    async fn ensure_attached(
        &self,
        client: &ControlClient,
        session_id: &str,
        force: bool,
    ) -> Result<Option<(SessionView, u64)>, rex_proto::ProtoError> {
        loop {
            let wait_fut = {
                let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                if !force && g.attached.contains(session_id) {
                    return Ok(None);
                }
                if let Some(existing) = g.in_flight.get(session_id) {
                    Some(Arc::clone(existing).notified_owned())
                } else {
                    g.in_flight
                        .insert(session_id.to_owned(), Arc::new(Notify::new()));
                    None
                }
            };

            if let Some(fut) = wait_fut {
                fut.await;
                if !force && self.is_attached(session_id) {
                    return Ok(None);
                }
                continue;
            }

            let mut guard = AttachInFlightGuard {
                state: self,
                session_id: session_id.to_owned(),
                armed: true,
            };
            let res = client.session_attach(session_id).await;
            let notify = {
                let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                if res.is_ok() {
                    g.attached.insert(session_id.to_owned());
                } else {
                    g.attached.remove(session_id);
                }
                g.in_flight.remove(session_id)
            };
            guard.armed = false;
            if let Some(n) = notify {
                n.notify_waiters();
            }
            return res.map(|(att, seq)| Some((att.view, seq)));
        }
    }
}

/// Returns `true` if an RPC error should be surfaced as a user-visible `RuntimeEvent::HostError`
/// toast, or `false` when it is an expected Rex lifecycle/teardown/reconnect race that should
/// only be logged at `tracing::debug!`.
fn should_surface_rpc_error(context: &str, err: &rex_proto::ProtoError) -> bool {
    if context == "connect" {
        return true;
    }
    if err.is_transient_disconnect() {
        return false;
    }
    match context {
        // Expected 404 races when sessions/windows/blocks close concurrently (e.g., ephemeral `-e`
        // windows, `tr3x` recreating panes, or multi-window teardown) or when another device
        // already answered/cancelled a shared `rex.ui` prompt (`client.reply`).
        "session.view"
        | "session.attach"
        | "com.superlogical.terminal.write"
        | "com.superlogical.terminal.resize"
        | "com.superlogical.terminal.set_theme"
        | "block.event"
        | "block.close"
        | "session.close_window"
        | "session.destroy"
        | "session.focus_window"
        | "session.focus_block"
        | "session.resize_pane"
        | "session.resize_layer"
        | "session.zoom_block"
        | "session.set_layer_state"
        | "session.move_block"
        | "session.move_window"
        | "session.float_block"
        | "client.key"
        | "client.reply" => !err.is_not_found(),

        // Ownership claims/releases can legitimately race `session.attach` (400/409) or teardown (404).
        "session.own_block" | "session.disown_block" => {
            !(err.is_not_found() || err.is_bad_request() || err.is_conflict())
        }

        _ => true,
    }
}

fn emit_rpc_error(
    emitter: &EventEmitter,
    host_id: String,
    context: impl Into<String>,
    err: &rex_proto::ProtoError,
) {
    let context = context.into();
    if should_surface_rpc_error(&context, err) {
        emitter.emit(RuntimeEvent::HostError {
            host_id,
            context,
            message: err.to_string(),
        });
    } else {
        tracing::debug!(
            host = %host_id,
            %context,
            error = %err,
            "suppressed expected Rex lifecycle/race RPC error"
        );
    }
}

struct ManagedHost {
    client: ControlClient,
    attach_state: Arc<HostAttachState>,
    event_task: tokio::task::JoinHandle<()>,
}

struct ActiveStream {
    cancel_tx: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

enum InternalMsg {
    HostEstablished {
        host_id: String,
        client: Box<ControlClient>,
        attach_state: Arc<HostAttachState>,
        event_task: tokio::task::JoinHandle<()>,
    },
    StreamEnded {
        key: BlockKey,
    },
}

async fn run_host_manager(
    config: HostManagerConfig,
    mut cmd_rx: mpsc::UnboundedReceiver<HostCommand>,
    emitter: EventEmitter,
) {
    let mut hosts: HashMap<String, ManagedHost> = HashMap::new();
    let mut streams: HashMap<BlockKey, ActiveStream> = HashMap::new();
    let (internal_tx, mut internal_rx) = mpsc::unbounded_channel::<InternalMsg>();

    // Optional live Omarchy theme + Hyprland IPC watcher.
    let _theme_handles: Option<ThemeRuntimeHandles> = if config.start_theme_watchers {
        let (_initial, mut theme_rx, handles) = start_live_watchers(config.material);
        let em = emitter.clone();
        tokio::spawn(async move {
            while let Some(ev) = theme_rx.recv().await {
                em.emit(RuntimeEvent::ThemeChanged(ev));
            }
        });
        Some(handles)
    } else {
        None
    };

    // Optional D-Bus interactive DesktopNotifier.
    let notifier: Option<Arc<DesktopNotifier>> = if config.start_notifier {
        let filter = Arc::new(Mutex::new(MuteFilter::default()));
        match DesktopNotifier::connect("omarex", filter).await {
            Ok((n, mut click_rx)) => {
                let em = emitter.clone();
                tokio::spawn(async move {
                    while let Some(target) = click_rx.recv().await {
                        em.emit(RuntimeEvent::NotificationClicked(target));
                    }
                });
                Some(Arc::new(n))
            }
            Err(err) => {
                tracing::debug!(error = %err, "D-Bus DesktopNotifier unavailable");
                None
            }
        }
    } else {
        None
    };

    let keymap_req = Arc::new(config.initial_keymap_request.clone());
    let client_label = config.client_label.clone();

    // Auto-dial local default Rex server if configured.
    if let Some(local_ep) = config.local_endpoint {
        spawn_host_dial(
            LOCAL_HOST_ID.to_owned(),
            local_ep,
            config.preferred_local_session_id,
            client_label.clone(),
            Arc::clone(&keymap_req),
            emitter.clone(),
            internal_tx.clone(),
        );
    }

    // Auto-dial saved remote hosts marked `auto_connect`.
    for saved in config.saved_hosts {
        if saved.auto_connect {
            match RexEndpoint::parse(&saved.endpoint) {
                Ok(ep) => spawn_host_dial(
                    saved.id,
                    ep,
                    None,
                    client_label.clone(),
                    Arc::clone(&keymap_req),
                    emitter.clone(),
                    internal_tx.clone(),
                ),
                Err(err) => emitter.emit(RuntimeEvent::HostError {
                    host_id: saved.id,
                    context: "parse_saved_host".into(),
                    message: err.to_string(),
                }),
            }
        }
    }

    loop {
        tokio::select! {
            biased;

            Some(internal) = internal_rx.recv() => {
                match internal {
                    InternalMsg::HostEstablished { host_id, client, attach_state, event_task } => {
                        if let Some(old) = hosts.insert(host_id, ManagedHost { client: *client, attach_state, event_task }) {
                            old.attach_state.clear_all();
                            old.client.shutdown();
                            old.event_task.abort();
                        }
                    }
                    InternalMsg::StreamEnded { key } => {
                        streams.remove(&key);
                    }
                }
            }

            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    HostCommand::Shutdown => {
                        for (_, mut st) in streams.drain() {
                            if let Some(tx) = st.cancel_tx.take() {
                                let _ = tx.send(());
                            }
                            let _ = tokio::time::timeout(Duration::from_millis(500), st.task).await;
                        }
                        for (_, host) in hosts.drain() {
                            host.client.shutdown();
                            host.event_task.abort();
                        }
                        return;
                    }

                    HostCommand::AddHost {
                        host_id,
                        label: _,
                        endpoint,
                        preferred_session_id,
                    } => {
                        spawn_host_dial(
                            host_id,
                            endpoint,
                            preferred_session_id,
                            client_label.clone(),
                            Arc::clone(&keymap_req),
                            emitter.clone(),
                            internal_tx.clone(),
                        );
                    }

                    HostCommand::RemoveHost { host_id } => {
                        let matching: Vec<BlockKey> = streams
                            .keys()
                            .filter(|k| k.host_id == host_id)
                            .cloned()
                            .collect();
                        for key in matching {
                            if let Some(mut st) = streams.remove(&key)
                                && let Some(tx) = st.cancel_tx.take()
                            {
                                let _ = tx.send(());
                            }
                        }
                        if let Some(host) = hosts.remove(&host_id) {
                            host.attach_state.clear_all();
                            host.client.shutdown();
                            host.event_task.abort();
                        }
                    }

                    // Prioritized unordered fast-path `client.reply`!
                    HostCommand::ClientReply { host_id, request } => {
                        if let Some(client) = hosts.get(&host_id).map(|h| h.client.clone()) {
                            let em = emitter.clone();
                            tokio::spawn(async move {
                                if let Err(err) = client.reply_fast(&request).await {
                                    emit_rpc_error(&em, host_id, "client.reply", &err);
                                }
                            });
                        }
                    }

                    HostCommand::SelectSession { host_id, session_id } => {
                        if let Some(host) = hosts.get(&host_id) {
                            let client = host.client.clone();
                            let attach_state = Arc::clone(&host.attach_state);
                            let em = emitter.clone();
                            tokio::spawn(async move {
                                match attach_state
                                    .ensure_attached(&client, &session_id, true)
                                    .await
                                {
                                    Ok(Some((view, seq))) => {
                                        em.emit(RuntimeEvent::SessionViewUpdated {
                                            host_id,
                                            session_id,
                                            view: Box::new(view),
                                            seq,
                                        });
                                    }
                                    Ok(None) => {}
                                    Err(err) => {
                                        em.emit(RuntimeEvent::SessionViewFetchFailed {
                                            host_id: host_id.clone(),
                                            session_id,
                                            session_not_found: err.is_session_not_found(),
                                        });
                                        emit_rpc_error(&em, host_id, "session.attach", &err);
                                    }
                                }
                            });
                        } else {
                            emitter.emit(RuntimeEvent::SessionViewFetchFailed {
                                host_id,
                                session_id,
                                session_not_found: false,
                            });
                        }
                    }

                    HostCommand::FetchView { host_id, session_id } => {
                        if let Some(host) = hosts.get(&host_id) {
                            let client = host.client.clone();
                            let attach_state = Arc::clone(&host.attach_state);
                            let em = emitter.clone();
                            tokio::spawn(async move {
                                match client.session_view(&session_id).await {
                                    Ok((view, seq)) => em.emit(RuntimeEvent::SessionViewUpdated {
                                        host_id,
                                        session_id,
                                        view: Box::new(view),
                                        seq,
                                    }),
                                    Err(err) => {
                                        let session_not_found = err.is_session_not_found();
                                        if session_not_found {
                                            attach_state.clear_session(&session_id);
                                        }
                                        em.emit(RuntimeEvent::SessionViewFetchFailed {
                                            host_id: host_id.clone(),
                                            session_id,
                                            session_not_found,
                                        });
                                        emit_rpc_error(&em, host_id, "session.view", &err);
                                    }
                                }
                            });
                        } else {
                            emitter.emit(RuntimeEvent::SessionViewFetchFailed {
                                host_id,
                                session_id,
                                session_not_found: false,
                            });
                        }
                    }

                    HostCommand::HydrateSessionMetadata {
                        host_id,
                        session_id,
                        block_ids,
                    } => {
                        if !block_ids.is_empty()
                            && let Some(client) = hosts.get(&host_id).map(|h| h.client.clone())
                        {
                            let em = emitter.clone();
                            tokio::spawn(async move {
                                let blocks =
                                    hydrate_blocks_concurrent(client, session_id.clone(), block_ids)
                                        .await;
                                if !blocks.is_empty() {
                                    em.emit(RuntimeEvent::SessionMetadataHydrated {
                                        host_id,
                                        session_id,
                                        blocks,
                                    });
                                }
                            });
                        }
                    }

                    HostCommand::OwnBlock { host_id, session_id, block_ids, mode } => {
                        if let Some(host) = hosts.get(&host_id) {
                            let client = host.client.clone();
                            let attach_state = Arc::clone(&host.attach_state);
                            let em = emitter.clone();
                            tokio::spawn(async move {
                                attach_state.wait_if_attaching(&session_id).await;
                                let req = SessionOwnBlockRequest {
                                    session_id,
                                    block_ids,
                                    mode: Some(mode),
                                };
                                if let Err(err) = client.session_own_block(&req).await {
                                    emit_rpc_error(&em, host_id, "session.own_block", &err);
                                }
                            });
                        }
                    }

                    HostCommand::DisownBlock { host_id, session_id, block_ids } => {
                        if let Some(host) = hosts.get(&host_id) {
                            let client = host.client.clone();
                            let attach_state = Arc::clone(&host.attach_state);
                            let em = emitter.clone();
                            tokio::spawn(async move {
                                attach_state.wait_if_attaching(&session_id).await;
                                let req = SessionDisownBlockRequest { session_id, block_ids };
                                if let Err(err) = client.session_disown_block(&req).await {
                                    emit_rpc_error(&em, host_id, "session.disown_block", &err);
                                }
                            });
                        }
                    }

                    HostCommand::TerminalWrite { host_id, session_id, block_id, data } => {
                        if let Some(client) = hosts.get(&host_id).map(|h| h.client.clone()) {
                            let em = emitter.clone();
                            tokio::spawn(async move {
                                if let Err(err) = client.terminal_write(&session_id, &block_id, data).await {
                                    emit_rpc_error(&em, host_id, "com.superlogical.terminal.write", &err);
                                }
                            });
                        }
                    }

                    HostCommand::TerminalResize { host_id, session_id, block_id, args } => {
                        if let Some(client) = hosts.get(&host_id).map(|h| h.client.clone()) {
                            let em = emitter.clone();
                            tokio::spawn(async move {
                                if let Err(err) = client.terminal_resize(&session_id, &block_id, args).await {
                                    emit_rpc_error(&em, host_id, "com.superlogical.terminal.resize", &err);
                                }
                            });
                        }
                    }

                    HostCommand::SetTheme { host_id, session_id, block_id, args } => {
                        if let Some(client) = hosts.get(&host_id).map(|h| h.client.clone()) {
                            let em = emitter.clone();
                            tokio::spawn(async move {
                                if let Err(err) = client.terminal_set_theme(&session_id, &block_id, args).await {
                                    emit_rpc_error(&em, host_id, "com.superlogical.terminal.set_theme", &err);
                                }
                            });
                        }
                    }

                    HostCommand::BlockEvent { host_id, session_id, block_id, event } => {
                        if let Some(client) = hosts.get(&host_id).map(|h| h.client.clone()) {
                            let em = emitter.clone();
                            tokio::spawn(async move {
                                let req = BlockEventRequest { session_id, block_id, event };
                                if let Err(err) = client.block_event(&req).await {
                                    emit_rpc_error(&em, host_id, "block.event", &err);
                                }
                            });
                        }
                    }

                    HostCommand::ClientAction { host_id, request } => {
                        if let Some(client) = hosts.get(&host_id).map(|h| h.client.clone()) {
                            let em = emitter.clone();
                            let action_name = request.action.clone();
                            tokio::spawn(async move {
                                match client.client_action(&request).await {
                                    Ok((response, seq)) => em.emit(RuntimeEvent::ClientActionCompleted {
                                        host_id,
                                        action: action_name,
                                        response,
                                        seq,
                                    }),
                                    Err(err) => emit_rpc_error(
                                        &em,
                                        host_id,
                                        format!("client.action({action_name})"),
                                        &err,
                                    ),
                                }
                            });
                        }
                    }

                    HostCommand::ClientKey { host_id, request } => {
                        if let Some(client) = hosts.get(&host_id).map(|h| h.client.clone()) {
                            let em = emitter.clone();
                            tokio::spawn(async move {
                                if let Err(err) = client.client_key(&request).await {
                                    emit_rpc_error(&em, host_id, "client.key", &err);
                                }
                            });
                        }
                    }

                    HostCommand::RefreshKeymap { host_id, request } => {
                        if let Some(client) = hosts.get(&host_id).map(|h| h.client.clone()) {
                            let em = emitter.clone();
                            tokio::spawn(async move {
                                match client.client_keymap(&request).await {
                                    Ok((keymap, seq)) => em.emit(RuntimeEvent::KeymapUpdated {
                                        host_id,
                                        keymap: Box::new(keymap),
                                        seq,
                                    }),
                                    Err(err) => emit_rpc_error(&em, host_id, "client.keymap", &err),
                                }
                            });
                        }
                    }

                    HostCommand::ReloadConfig { host_id } => {
                        if let Some(client) = hosts.get(&host_id).map(|h| h.client.clone()) {
                            let em = emitter.clone();
                            tokio::spawn(async move {
                                if let Err(err) = client.server_config_reload().await {
                                    emit_rpc_error(&em, host_id, "server.config_reload", &err);
                                }
                            });
                        }
                    }

                    HostCommand::SplitPane { host_id, request } => {
                        if let Some(client) = hosts.get(&host_id).map(|h| h.client.clone()) {
                            let em = emitter.clone();
                            let session_id = request.session_id.clone();
                            tokio::spawn(async move {
                                match client.session_new_split(&request).await {
                                    Ok((created, seq)) => {
                                        if let Ok((view, vseq)) =
                                            client.session_view(&session_id).await
                                        {
                                            em.emit(RuntimeEvent::SessionViewUpdated {
                                                host_id: host_id.clone(),
                                                session_id: session_id.clone(),
                                                view: Box::new(view),
                                                seq: vseq.max(seq),
                                            });
                                        }
                                        if !created.block_ids.is_empty() {
                                            let _ = client
                                                .session_own_block(&SessionOwnBlockRequest {
                                                    session_id,
                                                    block_ids: Some(created.block_ids),
                                                    mode: Some(OwnMode::Take),
                                                })
                                                .await;
                                        }
                                    }
                                    Err(err) => emit_rpc_error(&em, host_id, "session.new_split", &err),
                                }
                            });
                        }
                    }
                    HostCommand::CloseBlock { host_id, session_id, block_id, undoable } => {
                        dispatch_simple(&hosts, &emitter, host_id, "block.close", move |c| async move {
                            c.block_close(&BlockCloseRequest {
                                session_id,
                                block_id,
                                undoable: Some(undoable),
                            })
                            .await
                            .map(|_| ())
                        });
                    }
                    HostCommand::CloseWindow { host_id, session_id, window_id, undoable } => {
                        dispatch_simple(&hosts, &emitter, host_id, "session.close_window", move |c| async move {
                            c.session_close_window(&session_id, &window_id, undoable)
                                .await
                                .map(|_| ())
                        });
                    }
                    HostCommand::NewWindow { host_id, request } => {
                        if let Some(client) = hosts.get(&host_id).map(|h| h.client.clone()) {
                            let em = emitter.clone();
                            let session_id = request.session_id.clone();
                            tokio::spawn(async move {
                                match client.session_new_window(&request).await {
                                    Ok((created, seq)) => {
                                        let view = client
                                            .session_view(&session_id)
                                            .await
                                            .ok()
                                            .map(|(v, _)| Box::new(v));
                                        em.emit(RuntimeEvent::WindowCreated {
                                            host_id,
                                            session_id,
                                            window_id: created.window_id,
                                            block_ids: created.block_ids,
                                            view,
                                            seq,
                                        });
                                    }
                                    Err(err) => {
                                        emit_rpc_error(&em, host_id, "session.new_window", &err)
                                    }
                                }
                            });
                        }
                    }
                    HostCommand::NewSession { host_id, request } => {
                        if let Some(host) = hosts.get(&host_id) {
                            let client = host.client.clone();
                            let attach_state = Arc::clone(&host.attach_state);
                            let em = emitter.clone();
                            tokio::spawn(async move {
                                match client.session_create(&request).await {
                                    Ok((created, seq)) => {
                                        match attach_state
                                            .ensure_attached(&client, &created.session_id, true)
                                            .await
                                        {
                                            Ok(Some((view, att_seq))) => {
                                                em.emit(RuntimeEvent::SessionCreatedAndAttached {
                                                    host_id,
                                                    session_id: created.session_id,
                                                    view: Box::new(view),
                                                    seq: att_seq.max(seq),
                                                });
                                            }
                                            Ok(None) => {}
                                            Err(err) => {
                                                emit_rpc_error(&em, host_id, "session.attach", &err)
                                            }
                                        }
                                    }
                                    Err(err) => {
                                        emit_rpc_error(&em, host_id, "session.create", &err)
                                    }
                                }
                            });
                        }
                    }
                    HostCommand::CreateCommandSession { host_id, request } => {
                        if let Some(host) = hosts.get(&host_id) {
                            let client = host.client.clone();
                            let attach_state = Arc::clone(&host.attach_state);
                            let emitter = emitter.clone();
                            tokio::spawn(async move {
                                match client.session_create(&request).await {
                                    Ok((created, _seq)) => {
                                        match attach_state
                                            .ensure_attached(&client, &created.session_id, true)
                                            .await
                                        {
                                            Ok(Some((view, _seq))) => {
                                                emitter.emit(RuntimeEvent::CommandSessionReady {
                                                    host_id,
                                                    session_id: created.session_id,
                                                    view: Box::new(view),
                                                });
                                            }
                                            Ok(None) => {}
                                            Err(err) => {
                                                let _ = client
                                                    .session_destroy(&created.session_id)
                                                    .await;
                                                emit_rpc_error(
                                                    &emitter,
                                                    host_id,
                                                    "session.attach",
                                                    &err,
                                                );
                                            }
                                        }
                                    }
                                    Err(err) => {
                                        emit_rpc_error(&emitter, host_id, "session.create", &err);
                                    }
                                }
                            });
                        }
                    }
                    HostCommand::DestroySession { host_id, session_id } => {
                        dispatch_simple(&hosts, &emitter, host_id, "session.destroy", move |c| async move {
                            c.session_destroy(&session_id).await.map(|_| ())
                        });
                    }
                    HostCommand::FocusWindow { host_id, session_id, window_id } => {
                        dispatch_simple(&hosts, &emitter, host_id, "session.focus_window", move |c| async move {
                            c.session_focus_window(&session_id, &window_id).await.map(|_| ())
                        });
                    }
                    HostCommand::FocusBlock { host_id, session_id, block_id } => {
                        dispatch_simple(&hosts, &emitter, host_id, "session.focus_block", move |c| async move {
                            c.session_focus_block(&session_id, &block_id).await.map(|_| ())
                        });
                    }
                    HostCommand::UndoClose { host_id, session_id } => {
                        dispatch_simple(&hosts, &emitter, host_id, "client.undo_close", move |c| async move {
                            c.client_undo_close(&session_id).await.map(|_| ())
                        });
                    }
                    HostCommand::RenameSession { host_id, session_id, label } => {
                        dispatch_simple(&hosts, &emitter, host_id, "session.set_label", move |c| async move {
                            c.session_set_label(&session_id, &label).await.map(|_| ())
                        });
                    }
                    HostCommand::RenameWindow { host_id, session_id, window_id, label } => {
                        dispatch_simple(&hosts, &emitter, host_id, "session.set_window_label", move |c| async move {
                            c.session_set_window_label(&session_id, &window_id, &label)
                                .await
                                .map(|_| ())
                        });
                    }
                    HostCommand::RenameBlock { host_id, session_id, block_id, label } => {
                        dispatch_simple(&hosts, &emitter, host_id, "session.set_block_label", move |c| async move {
                            c.session_set_block_label(&session_id, &block_id, &label)
                                .await
                                .map(|_| ())
                        });
                    }
                    HostCommand::FloatBlock { host_id, request } => {
                        dispatch_simple(&hosts, &emitter, host_id, "session.float_block", move |c| async move {
                            c.session_float_block(&request).await.map(|_| ())
                        });
                    }
                    HostCommand::MoveBlock {
                        host_id,
                        session_id,
                        block_id,
                        anchor_block_id,
                        direction,
                        side,
                        ratio,
                    } => {
                        dispatch_simple(&hosts, &emitter, host_id, "session.move_block", move |c| async move {
                            let mut payload = serde_json::json!({
                                "session_id": session_id,
                                "block_id": block_id,
                                "anchor_block_id": anchor_block_id,
                                "direction": direction,
                                "side": side,
                            });
                            if let Some(r) = ratio
                                && let Some(obj) = payload.as_object_mut()
                            {
                                obj.insert("ratio".to_owned(), serde_json::json!(r));
                            }
                            c.call_raw("session.move_block", Some(payload))
                                .await
                                .map(|_| ())
                        });
                    }
                    HostCommand::MoveWindow {
                        host_id,
                        session_id,
                        window_id,
                        before_window_id,
                    } => {
                        dispatch_simple(&hosts, &emitter, host_id, "session.move_window", move |c| async move {
                            let mut payload = serde_json::json!({
                                "session_id": session_id,
                                "window_id": window_id,
                            });
                            if let Some(before) = before_window_id
                                && let Some(obj) = payload.as_object_mut()
                            {
                                obj.insert("before_window_id".to_owned(), serde_json::json!(before));
                            }
                            c.call_raw("session.move_window", Some(payload))
                                .await
                                .map(|_| ())
                        });
                    }
                    HostCommand::NewLayer { host_id, request } => {
                        dispatch_simple(&hosts, &emitter, host_id, "session.new_layer", move |c| async move {
                            c.session_new_layer(&request).await.map(|_| ())
                        });
                    }
                    HostCommand::SetLayerState { host_id, request } => {
                        dispatch_simple(&hosts, &emitter, host_id, "session.set_layer_state", move |c| async move {
                            c.session_set_layer_state(&request).await.map(|_| ())
                        });
                    }
                    HostCommand::ResizePane { host_id, request } => {
                        dispatch_simple(&hosts, &emitter, host_id, "session.resize_pane", move |c| async move {
                            c.session_resize_pane(&request).await.map(|_| ())
                        });
                    }
                    HostCommand::ResizeLayer { host_id, request } => {
                        dispatch_simple(&hosts, &emitter, host_id, "session.resize_layer", move |c| async move {
                            c.session_resize_layer(&request).await.map(|_| ())
                        });
                    }
                    HostCommand::ZoomBlock { host_id, request } => {
                        dispatch_simple(&hosts, &emitter, host_id, "session.zoom_block", move |c| async move {
                            c.session_zoom_block(&request).await.map(|_| ())
                        });
                    }

                    HostCommand::UploadFile {
                        host_id,
                        session_id,
                        block_id,
                        transfer_id,
                        local_path,
                        batch,
                    } => {
                        if let Some(client) = hosts.get(&host_id).map(|h| h.client.clone()) {
                            let em = emitter.clone();
                            tokio::spawn(async move {
                                let res = upload_file(
                                    &client,
                                    &session_id,
                                    &block_id,
                                    &local_path,
                                    batch,
                                    |progress| {
                                        em.emit(RuntimeEvent::TransferProgress {
                                            host_id: host_id.clone(),
                                            session_id: session_id.clone(),
                                            block_id: block_id.clone(),
                                            transfer_id: transfer_id.clone(),
                                            progress,
                                        });
                                    },
                                )
                                .await
                                .map_err(|e| e.to_string());
                                em.emit(RuntimeEvent::UploadFinished {
                                    host_id,
                                    session_id,
                                    block_id,
                                    transfer_id,
                                    result: res,
                                });
                            });
                        }
                    }

                    HostCommand::DownloadFile {
                        host_id,
                        session_id,
                        block_id,
                        transfer_id,
                        remote_path,
                        cache_dir,
                        if_none_match,
                        max_bytes,
                    } => {
                        if let Some(client) = hosts.get(&host_id).map(|h| h.client.clone()) {
                            let em = emitter.clone();
                            tokio::spawn(async move {
                                let limit = max_bytes.unwrap_or(DEFAULT_MAX_DOWNLOAD_BYTES);
                                let res = download_file(
                                    &client,
                                    &session_id,
                                    &block_id,
                                    &remote_path,
                                    &cache_dir,
                                    if_none_match,
                                    limit,
                                    |progress| {
                                        em.emit(RuntimeEvent::TransferProgress {
                                            host_id: host_id.clone(),
                                            session_id: session_id.clone(),
                                            block_id: block_id.clone(),
                                            transfer_id: transfer_id.clone(),
                                            progress,
                                        });
                                    },
                                )
                                .await
                                .map_err(|e| e.to_string());
                                em.emit(RuntimeEvent::DownloadFinished {
                                    host_id,
                                    session_id,
                                    block_id,
                                    transfer_id,
                                    result: res,
                                });
                            });
                        }
                    }

                    HostCommand::OpenBlockStream {
                        host_id,
                        session_id,
                        block_id,
                        ticket,
                    } => {
                        let key = BlockKey::new(host_id.clone(), session_id.clone(), block_id.clone());
                        if streams.contains_key(&key) {
                            continue;
                        }
                        if let Some(host) = hosts.get(&host_id) {
                            let client = host.client.clone();
                            let attach_state = Arc::clone(&host.attach_state);
                            let (cancel_tx, cancel_rx) = oneshot::channel();
                            let em = emitter.clone();
                            let done_tx = internal_tx.clone();
                            let task_key = key.clone();
                            let task = tokio::spawn(async move {
                                run_block_stream_task(
                                    client,
                                    attach_state,
                                    task_key.clone(),
                                    ticket,
                                    cancel_rx,
                                    em,
                                )
                                .await;
                                let _ = done_tx.send(InternalMsg::StreamEnded { key: task_key });
                            });
                            streams.insert(
                                key,
                                ActiveStream {
                                    cancel_tx: Some(cancel_tx),
                                    task,
                                },
                            );
                        } else {
                            emitter.emit(RuntimeEvent::DataStream(DataStreamEvent {
                                host_id,
                                session_id,
                                block_id,
                                kind: DataStreamEventKind::Error {
                                    message: "host is not connected".into(),
                                },
                            }));
                        }
                    }

                    HostCommand::CloseBlockStream {
                        host_id,
                        session_id,
                        block_id,
                    } => {
                        let key = BlockKey::new(host_id, session_id, block_id);
                        if let Some(mut st) = streams.remove(&key)
                            && let Some(tx) = st.cancel_tx.take()
                        {
                            let _ = tx.send(());
                        }
                    }

                    HostCommand::EvictSessionStreams {
                        host_id,
                        session_id,
                        disown,
                    } => {
                        let matching: Vec<BlockKey> = streams
                            .keys()
                            .filter(|k| k.host_id == host_id && k.session_id == session_id)
                            .cloned()
                            .collect();
                        for key in matching {
                            if let Some(mut st) = streams.remove(&key)
                                && let Some(tx) = st.cancel_tx.take()
                            {
                                let _ = tx.send(());
                            }
                        }
                        if disown && let Some(client) = hosts.get(&host_id).map(|h| h.client.clone()) {
                            tokio::spawn(async move {
                                let _ = client
                                    .session_disown_block(&SessionDisownBlockRequest {
                                        session_id,
                                        block_ids: None,
                                    })
                                    .await;
                            });
                        }
                    }

                    HostCommand::SendNotification(req) => {
                        if let Some(n) = notifier.as_ref().map(Arc::clone) {
                            tokio::spawn(async move {
                                if let Err(err) = n.send(req).await {
                                    tracing::debug!(error = %err, "failed to send desktop notification");
                                }
                            });
                        }
                    }
                }
            }

            else => return,
        }
    }
}

/// Fetches per-block OSC 7501 `program_status` (plus fallback `pwd`/`process` for non-OSC-7501
/// shell panes) concurrently over Rex's unordered RPC channel and returns one coalesced batch.
async fn hydrate_blocks_concurrent(
    client: ControlClient,
    session_id: String,
    block_ids: Vec<String>,
) -> Vec<HydratedBlockMeta> {
    let sem = Arc::new(tokio::sync::Semaphore::new(16));
    let mut join_set = tokio::task::JoinSet::new();

    for block_id in block_ids {
        let client = client.clone();
        let session_id = session_id.clone();
        let sem = Arc::clone(&sem);
        join_set.spawn(async move {
            let Ok(_permit) = sem.acquire_owned().await else {
                return None;
            };
            let mut meta = HydratedBlockMeta {
                block_id: block_id.clone(),
                ..Default::default()
            };

            match client.terminal_program_status(&session_id, &block_id).await {
                Ok((ps, _)) => {
                    meta.program_statuses = ps.records;
                }
                Err(err) if err.is_not_found() || err.is_transient_disconnect() => {
                    // Block or session already closed (or host disconnected): skip follow-up
                    // `pwd`/`process` RPCs rather than triggering two more 404 errors.
                    return None;
                }
                Err(_) => {}
            }

            // Plain shell blocks (without OSC 7501 status records) also hydrate `pwd` and
            // foreground process in parallel so every tab displays a clean project/process card.
            if meta.program_statuses.is_empty() {
                let (pwd_res, proc_res) = tokio::join!(
                    client.terminal_pwd(&session_id, &block_id),
                    client.terminal_process(&session_id, &block_id),
                );
                if let Ok((pwd, _)) = pwd_res {
                    meta.pwd = pwd.pwd;
                }
                if let Ok((proc_info, _)) = proc_res {
                    meta.foreground_process = proc_info
                        .foreground
                        .map(|p| p.name)
                        .or_else(|| proc_info.child.map(|p| p.name));
                }
            }

            if !meta.program_statuses.is_empty()
                || meta.pwd.is_some()
                || meta.foreground_process.is_some()
                || meta.title.is_some()
            {
                Some(meta)
            } else {
                None
            }
        });
    }

    let mut out = Vec::new();
    while let Some(res) = join_set.join_next().await {
        if let Ok(Some(meta)) = res {
            out.push(meta);
        }
    }
    out
}

fn dispatch_simple<F, Fut>(
    hosts: &HashMap<String, ManagedHost>,
    emitter: &EventEmitter,
    host_id: String,
    context: &'static str,
    f: F,
) where
    F: FnOnce(ControlClient) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), rex_proto::ProtoError>> + Send + 'static,
{
    if let Some(client) = hosts.get(&host_id).map(|h| h.client.clone()) {
        let em = emitter.clone();
        tokio::spawn(async move {
            if let Err(err) = f(client).await {
                emit_rpc_error(&em, host_id, context, &err);
            }
        });
    }
}

fn spawn_host_dial(
    host_id: String,
    endpoint: RexEndpoint,
    preferred_session_id: Option<String>,
    client_label: Option<String>,
    keymap_req: Arc<ClientKeymapRequest>,
    emitter: EventEmitter,
    internal_tx: mpsc::UnboundedSender<InternalMsg>,
) {
    tokio::spawn(async move {
        let mut cfg = ControlConfig::new(endpoint);
        cfg.client_info.label = client_label;

        match ControlClient::connect(cfg).await {
            Ok((client, welcome, mut notifications)) => {
                let attach_state = Arc::new(HostAttachState::default());
                let ev_client = client.clone();
                let ev_host_id = host_id.clone();
                let ev_emitter = emitter.clone();
                let ev_keymap_req = Arc::clone(&keymap_req);
                let ev_pref_sess = preferred_session_id.clone();
                let ev_attach_state = Arc::clone(&attach_state);

                let event_task = tokio::spawn(async move {
                    let mut initial_connected_seen = false;
                    while let Some(note) = notifications.recv().await {
                        match note {
                            ControlNotification::Connected {
                                client_id,
                                resumed,
                                api_revision,
                                ..
                            } => {
                                ev_emitter.emit(RuntimeEvent::HostConnected {
                                    host_id: ev_host_id.clone(),
                                    client_id,
                                    resumed,
                                    api_revision,
                                });
                                if !initial_connected_seen || !resumed {
                                    initial_connected_seen = true;
                                    ev_attach_state.clear_all();
                                    run_host_bootstrap(
                                        ev_client.clone(),
                                        ev_host_id.clone(),
                                        ev_pref_sess.clone(),
                                        Arc::clone(&ev_keymap_req),
                                        Arc::clone(&ev_attach_state),
                                        ev_emitter.clone(),
                                    )
                                    .await;
                                }
                            }
                            ControlNotification::Disconnected {
                                will_reconnect,
                                reason,
                            } => {
                                ev_emitter.emit(RuntimeEvent::HostDisconnected {
                                    host_id: ev_host_id.clone(),
                                    will_reconnect,
                                    reason,
                                });
                            }
                            ControlNotification::Event { seq, event } => {
                                if let ServerEvent::SessionDestroyed { ref session_id } = *event {
                                    ev_attach_state.clear_session(session_id);
                                }
                                forward_server_event(&ev_emitter, &ev_host_id, seq, *event);
                            }
                        }
                    }
                });

                tracing::info!(
                    host_id = %host_id,
                    client_id = %welcome.client.id,
                    api_revision = welcome.api_revision,
                    "connected to Rex host"
                );
                let _ = internal_tx.send(InternalMsg::HostEstablished {
                    host_id,
                    client: Box::new(client),
                    attach_state,
                    event_task,
                });
            }
            Err(err) => {
                emitter.emit(RuntimeEvent::HostError {
                    host_id,
                    context: "connect".into(),
                    message: err.to_string(),
                });
            }
        }
    });
}

async fn run_host_bootstrap(
    client: ControlClient,
    host_id: String,
    preferred_session_id: Option<String>,
    keymap_req: Arc<ClientKeymapRequest>,
    attach_state: Arc<HostAttachState>,
    emitter: EventEmitter,
) {
    let boot_req = SessionBootstrapRequest {
        event_scope: "all".into(),
        preferred_session_id,
        attach_session_ids: None,
        data_tickets: Some(vec![DataTicketSpec::terminal_v1()]),
    };

    let (boot_res, km_res, hosts_res, clients_res) = tokio::join!(
        client.bootstrap(&boot_req),
        client.client_keymap(&keymap_req),
        client.hosts_list(),
        client.client_list(),
    );

    if let Ok((clients, _)) = clients_res {
        emitter.emit(RuntimeEvent::ClientsListed {
            host_id: host_id.clone(),
            clients,
        });
    }

    match boot_res {
        Ok((bootstrap, seq)) => {
            if let Some(sel) = &bootstrap.selected_session {
                attach_state.mark_attached(&sel.session.session_id);
            }
            for att in &bootstrap.attached_sessions {
                attach_state.mark_attached(&att.session.session_id);
            }
            emitter.emit(RuntimeEvent::BootstrapReady {
                host_id: host_id.clone(),
                bootstrap: Box::new(bootstrap),
                seq,
            });
        }
        Err(err) => emit_rpc_error(&emitter, host_id.clone(), "session.bootstrap", &err),
    }

    match km_res {
        Ok((keymap, seq)) => emitter.emit(RuntimeEvent::KeymapUpdated {
            host_id: host_id.clone(),
            keymap: Box::new(keymap),
            seq,
        }),
        Err(err) => emit_rpc_error(&emitter, host_id.clone(), "client.keymap", &err),
    }

    if let Ok((hosts, _)) = hosts_res {
        emitter.emit(RuntimeEvent::HostsListUpdated { host_id, hosts });
    }
}

fn forward_server_event(emitter: &EventEmitter, host_id: &str, seq: u64, event: ServerEvent) {
    match event {
        ServerEvent::ClientPromptRequested(request) => {
            emitter.emit(RuntimeEvent::PromptRequested {
                host_id: host_id.to_owned(),
                seq,
                request: Box::new(request),
            });
        }
        ServerEvent::ClientPromptEnded(ev) => {
            emitter.emit(RuntimeEvent::PromptEnded {
                host_id: host_id.to_owned(),
                seq,
                event: ev,
            });
        }
        ServerEvent::ClientPerformRequested(request) => {
            emitter.emit(RuntimeEvent::PerformRequested {
                host_id: host_id.to_owned(),
                seq,
                request: Box::new(request),
            });
        }
        ServerEvent::ClientCallRequested(request) => {
            emitter.emit(RuntimeEvent::CallRequested {
                host_id: host_id.to_owned(),
                seq,
                request,
            });
        }
        other => {
            emitter.emit(RuntimeEvent::ControlEvent {
                host_id: host_id.to_owned(),
                seq,
                event: Box::new(other),
            });
        }
    }
}

async fn run_block_stream_task(
    control: ControlClient,
    attach_state: Arc<HostAttachState>,
    key: BlockKey,
    initial_ticket: Option<String>,
    mut cancel_rx: oneshot::Receiver<()>,
    emitter: EventEmitter,
) {
    let emit_stream = |kind: DataStreamEventKind| {
        emitter.emit(RuntimeEvent::DataStream(DataStreamEvent {
            host_id: key.host_id.clone(),
            session_id: key.session_id.clone(),
            block_id: key.block_id.clone(),
            kind,
        }));
    };

    // Ensure the control socket has attached to `key.session_id` before issuing `data.connect`,
    // preventing the Rex `409 conflict` race when switching to a cold session or opening a remote tab.
    if initial_ticket.is_none()
        && let Err(err) = attach_state
            .ensure_attached(&control, &key.session_id, false)
            .await
    {
        if err.is_not_found() {
            emit_stream(DataStreamEventKind::Closed {
                action: DataCloseAction::BlockClosed,
            });
        } else {
            emit_stream(DataStreamEventKind::Error {
                message: err.to_string(),
            });
        }
        return;
    }

    let connect_res = match initial_ticket {
        Some(ticket) => {
            TerminalDataStream::connect_with_ticket(
                control.endpoint().clone(),
                key.session_id.clone(),
                key.block_id.clone(),
                "terminal",
                &ticket,
            )
            .await
        }
        None => TerminalDataStream::connect(&control, &key.session_id, &key.block_id).await,
    };

    // Defense-in-depth: if `data.connect` still hits `409 conflict` (e.g. after a transparent
    // control reconnect where server-side attachments reset), force-attach and retry once.
    let connect_res = match connect_res {
        Err(ref err) if err.is_conflict() => {
            tracing::debug!(
                host = %key.host_id,
                session = %key.session_id,
                block = %key.block_id,
                "data.connect hit 409 conflict; attaching session and retrying once"
            );
            match attach_state
                .ensure_attached(&control, &key.session_id, true)
                .await
            {
                Ok(_) => {
                    TerminalDataStream::connect(&control, &key.session_id, &key.block_id).await
                }
                Err(attach_err) => Err(attach_err),
            }
        }
        other => other,
    };

    let mut stream = match connect_res {
        Ok(s) => s,
        Err(err) if err.is_not_found() => {
            tracing::debug!(
                host = %key.host_id,
                session = %key.session_id,
                block = %key.block_id,
                error = %err,
                "data.connect target block/session no longer exists"
            );
            emit_stream(DataStreamEventKind::Closed {
                action: DataCloseAction::BlockClosed,
            });
            return;
        }
        Err(err) => {
            emit_stream(DataStreamEventKind::Error {
                message: err.to_string(),
            });
            return;
        }
    };

    emit_stream(DataStreamEventKind::Opened {
        welcome: stream.welcome(),
    });

    let mut scanner = GhostSnpBoundaryScanner::new();
    let mut apc_framer = RexApcFramer::new(stream.channel_nonce());

    loop {
        tokio::select! {
            biased;

            _ = &mut cancel_rx => {
                let _ = stream.close_clean().await;
                emit_stream(DataStreamEventKind::Closed {
                    action: DataCloseAction::StreamEnded,
                });
                return;
            }

            res = stream.next_chunk() => {
                match res {
                    Ok(TerminalStreamChunk::Bytes(chunk)) => {
                        let bytes_received = stream.bytes_received();
                        if !scanner.is_finished() {
                            let (snap, live, finished_bytes) = scanner.scan_split(&chunk);
                            if !snap.is_empty() {
                                emit_stream(DataStreamEventKind::SnapshotChunk {
                                    bytes: chunk.slice_ref(snap),
                                    bytes_received,
                                });
                            }
                            if let Some(snapshot_bytes) = finished_bytes {
                                emit_stream(DataStreamEventKind::SnapshotComplete { snapshot_bytes });
                            }
                            if !live.is_empty() {
                                dispatch_framed_live(&mut apc_framer, live, bytes_received, &emit_stream);
                            }
                        } else {
                            dispatch_framed_live(&mut apc_framer, &chunk, bytes_received, &emit_stream);
                        }
                    }
                    Ok(TerminalStreamChunk::Resumed { bytes_received, welcome }) => {
                        emit_stream(DataStreamEventKind::Resumed { bytes_received, welcome });
                    }
                    Ok(TerminalStreamChunk::Closed(action)) => {
                        match action {
                            DataCloseAction::BlockClosed
                            | DataCloseAction::StreamEnded
                            | DataCloseAction::ProviderUnavailable => {
                                emit_stream(DataStreamEventKind::Closed { action });
                                return;
                            }
                            DataCloseAction::Resume { .. }
                            | DataCloseAction::ReconnectFreshSnapshot
                            | DataCloseAction::ProviderFailed
                            | DataCloseAction::ProviderCanceled
                            | DataCloseAction::HandshakeRejected { .. } => {
                                // Recover seamlessly via byte-counted RESUME or fresh ticket fallback.
                                let mut backoff = Duration::from_millis(150);
                                let mut recovered = false;
                                for _attempt in 0..6 {
                                    tokio::select! {
                                        biased;
                                        _ = &mut cancel_rx => {
                                            let _ = stream.close_clean().await;
                                            emit_stream(DataStreamEventKind::Closed {
                                                action: DataCloseAction::StreamEnded,
                                            });
                                            return;
                                        }
                                        _ = tokio::time::sleep(backoff) => {}
                                    }

                                    match stream.recover_or_reopen(&control).await {
                                        Ok(RecoveryOutcome::ResumedInPlace { bytes_received, welcome }) => {
                                            emit_stream(DataStreamEventKind::Resumed {
                                                bytes_received,
                                                welcome,
                                            });
                                            recovered = true;
                                            break;
                                        }
                                        Ok(RecoveryOutcome::ReopenedFreshSnapshot { welcome }) => {
                                            scanner = GhostSnpBoundaryScanner::new();
                                            apc_framer = RexApcFramer::new(welcome.channel_nonce);
                                            emit_stream(DataStreamEventKind::ReopenedFreshSnapshot {
                                                welcome,
                                            });
                                            recovered = true;
                                            break;
                                        }
                                        Err(err) if err.is_not_found() => {
                                            tracing::debug!(
                                                block_id = %key.block_id,
                                                error = %err,
                                                "data stream target no longer exists during recovery; stopping retries"
                                            );
                                            emit_stream(DataStreamEventKind::Closed {
                                                action: DataCloseAction::BlockClosed,
                                            });
                                            return;
                                        }
                                        Err(err) if err.is_conflict() => {
                                            tracing::debug!(
                                                block_id = %key.block_id,
                                                error = %err,
                                                "data stream recovery hit 409 conflict; re-attaching session"
                                            );
                                            let _ = attach_state
                                                .ensure_attached(&control, &key.session_id, true)
                                                .await;
                                            backoff = (backoff * 2).min(Duration::from_secs(2));
                                        }
                                        Err(err) => {
                                            tracing::debug!(
                                                block_id = %key.block_id,
                                                error = %err,
                                                "data stream recovery attempt failed"
                                            );
                                            backoff = (backoff * 2).min(Duration::from_secs(2));
                                        }
                                    }
                                }
                                if !recovered {
                                    emit_stream(DataStreamEventKind::Closed { action });
                                    return;
                                }
                            }
                        }
                    }
                    Err(err) => {
                        emit_stream(DataStreamEventKind::Error {
                            message: err.to_string(),
                        });
                        return;
                    }
                }
            }
        }
    }
}

fn dispatch_framed_live<F>(framer: &mut RexApcFramer, bytes: &[u8], bytes_received: u64, emit: &F)
where
    F: Fn(DataStreamEventKind),
{
    for item in framer.feed(bytes) {
        match item {
            FramedChunk::VtBytes(vt) => {
                emit(DataStreamEventKind::LiveVtChunk {
                    bytes: vt,
                    bytes_received,
                });
            }
            FramedChunk::Control(ctrl) => {
                emit(DataStreamEventKind::RexApc(ctrl));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use super::*;

    fn make_synthetic_ghostsnp_stream(nonce: u64) -> (Vec<u8>, usize) {
        let mut buf = Vec::new();
        // Header: GHOSTSNP v1 (10 bytes)
        buf.extend_from_slice(b"GHOSTSNP");
        buf.extend_from_slice(&1u16.to_le_bytes());

        // Record 1: TERMINAL (tag=1, payload=4 bytes)
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&4u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&[1, 2, 3, 4]);

        // Record 2: FINISH (tag=6, payload=0 bytes)
        buf.extend_from_slice(&6u16.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());

        let snapshot_len = buf.len();
        // Post-snapshot authenticated resize APC + plain VT bytes
        let apc = format!(
            "\x1b_rex1;{nonce};{{\"type\":\"resize\",\"cols\":120,\"rows\":40,\"cell_width_px\":8,\"cell_height_px\":16,\"owner\":null}}\x1b\\"
        );
        buf.extend_from_slice(apc.as_bytes());
        buf.extend_from_slice(b"prompt$ ");
        (buf, snapshot_len)
    }

    #[test]
    fn ghostsnp_boundary_scanner_splits_snapshot_and_verifies_trailing_rex_apcs() {
        let nonce = 777_888_999_u64;
        let (raw, expected_snap_len) = make_synthetic_ghostsnp_stream(nonce);

        for chunk_size in [1usize, 2, 5, 17, raw.len()] {
            let mut scanner = GhostSnpBoundaryScanner::new();
            let mut framer = RexApcFramer::new(nonce);
            let mut snap_out = Vec::new();
            let mut vt_out = Vec::new();
            let mut apcs = Vec::new();
            let mut finished_at = None;

            for chunk in raw.chunks(chunk_size) {
                let (snap, live, fin) = scanner.scan_split(chunk);
                snap_out.extend_from_slice(snap);
                if let Some(n) = fin {
                    finished_at = Some(n);
                }
                if !live.is_empty() {
                    for item in framer.feed(live) {
                        match item {
                            FramedChunk::VtBytes(b) => vt_out.extend_from_slice(&b),
                            FramedChunk::Control(c) => apcs.push(c),
                        }
                    }
                }
            }

            assert_eq!(snap_out.len(), expected_snap_len, "chunk_size={chunk_size}");
            assert_eq!(
                finished_at,
                Some(expected_snap_len as u64),
                "chunk_size={chunk_size}"
            );
            assert_eq!(vt_out, b"prompt$ ", "chunk_size={chunk_size}");
            assert_eq!(
                apcs,
                vec![RexControlMessage::Resize {
                    cols: 120,
                    rows: 40,
                    cell_width_px: 8,
                    cell_height_px: 16,
                    owner: None,
                }],
                "chunk_size={chunk_size}"
            );
        }
    }

    /// Strictly read-only live integration test against local `/run/user/1000/rex/server.sock`.
    ///
    /// Skips cleanly if local Rex server socket is absent. Never creates, modifies, focuses,
    /// owns, or closes any session/window/block.
    #[tokio::test]
    async fn live_host_manager_receives_bootstrap_keymap_and_data_stream() {
        let sock = match RexEndpoint::discover_default() {
            RexEndpoint::Unix(p) if p.exists() => p,
            _ => {
                let fallback = Path::new("/run/user/1000/rex/server.sock");
                if fallback.exists() {
                    fallback.to_path_buf()
                } else {
                    eprintln!("skipping live HostManager test: local server.sock absent");
                    return;
                }
            }
        };

        let cfg = HostManagerConfig {
            local_endpoint: Some(RexEndpoint::Unix(sock)),
            saved_hosts: Vec::new(),
            preferred_local_session_id: None,
            client_label: Some("omarex-hostmgr-readonly-test".into()),
            initial_keymap_request: ClientKeymapRequest::default(),
            start_theme_watchers: false,
            start_notifier: false,
            material: MaterialStyle::Classic,
            wake_ui: None,
        };

        let (mgr, mut events) = HostManager::spawn(cfg);

        let mut saw_connected = false;
        let mut saw_bootstrap = false;
        let mut saw_keymap = false;
        let mut t3_session_id: Option<String> = None;
        let mut t3_window_count: Option<usize> = None;
        let mut opened_stream_block: Option<(String, String)> = None;
        let mut saw_opened = false;
        let mut saw_snapshot_magic = false;
        let mut saw_snapshot_complete = false;
        let mut saw_resize_apc = false;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let Ok(Some(ev)) = tokio::time::timeout(remaining, events.recv()).await else {
                break;
            };
            match ev {
                RuntimeEvent::HostConnected {
                    host_id,
                    api_revision,
                    ..
                } => {
                    assert_eq!(host_id, LOCAL_HOST_ID);
                    assert_eq!(api_revision, rex_proto::TARGET_API_REVISION);
                    saw_connected = true;
                }
                RuntimeEvent::BootstrapReady {
                    host_id, bootstrap, ..
                } => {
                    assert_eq!(host_id, LOCAL_HOST_ID);
                    assert!(bootstrap.server_status.pid > 0);
                    saw_bootstrap = true;

                    // Verify live "T3 Code" session & its SessionView if present on this machine.
                    if let Some(t3) = bootstrap
                        .attach_all
                        .sessions
                        .iter()
                        .find(|s| s.label == "T3 Code")
                    {
                        t3_session_id = Some(t3.session_id.clone());
                        if let Some(sel) = &bootstrap.selected_session
                            && sel.session.session_id == t3.session_id
                        {
                            t3_window_count = Some(sel.view.windows.len());
                        } else {
                            mgr.send(HostCommand::FetchView {
                                host_id: LOCAL_HOST_ID.into(),
                                session_id: t3.session_id.clone(),
                            })
                            .expect("enqueue FetchView for T3 Code");
                        }
                    }

                    // If a live terminal ticket was issued, open a read-only data stream for it.
                    if let Some(grant) = bootstrap.data_tickets.first() {
                        opened_stream_block =
                            Some((grant.session_id.clone(), grant.block_id.clone()));
                        mgr.send(HostCommand::OpenBlockStream {
                            host_id: LOCAL_HOST_ID.into(),
                            session_id: grant.session_id.clone(),
                            block_id: grant.block_id.clone(),
                            ticket: Some(grant.ticket.clone()),
                        })
                        .expect("enqueue OpenBlockStream");
                    }
                }
                RuntimeEvent::SessionViewUpdated {
                    host_id,
                    session_id,
                    view,
                    ..
                } => {
                    assert_eq!(host_id, LOCAL_HOST_ID);
                    if t3_session_id.as_deref() == Some(session_id.as_str()) {
                        t3_window_count = Some(view.windows.len());
                    }
                }
                RuntimeEvent::KeymapUpdated {
                    host_id, keymap, ..
                } => {
                    assert_eq!(host_id, LOCAL_HOST_ID);
                    assert!(!keymap.config.instance_id.is_empty());
                    saw_keymap = true;
                }
                RuntimeEvent::DataStream(ds) => match ds.kind {
                    DataStreamEventKind::Opened { welcome } => {
                        assert_ne!(welcome.channel_nonce, 0);
                        saw_opened = true;
                    }
                    DataStreamEventKind::SnapshotChunk { bytes, .. } => {
                        if !saw_snapshot_magic && bytes.len() >= 8 {
                            assert_eq!(&bytes[..8], b"GHOSTSNP");
                            saw_snapshot_magic = true;
                        }
                    }
                    DataStreamEventKind::SnapshotComplete { snapshot_bytes } => {
                        assert!(snapshot_bytes >= 10);
                        saw_snapshot_complete = true;
                    }
                    DataStreamEventKind::RexApc(RexControlMessage::Resize {
                        cols, rows, ..
                    }) => {
                        assert!(cols > 0 && rows > 0);
                        saw_resize_apc = true;
                    }
                    DataStreamEventKind::Error { message } => {
                        panic!("unexpected data stream error: {message}");
                    }
                    _ => {}
                },
                RuntimeEvent::HostError {
                    context, message, ..
                } => {
                    panic!("unexpected host error during {context}: {message}");
                }
                _ => {}
            }

            let t3_done = t3_session_id.is_none() || t3_window_count.is_some();
            let stream_done = opened_stream_block.is_none()
                || (saw_opened && saw_snapshot_magic && saw_snapshot_complete && saw_resize_apc);
            if saw_connected && saw_bootstrap && saw_keymap && t3_done && stream_done {
                break;
            }
        }

        assert!(saw_connected, "expected HostConnected");
        assert!(saw_bootstrap, "expected BootstrapReady");
        assert!(saw_keymap, "expected KeymapUpdated");
        if t3_session_id.is_some() {
            let count = t3_window_count.expect("expected T3 Code SessionView");
            assert!(count > 0, "expected active windows in live T3 Code session");
            eprintln!("live HostManager verified T3 Code SessionView with {count} windows");
        }
        if let Some((session_id, block_id)) = opened_stream_block {
            assert!(saw_opened, "expected DataStream Opened");
            assert!(saw_snapshot_magic, "expected GHOSTSNP magic");
            assert!(saw_snapshot_complete, "expected SnapshotComplete");
            assert!(saw_resize_apc, "expected authenticated rex1 resize APC");
            let _ = mgr.send(HostCommand::CloseBlockStream {
                host_id: LOCAL_HOST_ID.into(),
                session_id,
                block_id,
            });
        }

        mgr.shutdown();
    }

    #[test]
    fn rpc_error_classifier_suppresses_lifecycle_races_and_keeps_actionable_errors() {
        use rex_proto::{ProtoError, RpcError};

        let sess_404 = ProtoError::Rpc(RpcError {
            code: "session_not_found".into(),
            message: "session session:dead not found".into(),
            status: 404,
        });
        let block_404 = ProtoError::Rpc(RpcError {
            code: "not_found".into(),
            message: "block not found".into(),
            status: 404,
        });
        let bad_400 = ProtoError::Rpc(RpcError {
            code: "bad_request".into(),
            message: "not attached".into(),
            status: 400,
        });
        let conflict_409 = ProtoError::Rpc(RpcError {
            code: "conflict".into(),
            message: "client not attached".into(),
            status: 409,
        });
        let internal_500 = ProtoError::Rpc(RpcError {
            code: "internal".into(),
            message: "server panic".into(),
            status: 500,
        });
        let closed = ProtoError::Closed("socket reset".into());
        let timeout = ProtoError::Timeout("timed out".into());

        // Expected 404 teardown / shared-prompt races are suppressed (debug-logged only).
        for ctx in [
            "session.view",
            "session.attach",
            "com.superlogical.terminal.write",
            "com.superlogical.terminal.resize",
            "com.superlogical.terminal.set_theme",
            "block.event",
            "block.close",
            "session.close_window",
            "session.destroy",
            "session.focus_window",
            "session.focus_block",
            "client.reply",
        ] {
            assert!(
                !should_surface_rpc_error(ctx, &sess_404),
                "{ctx} 404 session_not_found should not toast"
            );
            assert!(
                !should_surface_rpc_error(ctx, &block_404),
                "{ctx} 404 not_found should not toast"
            );
            assert!(
                should_surface_rpc_error(ctx, &internal_500),
                "{ctx} 500 internal must still surface"
            );
        }

        // Ownership races (400/404/409) are suppressed, while 500 still surfaces.
        for ctx in ["session.own_block", "session.disown_block"] {
            assert!(!should_surface_rpc_error(ctx, &sess_404));
            assert!(!should_surface_rpc_error(ctx, &bad_400));
            assert!(!should_surface_rpc_error(ctx, &conflict_409));
            assert!(should_surface_rpc_error(ctx, &internal_500));
        }

        // Transient disconnects do not toast for background/RPC commands, but DO surface on connect.
        assert!(!should_surface_rpc_error("session.view", &closed));
        assert!(!should_surface_rpc_error("session.bootstrap", &timeout));
        assert!(should_surface_rpc_error("connect", &closed));
        assert!(should_surface_rpc_error("connect", &timeout));

        // Explicit user creation/action failures still surface.
        assert!(should_surface_rpc_error("session.create", &bad_400));
        assert!(should_surface_rpc_error("client.action(t3_new)", &bad_400));
    }

    #[tokio::test]
    async fn host_attach_state_tracks_sessions_and_wakes_in_flight_waiters() {
        let state = Arc::new(HostAttachState::default());
        assert!(!state.is_attached("s1"));

        // Register an in-flight attach manually and verify `wait_if_attaching` blocks until resolved.
        {
            let mut g = state.inner.lock().unwrap();
            g.in_flight.insert("s1".into(), Arc::new(Notify::new()));
        }

        let waiter_state = Arc::clone(&state);
        let waiter = tokio::spawn(async move {
            waiter_state.wait_if_attaching("s1").await;
            waiter_state.is_attached("s1")
        });

        tokio::task::yield_now().await;
        state.mark_attached("s1");

        let attached_after_wake = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter should wake immediately on mark_attached")
            .expect("waiter join");
        assert!(attached_after_wake);

        state.clear_session("s1");
        assert!(!state.is_attached("s1"));

        state.mark_attached("s1");
        state.mark_attached("s2");
        state.clear_all();
        assert!(!state.is_attached("s1"));
        assert!(!state.is_attached("s2"));
    }
}
