//! Rex Control WebSocket (`rex.control.v1`, API rev 24) client with automatic `reconnect_token`
//! recovery, `seq` + `revision` tracking, and a dedicated unordered fast-path lane for `client.reply`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Message};

use crate::ProtoError;
use crate::endpoint::RexEndpoint;
use crate::transport::{RexWsStream, dial_websocket};
use crate::types::{
    BlockCloseRequest, BlockEventRequest, BlockMethodPayload, BlockOwnershipResponse,
    CONTROL_SUBPROTOCOL, ClientActionRequest, ClientActionResponse, ClientInfo, ClientKeyRequest,
    ClientKeyResponse, ClientKeymapRequest, ClientKeymapResponse, ClientListResponse,
    ClientPerformRequest, ClientPerformResponse, ClientReplyRequest, ControlRequest,
    CreatedWindowInfo, DataConnectRequest, DataConnectResponse, HelloMessage, HostsListResponse,
    HostsPutRequest, HostsPutResponse, HostsRemoveRequest, HostsRemoveResponse,
    IncomingControlFrame, MIN_COMPRESS_BYTES, ReconnectToken, RevisionResponse, ServerEvent,
    ServerStatus, SessionAttachResponse, SessionBootstrapRequest, SessionBootstrapResponse,
    SessionCreateRequest, SessionCreateResponse, SessionDisownBlockRequest,
    SessionFloatBlockRequest, SessionFloatBlockResponse, SessionListResponse,
    SessionNewBlockRequest, SessionNewBlockResponse, SessionNewLayerRequest,
    SessionNewLayerResponse, SessionNewSplitRequest, SessionNewSplitResponse,
    SessionNewWindowRequest, SessionOwnBlockRequest, SessionResizeLayerRequest,
    SessionResizePaneRequest, SessionSetLayerStateRequest, SessionSwapBlocksRequest,
    SessionSwapBlocksResponse, SessionTransferWindowRequest, SessionTransferWindowResponse,
    SessionView, SessionZoomBlockRequest, SessionZoomBlockResponse, TerminalBlobReadArgs,
    TerminalBlobReadResponse, TerminalClipboardReadArgs, TerminalClipboardReadResponse,
    TerminalDownloadBeginArgs, TerminalDownloadBeginResponse, TerminalListDirArgs,
    TerminalListDirResponse, TerminalModesResponse, TerminalPortsResponse, TerminalProcessResponse,
    TerminalProgramStatusResponse, TerminalPwdResponse, TerminalResizeArgs, TerminalSetThemeArgs,
    TerminalSizeResponse, TerminalTitleResponse, TerminalUploadBeginArgs,
    TerminalUploadBeginResponse, TerminalWriteArgs, TerminalWriteResponse, TunnelInfoResponse,
    WelcomeMessage,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_RECONNECT_GRACE: Duration = Duration::from_secs(60);

/// Configuration for [`ControlClient`].
#[derive(Debug, Clone)]
pub struct ControlConfig {
    pub endpoint: RexEndpoint,
    pub client_info: ClientInfo,
    pub rpc_timeout: Duration,
    pub auto_reconnect: bool,
    pub reconnect_grace: Duration,
}

impl ControlConfig {
    pub fn new(endpoint: RexEndpoint) -> Self {
        Self {
            endpoint,
            client_info: ClientInfo::omarex_default(),
            rpc_timeout: DEFAULT_RPC_TIMEOUT,
            auto_reconnect: true,
            reconnect_grace: DEFAULT_RECONNECT_GRACE,
        }
    }
}

/// Notifications pushed asynchronously by [`ControlClient`].
#[derive(Debug, Clone)]
pub enum ControlNotification {
    /// Control connection established (or seamlessly resumed via `reconnect_token`).
    Connected {
        client_id: String,
        resumed: bool,
        api_revision: u32,
        max_message_bytes: usize,
    },
    /// Control connection dropped.
    Disconnected {
        will_reconnect: bool,
        reason: String,
    },
    /// Ordered server or session event with its monotonic per-connection `seq`.
    Event { seq: u64, event: Box<ServerEvent> },
}

/// Raw RPC success result carrying both the decoded JSON payload and server `seq` watermark.
#[derive(Debug, Clone)]
pub struct RpcOutcome {
    pub seq: u64,
    pub payload: Value,
}

struct OutboundRpc {
    id: String,
    method: String,
    payload: Option<Value>,
    respond_to: oneshot::Sender<Result<RpcOutcome, ProtoError>>,
}

struct SharedState {
    client_id: Mutex<Option<String>>,
    api_revision: AtomicU64,
    last_event_seq: AtomicU64,
    session_revisions: Mutex<HashMap<String, u64>>,
    next_req_id: AtomicU64,
}

impl SharedState {
    fn new() -> Self {
        Self {
            client_id: Mutex::new(None),
            api_revision: AtomicU64::new(0),
            last_event_seq: AtomicU64::new(0),
            session_revisions: Mutex::new(HashMap::new()),
            next_req_id: AtomicU64::new(1),
        }
    }
}

/// Cloneable handle to an active Rex Control WebSocket (`rex.control.v1`, API rev 24).
///
/// Internally maintains two distinct outbound lanes:
/// - `priority_tx`: strictly prioritized unordered fast-path lane used by [`Self::reply_fast`]
///   (`client.reply`), guaranteeing interactive `rex.ui` prompts and `client_call_requested`
///   proxies never stall behind an in-flight ordered `client.action`.
/// - `normal_tx`: standard RPC lane for all other control requests.
#[derive(Clone)]
pub struct ControlClient {
    config: ControlConfig,
    priority_tx: mpsc::UnboundedSender<OutboundRpc>,
    normal_tx: mpsc::UnboundedSender<OutboundRpc>,
    shutdown_tx: mpsc::UnboundedSender<()>,
    state: Arc<SharedState>,
}

impl std::fmt::Debug for ControlClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlClient")
            .field("endpoint", &self.config.endpoint)
            .field("client_id", &self.client_id())
            .field("api_revision", &self.api_revision())
            .field("last_event_seq", &self.last_event_seq())
            .finish()
    }
}

