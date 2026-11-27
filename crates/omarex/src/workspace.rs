//! Multi-host Workspace state, bounded LRU warm-session cache, revision deduplication,
//! multi-device ownership policy, READY-before-resize gating, and theme wire bridge.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use omarchy_theme::{AppearanceMode, ThemePalette};
use rex_proto::RexEndpoint;
use rex_proto::types::{
    BlockEventPayload, OwnMode, PortEntry, PortProcessEntry, PortScanStatus, ProgramStatusRecord,
    SessionBootstrapResponse, SessionEvent, SessionSummary, SessionView, TailscaleStatus,
    TerminalPortsResponse, TerminalResizeArgs, TerminalSetThemeArgs, ThemeScheme,
};

/// Adaptive resize coalescing window (snappy single-frame settle while coalescing drags).
pub const DEFAULT_RESIZE_COALESCE: Duration = Duration::from_millis(16);

/// Upper bound on continuous drag resize deferral so resizing feels live rather than frozen.
pub const MAX_RESIZE_DEFER: Duration = Duration::from_millis(36);

/// `true` when a client other than `me` owns the block.
///
/// The owner's size is then authoritative: Rex only records our resize as *desired* and applies
/// it once ownership moves to us. A server size that differs from our grid is expected in that
/// state and must not trigger resize retries — otherwise two devices on the same tab fight.
fn owned_by_other(owner: Option<&str>, me: Option<&str>) -> bool {
    owner.is_some_and(|o| me != Some(o))
}

/// Connection lifecycle state of a Rex host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostStatus {
    Disconnected,
    Connecting,
    Connected {
        client_id: String,
        server_host_id: Option<String>,
        api_revision: u32,
        resumed: bool,
    },
    Reconnecting {
        reason: String,
    },
    Error(String),
}

/// Unique key identifying a session on a specific Rex host.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub host_id: String,
    pub session_id: String,
}

impl SessionKey {
    pub fn new(host_id: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            host_id: host_id.into(),
            session_id: session_id.into(),
        }
    }
}

/// Unique key identifying a terminal/content block on a specific Rex host.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlockKey {
    pub host_id: String,
    pub session_id: String,
    pub block_id: String,
}

impl BlockKey {
    pub fn new(
        host_id: impl Into<String>,
        session_id: impl Into<String>,
        block_id: impl Into<String>,
    ) -> Self {
        Self {
            host_id: host_id.into(),
            session_id: session_id.into(),
            block_id: block_id.into(),
        }
    }
}

/// Batched per-block metadata fetched off-thread in a single coalesced hydration pass.
#[derive(Debug, Clone, Default)]
pub struct HydratedBlockMeta {
    pub block_id: String,
    pub program_statuses: Vec<ProgramStatusRecord>,
    pub pwd: Option<String>,
    pub foreground_process: Option<String>,
    pub title: Option<String>,
}

/// Per-block runtime metadata tracked independently of `SessionView` rebuilds.
#[derive(Debug, Clone, Default)]
pub struct BlockRuntimeState {
    pub owner: Option<String>,
    pub title: Option<String>,
    pub pwd: Option<String>,
    pub foreground_process: Option<String>,
    pub program_statuses: Vec<ProgramStatusRecord>,
    pub exited_code: Option<i32>,
    pub has_bell: bool,
    /// `true` once `GHOSTSNP` reached the `READY` record and the pane is safe to resize.
    pub snapshot_ready: bool,
    /// Pending resize deferred until `snapshot_ready` becomes `true` or coalesce window elapses.
    pub pending_resize: Option<CoalescedResize>,
    /// Last resize successfully dispatched to Rex (avoids duplicate resize RPCs when server matches).
    pub last_sent_resize: Option<TerminalResizeArgs>,
    /// Authoritative `(columns, rows)` last reported by Rex via `RexApc::Resize` or `size_changed`.
    pub last_server_size: Option<(u16, u16)>,
    /// Live listening TCP ports reported by `terminal.ports` / `ports_changed`.
    pub ports_status: Option<PortScanStatus>,
    pub listening_ports: Vec<PortEntry>,
    pub port_processes: HashMap<i64, PortProcessEntry>,
}

impl BlockRuntimeState {
    pub fn apply_ports_response(&mut self, resp: TerminalPortsResponse) {
        self.ports_status = Some(resp.status);
        let mut ports: Vec<PortEntry> = resp
            .ports
            .into_iter()
            .filter(|p| {
                p.protocol.eq_ignore_ascii_case("tcp") || p.protocol.eq_ignore_ascii_case("tcp6")
            })
            .collect();
        ports.sort_by(|a, b| a.port.cmp(&b.port).then_with(|| a.address.cmp(&b.address)));
        // Deduplicate same port on multiple wildcard/loopback addresses so header pills stay clean,
        // preferring 127.0.0.1 / ::1 / 0.0.0.0.
        ports.dedup_by(|a, b| a.port == b.port);
        self.listening_ports = ports;
        self.port_processes.clear();
        for proc_entry in resp.processes {
            self.port_processes.insert(proc_entry.pid, proc_entry);
        }
    }

    pub fn port_process_label(&self, port: &PortEntry) -> Option<String> {
        port.pid
            .and_then(|pid| self.port_processes.get(&pid))
            .map(|p| p.display_label().to_owned())
    }
}

/// Normalizes a listening bind address (`0.0.0.0` / `::`) to a dialable loopback host.
pub fn normalize_tunnel_host(addr: &str) -> String {
    let trimmed = addr.trim().trim_start_matches('[').trim_end_matches(']');
    match trimmed {
        "" | "0.0.0.0" => "127.0.0.1".to_owned(),
        "::" => "::1".to_owned(),
        other => other.to_owned(),
    }
}

/// Formats a local browser HTTP URL for `(host, port)`, bracketing IPv6 literals and
/// RFC 6874-escaping `%` zone delimiters as `%25` without altering raw tunnel dial hosts.
pub fn format_port_http_url(host: &str, port: u16) -> String {
    let clean = host.trim().trim_start_matches('[').trim_end_matches(']');
    if clean.contains(':') {
        let escaped = clean.replace('%', "%25");
        format!("http://[{escaped}]:{port}")
    } else {
        format!("http://{clean}:{port}")
    }
}

/// Pending debounced terminal resize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoalescedResize {
    pub args: TerminalResizeArgs,
    pub requested_at: Instant,
    pub first_requested_at: Instant,
    pub immediate: bool,
}

/// Warmth level of a tracked session (prevents `orex`'s bug of tearing down panes on switch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionWarmth {
    /// Metadata listed from `session.list` / `bootstrap`, not attached to data streams.
    Cold,
    /// Attached & kept warm in the bounded LRU cache (streams stay alive/disowned in background).
    WarmBackground,
    /// Currently displayed in the active viewport.
    Active,
}

/// Complete tracked state for one session on a host.
#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub session_id: String,
    pub label: String,
    pub warmth: SessionWarmth,
    /// Authoritative `SessionView` (preserved even if `windows` is temporarily empty!).
    pub view: Option<SessionView>,
    /// Highest applied view revision (gates out-of-order `session_view_changed`).
    pub applied_revision: u64,
    /// Highest `session_view_changed` revision observed from server events.
    pub pending_revision: u64,
    /// Whether a `session.view` RPC is currently in flight (prevents duplicate fetches!).
    pub view_fetch_in_flight: bool,
    /// Per-block runtime metadata (`owner`, OSC 7501 status, `snapshot_ready`, coalesced resize).
    pub blocks: HashMap<String, BlockRuntimeState>,
}

