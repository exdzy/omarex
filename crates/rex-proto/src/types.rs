//! Strongly-typed serde models for Rex Control API revision 24 & terminal block methods.
//!
//! All array fields deserialize JSON `null` (produced by Go nil slices) as empty `Vec`s.
//! Sensitive fields such as `reconnect_token` are wrapped in [`ReconnectToken`] so they are
//! unconditionally redacted in `Debug` and `Display` output.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// Rex API revision implemented by `omarex`.
pub const TARGET_API_REVISION: u32 = 24;
/// Minimum Rex API revision accepted during handshake.
pub const MIN_API_REVISION: u32 = 0;

/// Control WebSocket subprotocol.
pub const CONTROL_SUBPROTOCOL: &str = "rex.control.v1";
/// Terminal data WebSocket subprotocol (GHOSTSNP v1 + live VT + `rex1;` APCs).
pub const DATA_TERMINAL_SUBPROTOCOL: &str = "rex.data.terminal.v1";
/// Bidirectional binary data WebSocket subprotocol (uploads, downloads, fs, tunnels).
pub const DATA_BYTES_SUBPROTOCOL: &str = "rex.data.bytes.v1";

/// Default server control message limit (1 MiB).
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 1_048_576;
/// Minimum payload size eligible for compression (frames `< 512B` and `hello` must NEVER be compressed).
pub const MIN_COMPRESS_BYTES: usize = 512;

/// Deserializes JSON `null` as `T::default()` (Go marshals nil slices as `null`).
pub fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    T: Default + Deserialize<'de>,
    D: Deserializer<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Returns `true` if `method` runs on the Rex server's strict FIFO ordered execution lane.
///
/// Crucially, `client.reply` is **NOT** ordered so it can never deadlock behind an in-flight
/// `client.action` awaiting a `rex.ui` prompt or remote `client_call_requested` proxy.
pub fn is_ordered_method(method: &str) -> bool {
    matches!(
        method,
        "block.close"
            | "block.event"
            | "client.action"
            | "client.key"
            | "client.undo_close"
            | "focus_history.reverse"
            | "session.close_layer"
            | "session.close_window"
            | "session.detach_block"
            | "session.disown_block"
            | "session.float_block"
            | "session.focus_block"
            | "session.focus_direction"
            | "session.focus_next_pane"
            | "session.focus_next_window"
            | "session.focus_previous_pane"
            | "session.focus_previous_window"
            | "session.focus_window"
            | "session.move_block"
            | "session.move_window"
            | "session.new_layer"
            | "session.new_split"
            | "session.new_window"
            | "session.own_block"
            | "session.set_layer_state"
            | "session.swap_blocks"
            | "session.transfer_window"
            | "session.zoom_block"
    )
}

/// Sensitive control reconnect token that is always redacted in `Debug` and `Display`.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct ReconnectToken(String);

impl ReconnectToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for ReconnectToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReconnectToken([REDACTED])")
    }
}

impl fmt::Display for ReconnectToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl Serialize for ReconnectToken {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ReconnectToken {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self)
    }
}