impl ControlClient {
    /// Dial the Rex server, perform the API rev 24 `hello`/`welcome` handshake, and spawn
    /// the background connection/reconnect actor.
    pub async fn connect(
        config: ControlConfig,
    ) -> Result<
        (
            Self,
            WelcomeMessage,
            mpsc::UnboundedReceiver<ControlNotification>,
        ),
        ProtoError,
    > {
        let mut ws = dial_websocket(&config.endpoint, "/api/control", CONTROL_SUBPROTOCOL).await?;
        let welcome = perform_control_handshake(&mut ws, &config.client_info, None).await?;
        Ok(Self::spawn_with_stream(config, ws, welcome))
    }

    /// Attach a [`ControlClient`] to an already-dialed WebSocket stream (used by tests and custom carriers).
    pub async fn connect_with_stream(
        config: ControlConfig,
        mut ws: RexWsStream,
    ) -> Result<
        (
            Self,
            WelcomeMessage,
            mpsc::UnboundedReceiver<ControlNotification>,
        ),
        ProtoError,
    > {
        let welcome = perform_control_handshake(&mut ws, &config.client_info, None).await?;
        Ok(Self::spawn_with_stream(config, ws, welcome))
    }

    fn spawn_with_stream(
        config: ControlConfig,
        ws: RexWsStream,
        welcome: WelcomeMessage,
    ) -> (
        Self,
        WelcomeMessage,
        mpsc::UnboundedReceiver<ControlNotification>,
    ) {
        let (priority_tx, priority_rx) = mpsc::unbounded_channel();
        let (normal_tx, normal_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = mpsc::unbounded_channel();
        let (notify_tx, notify_rx) = mpsc::unbounded_channel();

        let state = Arc::new(SharedState::new());
        if let Ok(mut guard) = state.client_id.lock() {
            *guard = Some(welcome.client.id.clone());
        }
        state
            .api_revision
            .store(u64::from(welcome.api_revision), Ordering::Relaxed);

        let _ = notify_tx.send(ControlNotification::Connected {
            client_id: welcome.client.id.clone(),
            resumed: welcome.client.resumed,
            api_revision: welcome.api_revision,
            max_message_bytes: welcome.limits.max_message_bytes,
        });

        let actor_config = config.clone();
        let actor_state = Arc::clone(&state);
        let initial_token = welcome.client.reconnect_token.clone();
        tokio::spawn(async move {
            run_control_actor(
                actor_config,
                ws,
                initial_token,
                priority_rx,
                normal_rx,
                shutdown_rx,
                notify_tx,
                actor_state,
            )
            .await;
        });

        let client = Self {
            config,
            priority_tx,
            normal_tx,
            shutdown_tx,
            state,
        };
        (client, welcome, notify_rx)
    }

    /// Returns the server-assigned `client:<uuid>` identifier.
    pub fn client_id(&self) -> Option<String> {
        self.state.client_id.lock().ok().and_then(|g| g.clone())
    }

    /// Returns the negotiated Rex API revision (target: 24).
    pub fn api_revision(&self) -> u32 {
        self.state.api_revision.load(Ordering::Relaxed) as u32
    }

    /// Returns the highest control `seq` observed on the active connection.
    pub fn last_event_seq(&self) -> u64 {
        self.state.last_event_seq.load(Ordering::Relaxed)
    }

    /// Returns the configured endpoint.
    pub fn endpoint(&self) -> &RexEndpoint {
        &self.config.endpoint
    }

    /// Record an applied `session.view` revision for a session.
    pub fn record_view_revision(&self, session_id: &str, revision: u64) {
        if let Ok(mut map) = self.state.session_revisions.lock() {
            let entry = map.entry(session_id.to_string()).or_insert(0);
            if revision > *entry {
                *entry = revision;
            }
        }
    }

    /// Returns `true` iff `revision` is strictly newer than the last recorded view revision for
    /// `session_id` (preventing redundant `session.view` fetches on out-of-order notifications).
    pub fn should_fetch_view(&self, session_id: &str, revision: u64) -> bool {
        self.state
            .session_revisions
            .lock()
            .map(|map| revision > map.get(session_id).copied().unwrap_or(0))
            .unwrap_or(true)
    }

    /// Cleanly close the control WebSocket with status `1000` and terminate the background actor.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
    }

    /// Issue a raw RPC call on the normal lane.
    pub async fn call_raw(
        &self,
        method: &str,
        payload: Option<Value>,
    ) -> Result<RpcOutcome, ProtoError> {
        self.dispatch_rpc(method, payload, false).await
    }

    /// Issue a raw RPC call on the high-priority unordered fast-path lane (used for `client.reply`).
    pub async fn call_priority_raw(
        &self,
        method: &str,
        payload: Option<Value>,
    ) -> Result<RpcOutcome, ProtoError> {
        self.dispatch_rpc(method, payload, true).await
    }

    async fn dispatch_rpc(
        &self,
        method: &str,
        payload: Option<Value>,
        priority: bool,
    ) -> Result<RpcOutcome, ProtoError> {
        let seq_id = self.state.next_req_id.fetch_add(1, Ordering::Relaxed);
        let id = format!("omx-{seq_id}");
        let (tx, rx) = oneshot::channel();
        let req = OutboundRpc {
            id,
            method: method.to_string(),
            payload,
            respond_to: tx,
        };

        let send_res = if priority {
            self.priority_tx.send(req)
        } else {
            self.normal_tx.send(req)
        };
        send_res.map_err(|_| ProtoError::Closed("control connection closed".into()))?;

        tokio::time::timeout(self.config.rpc_timeout, rx)
            .await
            .map_err(|_| ProtoError::Timeout(format!("RPC `{method}` timed out")))?
            .map_err(|_| ProtoError::Closed(format!("RPC `{method}` canceled by disconnect")))?
    }