impl SessionEntry {
    pub fn new(summary: SessionSummary) -> Self {
        Self {
            session_id: summary.session_id,
            label: summary.label,
            warmth: SessionWarmth::Cold,
            view: None,
            applied_revision: 0,
            pending_revision: 0,
            view_fetch_in_flight: false,
            blocks: HashMap::new(),
        }
    }

    /// Returns `true` if `session_view_changed(revision)` requires a new `session.view` fetch.
    ///
    /// Guarantees:
    /// - Strictly monotonic: out-of-order lower/equal revisions are ignored.
    /// - Single-flight: never launches duplicate concurrent `session.view` RPCs for the same session.
    pub fn note_view_changed(&mut self, revision: u64) -> bool {
        if revision <= self.applied_revision || revision <= self.pending_revision {
            return false;
        }
        self.pending_revision = revision;
        if self.view_fetch_in_flight {
            false
        } else {
            self.view_fetch_in_flight = true;
            true
        }
    }

    /// Applies an authoritative `SessionView` snapshot.
    ///
    /// **CRITICAL (`orex` B7 fix)**: NEVER destroys a session when `view.windows` is empty —
    /// empty windows during `tr3x` sync or tab close simply render an empty state.
    ///
    /// Returns `true` if another `session.view` fetch is needed immediately because a higher
    /// `session_view_changed` revision arrived while this fetch was in flight.
    pub fn apply_view(&mut self, view: SessionView) -> bool {
        self.view_fetch_in_flight = false;
        if view.revision < self.applied_revision {
            // Stale out-of-order response; check if we still need to catch up.
            return self.maybe_schedule_catchup_fetch();
        }

        self.applied_revision = view.revision;
        if !view.label.is_empty() {
            self.label = view.label.clone();
        }

        // Reconcile block ownership/presence without discarding live OSC 7501 / snapshot state.
        let mut live_blocks = HashSet::new();
        for win in &view.windows {
            for layer in &win.layers {
                for blk in &layer.blocks {
                    if let Some(id) = &blk.block_id {
                        live_blocks.insert(id.clone());
                        let entry = self.blocks.entry(id.clone()).or_default();
                        entry.owner = blk.owner.clone();
                    }
                }
            }
        }
        self.blocks.retain(|id, _| live_blocks.contains(id));
        self.view = Some(view);

        self.maybe_schedule_catchup_fetch()
    }

    /// Resets `view_fetch_in_flight` after a failed `session.view` RPC so subsequent
    /// `session_view_changed` events are never permanently wedged.
    pub fn clear_view_fetch_in_flight(&mut self) {
        self.view_fetch_in_flight = false;
    }

    fn maybe_schedule_catchup_fetch(&mut self) -> bool {
        if self.pending_revision > self.applied_revision && !self.view_fetch_in_flight {
            self.view_fetch_in_flight = true;
            true
        } else {
            false
        }
    }

    /// Returns `true` if `view` exists and currently has zero windows (for empty-state UI).
    #[cfg(test)]
    pub fn is_empty_view(&self) -> bool {
        self.view.as_ref().is_some_and(|v| v.windows.is_empty())
    }

    /// Collect all block IDs currently placed in `view`.
    pub fn all_block_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        if let Some(view) = &self.view {
            for win in &view.windows {
                for layer in &win.layers {
                    for blk in &layer.blocks {
                        if let Some(id) = &blk.block_id {
                            ids.push(id.clone());
                        }
                    }
                }
            }
        }
        ids
    }
}

/// One connected or configured Rex host (local socket, Tailscale HTTPS, or SSH).
#[derive(Debug, Clone)]
pub struct HostEntry {
    pub id: String,
    pub label: String,
    pub endpoint: RexEndpoint,
    pub is_local: bool,
    pub status: HostStatus,
    /// Ordered session IDs matching `session.list` / `session_moved` server order!
    pub session_order: Vec<String>,
    pub sessions: HashMap<String, SessionEntry>,
    pub keymap_hash: Option<String>,
    pub hosts_registry_hash: Option<String>,
    pub tailscale_status: Option<TailscaleStatus>,
    pub dropped_events_count: u64,
    pub reconnect_count: u64,
}

impl HostEntry {
    pub fn new(id: impl Into<String>, label: impl Into<String>, endpoint: RexEndpoint) -> Self {
        let is_local = endpoint.is_local_unix();
        Self {
            id: id.into(),
            label: label.into(),
            endpoint,
            is_local,
            status: HostStatus::Disconnected,
            session_order: Vec::new(),
            sessions: HashMap::new(),
            keymap_hash: None,
            hosts_registry_hash: None,
            tailscale_status: None,
            dropped_events_count: 0,
            reconnect_count: 0,
        }
    }

    pub fn client_id(&self) -> Option<&str> {
        match &self.status {
            HostStatus::Connected { client_id, .. } => Some(client_id.as_str()),
            _ => None,
        }
    }

    pub fn server_host_id(&self) -> Option<&str> {
        match &self.status {
            HostStatus::Connected { server_host_id, .. } => server_host_id.as_deref(),
            _ => None,
        }
    }

    /// Returns `true` if a block's `host_id` matches this local server's `host_id`
    /// (determines local file-path drop vs remote `upload_begin`).
    pub fn is_block_local_to_this_machine(&self, block_host_id: Option<&str>) -> bool {
        if !self.is_local {
            return false;
        }
        match (self.server_host_id(), block_host_id) {
            (Some(local), Some(blk)) => local == blk,
            _ => false,
        }
    }

    /// Apply a `session.bootstrap` snapshot cleanly in one pass.
    pub fn apply_bootstrap(&mut self, boot: SessionBootstrapResponse, api_revision: u32) {
        self.status = HostStatus::Connected {
            client_id: boot.client_id,
            server_host_id: boot.server_status.host_id,
            api_revision,
            resumed: false,
        };
        self.tailscale_status = boot.server_status.tailscale.clone();

        let mut new_order = Vec::with_capacity(boot.attach_all.sessions.len());
        let mut live_ids = HashSet::with_capacity(boot.attach_all.sessions.len());
        for summary in boot.attach_all.sessions {
            live_ids.insert(summary.session_id.clone());
            new_order.push(summary.session_id.clone());
            self.sessions
                .entry(summary.session_id.clone())
                .and_modify(|e| e.label = summary.label.clone())
                .or_insert_with(|| SessionEntry::new(summary));
        }
        self.session_order = new_order;
        self.sessions.retain(|id, _| live_ids.contains(id));

        if let Some(sel) = boot.selected_session
            && let Some(entry) = self.sessions.get_mut(&sel.session.session_id)
        {
            entry.apply_view(sel.view);
        }
        for att in boot.attached_sessions {
            if let Some(entry) = self.sessions.get_mut(&att.session.session_id) {
                entry.apply_view(att.view);
            }
        }
    }

    /// Upsert a newly created session at the end of `session_order`.
    pub fn on_session_created(&mut self, summary: SessionSummary) {
        if !self.sessions.contains_key(&summary.session_id) {
            self.session_order.push(summary.session_id.clone());
        }
        self.sessions
            .entry(summary.session_id.clone())
            .and_modify(|e| e.label = summary.label.clone())
            .or_insert_with(|| SessionEntry::new(summary));
    }