// ============================================================================
// Control WebSocket Handshake & Framing (`rex.control.v1`)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientInfo {
    /// Must be `"app"` for GUI clients so Rex dispatches `client_prompt_requested` and `tr3x` discovers us.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl ClientInfo {
    /// Default `omarex` app metadata (`kind = "app"`, `platform = "linux"`).
    pub fn omarex_default() -> Self {
        Self {
            kind: Some("app".into()),
            platform: Some("linux".into()),
            name: Some("omarex".into()),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            label: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub api_revision: u32,
    pub api_min_revision: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconnect_token: Option<ReconnectToken>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<ClientInfo>,
}

impl HelloMessage {
    pub fn new(client: ClientInfo, reconnect_token: Option<ReconnectToken>) -> Self {
        Self {
            msg_type: "hello".into(),
            api_revision: TARGET_API_REVISION,
            api_min_revision: MIN_API_REVISION,
            reconnect_token,
            client: Some(client),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WelcomeLimits {
    #[serde(default = "default_max_message_bytes")]
    pub max_message_bytes: usize,
}

fn default_max_message_bytes() -> usize {
    DEFAULT_MAX_MESSAGE_BYTES
}

impl Default for WelcomeLimits {
    fn default() -> Self {
        Self {
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WelcomeCarrier {
    pub kind: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub alpn: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WelcomeClient {
    pub id: String,
    #[serde(default)]
    pub reconnect_token: Option<ReconnectToken>,
    #[serde(default)]
    pub resumed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WelcomeMessage {
    #[serde(default)]
    pub protocol: String,
    #[serde(default)]
    pub limits: WelcomeLimits,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub carriers: Vec<WelcomeCarrier>,
    pub client: WelcomeClient,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub features: Vec<String>,
    #[serde(default)]
    pub api_revision: u32,
    #[serde(default)]
    pub api_server_revision: u32,
    #[serde(default)]
    pub api_min_revision: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ControlRequest<'a> {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub id: &'a str,
    pub method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcError {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub status: u16,
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.status != 0 {
            write!(f, "{} ({}): {}", self.code, self.status, self.message)
        } else {
            write!(f, "{}: {}", self.code, self.message)
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ControlResponseFrame {
    pub id: String,
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub payload: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ControlEventFrame {
    pub seq: u64,
    pub payload: RawTypedPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawTypedPayload {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IncomingControlFrame {
    Welcome(WelcomeMessage),
    Response(ControlResponseFrame),
    Event(ControlEventFrame),
}

// ============================================================================
// Principal & Connected Client Identity
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Principal {
    pub kind: String,
    pub transport: String,
    pub display: String,
    #[serde(default)]
    pub remote_addr: Option<String>,
    #[serde(default)]
    pub uid: Option<u32>,
    #[serde(default)]
    pub gid: Option<u32>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub sid: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub login_name: Option<String>,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub tags: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub path: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectedClient {
    pub client_id: String,
    #[serde(default)]
    pub info: Option<ClientInfo>,
    pub principal: Principal,
    pub connected_at: String,
    #[serde(default)]
    pub connection_state: Option<String>,
    #[serde(default)]
    pub connection_lost_at: Option<String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub session_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequesterInfo {
    pub client_id: String,
    #[serde(default)]
    pub info: Option<ClientInfo>,
    pub principal: Principal,
}

// ============================================================================
// Server, Session & Block Events
// ============================================================================

/// Parsed server-level or session-wrapped event received over `/api/control`.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerEvent {
    ClientJoined {
        client: ConnectedClient,
    },
    ClientChanged {
        client: ConnectedClient,
    },
    ClientLeft {
        client_id: String,
    },
    ClientPromptRequested(ClientPromptRequestedEvent),
    ClientPromptEnded(ClientPromptEndedEvent),
    ClientPerformRequested(ClientPerformRequestedEvent),
    ClientPerformed(ClientPerformedEvent),
    ClientCallRequested(ClientCallRequestedEvent),
    ClientScriptEventsDropped {
        session_id: String,
        client_id: String,
        event_name: String,
    },
    EventDropped(EventDroppedEvent),
    HostsChanged {
        hash: String,
    },
    KeymapChanged {
        generation: u64,
        hash: String,
    },
    SessionCreated {
        session: SessionSummary,
    },
    SessionDestroyed {
        session_id: String,
    },
    SessionMoved {
        session_id: String,
        before_session_id: Option<String>,
    },
    TailscaleStatusChanged(TailscaleStatus),
    SessionEvent {
        session_id: String,
        event: SessionEvent,
    },
    Unknown {
        event_type: String,
        payload: Value,
    },
}

impl ServerEvent {
    pub fn from_raw(raw: RawTypedPayload) -> Result<Self, serde_json::Error> {
        let p = raw.payload;
        match raw.event_type.as_str() {
            "client_joined" => {
                #[derive(Deserialize)]
                struct Wrap {
                    client: ConnectedClient,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::ClientJoined { client: w.client })
            }
            "client_changed" => {
                #[derive(Deserialize)]
                struct Wrap {
                    client: ConnectedClient,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::ClientChanged { client: w.client })
            }
            "client_left" => {
                #[derive(Deserialize)]
                struct Wrap {
                    client_id: String,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::ClientLeft {
                    client_id: w.client_id,
                })
            }
            "client_prompt_requested" => {
                Ok(Self::ClientPromptRequested(serde_json::from_value(p)?))
            }
            "client_prompt_ended" => Ok(Self::ClientPromptEnded(serde_json::from_value(p)?)),
            "client_perform_requested" => {
                Ok(Self::ClientPerformRequested(serde_json::from_value(p)?))
            }
            "client_performed" => Ok(Self::ClientPerformed(serde_json::from_value(p)?)),
            "client_call_requested" => Ok(Self::ClientCallRequested(serde_json::from_value(p)?)),
            "client_script_events_dropped" => {
                #[derive(Deserialize)]
                struct Wrap {
                    session_id: String,
                    client_id: String,
                    event_name: String,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::ClientScriptEventsDropped {
                    session_id: w.session_id,
                    client_id: w.client_id,
                    event_name: w.event_name,
                })
            }
            "event_dropped" => Ok(Self::EventDropped(serde_json::from_value(p)?)),
            "hosts_changed" => {
                #[derive(Deserialize)]
                struct Wrap {
                    hash: String,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::HostsChanged { hash: w.hash })
            }
            "keymap_changed" => {
                #[derive(Deserialize)]
                struct Wrap {
                    generation: u64,
                    hash: String,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::KeymapChanged {
                    generation: w.generation,
                    hash: w.hash,
                })
            }
            "session_created" => {
                #[derive(Deserialize)]
                struct Wrap {
                    session: SessionSummary,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::SessionCreated { session: w.session })
            }
            "session_destroyed" => {
                #[derive(Deserialize)]
                struct Wrap {
                    session_id: String,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::SessionDestroyed {
                    session_id: w.session_id,
                })
            }
            "session_moved" => {
                #[derive(Deserialize)]
                struct Wrap {
                    session_id: String,
                    #[serde(default)]
                    before_session_id: Option<String>,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::SessionMoved {
                    session_id: w.session_id,
                    before_session_id: w.before_session_id,
                })
            }
            "tailscale_status_changed" => {
                Ok(Self::TailscaleStatusChanged(serde_json::from_value(p)?))
            }
            "session_event" => {
                #[derive(Deserialize)]
                struct Wrap {
                    session_id: String,
                    event: RawTypedPayload,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::SessionEvent {
                    session_id: w.session_id,
                    event: SessionEvent::from_raw(w.event)?,
                })
            }
            _ => Ok(Self::Unknown {
                event_type: raw.event_type,
                payload: p,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    ActiveWindowChanged {
        window_id: Option<String>,
    },
    BlockCreated {
        block_id: String,
    },
    BlockClosed {
        block_id: String,
    },
    BlockOwnerChanged {
        block_id: String,
        owner: Option<String>,
        previous: Option<String>,
        reason: Option<String>,
    },
    BlockEvent(BlockEventPayload),
    ClientConnected {
        client_id: String,
        principal: Principal,
        connected_at: String,
    },
    ClientConnectionChanged {
        client_id: String,
        principal: Principal,
        connection_state: String,
        connection_lost_at: Option<String>,
    },
    ClientDisconnected {
        client_id: String,
        principal: Principal,
    },
    CreatorChanged,
    SessionLabelChanged {
        label: String,
    },
    SessionViewChanged {
        revision: u64,
    },
    WindowCreated {
        window_id: String,
        label: String,
    },
    WindowClosed {
        window_id: String,
    },
    WindowLabelChanged {
        window_id: String,
        label: String,
    },
    Unknown {
        event_type: String,
        payload: Value,
    },
}

impl SessionEvent {
    pub fn from_raw(raw: RawTypedPayload) -> Result<Self, serde_json::Error> {
        let p = raw.payload;
        match raw.event_type.as_str() {
            "active_window_changed" => {
                #[derive(Deserialize)]
                struct Wrap {
                    #[serde(default)]
                    window_id: Option<String>,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::ActiveWindowChanged {
                    window_id: w.window_id,
                })
            }
            "block_created" => {
                #[derive(Deserialize)]
                struct Wrap {
                    block_id: String,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::BlockCreated {
                    block_id: w.block_id,
                })
            }
            "block_closed" => {
                #[derive(Deserialize)]
                struct Wrap {
                    block_id: String,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::BlockClosed {
                    block_id: w.block_id,
                })
            }
            "block_owner_changed" => {
                #[derive(Deserialize)]
                struct Wrap {
                    block_id: String,
                    #[serde(default)]
                    owner: Option<String>,
                    #[serde(default)]
                    previous: Option<String>,
                    #[serde(default)]
                    reason: Option<String>,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::BlockOwnerChanged {
                    block_id: w.block_id,
                    owner: w.owner,
                    previous: w.previous,
                    reason: w.reason,
                })
            }
            "block_event" => Ok(Self::BlockEvent(serde_json::from_value(p)?)),
            "client_connected" => {
                #[derive(Deserialize)]
                struct Wrap {
                    client_id: String,
                    principal: Principal,
                    connected_at: String,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::ClientConnected {
                    client_id: w.client_id,
                    principal: w.principal,
                    connected_at: w.connected_at,
                })
            }
            "client_connection_changed" => {
                #[derive(Deserialize)]
                struct Wrap {
                    client_id: String,
                    principal: Principal,
                    connection_state: String,
                    #[serde(default)]
                    connection_lost_at: Option<String>,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::ClientConnectionChanged {
                    client_id: w.client_id,
                    principal: w.principal,
                    connection_state: w.connection_state,
                    connection_lost_at: w.connection_lost_at,
                })
            }
            "client_disconnected" => {
                #[derive(Deserialize)]
                struct Wrap {
                    client_id: String,
                    principal: Principal,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::ClientDisconnected {
                    client_id: w.client_id,
                    principal: w.principal,
                })
            }
            "creator_changed" => Ok(Self::CreatorChanged),
            "session_label_changed" => {
                #[derive(Deserialize)]
                struct Wrap {
                    label: String,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::SessionLabelChanged { label: w.label })
            }
            "session_view_changed" => {
                #[derive(Deserialize)]
                struct Wrap {
                    revision: u64,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::SessionViewChanged {
                    revision: w.revision,
                })
            }
            "window_created" => {
                #[derive(Deserialize)]
                struct Wrap {
                    window_id: String,
                    #[serde(default)]
                    label: String,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::WindowCreated {
                    window_id: w.window_id,
                    label: w.label,
                })
            }
            "window_closed" => {
                #[derive(Deserialize)]
                struct Wrap {
                    window_id: String,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::WindowClosed {
                    window_id: w.window_id,
                })
            }
            "window_label_changed" => {
                #[derive(Deserialize)]
                struct Wrap {
                    window_id: String,
                    label: String,
                }
                let w: Wrap = serde_json::from_value(p)?;
                Ok(Self::WindowLabelChanged {
                    window_id: w.window_id,
                    label: w.label,
                })
            }
            _ => Ok(Self::Unknown {
                event_type: raw.event_type,
                payload: p,
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BlockEventPayload {
    pub block_id: String,
    #[serde(default)]
    pub creator_name: Option<String>,
    pub name: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventDroppedEvent {
    pub event_type: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub block_id: Option<String>,
    #[serde(default)]
    pub creator_name: Option<String>,
    #[serde(default)]
    pub block_event_name: Option<String>,
    pub size: u64,
}

// ============================================================================
// Prompts (`rex.ui`), Client Perform, and Multi-Server Call Proxy
// ============================================================================

/// All 16 standard `rex.ui.pick` icons plus forward-compatible `Unknown`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PromptIcon {
    Document,
    Folder,
    Branch,
    Terminal,
    Code,
    Server,
    Database,
    Package,
    Globe,
    Link,
    Person,
    Tag,
    Clock,
    Star,
    Gear,
    Plus,
    Unknown(String),
}

impl PromptIcon {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Document => "document",
            Self::Folder => "folder",
            Self::Branch => "branch",
            Self::Terminal => "terminal",
            Self::Code => "code",
            Self::Server => "server",
            Self::Database => "database",
            Self::Package => "package",
            Self::Globe => "globe",
            Self::Link => "link",
            Self::Person => "person",
            Self::Tag => "tag",
            Self::Clock => "clock",
            Self::Star => "star",
            Self::Gear => "gear",
            Self::Plus => "plus",
            Self::Unknown(s) => s.as_str(),
        }
    }
}

impl Serialize for PromptIcon {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PromptIcon {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "document" => Self::Document,
            "folder" => Self::Folder,
            "branch" => Self::Branch,
            "terminal" => Self::Terminal,
            "code" => Self::Code,
            "server" => Self::Server,
            "database" => Self::Database,
            "package" => Self::Package,
            "globe" => Self::Globe,
            "link" => Self::Link,
            "person" => Self::Person,
            "tag" => Self::Tag,
            "clock" => Self::Clock,
            "star" => Self::Star,
            "gear" => Self::Gear,
            "plus" => Self::Plus,
            _ => Self::Unknown(s),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptKind {
    Input,
    Pick,
    Unknown(String),
}

impl Serialize for PromptKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Input => serializer.serialize_str("input"),
            Self::Pick => serializer.serialize_str("pick"),
            Self::Unknown(s) => serializer.serialize_str(s),
        }
    }
}

impl<'de> Deserialize<'de> for PromptKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "input" => Self::Input,
            "pick" => Self::Pick,
            _ => Self::Unknown(s),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptItem {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<PromptIcon>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default)]
    pub selected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptSpec {
    pub kind: PromptKind,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
    #[serde(default)]
    pub multiple: bool,
    #[serde(default)]
    pub allow_other: bool,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub items: Vec<PromptItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientPromptRequestedEvent {
    pub call_id: String,
    #[serde(default)]
    pub action: Option<String>,
    pub prompt: PromptSpec,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientPromptEndedEvent {
    pub call_id: String,
    /// `"answered"`, `"timeout"`, `"cancelled"`, or unknown future value.
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientPerformRequestedEvent {
    pub call_id: String,
    pub action: String,
    #[serde(default)]
    pub args: Option<Value>,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub block_id: Option<String>,
    pub requester: RequesterInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientPerformedEvent {
    pub client_id: String,
    pub action: String,
    pub requester: RequesterInfo,
    pub outcome: String,
    #[serde(default)]
    pub error: Option<ReplyError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientCallRequestedEvent {
    pub call_id: String,
    pub server: String,
    pub method: String,
    #[serde(default)]
    pub args: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplyError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientReplyRequest {
    pub call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ReplyError>,
}

impl ClientReplyRequest {
    pub fn ok(call_id: impl Into<String>, result: Value) -> Self {
        Self {
            call_id: call_id.into(),
            result: Some(result),
            error: None,
        }
    }

    pub fn prompt_text(call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::ok(call_id, serde_json::json!({ "text": text.into() }))
    }

    pub fn prompt_ids(call_id: impl Into<String>, ids: Vec<String>) -> Self {
        Self::ok(call_id, serde_json::json!({ "ids": ids }))
    }

    pub fn prompt_cancelled(call_id: impl Into<String>) -> Self {
        Self::ok(call_id, serde_json::json!({ "cancelled": true }))
    }

    pub fn err(
        call_id: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            result: None,
            error: Some(ReplyError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

// ============================================================================
// Server Status, Sessions, Bootstrap & Layout Views
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TailscaleAccount {
    pub login_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TailscaleHttps {
    pub state: String,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TailscaleStatus {
    pub state: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub dns_name: Option<String>,
    #[serde(default)]
    pub auth_url: Option<String>,
    #[serde(default)]
    pub account: Option<TailscaleAccount>,
    #[serde(default)]
    pub tailnet_name: Option<String>,
    #[serde(default)]
    pub allow_tailnet_members: bool,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub tags: Vec<String>,
    #[serde(default)]
    pub https: Option<TailscaleHttps>,
    #[serde(default)]
    pub control_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerConfigStatus {
    pub generation: u64,
    #[serde(default)]
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerListener {
    pub endpoint: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub addresses: Vec<String>,
    pub transport: String,
    #[serde(default)]
    pub admission: Option<String>,
    #[serde(default)]
    pub primary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerStatus {
    pub pid: u32,
    pub instance_id: String,
    #[serde(default)]
    pub host_id: Option<String>,
    pub started_at: String,
    #[serde(default)]
    pub exit_empty: bool,
    #[serde(default)]
    pub session_count: u64,
    #[serde(default)]
    pub tailscale: Option<TailscaleStatus>,
    #[serde(default)]
    pub api_revision: Option<u32>,
    #[serde(default)]
    pub api_min_revision: Option<u32>,
    #[serde(default)]
    pub config: Option<ServerConfigStatus>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub listeners: Vec<ServerListener>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DataTicketSpec {
    pub creator_name: String,
    pub data_ref_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subprotocol: Option<String>,
}

impl DataTicketSpec {
    pub fn terminal_v1() -> Self {
        Self {
            creator_name: "com.superlogical.terminal".into(),
            data_ref_id: "terminal".into(),
            subprotocol: Some(DATA_TERMINAL_SUBPROTOCOL.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionBootstrapRequest {
    /// `"session"` or `"all"` (`"all"` recommended for GUI clients).
    pub event_scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach_session_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_tickets: Option<Vec<DataTicketSpec>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachAllSnapshot {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub sessions: Vec<SessionSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttachedSessionSnapshot {
    pub session: SessionSummary,
    pub view: SessionView,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DataTicketGrant {
    pub session_id: String,
    pub block_id: String,
    pub data_ref_id: String,
    #[serde(default)]
    pub subprotocol: Option<String>,
    pub ticket: String,
    pub expires_in_ms: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionBootstrapResponse {
    pub client_id: String,
    pub server_status: ServerStatus,
    pub attach_all: AttachAllSnapshot,
    #[serde(default)]
    pub selected_session: Option<AttachedSessionSnapshot>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub attached_sessions: Vec<AttachedSessionSnapshot>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub data_tickets: Vec<DataTicketGrant>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionView {
    pub revision: u64,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub active_window_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub windows: Vec<WindowView>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowView {
    #[serde(default)]
    pub window_id: Option<String>,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub focused_block_id: Option<String>,
    #[serde(default)]
    pub zoomed_block_id: Option<String>,
    #[serde(default)]
    pub tiled_zoomed_block_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub layers: Vec<LayerView>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LayerView {
    #[serde(default)]
    pub layer_id: Option<String>,
    /// `"tiled"` or `"floating"`.
    pub kind: String,
    /// Normalized `[x, y, w, h]` fractions in `0.0..=1.0`.
    pub bounds: [f64; 4],
    /// `"normal"`, `"minimized"`, or `"maximized"` (for floating layers).
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub layout: Option<LayoutNode>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub blocks: Vec<BlockView>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LayoutNode {
    pub rect: [f64; 4],
    #[serde(default)]
    pub block_id: Option<String>,
    #[serde(default)]
    pub split: Option<Box<SplitNode>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SplitNode {
    #[serde(default)]
    pub split_id: Option<String>,
    /// `"horizontal"` (before=left, after=right) or `"vertical"` (before=top, after=bottom).
    pub direction: String,
    pub ratio: f64,
    pub before: LayoutNode,
    pub after: LayoutNode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockView {
    #[serde(default)]
    pub block_id: Option<String>,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub creator_name: Option<String>,
    #[serde(default)]
    pub flavor: Option<String>,
    #[serde(default)]
    pub host_id: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
}

// ============================================================================
// Session / Window / Pane / Ownership Mutation RPCs
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BlockCreateSpec {
    pub flavor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LayoutCreateSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<BlockCreateSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split: Option<Box<SplitCreateSpec>>,
}

impl LayoutCreateSpec {
    pub fn shell() -> Self {
        Self {
            block: Some(BlockCreateSpec {
                flavor: "com.superlogical.terminal.shell".into(),
                label: None,
                options: None,
            }),
            split: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SplitCreateSpec {
    pub direction: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f64>,
    pub before: LayoutCreateSpec,
    pub after: LayoutCreateSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InitialWindowSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_label: Option<String>,
    pub layout: LayoutCreateSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionCreateRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_windows: Option<Vec<InitialWindowSpec>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreatedWindowInfo {
    pub window_id: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub block_ids: Vec<String>,
    #[serde(default)]
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionCreateResponse {
    pub session_id: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub initial_windows: Vec<CreatedWindowInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionListResponse {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub sessions: Vec<SessionSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionAttachResponse {
    pub client_id: String,
    pub view: SessionView,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevisionResponse {
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionNewWindowRequest {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<LayoutCreateSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionNewSplitRequest {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
    /// `"horizontal"` or `"vertical"`.
    pub direction: String,
    /// `"before"` or `"after"`.
    pub side: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f64>,
    pub layout: LayoutCreateSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionNewSplitResponse {
    pub anchor_id: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub block_ids: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub split_ids: Vec<String>,
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionNewLayerRequest {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
    pub bounds: [f64; 4],
    pub layout: LayoutCreateSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionNewLayerResponse {
    pub layer_id: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub block_ids: Vec<String>,
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionFloatBlockRequest {
    pub session_id: String,
    pub block_id: String,
    pub bounds: [f64; 4],
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionFloatBlockResponse {
    pub layer_id: String,
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionZoomBlockRequest {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zoom: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionZoomBlockResponse {
    pub block_id: String,
    pub zoomed: bool,
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionResizePaneRequest {
    pub session_id: String,
    pub direction: String,
    pub before_block_id: String,
    pub after_block_id: String,
    pub ratio: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionResizeLayerRequest {
    pub session_id: String,
    pub layer_id: String,
    pub bounds: [f64; 4],
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSetLayerStateRequest {
    pub session_id: String,
    pub layer_id: String,
    /// `"normal"`, `"minimized"`, or `"maximized"`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<bool>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OwnMode {
    Take,
    Request,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionOwnBlockRequest {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<OwnMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionDisownBlockRequest {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockOwnershipEntry {
    pub block_id: String,
    #[serde(default)]
    pub owner: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockOwnershipResponse {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub blocks: Vec<BlockOwnershipEntry>,
}

// ============================================================================
// Client Keymap, Key, Action & Perform RPCs
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionDefinition {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keywords: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeats: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeyBinding {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub keys: Vec<String>,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyRemoval {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeymapLayer {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bindings: Option<Vec<KeyBinding>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removals: Option<Vec<KeyRemoval>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ClientKeymapRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<ActionDefinition>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_actions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<Vec<KeymapLayer>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<Vec<KeymapLayer>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeymapDiagnostic {
    pub severity: String,
    #[serde(default)]
    pub source: Option<String>,
    pub message: String,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeymapMode {
    pub name: String,
    pub exclusive: bool,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub blocked: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeymapConfigInfo {
    pub path: String,
    pub exists: bool,
    pub generation: u64,
    pub instance_id: String,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientKeymapResponse {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub bindings: Vec<KeyBinding>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub diagnostics: Vec<KeymapDiagnostic>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub actions: Vec<ActionDefinition>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub modes: Vec<KeymapMode>,
    pub config: KeymapConfigInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionServerRef {
    pub name: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientActionRequest {
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub servers: Option<Vec<ActionServerRef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueuedClientAction {
    pub action: String,
    #[serde(default)]
    pub args: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientActionResponse {
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub client_actions: Vec<QueuedClientAction>,
    #[serde(default)]
    pub ordered_after: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeyAction {
    Press,
    Release,
    Repeat,
}

/// Canonical modifier bitset formatter matching Rex's `mods` / `consumed_mods` wire format:
/// `shift`, `ctrl`, `alt`, `super`, `caps_lock`, `num_lock`, `shift_side`, `ctrl_side`, `alt_side`, `super_side`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyModifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub super_key: bool,
    pub caps_lock: bool,
    pub num_lock: bool,
    pub shift_side: bool,
    pub ctrl_side: bool,
    pub alt_side: bool,
    pub super_side: bool,
}

impl KeyModifiers {
    pub fn to_rex_string(self) -> String {
        let mut parts = Vec::with_capacity(6);
        if self.shift {
            parts.push("shift");
        }
        if self.ctrl {
            parts.push("ctrl");
        }
        if self.alt {
            parts.push("alt");
        }
        if self.super_key {
            parts.push("super");
        }
        if self.caps_lock {
            parts.push("caps_lock");
        }
        if self.num_lock {
            parts.push("num_lock");
        }
        if self.shift_side {
            parts.push("shift_side");
        }
        if self.ctrl_side {
            parts.push("ctrl_side");
        }
        if self.alt_side {
            parts.push("alt_side");
        }
        if self.super_side {
            parts.push("super_side");
        }
        parts.join("+")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyInputEvent {
    /// Physical W3C KeyboardEvent code in `snake_case` (e.g. `"key_a"`, `"enter"`, `"bracket_left"`).
    pub key: String,
    /// Modifier string joined with `+` in canonical Rex order (`""` for none).
    pub mods: String,
    /// Modifiers consumed by the keyboard layout to produce `text` (`""` for none).
    pub consumed_mods: String,
    pub text: String,
    /// Unicode code point produced without Shift (0 if none).
    pub unshifted_codepoint: i32,
    pub composing: bool,
    pub action: KeyAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientKeyRequest {
    pub session_id: String,
    pub key: KeyInputEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<Vec<KeyInputEvent>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientKeyResponse {
    pub handled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientPerformRequest {
    pub client_id: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientPerformResponse {
    #[serde(default)]
    pub result: Option<Value>,
    pub performed_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientListResponse {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub clients: Vec<ConnectedClient>,
}

// ============================================================================
// Block Input Events & `com.superlogical.terminal.*` Block Methods
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum BlockInputEvent {
    Key(KeyInputEvent),
    Paste {
        text: String,
    },
    Focus {
        /// `"in"` or `"out"`.
        action: String,
    },
    Mouse {
        /// `"press"`, `"release"`, or `"move"`.
        action: String,
        button: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        buttons: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mods: Option<String>,
        x: f64,
        y: f64,
        #[serde(default)]
        pixel: bool,
    },
    Wheel {
        x: f64,
        y: f64,
        #[serde(default)]
        pixel: bool,
        delta_x: f64,
        delta_y: f64,
        #[serde(default)]
        delta_z: f64,
        /// `"pixel"`, `"line"`, or `"page"`.
        delta_mode: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mods: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BlockEventRequest {
    pub session_id: String,
    pub block_id: String,
    pub event: BlockInputEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockCloseRequest {
    pub session_id: String,
    pub block_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undoable: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BlockMethodPayload<T> {
    pub session_id: String,
    pub block_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<T>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalWriteArgs {
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalWriteResponse {
    pub bytes_written: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalResizeArgs {
    pub columns: u16,
    pub rows: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_width_px: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_height_px: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThemeScheme {
    Light,
    Dark,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TerminalSetThemeArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Entries formatted as `"<index>=#RRGGBB"` or `"<index>=RRGGBB"` for indices `0..=255`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub palette: Option<Vec<String>>,
    /// Explicit Mode 2031 scheme (`"dark"` or `"light"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<ThemeScheme>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessOwner {
    pub pid: u32,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalTitleResponse {
    pub title: String,
    #[serde(default)]
    pub owner: Option<ProcessOwner>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalPwdResponse {
    #[serde(default)]
    pub pwd: Option<String>,
    #[serde(default)]
    pub owner: Option<ProcessOwner>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExitStatusInfo {
    pub exit_code: i32,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    #[serde(default)]
    pub user_id: u32,
    #[serde(default)]
    pub user: Option<String>,
    pub name: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub invoked_path: Option<String>,
    #[serde(default)]
    pub argv0: Option<String>,
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalProcessResponse {
    #[serde(default)]
    pub last_exit: Option<ExitStatusInfo>,
    #[serde(default)]
    pub child: Option<ProcessInfo>,
    #[serde(default)]
    pub foreground: Option<ProcessInfo>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub ancestors: Vec<ProcessInfo>,
    #[serde(default)]
    pub home: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProgramStatusRecord {
    #[serde(default)]
    pub id: Option<String>,
    /// `"idle"`, `"working"`, `"done"`, `"blocked"`, or `"error"`.
    pub state: String,
    /// `"permission"`, `"question"`, or `"auth"` (when `state == "blocked"`).
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub progress: Option<u8>,
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default)]
    pub effective_app: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub msg: Option<String>,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub owner: Option<ProcessOwner>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalProgramStatusResponse {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub records: Vec<ProgramStatusRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DesiredSizeEntry {
    #[serde(default)]
    pub client_id: Option<String>,
    pub columns: u16,
    pub rows: u16,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalSizeResponse {
    pub columns: u16,
    pub rows: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub desired: Vec<DesiredSizeEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalModeEntry {
    pub name: String,
    pub number: u32,
    #[serde(default)]
    pub ansi: bool,
    pub set: bool,
    #[serde(default)]
    pub default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalModesResponse {
    pub screen: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub modes: Vec<TerminalModeEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalUploadBeginArgs {
    /// `"upload:<uuid-v4-lowercase>"`.
    #[serde(rename = "ref")]
    pub data_ref: String,
    pub name: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalUploadBeginResponse {
    pub ticket: String,
    pub expires_in_ms: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalDownloadBeginArgs {
    /// `"download:<uuid-v4-lowercase>"`.
    #[serde(rename = "ref")]
    pub data_ref: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_none_match: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalDownloadBeginResponse {
    #[serde(default)]
    pub ticket: Option<String>,
    #[serde(default)]
    pub expires_in_ms: Option<u32>,
    #[serde(default)]
    pub not_modified: bool,
    pub path: String,
    pub name: String,
    pub size: u64,
    pub modified_at: String,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalClipboardReadArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalClipboardReadResponse {
    pub id: u64,
    pub data: String,
    pub total: u64,
    pub eof: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ListDirStatus {
    Ok,
    Missing,
    NotDir,
    Denied,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    #[serde(default)]
    pub file: bool,
    #[serde(default)]
    pub symlink: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalListDirArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalListDirResponse {
    pub status: ListDirStatus,
    pub path: String,
    pub home: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub entries: Vec<DirEntry>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClipboardWrittenEvent {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub id: Option<u64>,
    #[serde(default)]
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PortScanStatus {
    Ok,
    Partial,
    Unsupported,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortEntry {
    pub port: u16,
    pub address: String,
    pub protocol: String,
    pub first_seen_at: String,
    #[serde(default)]
    pub pid: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortProcessEntry {
    pub pid: i64,
    pub user_id: i64,
    pub name: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub invoked_path: Option<String>,
    #[serde(default)]
    pub argv0: Option<String>,
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

impl PortProcessEntry {
    pub fn display_label(&self) -> &str {
        self.script
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| self.argv0.as_deref().filter(|s| !s.is_empty()))
            .unwrap_or(self.name.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalPortsResponse {
    pub status: PortScanStatus,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub ports: Vec<PortEntry>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub processes: Vec<PortProcessEntry>,
    #[serde(default)]
    pub truncated: Option<bool>,
    #[serde(default)]
    pub id: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalBlobReadArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalBlobReadResponse {
    pub id: u64,
    pub data: String,
    pub total: u64,
    pub eof: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionNewBlockRequest {
    pub session_id: String,
    pub flavor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionNewBlockResponse {
    #[serde(default)]
    pub block_id: Option<String>,
    #[serde(default)]
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TunnelDialOptions {
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_address: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TunnelListenOptions {
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TunnelInfoResponse {
    pub flavor: String,
    pub connections: u64,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub client_address: Option<String>,
    #[serde(default)]
    pub client_target: Option<String>,
    #[serde(default)]
    pub last_owner: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TunnelConnectionRequestedEvent {
    #[serde(rename = "ref")]
    pub data_ref_id: String,
    pub peer: String,
    pub client: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSwapBlocksRequest {
    pub session_id: String,
    pub block_id: String,
    pub other_block_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSwapBlocksResponse {
    #[serde(default)]
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionTransferWindowRequest {
    pub session_id: String,
    pub window_id: String,
    pub to_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_window_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionTransferWindowResponse {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub to_revision: u64,
}

// ============================================================================
// Data Ticket & Multi-Host Registry RPCs
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DataConnectRequest {
    pub session_id: String,
    pub block_id: String,
    pub data_ref_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subprotocol: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DataConnectResponse {
    pub ticket: String,
    pub expires_in_ms: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub label: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub endpoints: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostsListResponse {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub hosts: Vec<HostEntry>,
    pub hash: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub diagnostics: Vec<KeymapDiagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostsPutRequest {
    pub host: HostEntry,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostsPutResponse {
    pub host: HostEntry,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostsRemoveRequest {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostsRemoveResponse {
    pub removed: bool,
    pub hash: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_token_is_redacted_in_debug_and_display() {
        let tok = ReconnectToken::new("super-secret-22-char-tk");
        assert!(!format!("{tok:?}").contains("super-secret"));
        assert!(!format!("{tok}").contains("super-secret"));
        assert_eq!(tok.expose_secret(), "super-secret-22-char-tk");

        let hello = HelloMessage::new(ClientInfo::omarex_default(), Some(tok));
        assert!(!format!("{hello:?}").contains("super-secret"));
        let serialized = serde_json::to_string(&hello).unwrap();
        assert!(serialized.contains("super-secret-22-char-tk"));
        assert!(serialized.len() < MIN_COMPRESS_BYTES);
    }

    #[test]
    fn ordered_method_classification_matches_rev24() {
        assert!(is_ordered_method("client.action"));
        assert!(is_ordered_method("client.key"));
        assert!(is_ordered_method("session.own_block"));
        assert!(is_ordered_method("block.event"));
        // Critical: client.reply MUST be unordered to prevent rex.ui deadlocks.
        assert!(!is_ordered_method("client.reply"));
        assert!(!is_ordered_method("session.view"));
        assert!(!is_ordered_method("data.connect"));
        assert!(!is_ordered_method("com.superlogical.terminal.write"));
    }

    #[test]
    fn serde_round_trips_rev24_payloads_and_go_null_arrays() {
        let view_json = r#"{
            "revision": 412,
            "session_id": "session:01a216bc-52f2-72c7-aa9a-e94e1d60b77a",
            "label": "T3 Code",
            "active_window_id": "window:1",
            "windows": [{
                "window_id": "window:1",
                "label": "Thread 1",
                "focused_block_id": "block:1",
                "tiled_zoomed_block_id": null,
                "layers": [{
                    "layer_id": "layer:1",
                    "kind": "tiled",
                    "bounds": [0, 0, 1, 1],
                    "layout": {
                        "rect": [0, 0, 1, 1],
                        "split": {
                            "split_id": "split:1",
                            "direction": "horizontal",
                            "ratio": 0.5,
                            "before": { "rect": [0, 0, 0.5, 1], "block_id": "block:1" },
                            "after": { "rect": [0.5, 0, 0.5, 1], "block_id": "block:2" }
                        }
                    },
                    "blocks": [
                        { "block_id": "block:1", "label": "t3:1baa1939/abc", "creator_name": "com.superlogical.terminal" }
                    ]
                }]
            }]
        }"#;
        let view: SessionView = serde_json::from_str(view_json).unwrap();
        assert_eq!(view.revision, 412);
        assert_eq!(view.windows[0].layers[0].blocks.len(), 1);
        assert!(
            view.windows[0].layers[0]
                .layout
                .as_ref()
                .unwrap()
                .split
                .is_some()
        );

        // Go nil slice -> null array handling
        let empty_hosts: HostsListResponse =
            serde_json::from_str(r#"{"hosts":null,"hash":"abc","diagnostics":null}"#).unwrap();
        assert!(empty_hosts.hosts.is_empty());
        assert!(empty_hosts.diagnostics.is_empty());

        // Prompt with all 16 icons and forward-compat unknown icon
        let prompt_raw: RawTypedPayload = serde_json::from_str(
            r#"{
                "type": "client_prompt_requested",
                "payload": {
                    "call_id": "p-1",
                    "action": "t3_inbox",
                    "timeout_ms": 60000,
                    "prompt": {
                        "kind": "pick",
                        "title": "Select Thread",
                        "multiple": true,
                        "allow_other": false,
                        "items": [
                            {"id": "1", "title": "A", "icon": "branch", "selected": true},
                            {"id": "2", "title": "B", "icon": "future_icon"}
                        ]
                    }
                }
            }"#,
        )
        .unwrap();
        let ev = ServerEvent::from_raw(prompt_raw).unwrap();
        match ev {
            ServerEvent::ClientPromptRequested(req) => {
                assert_eq!(req.prompt.kind, PromptKind::Pick);
                assert_eq!(req.prompt.items[0].icon, Some(PromptIcon::Branch));
                assert_eq!(
                    req.prompt.items[1].icon,
                    Some(PromptIcon::Unknown("future_icon".into()))
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // Canonical modifier serialization
        let mods = KeyModifiers {
            ctrl: true,
            shift: true,
            super_key: true,
            ..Default::default()
        };
        assert_eq!(mods.to_rex_string(), "shift+ctrl+super");
    }

    #[test]
    fn rev24_m4_and_m5_schemas_deserialize_and_roundtrip() {
        // list_dir with Go nil slice `entries: null` and `status: "denied"`
        let denied: TerminalListDirResponse = serde_json::from_str(
            r#"{"entries":null,"home":"/home/mike","path":"/root","status":"denied","truncated":false}"#,
        )
        .unwrap();
        assert_eq!(denied.status, ListDirStatus::Denied);
        assert!(denied.entries.is_empty());

        let ok_dir: TerminalListDirResponse = serde_json::from_str(
            r#"{"entries":[{"name":"src"},{"name":"Cargo.toml","file":true},{"name":"link","symlink":true}],"home":"/home/mike","path":"/work","status":"ok","truncated":true}"#,
        )
        .unwrap();
        assert_eq!(ok_dir.status, ListDirStatus::Ok);
        assert!(ok_dir.truncated);
        assert_eq!(ok_dir.entries.len(), 3);
        assert!(!ok_dir.entries[0].file);
        assert!(ok_dir.entries[1].file);
        assert!(ok_dir.entries[2].symlink);

        // clipboard_written matching live Rev 24 schema
        let cw_inline: ClipboardWrittenEvent = serde_json::from_str(r#"{"text":"hello"}"#).unwrap();
        assert_eq!(cw_inline.text.as_deref(), Some("hello"));
        let cw_chunked: ClipboardWrittenEvent =
            serde_json::from_str(r#"{"id":12,"bytes":65536}"#).unwrap();
        assert_eq!(cw_chunked.id, Some(12));
        assert_eq!(cw_chunked.bytes, Some(65536));
        assert!(cw_chunked.text.is_none());

        // ports / ports_changed with truncated blob_read
        let ports: TerminalPortsResponse = serde_json::from_str(
            r#"{
                "status": "partial",
                "ports": [{"port":3000,"address":"0.0.0.0","protocol":"tcp","first_seen_at":"2026-11-26T20:00:00Z","pid":42}],
                "processes": [{"pid":42,"user_id":1000,"name":"node","script":"vite","argv0":"node"}],
                "truncated": true,
                "id": 7
            }"#,
        )
        .unwrap();
        assert_eq!(ports.status, PortScanStatus::Partial);
        assert_eq!(ports.truncated, Some(true));
        assert_eq!(ports.id, Some(7));
        assert_eq!(ports.processes[0].display_label(), "vite");

        let blob: TerminalBlobReadResponse =
            serde_json::from_str(r#"{"id":7,"data":"{}","total":2,"eof":true}"#).unwrap();
        assert!(blob.eof);

        // tunnel dial, info, connection_requested, and session new_block/swap_blocks/transfer_window
        let dial_json = serde_json::to_value(TunnelDialOptions {
            host: "127.0.0.1".into(),
            port: 3000,
            client_address: Some("127.0.0.1:3000".into()),
        })
        .unwrap();
        assert_eq!(dial_json["port"], 3000);

        let info: TunnelInfoResponse =
            serde_json::from_str(r#"{"flavor":"dial","connections":2,"target":"127.0.0.1:3000"}"#)
                .unwrap();
        assert_eq!(info.connections, 2);

        let conn_req: TunnelConnectionRequestedEvent =
            serde_json::from_str(r#"{"ref":"conn:1","peer":"127.0.0.1:51234","client":"c1"}"#)
                .unwrap();
        assert_eq!(conn_req.data_ref_id, "conn:1");

        let nb: SessionNewBlockResponse =
            serde_json::from_str(r#"{"block_id":"blk-9","revision":15}"#).unwrap();
        assert_eq!(nb.block_id.as_deref(), Some("blk-9"));

        let swap = SessionSwapBlocksRequest {
            session_id: "s1".into(),
            block_id: "b1".into(),
            other_block_id: "b2".into(),
        };
        assert_eq!(serde_json::to_value(&swap).unwrap()["other_block_id"], "b2");

        let xfer = SessionTransferWindowRequest {
            session_id: "s1".into(),
            window_id: "w1".into(),
            to_session_id: "s2".into(),
            before_window_id: None,
            focus: Some(true),
        };
        let xfer_val = serde_json::to_value(&xfer).unwrap();
        assert_eq!(xfer_val["to_session_id"], "s2");
        assert!(xfer_val.get("before_window_id").is_none());
    }
}