    /// Issue a strongly-typed RPC call, returning `(response, server_seq)`.
    pub async fn call<Req, Res>(&self, method: &str, req: &Req) -> Result<(Res, u64), ProtoError>
    where
        Req: Serialize,
        Res: DeserializeOwned,
    {
        let payload = serde_json::to_value(req)?;
        let out = self.call_raw(method, Some(payload)).await?;
        let parsed: Res = serde_json::from_value(out.payload)?;
        Ok((parsed, out.seq))
    }

    /// Issue a strongly-typed RPC call with empty/no request payload.
    pub async fn call_empty<Res>(&self, method: &str) -> Result<(Res, u64), ProtoError>
    where
        Res: DeserializeOwned,
    {
        let out = self.call_raw(method, Some(serde_json::json!({}))).await?;
        let parsed: Res = serde_json::from_value(out.payload)?;
        Ok((parsed, out.seq))
    }

    /// Issue a strongly-typed block method (`<creator>.<method>`) wrapped in `{session_id, block_id, args}`.
    pub async fn call_block<Args, Res>(
        &self,
        session_id: &str,
        block_id: &str,
        full_method: &str,
        args: Option<Args>,
    ) -> Result<(Res, u64), ProtoError>
    where
        Args: Serialize,
        Res: DeserializeOwned,
    {
        let envelope = BlockMethodPayload {
            session_id: session_id.to_string(),
            block_id: block_id.to_string(),
            args,
        };
        let payload = serde_json::to_value(&envelope)?;
        let out = self.call_raw(full_method, Some(payload)).await?;
        let parsed: Res = serde_json::from_value(out.payload)?;
        Ok((parsed, out.seq))
    }

    /// Send `client.reply` on the **unordered fast-path priority lane** so interactive prompts,
    /// `client_perform_requested`, and `client_call_requested` proxies never block behind an
    /// in-flight ordered `client.action`.
    pub async fn reply_fast(&self, req: &ClientReplyRequest) -> Result<u64, ProtoError> {
        let payload = serde_json::to_value(req)?;
        let out = self
            .call_priority_raw("client.reply", Some(payload))
            .await?;
        Ok(out.seq)
    }

    // ------------------------------------------------------------------------
    // High-level API Rev 24 RPC wrappers
    // ------------------------------------------------------------------------

    /// Single-RTT startup bootstrap (`session.bootstrap`), recording view revisions automatically.
    pub async fn bootstrap(
        &self,
        req: &SessionBootstrapRequest,
    ) -> Result<(SessionBootstrapResponse, u64), ProtoError> {
        let (res, seq): (SessionBootstrapResponse, u64) =
            self.call("session.bootstrap", req).await?;
        if let Some(sel) = &res.selected_session {
            self.record_view_revision(&sel.session.session_id, sel.view.revision);
        }
        for att in &res.attached_sessions {
            self.record_view_revision(&att.session.session_id, att.view.revision);
        }
        Ok((res, seq))
    }

    pub async fn session_list(&self) -> Result<(SessionListResponse, u64), ProtoError> {
        self.call_empty("session.list").await
    }

    pub async fn session_view(&self, session_id: &str) -> Result<(SessionView, u64), ProtoError> {
        let (view, seq): (SessionView, u64) = self
            .call(
                "session.view",
                &serde_json::json!({ "session_id": session_id }),
            )
            .await?;
        self.record_view_revision(session_id, view.revision);
        Ok((view, seq))
    }

    pub async fn session_attach(
        &self,
        session_id: &str,
    ) -> Result<(SessionAttachResponse, u64), ProtoError> {
        let (res, seq): (SessionAttachResponse, u64) = self
            .call(
                "session.attach",
                &serde_json::json!({ "session_id": session_id }),
            )
            .await?;
        self.record_view_revision(session_id, res.view.revision);
        Ok((res, seq))
    }

    pub async fn session_detach(&self, session_id: &str) -> Result<u64, ProtoError> {
        let out = self
            .call_raw(
                "session.detach",
                Some(serde_json::json!({ "session_id": session_id })),
            )
            .await?;
        Ok(out.seq)
    }

    pub async fn session_create(
        &self,
        req: &SessionCreateRequest,
    ) -> Result<(SessionCreateResponse, u64), ProtoError> {
        self.call("session.create", req).await
    }

    pub async fn session_destroy(&self, session_id: &str) -> Result<u64, ProtoError> {
        let out = self
            .call_raw(
                "session.destroy",
                Some(serde_json::json!({ "session_id": session_id })),
            )
            .await?;
        Ok(out.seq)
    }

    pub async fn session_set_label(
        &self,
        session_id: &str,
        label: &str,
    ) -> Result<(RevisionResponse, u64), ProtoError> {
        self.call(
            "session.set_label",
            &serde_json::json!({ "session_id": session_id, "label": label }),
        )
        .await
    }

    pub async fn session_set_window_label(
        &self,
        session_id: &str,
        window_id: &str,
        label: &str,
    ) -> Result<(RevisionResponse, u64), ProtoError> {
        self.call(
            "session.set_window_label",
            &serde_json::json!({ "session_id": session_id, "window_id": window_id, "label": label }),
        )
        .await
    }

    pub async fn session_set_block_label(
        &self,
        session_id: &str,
        block_id: &str,
        label: &str,
    ) -> Result<(RevisionResponse, u64), ProtoError> {
        self.call(
            "session.set_block_label",
            &serde_json::json!({ "session_id": session_id, "block_id": block_id, "label": label }),
        )
        .await
    }