    /// Remove a server-destroyed session (only invoked upon explicit `session_destroyed` event).
    pub fn on_session_destroyed(&mut self, session_id: &str) {
        self.sessions.remove(session_id);
        self.session_order.retain(|id| id != session_id);
    }

    /// Reorder a session in `session_order` upon `session_moved`.
    pub fn on_session_moved(&mut self, session_id: &str, before_session_id: Option<&str>) {
        let Some(pos) = self.session_order.iter().position(|id| id == session_id) else {
            return;
        };
        let moved = self.session_order.remove(pos);
        match before_session_id
            .and_then(|target| self.session_order.iter().position(|id| id == target))
        {
            Some(idx) => self.session_order.insert(idx, moved),
            None => self.session_order.push(moved),
        }
    }
}

/// Reason a client interaction wants to evaluate block ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipTrigger {
    /// Passive pane display / initial attach: only claim if unowned (`OwnMode::Request`).
    AttachOrView,
    /// User clicked/focused a pane: claim if unowned (`OwnMode::Request`), preserving other device ownership.
    FocusPane,
    /// User typed into pane, dragged a divider/window resize, or invoked explicit `t3_fit`:
    /// forcibly claim (`OwnMode::Take`).
    ExplicitInteractOrFit,
}

/// Ownership RPC command produced by [`OwnershipPolicy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipDecision {
    /// No ownership RPC needed (already owned by us, or owned by another device on passive view).
    NoChange,
    /// Issue `session.own_block` with `mode`.
    Own { mode: OwnMode },
    /// Issue `session.disown_block` (e.g., when backgrounding/minimizing a layer).
    Disown,
}

/// Multi-device ownership policy respecting `tr3x`'s `Fit to This Device` semantics (`orex` B6 fix).
#[derive(Debug, Clone, Copy, Default)]
pub struct OwnershipPolicy {
    /// If `true`, clicking/focusing a pane claims ownership even if another device owns it.
    /// Default is `false` so multi-device setups respect explicit `t3_fit` / interaction.
    pub take_on_passive_focus: bool,
}

impl OwnershipPolicy {
    pub fn decide(
        self,
        my_client_id: Option<&str>,
        current_owner: Option<&str>,
        trigger: OwnershipTrigger,
    ) -> OwnershipDecision {
        let Some(me) = my_client_id else {
            return OwnershipDecision::NoChange;
        };
        if current_owner == Some(me) {
            return OwnershipDecision::NoChange;
        }
        match trigger {
            OwnershipTrigger::AttachOrView => {
                if current_owner.is_none() {
                    OwnershipDecision::Own {
                        mode: OwnMode::Request,
                    }
                } else {
                    OwnershipDecision::NoChange
                }
            }
            OwnershipTrigger::FocusPane => {
                if current_owner.is_none() {
                    OwnershipDecision::Own {
                        mode: OwnMode::Request,
                    }
                } else if self.take_on_passive_focus {
                    OwnershipDecision::Own {
                        mode: OwnMode::Take,
                    }
                } else {
                    OwnershipDecision::NoChange
                }
            }
            OwnershipTrigger::ExplicitInteractOrFit => OwnershipDecision::Own {
                mode: OwnMode::Take,
            },
        }
    }
}

/// Follow-up actions produced when switching the active session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionSwitchPlan {
    /// Newly activated session requires `session.attach` + `data.connect` (was Cold).
    pub needs_attach: bool,
    /// Previously active session moved to `WarmBackground` (kept warm — streams NOT torn down!).
    pub demoted_to_warm: Option<SessionKey>,
    /// Least-recently-used warm sessions evicted to stay within `warm_session_limit`
    /// (caller should cleanly `disown_block` + close streams with WS `1000` for these only).
    pub evicted_sessions: Vec<SessionKey>,
}

/// Top-level multi-host workspace coordinator for `omarex`.
#[derive(Debug)]
pub struct WorkspaceState {
    pub hosts: HashMap<String, HostEntry>,
    pub host_order: Vec<String>,
    pub active_session: Option<SessionKey>,
    /// Sessions currently displayed in any open OS window (protected from warm-LRU eviction).
    pub pinned_sessions: HashSet<SessionKey>,
    /// Bounded LRU queue of warm sessions (most-recently-used at back).
    warm_lru: VecDeque<SessionKey>,
    warm_session_limit: usize,
    pub ownership_policy: OwnershipPolicy,
    pub resize_coalesce: Duration,
}

impl WorkspaceState {
    pub fn new(warm_session_limit: usize) -> Self {
        Self {
            hosts: HashMap::new(),
            host_order: Vec::new(),
            active_session: None,
            pinned_sessions: HashSet::new(),
            warm_lru: VecDeque::new(),
            warm_session_limit: warm_session_limit.max(1),
            ownership_policy: OwnershipPolicy::default(),
            resize_coalesce: DEFAULT_RESIZE_COALESCE,
        }
    }

    pub fn add_host(&mut self, host: HostEntry) {
        if !self.hosts.contains_key(&host.id) {
            self.host_order.push(host.id.clone());
        }
        self.hosts.insert(host.id.clone(), host);
    }

    pub fn host(&self, host_id: &str) -> Option<&HostEntry> {
        self.hosts.get(host_id)
    }

    pub fn host_mut(&mut self, host_id: &str) -> Option<&mut HostEntry> {
        self.hosts.get_mut(host_id)
    }

    pub fn session(&self, key: &SessionKey) -> Option<&SessionEntry> {
        self.hosts.get(&key.host_id)?.sessions.get(&key.session_id)
    }

    pub fn session_mut(&mut self, key: &SessionKey) -> Option<&mut SessionEntry> {
        self.hosts
            .get_mut(&key.host_id)?
            .sessions
            .get_mut(&key.session_id)
    }

    pub fn block(&self, key: &BlockKey) -> Option<&BlockRuntimeState> {
        self.hosts
            .get(&key.host_id)?
            .sessions
            .get(&key.session_id)?
            .blocks
            .get(&key.block_id)
    }

    pub fn block_mut(&mut self, key: &BlockKey) -> Option<&mut BlockRuntimeState> {
        self.hosts
            .get_mut(&key.host_id)?
            .sessions
            .get_mut(&key.session_id)?
            .blocks
            .get_mut(&key.block_id)
    }