    pub async fn session_new_window(
        &self,
        req: &SessionNewWindowRequest,
    ) -> Result<(CreatedWindowInfo, u64), ProtoError> {
        self.call("session.new_window", req).await
    }

    pub async fn session_new_split(
        &self,
        req: &SessionNewSplitRequest,
    ) -> Result<(SessionNewSplitResponse, u64), ProtoError> {
        self.call("session.new_split", req).await
    }

    pub async fn session_new_layer(
        &self,
        req: &SessionNewLayerRequest,
    ) -> Result<(SessionNewLayerResponse, u64), ProtoError> {
        self.call("session.new_layer", req).await
    }

    pub async fn session_float_block(
        &self,
        req: &SessionFloatBlockRequest,
    ) -> Result<(SessionFloatBlockResponse, u64), ProtoError> {
        self.call("session.float_block", req).await
    }

    pub async fn session_zoom_block(
        &self,
        req: &SessionZoomBlockRequest,
    ) -> Result<(SessionZoomBlockResponse, u64), ProtoError> {
        self.call("session.zoom_block", req).await
    }

    pub async fn session_resize_pane(
        &self,
        req: &SessionResizePaneRequest,
    ) -> Result<(RevisionResponse, u64), ProtoError> {
        self.call("session.resize_pane", req).await
    }

    pub async fn session_resize_layer(
        &self,
        req: &SessionResizeLayerRequest,
    ) -> Result<(RevisionResponse, u64), ProtoError> {
        self.call("session.resize_layer", req).await
    }

    pub async fn session_set_layer_state(
        &self,
        req: &SessionSetLayerStateRequest,
    ) -> Result<(RevisionResponse, u64), ProtoError> {
        self.call("session.set_layer_state", req).await
    }

    pub async fn session_own_block(
        &self,
        req: &SessionOwnBlockRequest,
    ) -> Result<(BlockOwnershipResponse, u64), ProtoError> {
        self.call("session.own_block", req).await
    }

    pub async fn session_disown_block(
        &self,
        req: &SessionDisownBlockRequest,
    ) -> Result<(BlockOwnershipResponse, u64), ProtoError> {
        self.call("session.disown_block", req).await
    }

    pub async fn session_focus_window(
        &self,
        session_id: &str,
        window_id: &str,
    ) -> Result<(RevisionResponse, u64), ProtoError> {
        self.call(
            "session.focus_window",
            &serde_json::json!({ "session_id": session_id, "window_id": window_id }),
        )
        .await
    }

    pub async fn session_focus_block(
        &self,
        session_id: &str,
        block_id: &str,
    ) -> Result<(RevisionResponse, u64), ProtoError> {
        self.call(
            "session.focus_block",
            &serde_json::json!({ "session_id": session_id, "block_id": block_id }),
        )
        .await
    }

    pub async fn session_close_window(
        &self,
        session_id: &str,
        window_id: &str,
        undoable: bool,
    ) -> Result<(RevisionResponse, u64), ProtoError> {
        self.call(
            "session.close_window",
            &serde_json::json!({
                "session_id": session_id,
                "window_id": window_id,
                "undoable": undoable,
            }),
        )
        .await
    }

    pub async fn session_close_layer(
        &self,
        session_id: &str,
        layer_id: &str,
        undoable: bool,
    ) -> Result<(RevisionResponse, u64), ProtoError> {
        self.call(
            "session.close_layer",
            &serde_json::json!({
                "session_id": session_id,
                "layer_id": layer_id,
                "undoable": undoable,
            }),
        )
        .await
    }

    pub async fn block_close(&self, req: &BlockCloseRequest) -> Result<u64, ProtoError> {
        let out = self
            .call_raw("block.close", Some(serde_json::to_value(req)?))
            .await?;
        Ok(out.seq)
    }

    pub async fn block_event(&self, req: &BlockEventRequest) -> Result<u64, ProtoError> {
        let out = self
            .call_raw("block.event", Some(serde_json::to_value(req)?))
            .await?;
        Ok(out.seq)
    }

    pub async fn data_connect(
        &self,
        req: &DataConnectRequest,
    ) -> Result<(DataConnectResponse, u64), ProtoError> {
        self.call("data.connect", req).await
    }

    pub async fn client_keymap(
        &self,
        req: &ClientKeymapRequest,
    ) -> Result<(ClientKeymapResponse, u64), ProtoError> {
        self.call("client.keymap", req).await
    }

    pub async fn client_action(
        &self,
        req: &ClientActionRequest,
    ) -> Result<(ClientActionResponse, u64), ProtoError> {
        self.call("client.action", req).await
    }

    pub async fn client_key(
        &self,
        req: &ClientKeyRequest,
    ) -> Result<(ClientKeyResponse, u64), ProtoError> {
        self.call("client.key", req).await
    }

    pub async fn client_perform(
        &self,
        req: &ClientPerformRequest,
    ) -> Result<(ClientPerformResponse, u64), ProtoError> {
        self.call("client.perform", req).await
    }

    pub async fn client_list(&self) -> Result<(ClientListResponse, u64), ProtoError> {
        self.call_empty("client.list").await
    }

    pub async fn client_undo_close(&self, session_id: &str) -> Result<(Value, u64), ProtoError> {
        let out = self
            .call_raw(
                "client.undo_close",
                Some(serde_json::json!({ "session_id": session_id })),
            )
            .await?;
        Ok((out.payload, out.seq))
    }

    pub async fn hosts_list(&self) -> Result<(HostsListResponse, u64), ProtoError> {
        self.call_empty("hosts.list").await
    }

    pub async fn hosts_put(
        &self,
        req: &HostsPutRequest,
    ) -> Result<(HostsPutResponse, u64), ProtoError> {
        self.call("hosts.put", req).await
    }