    /// Select a session as active while preserving recently visited sessions in the bounded LRU cache.
    ///
    /// Switching away from `"T3 Code"` and back within the LRU window returns `needs_attach = false`
    /// and evicts ZERO streams — eliminating `orex`'s destructive `closeAllPanes()` teardown!
    pub fn select_session(&mut self, target: SessionKey) -> SessionSwitchPlan {
        if self.active_session.as_ref() == Some(&target) {
            return SessionSwitchPlan::default();
        }

        let mut plan = SessionSwitchPlan::default();

        // Demote previous active session to WarmBackground (unless still pinned by another OS window).
        if let Some(prev) = self.active_session.take() {
            let prev_pinned = self.pinned_sessions.contains(&prev);
            if let Some(entry) = self.session_mut(&prev) {
                entry.warmth = if prev_pinned {
                    SessionWarmth::Active
                } else {
                    SessionWarmth::WarmBackground
                };
            }
            self.touch_lru(&prev);
            plan.demoted_to_warm = Some(prev);
        }

        // Promote target session.
        let was_cold = match self.session_mut(&target) {
            Some(entry) => {
                let cold = entry.warmth == SessionWarmth::Cold;
                entry.warmth = SessionWarmth::Active;
                cold
            }
            None => true,
        };
        plan.needs_attach = was_cold;
        self.touch_lru(&target);
        self.active_session = Some(target.clone());

        // Evict only the oldest background sessions exceeding the bounded warm cache limit,
        // skipping `target` and any session pinned by another open OS window.
        while self.warm_lru.len() > self.warm_session_limit {
            let Some(evict_idx) = self
                .warm_lru
                .iter()
                .position(|k| k != &target && !self.pinned_sessions.contains(k))
            else {
                break;
            };
            let oldest = self.warm_lru.remove(evict_idx).expect("valid warm_lru idx");
            if let Some(entry) = self.session_mut(&oldest) {
                entry.warmth = SessionWarmth::Cold;
                for blk in entry.blocks.values_mut() {
                    blk.snapshot_ready = false;
                    blk.pending_resize = None;
                }
            }
            plan.evicted_sessions.push(oldest);
        }

        plan
    }

    /// Process an incoming `SessionEvent` on `(host_id, session_id)`.
    ///
    /// Returns `true` if the caller should issue a `session.view` RPC (single-flight deduplicated).
    pub fn apply_session_event(
        &mut self,
        host_id: &str,
        session_id: &str,
        event: &SessionEvent,
    ) -> bool {
        let Some(host) = self.hosts.get_mut(host_id) else {
            return false;
        };
        let Some(session) = host.sessions.get_mut(session_id) else {
            return false;
        };

        match event {
            SessionEvent::SessionViewChanged { revision } => session.note_view_changed(*revision),
            SessionEvent::SessionLabelChanged { label } => {
                session.label = label.clone();
                if let Some(v) = &mut session.view {
                    v.label = label.clone();
                }
                false
            }
            SessionEvent::WindowLabelChanged { window_id, label } => {
                if let Some(v) = &mut session.view
                    && let Some(w) = v
                        .windows
                        .iter_mut()
                        .find(|w| w.window_id.as_deref() == Some(window_id.as_str()))
                {
                    w.label = label.clone();
                }
                false
            }
            SessionEvent::ActiveWindowChanged { window_id } => {
                if let Some(v) = &mut session.view {
                    v.active_window_id = window_id.clone();
                }
                false
            }
            SessionEvent::BlockOwnerChanged {
                block_id, owner, ..
            } => {
                let blk = session.blocks.entry(block_id.clone()).or_default();
                blk.owner = owner.clone();
                if let Some(v) = &mut session.view {
                    for w in &mut v.windows {
                        for l in &mut w.layers {
                            for b in &mut l.blocks {
                                if b.block_id.as_deref() == Some(block_id.as_str()) {
                                    b.owner = owner.clone();
                                }
                            }
                        }
                    }
                }
                false
            }
            SessionEvent::BlockClosed { block_id } => {
                session.blocks.remove(block_id);
                false
            }
            SessionEvent::BlockEvent(blk_ev) => {
                let blk = session.blocks.entry(blk_ev.block_id.clone()).or_default();
                apply_block_event_to_state(blk, blk_ev);
                false
            }
            _ => false,
        }
    }

    /// Applies one coalesced batch of hydrated block metadata (OSC 7501 `program_status`,
    /// `pwd`, foreground process, title) to a session without N+1 UI thrashing.
    pub fn apply_hydrated_metadata(
        &mut self,
        host_id: &str,
        session_id: &str,
        blocks: Vec<HydratedBlockMeta>,
    ) -> usize {
        let Some(session) = self
            .hosts
            .get_mut(host_id)
            .and_then(|h| h.sessions.get_mut(session_id))
        else {
            return 0;
        };

        let mut updated = 0usize;
        for meta in blocks {
            let entry = session.blocks.entry(meta.block_id).or_default();
            if !meta.program_statuses.is_empty() {
                entry.program_statuses = meta.program_statuses;
            }
            if meta.pwd.is_some() {
                entry.pwd = meta.pwd;
            }
            if meta.foreground_process.is_some() {
                entry.foreground_process = meta.foreground_process;
            }
            if let Some(title) = meta.title
                && !title.trim().is_empty()
            {
                entry.title = Some(title);
            }
            updated += 1;
        }
        updated
    }

    /// Returns the earliest deadline at which a queued, `snapshot_ready` coalesced resize will
    /// become due — allowing the GUI event loop to sleep with `ControlFlow::WaitUntil` instead of
    /// polling.
    pub fn next_resize_deadline(&self) -> Option<Instant> {
        let window = self.resize_coalesce;
        let max_defer = MAX_RESIZE_DEFER.max(window);
        let mut earliest: Option<Instant> = None;
        for host in self.hosts.values() {
            for session in host.sessions.values() {
                for blk in session.blocks.values() {
                    if blk.snapshot_ready
                        && let Some(pending) = &blk.pending_resize
                    {
                        let due = if pending.immediate {
                            pending.requested_at
                        } else {
                            (pending.requested_at + window)
                                .min(pending.first_requested_at + max_defer)
                        };
                        earliest = Some(earliest.map_or(due, |prev| prev.min(due)));
                    }
                }
            }
        }
        earliest
    }

    /// Mark a block's `GHOSTSNP` snapshot as `READY`.
    ///
    /// If a resize was requested before `READY`, it is returned immediately so resize sequencing
    /// never races initial snapshot decode (`orex` B6 fix).
    pub fn mark_snapshot_ready(&mut self, key: &BlockKey) -> Option<TerminalResizeArgs> {
        let host = self.hosts.get_mut(&key.host_id)?;
        let me = host.client_id().map(str::to_owned);
        let session = host.sessions.get_mut(&key.session_id)?;
        let blk = session.blocks.entry(key.block_id.clone()).or_default();
        blk.snapshot_ready = true;
        if let Some(pending) = blk.pending_resize.take() {
            let server_matches = owned_by_other(blk.owner.as_deref(), me.as_deref())
                || blk
                    .last_server_size
                    .is_some_and(|(c, r)| c == pending.args.columns && r == pending.args.rows);
            if blk.last_sent_resize.as_ref() != Some(&pending.args) || !server_matches {
                blk.last_sent_resize = Some(pending.args.clone());
                return Some(pending.args);
            }
        }
        None
    }

    /// Record an authoritative size/owner update from Rex (`RexApc::Resize` or `size_changed`).
    ///
    /// Clears stale `last_sent_resize` whenever the server's actual dimensions diverge from what
    /// we previously sent (e.g. another client or side window shrank the block), ensuring that
    /// expanding back to the previous viewport dimensions is NEVER silently suppressed!
    pub fn note_server_resize(
        &mut self,
        key: &BlockKey,
        cols: u16,
        rows: u16,
        owner: Option<String>,
    ) {
        let Some(host) = self.hosts.get_mut(&key.host_id) else {
            return;
        };
        let me = host.client_id().map(str::to_owned);
        let Some(session) = host.sessions.get_mut(&key.session_id) else {
            return;
        };
        let blk = session.blocks.entry(key.block_id.clone()).or_default();
        blk.last_server_size = Some((cols, rows));
        blk.owner = owner;
        // A size set by another owner says nothing about our desired size: keep it cached so we
        // don't re-send it (and fight that device) on every frame.
        if !owned_by_other(blk.owner.as_deref(), me.as_deref())
            && blk
                .last_sent_resize
                .as_ref()
                .is_some_and(|sent| sent.columns != cols || sent.rows != rows)
        {
            blk.last_sent_resize = None;
        }
    }