    pub async fn hosts_remove(
        &self,
        req: &HostsRemoveRequest,
    ) -> Result<(HostsRemoveResponse, u64), ProtoError> {
        self.call("hosts.remove", req).await
    }

    pub async fn server_status(&self) -> Result<(ServerStatus, u64), ProtoError> {
        self.call_empty("server.status").await
    }

    pub async fn server_config_reload(&self) -> Result<(Value, u64), ProtoError> {
        let out = self
            .call_raw("server.config_reload", Some(serde_json::json!({})))
            .await?;
        Ok((out.payload, out.seq))
    }

    // ------------------------------------------------------------------------
    // `com.superlogical.terminal.*` Block Method Wrappers
    // ------------------------------------------------------------------------

    pub async fn terminal_write(
        &self,
        session_id: &str,
        block_id: &str,
        data: impl Into<String>,
    ) -> Result<(TerminalWriteResponse, u64), ProtoError> {
        self.call_block(
            session_id,
            block_id,
            "com.superlogical.terminal.write",
            Some(TerminalWriteArgs { data: data.into() }),
        )
        .await
    }

    pub async fn terminal_resize(
        &self,
        session_id: &str,
        block_id: &str,
        args: TerminalResizeArgs,
    ) -> Result<u64, ProtoError> {
        let (_, seq): (Value, u64) = self
            .call_block(
                session_id,
                block_id,
                "com.superlogical.terminal.resize",
                Some(args),
            )
            .await?;
        Ok(seq)
    }

    pub async fn terminal_set_theme(
        &self,
        session_id: &str,
        block_id: &str,
        args: TerminalSetThemeArgs,
    ) -> Result<u64, ProtoError> {
        let (_, seq): (Value, u64) = self
            .call_block(
                session_id,
                block_id,
                "com.superlogical.terminal.set_theme",
                Some(args),
            )
            .await?;
        Ok(seq)
    }

    pub async fn terminal_title(
        &self,
        session_id: &str,
        block_id: &str,
    ) -> Result<(TerminalTitleResponse, u64), ProtoError> {
        self.call_block::<(), _>(
            session_id,
            block_id,
            "com.superlogical.terminal.title",
            None,
        )
        .await
    }

    pub async fn terminal_pwd(
        &self,
        session_id: &str,
        block_id: &str,
    ) -> Result<(TerminalPwdResponse, u64), ProtoError> {
        self.call_block::<(), _>(session_id, block_id, "com.superlogical.terminal.pwd", None)
            .await
    }

    pub async fn terminal_process(
        &self,
        session_id: &str,
        block_id: &str,
    ) -> Result<(TerminalProcessResponse, u64), ProtoError> {
        self.call_block::<(), _>(
            session_id,
            block_id,
            "com.superlogical.terminal.process",
            None,
        )
        .await
    }

    pub async fn terminal_program_status(
        &self,
        session_id: &str,
        block_id: &str,
    ) -> Result<(TerminalProgramStatusResponse, u64), ProtoError> {
        self.call_block::<(), _>(
            session_id,
            block_id,
            "com.superlogical.terminal.program_status",
            None,
        )
        .await
    }

    pub async fn terminal_size(
        &self,
        session_id: &str,
        block_id: &str,
    ) -> Result<(TerminalSizeResponse, u64), ProtoError> {
        self.call_block::<(), _>(session_id, block_id, "com.superlogical.terminal.size", None)
            .await
    }

    pub async fn terminal_modes(
        &self,
        session_id: &str,
        block_id: &str,
    ) -> Result<(TerminalModesResponse, u64), ProtoError> {
        self.call_block::<(), _>(
            session_id,
            block_id,
            "com.superlogical.terminal.modes",
            None,
        )
        .await
    }

    pub async fn terminal_clipboard_read(
        &self,
        session_id: &str,
        block_id: &str,
        args: TerminalClipboardReadArgs,
    ) -> Result<(TerminalClipboardReadResponse, u64), ProtoError> {
        self.call_block(
            session_id,
            block_id,
            "com.superlogical.terminal.clipboard_read",
            Some(args),
        )
        .await
    }

    pub async fn terminal_list_dir(
        &self,
        session_id: &str,
        block_id: &str,
        args: TerminalListDirArgs,
    ) -> Result<(TerminalListDirResponse, u64), ProtoError> {
        self.call_block(
            session_id,
            block_id,
            "com.superlogical.terminal.list_dir",
            Some(args),
        )
        .await
    }

    pub async fn terminal_ports(
        &self,
        session_id: &str,
        block_id: &str,
    ) -> Result<(TerminalPortsResponse, u64), ProtoError> {
        self.call_block::<serde_json::Value, _>(
            session_id,
            block_id,
            "com.superlogical.terminal.ports",
            Some(serde_json::json!({})),
        )
        .await
    }

    pub async fn terminal_blob_read(
        &self,
        session_id: &str,
        block_id: &str,
        args: TerminalBlobReadArgs,
    ) -> Result<(TerminalBlobReadResponse, u64), ProtoError> {
        self.call_block(
            session_id,
            block_id,
            "com.superlogical.terminal.blob_read",
            Some(args),
        )
        .await
    }

    pub async fn tunnel_info(
        &self,
        session_id: &str,
        block_id: &str,
    ) -> Result<(TunnelInfoResponse, u64), ProtoError> {
        self.call_block::<serde_json::Value, _>(
            session_id,
            block_id,
            "com.superlogical.tunnel.info",
            Some(serde_json::json!({})),
        )
        .await
    }

    pub async fn session_new_block(
        &self,
        req: &SessionNewBlockRequest,
    ) -> Result<(SessionNewBlockResponse, u64), ProtoError> {
        self.call("session.new_block", req).await
    }