    /// Clears cached `last_sent_resize` for a block (e.g. after ownership transfers to us or a
    /// fresh snapshot opens) so the next layout pass can immediately reconcile with the server.
    pub fn invalidate_sent_resize(&mut self, key: &BlockKey) {
        if let Some(blk) = self
            .hosts
            .get_mut(&key.host_id)
            .and_then(|h| h.sessions.get_mut(&key.session_id))
            .and_then(|s| s.blocks.get_mut(&key.block_id))
        {
            blk.last_sent_resize = None;
        }
    }

    /// Resets `view_fetch_in_flight` for `(host_id, session_id)` after a failed `session.view` RPC.
    pub fn clear_view_fetch_in_flight(&mut self, host_id: &str, session_id: &str) {
        if let Some(sess) = self
            .hosts
            .get_mut(host_id)
            .and_then(|h| h.sessions.get_mut(session_id))
        {
            sess.clear_view_fetch_in_flight();
        }
    }

    /// Clears cached `last_sent_resize` across all blocks on `host_id` (e.g. after a transient
    /// `com.superlogical.terminal.resize` `HostError`) so future resizes can retry cleanly.
    pub fn invalidate_host_sent_resizes(&mut self, host_id: &str) {
        if let Some(host) = self.hosts.get_mut(host_id) {
            for session in host.sessions.values_mut() {
                for blk in session.blocks.values_mut() {
                    blk.last_sent_resize = None;
                }
            }
        }
    }

    /// Queue a desired resize for a block (coalesced over `self.resize_coalesce` and strictly gated
    /// behind `snapshot_ready`).
    ///
    /// Re-queuing the exact same `args` NEVER resets `requested_at`, preventing continuous repaints
    /// from starving the debounce timer.
    pub fn queue_resize(&mut self, key: &BlockKey, args: TerminalResizeArgs, now: Instant) {
        self.queue_resize_with_priority(key, args, now, false);
    }

    /// Queue a resize that flushes immediately once `snapshot_ready == true` (used for discrete
    /// Hyprland tiling resizes, sidebar toggles, and ownership restoration).
    pub fn queue_resize_immediate(
        &mut self,
        key: &BlockKey,
        args: TerminalResizeArgs,
        now: Instant,
    ) {
        self.queue_resize_with_priority(key, args, now, true);
    }

    fn queue_resize_with_priority(
        &mut self,
        key: &BlockKey,
        args: TerminalResizeArgs,
        now: Instant,
        immediate: bool,
    ) {
        let Some(host) = self.hosts.get_mut(&key.host_id) else {
            return;
        };
        let me = host.client_id().map(str::to_owned);
        let Some(session) = host.sessions.get_mut(&key.session_id) else {
            return;
        };
        let blk = session.blocks.entry(key.block_id.clone()).or_default();

        // Only skip when BOTH last_sent_resize AND server's authoritative size already match `args`
        // (another owner's size never counts as divergence — see `owned_by_other`).
        let server_diverges = !owned_by_other(blk.owner.as_deref(), me.as_deref())
            && blk
                .last_server_size
                .is_some_and(|(c, r)| c != args.columns || r != args.rows);
        if !server_diverges
            && blk.last_sent_resize.as_ref() == Some(&args)
            && blk.pending_resize.is_none()
        {
            return;
        }

        // Critical: if identical args are already queued, keep the existing deadline (never starve!).
        if let Some(existing) = blk.pending_resize.as_mut()
            && existing.args == args
        {
            if immediate {
                existing.immediate = true;
            }
            return;
        }

        let first_requested_at = blk
            .pending_resize
            .as_ref()
            .map_or(now, |p| p.first_requested_at);
        blk.pending_resize = Some(CoalescedResize {
            args,
            requested_at: now,
            first_requested_at,
            immediate,
        });
    }

    /// Drain all coalesced resizes whose `snapshot_ready == true` and coalesce deadline has elapsed.
    pub fn drain_ready_resizes(&mut self, now: Instant) -> Vec<(BlockKey, TerminalResizeArgs)> {
        let mut ready = Vec::new();
        let window = self.resize_coalesce;
        let max_defer = MAX_RESIZE_DEFER.max(window);

        for (host_id, host) in &mut self.hosts {
            let me = host.client_id().map(str::to_owned);
            for (session_id, session) in &mut host.sessions {
                for (block_id, blk) in &mut session.blocks {
                    if !blk.snapshot_ready {
                        continue;
                    }
                    let due = blk.pending_resize.as_ref().is_some_and(|p| {
                        p.immediate
                            || now.saturating_duration_since(p.requested_at) >= window
                            || now.saturating_duration_since(p.first_requested_at) >= max_defer
                    });
                    if due && let Some(pending) = blk.pending_resize.take() {
                        let server_diverges = !owned_by_other(blk.owner.as_deref(), me.as_deref())
                            && blk.last_server_size.is_some_and(|(c, r)| {
                                c != pending.args.columns || r != pending.args.rows
                            });
                        if blk.last_sent_resize.as_ref() != Some(&pending.args) || server_diverges {
                            blk.last_sent_resize = Some(pending.args.clone());
                            ready.push((
                                BlockKey::new(
                                    host_id.clone(),
                                    session_id.clone(),
                                    block_id.clone(),
                                ),
                                pending.args,
                            ));
                        }
                    }
                }
            }
        }
        ready
    }

    /// Evaluate ownership decision for a block under `self.ownership_policy`.
    pub fn decide_ownership(&self, key: &BlockKey, trigger: OwnershipTrigger) -> OwnershipDecision {
        let Some(host) = self.hosts.get(&key.host_id) else {
            return OwnershipDecision::NoChange;
        };
        let current_owner = host
            .sessions
            .get(&key.session_id)
            .and_then(|s| s.blocks.get(&key.block_id))
            .and_then(|b| b.owner.as_deref());
        self.ownership_policy
            .decide(host.client_id(), current_owner, trigger)
    }

    /// Remove a destroyed session from LRU and active selection.
    pub fn on_session_destroyed(&mut self, host_id: &str, session_id: &str) {
        if let Some(host) = self.hosts.get_mut(host_id) {
            host.on_session_destroyed(session_id);
        }
        let target = SessionKey::new(host_id, session_id);
        self.warm_lru.retain(|k| k != &target);
        if self.active_session.as_ref() == Some(&target) {
            self.active_session = None;
        }
    }

    /// Removes a host and all of its tracked sessions from workspace state and LRU cache.
    pub fn remove_host(&mut self, host_id: &str) -> Vec<SessionKey> {
        let removed_sessions: Vec<SessionKey> = self
            .hosts
            .remove(host_id)
            .map(|h| {
                h.session_order
                    .into_iter()
                    .map(|sid| SessionKey::new(host_id, sid))
                    .collect()
            })
            .unwrap_or_default();
        self.host_order.retain(|id| id != host_id);
        self.warm_lru.retain(|k| k.host_id != host_id);
        if self
            .active_session
            .as_ref()
            .is_some_and(|k| k.host_id == host_id)
        {
            self.active_session = None;
        }
        removed_sessions
    }