    pub async fn session_swap_blocks(
        &self,
        req: &SessionSwapBlocksRequest,
    ) -> Result<(SessionSwapBlocksResponse, u64), ProtoError> {
        self.call("session.swap_blocks", req).await
    }

    pub async fn session_transfer_window(
        &self,
        req: &SessionTransferWindowRequest,
    ) -> Result<(SessionTransferWindowResponse, u64), ProtoError> {
        self.call("session.transfer_window", req).await
    }

    pub async fn terminal_upload_begin(
        &self,
        session_id: &str,
        block_id: &str,
        args: TerminalUploadBeginArgs,
    ) -> Result<(TerminalUploadBeginResponse, u64), ProtoError> {
        self.call_block(
            session_id,
            block_id,
            "com.superlogical.terminal.upload_begin",
            Some(args),
        )
        .await
    }

    pub async fn terminal_download_begin(
        &self,
        session_id: &str,
        block_id: &str,
        args: TerminalDownloadBeginArgs,
    ) -> Result<(TerminalDownloadBeginResponse, u64), ProtoError> {
        self.call_block(
            session_id,
            block_id,
            "com.superlogical.terminal.download_begin",
            Some(args),
        )
        .await
    }
}

/// Perform the Rex Control API rev 24 `hello` -> `welcome` handshake within 5 seconds.
async fn perform_control_handshake(
    ws: &mut RexWsStream,
    client_info: &ClientInfo,
    reconnect_token: Option<ReconnectToken>,
) -> Result<WelcomeMessage, ProtoError> {
    let hello = HelloMessage::new(client_info.clone(), reconnect_token);
    let hello_json = serde_json::to_string(&hello)?;
    if hello_json.len() >= MIN_COMPRESS_BYTES {
        return Err(ProtoError::Protocol(format!(
            "hello frame is {} bytes (must be < {MIN_COMPRESS_BYTES} bytes)",
            hello_json.len()
        )));
    }

    ws.send_message(Message::Text(hello_json.into())).await?;

    let frame = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        loop {
            match ws.next_message().await {
                Some(Ok(Message::Text(text))) => return Ok(text),
                Some(Ok(Message::Ping(data))) => {
                    ws.send_message(Message::Pong(data)).await?;
                }
                Some(Ok(Message::Pong(_))) => continue,
                Some(Ok(Message::Close(cf))) => {
                    return Err(ProtoError::Closed(format!(
                        "server closed control WS during handshake: {cf:?}"
                    )));
                }
                Some(Ok(other)) => {
                    return Err(ProtoError::Protocol(format!(
                        "unexpected non-text WS frame during handshake: {other:?}"
                    )));
                }
                Some(Err(err)) => return Err(ProtoError::Transport(err.to_string())),
                None => {
                    return Err(ProtoError::Closed(
                        "control WS closed before welcome".into(),
                    ));
                }
            }
        }
    })
    .await
    .map_err(|_| ProtoError::Timeout("timed out waiting for control welcome".into()))??;

    match serde_json::from_str::<IncomingControlFrame>(frame.as_str())? {
        IncomingControlFrame::Welcome(welcome) => Ok(welcome),
        IncomingControlFrame::Response(resp) => {
            if let Some(err) = resp.error {
                Err(ProtoError::Rpc(err))
            } else {
                Err(ProtoError::Protocol(
                    "expected welcome frame, got response".into(),
                ))
            }
        }
        IncomingControlFrame::Event(_) => Err(ProtoError::Protocol(
            "expected welcome frame, got event".into(),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_control_actor(
    config: ControlConfig,
    mut ws: RexWsStream,
    mut reconnect_token: Option<ReconnectToken>,
    mut priority_rx: mpsc::UnboundedReceiver<OutboundRpc>,
    mut normal_rx: mpsc::UnboundedReceiver<OutboundRpc>,
    mut shutdown_rx: mpsc::UnboundedReceiver<()>,
    notify_tx: mpsc::UnboundedSender<ControlNotification>,
    state: Arc<SharedState>,
) {
    let mut pending: HashMap<String, oneshot::Sender<Result<RpcOutcome, ProtoError>>> =
        HashMap::new();

    loop {
        let disconnect_reason: Option<String> = loop {
            tokio::select! {
                biased;

                // 1. Shutdown always wins immediately.
                _ = shutdown_rx.recv() => {
                    let _ = ws.close(Some(CloseFrame {
                        code: CloseCode::Normal,
                        reason: "client shutdown".into(),
                    })).await;
                    fail_all_pending(&mut pending, "client shutdown");
                    let _ = notify_tx.send(ControlNotification::Disconnected {
                        will_reconnect: false,
                        reason: "client shutdown".into(),
                    });
                    return;
                }

                // 2. Unordered fast-path priority lane (`client.reply`) is ALWAYS drained before normal RPCs.
                Some(rpc) = priority_rx.recv() => {
                    if let Err(err) = send_rpc(&mut ws, rpc, &mut pending).await {
                        break Some(err.to_string());
                    }
                }

                // 3. Incoming WebSocket frames (responses, events, keepalive pings).
                msg = ws.next_message() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            handle_incoming_text(
                                text.as_str(),
                                &mut pending,
                                &notify_tx,
                                &state,
                            );
                        }
                        Some(Ok(Message::Ping(data))) => {
                            if let Err(err) = ws.send_message(Message::Pong(data)).await {
                                break Some(format!("failed to send pong: {err}"));
                            }
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(Message::Close(cf))) => {
                            let reason = cf
                                .as_ref()
                                .map(|c| format!("{} {}", u16::from(c.code), c.reason))
                                .unwrap_or_else(|| "server close".into());
                            break Some(reason);
                        }
                        Some(Ok(Message::Binary(_))) => {
                            tracing::warn!("ignoring unexpected binary frame on /api/control");
                        }
                        Some(Ok(Message::Frame(_))) => {}
                        Some(Err(err)) => {
                            break Some(err.to_string());
                        }
                        None => {
                            break Some("control socket EOF".into());
                        }
                    }
                }

                // 4. Normal RPC lane (ordered and regular methods).
                Some(rpc) = normal_rx.recv() => {
                    if let Err(err) = send_rpc(&mut ws, rpc, &mut pending).await {
                        break Some(err.to_string());
                    }
                }
            }
        };

        let reason = disconnect_reason.unwrap_or_else(|| "disconnected".into());
        fail_all_pending(&mut pending, &reason);

        if !config.auto_reconnect {
            let _ = notify_tx.send(ControlNotification::Disconnected {
                will_reconnect: false,
                reason,
            });
            return;
        }

        let _ = notify_tx.send(ControlNotification::Disconnected {
            will_reconnect: true,
            reason,
        });

        // Attempt automatic reconnect using `reconnect_token` within 60s grace window,
        // falling back cleanly to a fresh session handshake if grace expires or token fails.
        let lost_at = Instant::now();
        let mut backoff = Duration::from_millis(200);

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    let _ = notify_tx.send(ControlNotification::Disconnected {
                        will_reconnect: false,
                        reason: "client shutdown".into(),
                    });
                    return;
                }
                _ = tokio::time::sleep(backoff) => {}
            }

            if lost_at.elapsed() >= config.reconnect_grace {
                reconnect_token = None;
            }

            match dial_websocket(&config.endpoint, "/api/control", CONTROL_SUBPROTOCOL).await {
                Ok(mut new_ws) => {
                    let token_attempt = reconnect_token.clone();
                    match perform_control_handshake(&mut new_ws, &config.client_info, token_attempt)
                        .await
                    {
                        Ok(welcome) => {
                            ws = new_ws;
                            reconnect_token = welcome.client.reconnect_token.clone();
                            state.last_event_seq.store(0, Ordering::Relaxed);
                            state
                                .api_revision
                                .store(u64::from(welcome.api_revision), Ordering::Relaxed);
                            if let Ok(mut guard) = state.client_id.lock() {
                                *guard = Some(welcome.client.id.clone());
                            }
                            let _ = notify_tx.send(ControlNotification::Connected {
                                client_id: welcome.client.id,
                                resumed: welcome.client.resumed,
                                api_revision: welcome.api_revision,
                                max_message_bytes: welcome.limits.max_message_bytes,
                            });
                            break;
                        }
                        Err(err) => {
                            tracing::warn!(
                                error = %err,
                                "control reconnect handshake failed; clearing reconnect_token for fresh retry"
                            );
                            reconnect_token = None;
                        }
                    }
                }
                Err(err) => {
                    tracing::debug!(error = %err, "control reconnect dial failed");
                }
            }

            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    }
}