    fn touch_lru(&mut self, key: &SessionKey) {
        self.warm_lru.retain(|k| k != key);
        self.warm_lru.push_back(key.clone());
    }
}

fn apply_block_event_to_state(blk: &mut BlockRuntimeState, ev: &BlockEventPayload) {
    match ev.name.as_str() {
        "program_status_changed" => {
            let raw = ev.payload.get("record").unwrap_or(&ev.payload);
            if let Ok(mut rec) = serde_json::from_value::<ProgramStatusRecord>(raw.clone()) {
                // Tolerate `message` alias if a block event uses `message` instead of `msg`.
                if rec.msg.is_none()
                    && let Some(m) = raw.get("message").and_then(|v| v.as_str())
                {
                    rec.msg = Some(m.to_owned());
                }
                let rec_id = rec.id.as_deref().unwrap_or("");
                if let Some(existing) = blk
                    .program_statuses
                    .iter_mut()
                    .find(|r| r.id.as_deref().unwrap_or("") == rec_id)
                {
                    *existing = rec;
                } else {
                    blk.program_statuses.push(rec);
                }
            }
        }
        "program_status_removed" => {
            let target_id = ev
                .payload
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            blk.program_statuses
                .retain(|r| r.id.as_deref().unwrap_or("") != target_id);
        }
        "title_changed" => {
            if let Some(title) = ev.payload.get("title").and_then(|v| v.as_str()) {
                blk.title = (!title.trim().is_empty()).then(|| title.to_owned());
            }
        }
        "pwd_changed" => {
            if let Some(pwd) = ev.payload.get("pwd").and_then(|v| v.as_str()) {
                blk.pwd = (!pwd.trim().is_empty()).then(|| pwd.to_owned());
            }
        }
        "process_changed" => {
            let fg_name = ev
                .payload
                .get("foreground")
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
                .or_else(|| {
                    ev.payload
                        .get("child")
                        .and_then(|v| v.get("name"))
                        .and_then(|v| v.as_str())
                });
            if let Some(name) = fg_name {
                blk.foreground_process = Some(name.to_owned());
            }
            if let Some(code) = ev
                .payload
                .get("last_exit")
                .and_then(|v| v.get("exit_code"))
                .and_then(|v| v.as_i64())
            {
                blk.exited_code = Some(code as i32);
            }
        }
        "bell" => {
            blk.has_bell = true;
        }
        "child_exited" => {
            if let Some(code) = ev.payload.get("exit_code").and_then(|v| v.as_i64()) {
                blk.exited_code = Some(code as i32);
            }
        }
        "size_changed" => {
            if let (Some(cols), Some(rows)) = (
                ev.payload.get("columns").and_then(|v| v.as_u64()),
                ev.payload.get("rows").and_then(|v| v.as_u64()),
            ) {
                let c = cols.min(u64::from(u16::MAX)) as u16;
                let r = rows.min(u64::from(u16::MAX)) as u16;
                blk.last_server_size = Some((c, r));
                if blk
                    .last_sent_resize
                    .as_ref()
                    .is_some_and(|sent| sent.columns != c || sent.rows != r)
                {
                    blk.last_sent_resize = None;
                }
            }
            if let Some(owner_val) = ev.payload.get("owner") {
                blk.owner = owner_val.as_str().map(str::to_owned);
            }
        }
        "ports_changed" => {
            if let Ok(resp) = serde_json::from_value::<TerminalPortsResponse>(ev.payload.clone())
                && !resp.truncated.unwrap_or(false)
            {
                blk.apply_ports_response(resp);
            }
        }
        _ => {}
    }
}

/// Convert an [`omarchy_theme::ThemePalette`] into the exact Rex API rev 24
/// `com.superlogical.terminal.set_theme` wire payload with explicit Mode 2031 `scheme`.
pub fn theme_palette_to_rex_args(palette: &ThemePalette) -> TerminalSetThemeArgs {
    let raw = palette.to_rex_set_theme_args();
    let scheme = match palette.mode {
        AppearanceMode::Dark => ThemeScheme::Dark,
        AppearanceMode::Light => ThemeScheme::Light,
    };
    TerminalSetThemeArgs {
        foreground: Some(raw.foreground),
        background: Some(raw.background),
        cursor: Some(raw.cursor),
        palette: Some(raw.palette),
        scheme: Some(scheme),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rex_proto::types::{BlockView, LayerView, LayoutNode, WindowView};

    fn dummy_view(session_id: &str, rev: u64, block_ids: &[&str]) -> SessionView {
        let windows = block_ids
            .iter()
            .enumerate()
            .map(|(i, b)| WindowView {
                window_id: Some(format!("window:{i}")),
                label: format!("Tab {i}"),
                focused_block_id: Some((*b).into()),
                zoomed_block_id: None,
                tiled_zoomed_block_id: None,
                layers: vec![LayerView {
                    layer_id: Some(format!("layer:{i}")),
                    kind: "tiled".into(),
                    bounds: [0.0, 0.0, 1.0, 1.0],
                    state: None,
                    layout: Some(LayoutNode {
                        rect: [0.0, 0.0, 1.0, 1.0],
                        block_id: Some((*b).into()),
                        split: None,
                    }),
                    blocks: vec![BlockView {
                        block_id: Some((*b).into()),
                        label: format!("t3:1baa1939/thread-{i}"),
                        creator_name: Some("com.superlogical.terminal".into()),
                        flavor: Some("com.superlogical.terminal.shell".into()),
                        host_id: Some("host-local-1".into()),
                        owner: None,
                    }],
                }],
            })
            .collect();
        SessionView {
            revision: rev,
            session_id: Some(session_id.into()),
            label: "T3 Code".into(),
            active_window_id: block_ids.first().map(|_| "window:0".into()),
            windows,
        }
    }

    fn setup_workspace_with_sessions(limit: usize, ids: &[&str]) -> WorkspaceState {
        let mut ws = WorkspaceState::new(limit);
        let mut host = HostEntry::new(
            "local",
            "Local",
            RexEndpoint::parse("unix:///run/user/1000/rex/server.sock").unwrap(),
        );
        host.status = HostStatus::Connected {
            client_id: "client:me".into(),
            server_host_id: Some("host-local-1".into()),
            api_revision: 24,
            resumed: false,
        };
        for id in ids {
            host.on_session_created(SessionSummary {
                session_id: (*id).into(),
                label: (*id).into(),
            });
        }
        ws.add_host(host);
        ws
    }

    #[test]
    fn revision_gating_deduplicates_and_catches_up_out_of_order_views() {
        let mut entry = SessionEntry::new(SessionSummary {
            session_id: "s1".into(),
            label: "T3 Code".into(),
        });

        // First notification starts single-flight fetch.
        assert!(entry.note_view_changed(10));
        // Duplicate or older revisions do NOT trigger extra fetches.
        assert!(!entry.note_view_changed(10));
        assert!(!entry.note_view_changed(9));
        // Higher revision while fetch is in flight is recorded without starting a 2nd concurrent RPC.
        assert!(!entry.note_view_changed(12));

        // Completing rev 10 triggers exactly ONE catch-up fetch for rev 12.
        assert!(entry.apply_view(dummy_view("s1", 10, &["b1"])));
        // Completing rev 12 settles cleanly with no further fetch.
        assert!(!entry.apply_view(dummy_view("s1", 12, &["b1"])));
        assert_eq!(entry.applied_revision, 12);
        // Out-of-order rev 11 is ignored cleanly.
        assert!(!entry.note_view_changed(11));
    }

    #[test]
    fn empty_session_view_is_preserved_and_never_destroyed() {
        let mut entry = SessionEntry::new(SessionSummary {
            session_id: "t3".into(),
            label: "T3 Code".into(),
        });
        entry.apply_view(dummy_view("t3", 5, &["b1"]));
        assert!(!entry.is_empty_view());

        // Empty windows array during tr3x sync must NEVER destroy the session!
        entry.apply_view(dummy_view("t3", 6, &[]));
        assert!(entry.is_empty_view());
        assert_eq!(entry.applied_revision, 6);
        assert_eq!(entry.label, "T3 Code");
        assert!(entry.blocks.is_empty());
    }

    #[test]
    fn lru_warm_session_cache_preserves_recent_sessions_and_evicts_oldest() {
        let mut ws = setup_workspace_with_sessions(2, &["t3", "work", "ops"]);

        // 1. Activate "t3" (Cold -> Active)
        let p1 = ws.select_session(SessionKey::new("local", "t3"));
        assert!(p1.needs_attach);
        assert!(p1.evicted_sessions.is_empty());

        // 2. Switch to "work" ("t3" stays WarmBackground — ZERO teardown!)
        let p2 = ws.select_session(SessionKey::new("local", "work"));
        assert!(p2.needs_attach);
        assert_eq!(p2.demoted_to_warm, Some(SessionKey::new("local", "t3")));
        assert!(p2.evicted_sessions.is_empty());
        assert_eq!(
            ws.session(&SessionKey::new("local", "t3")).unwrap().warmth,
            SessionWarmth::WarmBackground
        );

        // 3. Switch back to "t3" -> ALREADY WARM! Zero attach, zero re-decode!
        let p3 = ws.select_session(SessionKey::new("local", "t3"));
        assert!(!p3.needs_attach);
        assert!(p3.evicted_sessions.is_empty());

        // 4. Switch to "ops" -> limit is 2, so oldest ("work") is evicted, while "t3" stays warm!
        let p4 = ws.select_session(SessionKey::new("local", "ops"));
        assert!(p4.needs_attach);
        assert_eq!(p4.evicted_sessions, vec![SessionKey::new("local", "work")]);
        assert_eq!(
            ws.session(&SessionKey::new("local", "work"))
                .unwrap()
                .warmth,
            SessionWarmth::Cold
        );
        assert_eq!(
            ws.session(&SessionKey::new("local", "t3")).unwrap().warmth,
            SessionWarmth::WarmBackground
        );
    }

    #[test]
    fn ownership_policy_respects_multi_device_and_t3_fit() {
        let mut ws = setup_workspace_with_sessions(4, &["t3"]);
        let key = BlockKey::new("local", "t3", "b1");

        // Unowned block -> passive attach/focus politely uses OwnMode::Request.
        assert_eq!(
            ws.decide_ownership(&key, OwnershipTrigger::AttachOrView),
            OwnershipDecision::Own {
                mode: OwnMode::Request
            }
        );

        // Another device owns b1 -> passive view & focus do NOT hijack it!
        ws.apply_session_event(
            "local",
            "t3",
            &SessionEvent::BlockOwnerChanged {
                block_id: "b1".into(),
                owner: Some("client:ipad".into()),
                previous: None,
                reason: Some("take".into()),
            },
        );
        assert_eq!(
            ws.decide_ownership(&key, OwnershipTrigger::AttachOrView),
            OwnershipDecision::NoChange
        );
        assert_eq!(
            ws.decide_ownership(&key, OwnershipTrigger::FocusPane),
            OwnershipDecision::NoChange
        );
        // Explicit typing / resize / t3_fit forcibly claims with OwnMode::Take!
        assert_eq!(
            ws.decide_ownership(&key, OwnershipTrigger::ExplicitInteractOrFit),
            OwnershipDecision::Own {
                mode: OwnMode::Take
            }
        );

        // Once owned by us, no redundant RPCs are issued.
        ws.apply_session_event(
            "local",
            "t3",
            &SessionEvent::BlockOwnerChanged {
                block_id: "b1".into(),
                owner: Some("client:me".into()),
                previous: Some("client:ipad".into()),
                reason: Some("take".into()),
            },
        );
        assert_eq!(
            ws.decide_ownership(&key, OwnershipTrigger::ExplicitInteractOrFit),
            OwnershipDecision::NoChange
        );
    }

    #[test]
    fn resize_is_strictly_gated_until_snapshot_ready_and_coalesced() {
        let mut ws = setup_workspace_with_sessions(4, &["t3"]);
        let key = BlockKey::new("local", "t3", "b1");
        let t0 = Instant::now();

        let r1 = TerminalResizeArgs {
            columns: 120,
            rows: 40,
            cell_width_px: Some(9),
            cell_height_px: Some(18),
        };
        let r2 = TerminalResizeArgs {
            columns: 140,
            rows: 44,
            cell_width_px: Some(9),
            cell_height_px: Some(18),
        };

        // Queue resizes before snapshot READY -> drain returns nothing!
        ws.queue_resize(&key, r1, t0);
        ws.queue_resize(&key, r2.clone(), t0 + Duration::from_millis(5));
        assert!(
            ws.drain_ready_resizes(t0 + Duration::from_secs(1))
                .is_empty()
        );

        // Snapshot hits READY -> latest coalesced resize flushes immediately!
        let flushed = ws.mark_snapshot_ready(&key);
        assert_eq!(flushed, Some(r2));

        // Subsequent resize waits for coalesce window (24ms) before draining.
        let r3 = TerminalResizeArgs {
            columns: 160,
            rows: 50,
            cell_width_px: Some(9),
            cell_height_px: Some(18),
        };
        let t1 = t0 + Duration::from_millis(100);
        ws.queue_resize(&key, r3.clone(), t1);
        assert!(
            ws.drain_ready_resizes(t1 + Duration::from_millis(10))
                .is_empty()
        );
        let due = ws.drain_ready_resizes(t1 + Duration::from_millis(30));
        assert_eq!(due, vec![(key, r3)]);
    }

    #[test]
    fn theme_palette_converts_cleanly_to_rex_set_theme_args() {
        let dark = ThemePalette::default_dark();
        let dark_args = theme_palette_to_rex_args(&dark);
        assert_eq!(dark_args.scheme, Some(ThemeScheme::Dark));
        assert_eq!(dark_args.palette.as_ref().map(Vec::len), Some(16));
        assert!(dark_args.background.as_deref().unwrap().starts_with('#'));

        let light = ThemePalette::default_light();
        let light_args = theme_palette_to_rex_args(&light);
        assert_eq!(light_args.scheme, Some(ThemeScheme::Light));
        assert_eq!(light_args.palette.as_ref().map(Vec::len), Some(16));
    }

    #[test]
    fn block_events_and_hydrated_metadata_update_runtime_state() {
        let mut ws = setup_workspace_with_sessions(4, &["t3"]);
        let sk = SessionKey::new("local", "t3");
        ws.session_mut(&sk)
            .unwrap()
            .apply_view(dummy_view("t3", 1, &["b1"]));

        // Coalesced batch hydration sets initial program_status + pwd.
        let count = ws.apply_hydrated_metadata(
            "local",
            "t3",
            vec![HydratedBlockMeta {
                block_id: "b1".into(),
                program_statuses: vec![ProgramStatusRecord {
                    id: Some("rec1".into()),
                    state: "working".into(),
                    kind: None,
                    progress: None,
                    app: Some("t3code".into()),
                    effective_app: Some("t3code".into()),
                    title: Some("Thread 1".into()),
                    msg: Some("omarex · Working".into()),
                    updated_at: "2026-11-26T10:15:00+01:00".into(),
                    owner: None,
                }],
                pwd: Some("file://localhost/home/user/projects/omarex".into()),
                foreground_process: Some("tr3x".into()),
                title: None,
            }],
        );
        assert_eq!(count, 1);
        let blk = ws.session(&sk).unwrap().blocks.get("b1").unwrap();
        assert_eq!(blk.program_statuses.len(), 1);
        assert_eq!(blk.program_statuses[0].state, "working");

        // Live BlockEvent updates existing record and removes it cleanly.
        let changed = SessionEvent::BlockEvent(BlockEventPayload {
            block_id: "b1".into(),
            creator_name: Some("com.superlogical.terminal".into()),
            name: "program_status_changed".into(),
            payload: serde_json::json!({
                "record": {
                    "id": "rec1",
                    "state": "blocked",
                    "app": "claude",
                    "msg": "Approve tool?"
                }
            }),
        });
        assert!(!ws.apply_session_event("local", "t3", &changed));
        let blk = ws.session(&sk).unwrap().blocks.get("b1").unwrap();
        assert_eq!(blk.program_statuses[0].state, "blocked");
        assert_eq!(blk.program_statuses[0].app.as_deref(), Some("claude"));

        let removed = SessionEvent::BlockEvent(BlockEventPayload {
            block_id: "b1".into(),
            creator_name: Some("com.superlogical.terminal".into()),
            name: "program_status_removed".into(),
            payload: serde_json::json!({ "id": "rec1" }),
        });
        assert!(!ws.apply_session_event("local", "t3", &removed));
        assert!(
            ws.session(&sk)
                .unwrap()
                .blocks
                .get("b1")
                .unwrap()
                .program_statuses
                .is_empty()
        );
    }

    #[test]
    fn resize_coalesce_never_starves_and_reconciles_shrink_expand_and_disown() {
        let mut ws = setup_workspace_with_sessions(4, &["t3"]);
        let sk = SessionKey::new("local", "t3");
        let mut view = dummy_view("t3", 1, &["b1"]);
        view.windows[0].layers[0].blocks[0].owner = Some("client:other".into());
        ws.session_mut(&sk).unwrap().apply_view(view);
        let key = BlockKey::new("local", "t3", "b1");
        assert_eq!(
            ws.block(&key).and_then(|b| b.owner.as_deref()),
            Some("client:other")
        );

        // `apply_view` with `owner: None` must clear stale owner back to `None`.
        let disowned_view = dummy_view("t3", 2, &["b1"]);
        ws.session_mut(&sk).unwrap().apply_view(disowned_view);
        assert_eq!(ws.block(&key).and_then(|b| b.owner.as_deref()), None);

        let _ = ws.mark_snapshot_ready(&key);

        // Re-queuing the exact same dimensions at 120Hz (~8ms intervals) MUST NOT reset the
        // debounce deadline or starve the resize.
        let wide = TerminalResizeArgs {
            columns: 190,
            rows: 50,
            cell_width_px: Some(9),
            cell_height_px: Some(18),
        };
        let t0 = Instant::now();
        ws.queue_resize(&key, wide.clone(), t0);
        ws.queue_resize(&key, wide.clone(), t0 + Duration::from_millis(8));
        ws.queue_resize(&key, wide.clone(), t0 + Duration::from_millis(14));
        let due = ws.drain_ready_resizes(t0 + Duration::from_millis(18));
        assert_eq!(due, vec![(key.clone(), wide.clone())]);

        // Server shrinks the unowned block to 82x50 -> stale last_sent_resize cleared.
        ws.note_server_resize(&key, 82, 50, None);

        // Expanding back to the exact previous 190x50 dimensions MUST NOT be dropped!
        let t1 = t0 + Duration::from_millis(100);
        ws.queue_resize_immediate(&key, wide.clone(), t1);
        let restored = ws.drain_ready_resizes(t1);
        assert_eq!(restored, vec![(key.clone(), wide.clone())]);

        // Another device (side window / Mac mini) takes the block at 82x50: its size is
        // authoritative, so re-queuing our desired size must NOT re-send it (no resize fight).
        ws.note_server_resize(&key, 82, 50, Some("client:side".into()));
        let t2 = t1 + Duration::from_millis(100);
        ws.queue_resize_immediate(&key, wide.clone(), t2);
        assert!(ws.drain_ready_resizes(t2).is_empty());

        // A real local window resize invalidates the cache and reclaims: resize goes out again.
        ws.invalidate_sent_resize(&key);
        let t3 = t2 + Duration::from_millis(100);
        ws.queue_resize_immediate(&key, wide.clone(), t3);
        assert_eq!(ws.drain_ready_resizes(t3), vec![(key, wide)]);
    }

    #[test]
    fn clear_view_fetch_in_flight_unwedges_session_after_failed_view_rpc() {
        let mut ws = setup_workspace_with_sessions(4, &["s1"]);
        let sk = SessionKey::new("local", "s1");

        // First view-changed event sets `view_fetch_in_flight = true`.
        assert!(ws.apply_session_event(
            "local",
            "s1",
            &SessionEvent::SessionViewChanged { revision: 2 },
        ));
        assert!(ws.session(&sk).unwrap().view_fetch_in_flight);

        // Simulate `session.view` failing (e.g. transient disconnect): resetting the flag allows
        // the next higher revision to trigger a fresh fetch instead of deadlocking forever.
        ws.clear_view_fetch_in_flight("local", "s1");
        assert!(!ws.session(&sk).unwrap().view_fetch_in_flight);
        assert!(ws.apply_session_event(
            "local",
            "s1",
            &SessionEvent::SessionViewChanged { revision: 3 },
        ));
    }

    #[test]
    fn ports_accepts_tcp6_and_formats_ipv6_zone_urls_per_rfc6874() {
        let mut blk = BlockRuntimeState::default();
        blk.apply_ports_response(TerminalPortsResponse {
            status: PortScanStatus::Ok,
            ports: vec![
                PortEntry {
                    port: 5353,
                    address: "0.0.0.0".into(),
                    protocol: "udp".into(),
                    first_seen_at: "2026-11-26T20:00:00Z".into(),
                    pid: None,
                },
                PortEntry {
                    port: 3000,
                    address: "fe80::1%1".into(),
                    protocol: "tcp6".into(),
                    first_seen_at: "2026-11-26T20:00:00Z".into(),
                    pid: None,
                },
            ],
            processes: Vec::new(),
            truncated: None,
            id: None,
        });
        assert_eq!(blk.listening_ports.len(), 1);
        assert_eq!(blk.listening_ports[0].port, 3000);
        assert_eq!(normalize_tunnel_host("fe80::1%1"), "fe80::1%1");
        assert_eq!(
            format_port_http_url(&normalize_tunnel_host("fe80::1%1"), 3000),
            "http://[fe80::1%251]:3000"
        );
        assert_eq!(
            format_port_http_url(&normalize_tunnel_host("0.0.0.0"), 8080),
            "http://127.0.0.1:8080"
        );
    }
}