async fn send_rpc(
    ws: &mut RexWsStream,
    rpc: OutboundRpc,
    pending: &mut HashMap<String, oneshot::Sender<Result<RpcOutcome, ProtoError>>>,
) -> Result<(), ProtoError> {
    let frame = ControlRequest {
        msg_type: "request",
        id: &rpc.id,
        method: &rpc.method,
        payload: rpc.payload,
    };
    let text = match serde_json::to_string(&frame) {
        Ok(t) => t,
        Err(err) => {
            let _ = rpc.respond_to.send(Err(ProtoError::Serde(err.to_string())));
            return Ok(());
        }
    };

    pending.insert(rpc.id.clone(), rpc.respond_to);
    if let Err(err) = ws.send_message(Message::Text(text.into())).await {
        if let Some(tx) = pending.remove(&rpc.id) {
            let _ = tx.send(Err(ProtoError::Transport(err.to_string())));
        }
        return Err(err);
    }
    Ok(())
}

fn handle_incoming_text(
    text: &str,
    pending: &mut HashMap<String, oneshot::Sender<Result<RpcOutcome, ProtoError>>>,
    notify_tx: &mpsc::UnboundedSender<ControlNotification>,
    state: &Arc<SharedState>,
) {
    match serde_json::from_str::<IncomingControlFrame>(text) {
        Ok(IncomingControlFrame::Response(resp)) => {
            if resp.seq > 0 {
                state.last_event_seq.fetch_max(resp.seq, Ordering::Relaxed);
            }
            if let Some(tx) = pending.remove(&resp.id) {
                if let Some(err) = resp.error {
                    let _ = tx.send(Err(ProtoError::Rpc(err)));
                } else {
                    let _ = tx.send(Ok(RpcOutcome {
                        seq: resp.seq,
                        payload: resp.payload.unwrap_or(Value::Object(Default::default())),
                    }));
                }
            } else {
                tracing::debug!(id = %resp.id, "received response for unknown/timed-out request");
            }
        }
        Ok(IncomingControlFrame::Event(ev_frame)) => {
            state
                .last_event_seq
                .fetch_max(ev_frame.seq, Ordering::Relaxed);
            match ServerEvent::from_raw(ev_frame.payload) {
                Ok(event) => {
                    let _ = notify_tx.send(ControlNotification::Event {
                        seq: ev_frame.seq,
                        event: Box::new(event),
                    });
                }
                Err(err) => {
                    tracing::warn!(seq = ev_frame.seq, error = %err, "failed to decode server event");
                }
            }
        }
        Ok(IncomingControlFrame::Welcome(_)) => {
            tracing::warn!("unexpected duplicate welcome frame on control connection");
        }
        Err(err) => {
            tracing::warn!(error = %err, "failed to parse incoming control JSON frame");
        }
    }
}

fn fail_all_pending(
    pending: &mut HashMap<String, oneshot::Sender<Result<RpcOutcome, ProtoError>>>,
    reason: &str,
) {
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(ProtoError::Closed(reason.to_string())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::protocol::Role;

    /// Proves that `reply_fast` (`client.reply`) executes concurrently over the unordered fast-path
    /// lane while an ordered `client.action` is stalled awaiting a `rex.ui` prompt answer.
    #[tokio::test]
    async fn unordered_client_reply_does_not_block_behind_in_flight_client_action() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let client_ws =
            tokio_tungstenite::WebSocketStream::from_raw_socket(client_io, Role::Client, None)
                .await;
        let mut server_ws =
            tokio_tungstenite::WebSocketStream::from_raw_socket(server_io, Role::Server, None)
                .await;

        let server_task = tokio::spawn(async move {
            // 1. Expect hello
            let hello_msg = server_ws.next().await.unwrap().unwrap();
            let hello: Value = serde_json::from_str(hello_msg.to_text().unwrap()).unwrap();
            assert_eq!(hello["type"], "hello");
            assert_eq!(hello["api_revision"], 24);
            assert_eq!(hello["client"]["kind"], "app");

            // 2. Send welcome
            server_ws
                .send(Message::Text(
                    serde_json::json!({
                        "type": "welcome",
                        "protocol": "rex.control.v1",
                        "limits": { "max_message_bytes": 1048576 },
                        "carriers": [],
                        "client": {
                            "id": "client:test-1",
                            "reconnect_token": "tok1234567890123456789",
                            "resumed": false
                        },
                        "features": [],
                        "api_revision": 24,
                        "api_server_revision": 24,
                        "api_min_revision": 0
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();

            // 3. Expect client.action request (do NOT reply yet — hold it open waiting for prompt!)
            let action_msg = server_ws.next().await.unwrap().unwrap();
            let action_req: Value = serde_json::from_str(action_msg.to_text().unwrap()).unwrap();
            assert_eq!(action_req["method"], "client.action");
            let action_id = action_req["id"].as_str().unwrap().to_string();

            // 4. Push client_prompt_requested while client.action is still pending
            server_ws
                .send(Message::Text(
                    serde_json::json!({
                        "type": "event",
                        "seq": 1,
                        "payload": {
                            "type": "client_prompt_requested",
                            "payload": {
                                "call_id": "prompt-99",
                                "action": "t3_rename",
                                "timeout_ms": 30000,
                                "prompt": {
                                    "kind": "input",
                                    "title": "Rename Thread"
                                }
                            }
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();

            // 5. Expect unordered client.reply to arrive IMMEDIATELY while client.action is blocked!
            let reply_msg = server_ws.next().await.unwrap().unwrap();
            let reply_req: Value = serde_json::from_str(reply_msg.to_text().unwrap()).unwrap();
            assert_eq!(reply_req["method"], "client.reply");
            assert_eq!(reply_req["payload"]["call_id"], "prompt-99");
            assert_eq!(reply_req["payload"]["result"]["text"], "New Title");
            let reply_id = reply_req["id"].as_str().unwrap().to_string();

            // 6. Respond to client.reply, then unblock and respond to client.action
            server_ws
                .send(Message::Text(
                    serde_json::json!({
                        "type": "response",
                        "id": reply_id,
                        "seq": 2,
                        "payload": {}
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();

            server_ws
                .send(Message::Text(
                    serde_json::json!({
                        "type": "response",
                        "id": action_id,
                        "seq": 3,
                        "payload": {
                            "result": {"renamed": true},
                            "client_actions": []
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
        });

        let mut cfg = ControlConfig::new(RexEndpoint::parse("unix:///tmp/mock.sock").unwrap());
        cfg.auto_reconnect = false;

        let (client, welcome, mut events) =
            ControlClient::connect_with_stream(cfg, RexWsStream::from_duplex(client_ws))
                .await
                .unwrap();
        assert_eq!(welcome.client.id, "client:test-1");
        assert_eq!(client.api_revision(), 24);

        // Drain initial Connected notification
        let first = events.recv().await.unwrap();
        assert!(matches!(first, ControlNotification::Connected { .. }));

        // Spawn blocking ordered client.action concurrently
        let action_client = client.clone();
        let action_fut = tokio::spawn(async move {
            action_client
                .client_action(&ClientActionRequest {
                    action: "t3_rename".into(),
                    args: None,
                    origin: Some("key".into()),
                    session_id: Some("session:1".into()),
                    block_id: None,
                    servers: None,
                    server: None,
                })
                .await
        });

        // Receive client_prompt_requested while action_fut is STILL awaiting!
        let ev = events.recv().await.unwrap();
        let call_id = match ev {
            ControlNotification::Event { seq: 1, event } => match *event {
                ServerEvent::ClientPromptRequested(req) => req.call_id,
                other => panic!("expected ClientPromptRequested, got {other:?}"),
            },
            other => panic!("expected Event(seq=1), got {other:?}"),
        };

        // Reply on fast-path lane while client.action is still in-flight!
        let reply_seq = client
            .reply_fast(&ClientReplyRequest::prompt_text(call_id, "New Title"))
            .await
            .unwrap();
        assert_eq!(reply_seq, 2);

        // Now client.action unblocks and completes!
        let (action_res, action_seq) = action_fut.await.unwrap().unwrap();
        assert_eq!(action_seq, 3);
        assert_eq!(action_res.result.unwrap()["renamed"], true);
        assert_eq!(client.last_event_seq(), 3);

        // Verify revision gating helper
        assert!(client.should_fetch_view("session:1", 5));
        client.record_view_revision("session:1", 5);
        assert!(!client.should_fetch_view("session:1", 4));
        assert!(!client.should_fetch_view("session:1", 5));
        assert!(client.should_fetch_view("session:1", 6));

        client.shutdown();
        server_task.await.unwrap();
    }
}
