//! Rex Data WebSocket protocol (`/api/data`, `rex.data.terminal.v1` & `rex.data.bytes.v1`).
//!
//! Implements:
//! - Exact little-endian binary handshake frames (`HELLO` 17B, `RESUME` 25B, `WELCOME` 32B, `REJECT` 2B)
//! - RFC 4648 unpadded base64url ticket decoding
//! - Close-code classification (`1000`, `1001`, `1008`, `1011`, `1013`, `4000`)
//! - Strict `O(N)` streaming `ESC _ rex1;<channel_nonce>;<json> ESC \` APC extractor with
//!   `channel_nonce` verification (passing Kitty Graphics `ESC _ G ...` APCs through untouched)
//! - [`TerminalDataStream`] with exact consumed-byte counting and automatic `RESUME` / re-ticket recovery
//! - [`BytesDataStream`] for bidirectional `rex.data.bytes.v1` streams

use std::fmt;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use serde::Deserialize;
use serde_json::Value;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Message};

use crate::ProtoError;
use crate::control::ControlClient;
use crate::endpoint::RexEndpoint;
use crate::transport::{RexWsStream, dial_websocket};
use crate::types::{
    DATA_BYTES_SUBPROTOCOL, DATA_TERMINAL_SUBPROTOCOL, DataConnectRequest, ThemeScheme,
};

const DATA_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REX_APC_BYTES: usize = 1024 * 1024;
const REX_APC_PREFIX: &[u8] = b"\x1b_rex1;";

pub const FRAME_TAG_HELLO: u8 = 0x01;
pub const FRAME_TAG_RESUME: u8 = 0x02;
pub const FRAME_TAG_WELCOME: u8 = 0x81;
pub const FRAME_TAG_REJECT: u8 = 0x82;

pub const HELLO_FRAME_LEN: usize = 17;
pub const RESUME_FRAME_LEN: usize = 25;
pub const WELCOME_FRAME_LEN: usize = 32;
pub const REJECT_FRAME_LEN: usize = 2;

/// Redacted 16-byte data reconnect token (never printed in `Debug` logs).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DataReconnectToken([u8; 16]);

impl DataReconnectToken {
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub fn expose_bytes(&self) -> [u8; 16] {
        self.0
    }
}

impl fmt::Debug for DataReconnectToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DataReconnectToken([REDACTED])")
    }
}

/// Decoded 32-byte server `WELCOME` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataWelcome {
    pub reconnect_token: DataReconnectToken,
    pub resume_ttl_ms: u32,
    pub channel_nonce: u64,
}

/// Server `REJECT` reason code byte (`0x01..=0x07`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataRejectReason {
    InvalidOrExpiredTicket,
    DuplicateStream,
    StreamUnavailableOrCountTooLow,
    LimitExceeded,
    MalformedOrCountTooHigh,
    ServerError,
    SubprotocolMismatch,
    Unknown(u8),
}

impl DataRejectReason {
    pub fn from_byte(b: u8) -> Self {
        match b {
            0x01 => Self::InvalidOrExpiredTicket,
            0x02 => Self::DuplicateStream,
            0x03 => Self::StreamUnavailableOrCountTooLow,
            0x04 => Self::LimitExceeded,
            0x05 => Self::MalformedOrCountTooHigh,
            0x06 => Self::ServerError,
            0x07 => Self::SubprotocolMismatch,
            other => Self::Unknown(other),
        }
    }

    pub fn to_byte(self) -> u8 {
        match self {
            Self::InvalidOrExpiredTicket => 0x01,
            Self::DuplicateStream => 0x02,
            Self::StreamUnavailableOrCountTooLow => 0x03,
            Self::LimitExceeded => 0x04,
            Self::MalformedOrCountTooHigh => 0x05,
            Self::ServerError => 0x06,
            Self::SubprotocolMismatch => 0x07,
            Self::Unknown(b) => b,
        }
    }
}

impl fmt::Display for DataRejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?} (0x{:02x})", self.to_byte())
    }
}

/// Action to take when a `/api/data` WebSocket closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataCloseAction {
    /// Clean stream end (`1000`). Do not resume.
    StreamEnded,
    /// Missed keepalive pings or transient drop (`1001 keepalive timeout` / EOF). Attempt `RESUME`.
    Resume { reason: String },
    /// Server stopped provider (`1001 data provider canceled`). Re-ticket once control is live.
    ProviderCanceled,
    /// Output overflowed resume window (`1013 data reconnect required`). Fetch new ticket + `HELLO`.
    ReconnectFreshSnapshot,
    /// Provider failed (`1011`). Retry with new ticket & backoff.
    ProviderFailed,
    /// Block does not expose this data ref / subprotocol (`1008`). Fatal.
    ProviderUnavailable,
    /// Block closed permanently (`4000 block closed`). Tear down terminal; never reconnect.
    BlockClosed,
    /// Handshake rejected (preceded by `REJECT` frame).
    HandshakeRejected { code: u16, reason: String },
}

/// Classify a `/api/data` WebSocket close frame according to the Rex protocol spec.
pub fn classify_data_close(code: u16, reason: &str) -> DataCloseAction {
    match (code, reason) {
        (1000, _) => DataCloseAction::StreamEnded,
        (4000, _) => DataCloseAction::BlockClosed,
        (1001, "keepalive timeout") => DataCloseAction::Resume {
            reason: reason.to_string(),
        },
        (1001, "data provider canceled") => DataCloseAction::ProviderCanceled,
        (1001, _) => DataCloseAction::Resume {
            reason: reason.to_string(),
        },
        (1008, "data provider unavailable") => DataCloseAction::ProviderUnavailable,
        (1011, "data provider failed") => DataCloseAction::ProviderFailed,
        (1013, "data reconnect required") => DataCloseAction::ReconnectFreshSnapshot,
        (_, "data handshake rejected") => DataCloseAction::HandshakeRejected {
            code,
            reason: reason.to_string(),
        },
        (1013, _) => DataCloseAction::ReconnectFreshSnapshot,
        _ => DataCloseAction::Resume {
            reason: format!("close {code}: {reason}"),
        },
    }
}

/// Decode a 22-char unpadded base64url ticket into its exact 16 raw bytes.
pub fn decode_ticket_16(ticket: &str) -> Result<[u8; 16], ProtoError> {
    let raw = decode_base64url(ticket)?;
    raw.try_into().map_err(|v: Vec<u8>| {
        ProtoError::Protocol(format!(
            "data ticket decoded to {} bytes (expected 16)",
            v.len()
        ))
    })
}

/// Encode 16 raw ticket bytes into unpadded RFC 4648 base64url (22 chars).
pub fn encode_ticket_16(bytes: &[u8; 16]) -> String {
    encode_base64url(bytes)
}

/// Encode a 17-byte `HELLO` binary frame: `[0x01][16-byte ticket]`.
pub fn encode_hello(ticket: [u8; 16]) -> [u8; HELLO_FRAME_LEN] {
    let mut buf = [0u8; HELLO_FRAME_LEN];
    buf[0] = FRAME_TAG_HELLO;
    buf[1..17].copy_from_slice(&ticket);
    buf
}

/// Encode a 25-byte `RESUME` binary frame: `[0x02][16-byte token][u64 LE bytes_received]`.
pub fn encode_resume(token: DataReconnectToken, bytes_received: u64) -> [u8; RESUME_FRAME_LEN] {
    let mut buf = [0u8; RESUME_FRAME_LEN];
    buf[0] = FRAME_TAG_RESUME;
    buf[1..17].copy_from_slice(&token.expose_bytes());
    buf[17..25].copy_from_slice(&bytes_received.to_le_bytes());
    buf
}

/// Decode a 32-byte `WELCOME` frame or 2-byte `REJECT` frame.
pub fn decode_server_handshake(frame: &[u8]) -> Result<DataWelcome, ProtoError> {
    if frame.is_empty() {
        return Err(ProtoError::Protocol("empty data handshake frame".into()));
    }
    match frame[0] {
        FRAME_TAG_WELCOME => {
            if frame.len() != WELCOME_FRAME_LEN {
                return Err(ProtoError::Protocol(format!(
                    "WELCOME frame must be {WELCOME_FRAME_LEN} bytes, got {}",
                    frame.len()
                )));
            }
            let mut token = [0u8; 16];
            token.copy_from_slice(&frame[1..17]);
            let resume_ttl_ms = u32::from_le_bytes([frame[20], frame[21], frame[22], frame[23]]);
            let channel_nonce = u64::from_le_bytes([
                frame[24], frame[25], frame[26], frame[27], frame[28], frame[29], frame[30],
                frame[31],
            ]);
            if channel_nonce == 0 {
                return Err(ProtoError::Protocol(
                    "WELCOME channel_nonce must be non-zero".into(),
                ));
            }
            Ok(DataWelcome {
                reconnect_token: DataReconnectToken::from_bytes(token),
                resume_ttl_ms,
                channel_nonce,
            })
        }
        FRAME_TAG_REJECT => {
            if frame.len() < REJECT_FRAME_LEN {
                return Err(ProtoError::Protocol(
                    "truncated REJECT handshake frame".into(),
                ));
            }
            Err(ProtoError::DataRejected(DataRejectReason::from_byte(
                frame[1],
            )))
        }
        other => Err(ProtoError::Protocol(format!(
            "unexpected data handshake tag 0x{other:02x}"
        ))),
    }
}

// ============================================================================
// Strict O(N) Streaming `rex1;<channel_nonce>;<json>` APC Framer
// ============================================================================

/// Authenticated in-band Rex control message extracted from `ESC _ rex1;<nonce>;<json> ESC \`.
#[derive(Debug, Clone, PartialEq)]
pub enum RexControlMessage {
    Resize {
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        owner: Option<String>,
    },
    Theme {
        foreground: Option<String>,
        background: Option<String>,
        cursor: Option<String>,
        palette: Vec<String>,
        scheme: Option<ThemeScheme>,
    },
    ChildExited {
        exit_code: i32,
    },
    Unknown {
        msg_type: String,
        raw: Value,
    },
}

impl RexControlMessage {
    pub fn parse_json(json_str: &str) -> Result<Self, serde_json::Error> {
        let raw: Value = serde_json::from_str(json_str)?;
        let msg_type = raw
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match msg_type.as_str() {
            "resize" => {
                #[derive(Deserialize)]
                struct ResizeMsg {
                    cols: u16,
                    rows: u16,
                    #[serde(default)]
                    cell_width_px: u32,
                    #[serde(default)]
                    cell_height_px: u32,
                    #[serde(default)]
                    owner: Option<String>,
                }
                let r: ResizeMsg = serde_json::from_value(raw)?;
                Ok(Self::Resize {
                    cols: r.cols,
                    rows: r.rows,
                    cell_width_px: r.cell_width_px,
                    cell_height_px: r.cell_height_px,
                    owner: r.owner,
                })
            }
            "theme" => {
                #[derive(Deserialize)]
                struct ThemeMsg {
                    #[serde(default)]
                    foreground: Option<String>,
                    #[serde(default)]
                    background: Option<String>,
                    #[serde(default)]
                    cursor: Option<String>,
                    #[serde(default, deserialize_with = "crate::types::deserialize_null_default")]
                    palette: Vec<String>,
                    #[serde(default)]
                    scheme: Option<ThemeScheme>,
                }
                let t: ThemeMsg = serde_json::from_value(raw)?;
                Ok(Self::Theme {
                    foreground: t.foreground,
                    background: t.background,
                    cursor: t.cursor,
                    palette: t.palette,
                    scheme: t.scheme,
                })
            }
            "child_exited" => {
                #[derive(Deserialize)]
                struct ExitMsg {
                    exit_code: i32,
                }
                let e: ExitMsg = serde_json::from_value(raw)?;
                Ok(Self::ChildExited {
                    exit_code: e.exit_code,
                })
            }
            _ => Ok(Self::Unknown { msg_type, raw }),
        }
    }
}

/// Output items produced in exact stream order by [`RexApcFramer`].
#[derive(Debug, Clone, PartialEq)]
pub enum FramedChunk {
    /// Raw VT bytes (including Kitty Graphics `ESC _ G ...`, OSCs, CSI, text) to feed into `libghostty-vt`.
    VtBytes(Bytes),
    /// Nonce-authenticated Rex control APC message.
    Control(RexControlMessage),
}

#[derive(Debug, Clone)]
enum FramerState {
    /// Normal scanning state; emits bytes until `0x1b`.
    Ground,
    /// Matched `prefix_len` bytes of `b"\x1b_rex1;"` (`1..7`).
    MatchingPrefix(usize),
    /// Inside `rex1;` APC body; accumulating bytes until `ESC \`.
    InPayload(Vec<u8>),
    /// Saw `0x1b` inside `InPayload`; if next byte is `b'\\'`, APC terminates.
    InPayloadEsc(Vec<u8>),
}

/// Strict `O(N)` linear-time streaming framer that extracts nonce-verified
/// `ESC _ rex1;<channel_nonce>;<json> ESC \` control APCs across arbitrary chunk boundaries.
///
/// Security & compatibility invariants:
/// - **Constant-work per byte**: zero quadratic rescans or `remove(0)` memmoves.
/// - **Kitty Graphics safe**: `ESC _ G ...` diverges at byte 2 (`'G' != 'r'`) and is flushed
///   verbatim into [`FramedChunk::VtBytes`].
/// - **Nonce verification**: Any `rex1;` APC whose decimal nonce does not equal
///   `expected_channel_nonce` (e.g., spoofed by an untrusted PTY program) is **rejected** and
///   never emitted as [`FramedChunk::Control`].
#[derive(Debug, Clone)]
pub struct RexApcFramer {
    expected_nonce: u64,
    state: FramerState,
    spoofed_count: u64,
}

impl RexApcFramer {
    pub fn new(expected_channel_nonce: u64) -> Self {
        Self {
            expected_nonce: expected_channel_nonce,
            state: FramerState::Ground,
            spoofed_count: 0,
        }
    }

    /// Number of spoofed/unauthenticated `rex1;` APCs ignored so far.
    pub fn spoofed_apc_count(&self) -> u64 {
        self.spoofed_count
    }

    /// Process a chunk of post-snapshot stream bytes in strict `O(N)` time.
    pub fn feed(&mut self, input: &[u8]) -> Vec<FramedChunk> {
        let mut out = Vec::new();
        let mut vt_buf = BytesMut::new();
        let mut i = 0usize;

        while i < input.len() {
            match &mut self.state {
                FramerState::Ground => {
                    // Fast SIMD-friendly slice scan for next ESC (`0x1b`).
                    if let Some(rel) = input[i..].iter().position(|&b| b == 0x1b) {
                        if rel > 0 {
                            vt_buf.extend_from_slice(&input[i..i + rel]);
                        }
                        i += rel + 1;
                        self.state = FramerState::MatchingPrefix(1);
                    } else {
                        vt_buf.extend_from_slice(&input[i..]);
                        i = input.len();
                    }
                }
                FramerState::MatchingPrefix(matched) => {
                    let b = input[i];
                    if b == REX_APC_PREFIX[*matched] {
                        *matched += 1;
                        i += 1;
                        if *matched == REX_APC_PREFIX.len() {
                            if !vt_buf.is_empty() {
                                out.push(FramedChunk::VtBytes(vt_buf.split().freeze()));
                            }
                            self.state = FramerState::InPayload(Vec::with_capacity(128));
                        }
                    } else {
                        // Prefix diverged (e.g., Kitty Graphics `ESC _ G`, CSI `ESC [`, OSC `ESC ]`).
                        vt_buf.extend_from_slice(&REX_APC_PREFIX[..*matched]);
                        self.state = FramerState::Ground;
                        // Re-process byte `b` in Ground state (in case `b == 0x1b`).
                    }
                }
                FramerState::InPayload(buf) => {
                    if let Some(rel) = input[i..].iter().position(|&b| b == 0x1b) {
                        let end = i + rel;
                        if buf.len() + rel <= MAX_REX_APC_BYTES {
                            buf.extend_from_slice(&input[i..end]);
                            let owned = std::mem::take(buf);
                            self.state = FramerState::InPayloadEsc(owned);
                        } else {
                            // Oversized untrusted APC — abort safely and flush to ground.
                            tracing::warn!("ignoring oversized rex1 APC (> {MAX_REX_APC_BYTES} B)");
                            self.spoofed_count += 1;
                            self.state = FramerState::Ground;
                        }
                        i = end + 1;
                    } else {
                        let rem = input.len() - i;
                        if buf.len() + rem <= MAX_REX_APC_BYTES {
                            buf.extend_from_slice(&input[i..]);
                        } else {
                            tracing::warn!("ignoring oversized rex1 APC (> {MAX_REX_APC_BYTES} B)");
                            self.spoofed_count += 1;
                            self.state = FramerState::Ground;
                        }
                        i = input.len();
                    }
                }
                FramerState::InPayloadEsc(buf) => {
                    let b = input[i];
                    i += 1;
                    if b == b'\\' {
                        // Complete ST (`ESC \`) reached! Validate nonce & parse JSON.
                        let payload = std::mem::take(buf);
                        self.state = FramerState::Ground;
                        if let Some(ctrl) = self.verify_and_parse(&payload) {
                            out.push(FramedChunk::Control(ctrl));
                        }
                    } else if buf.len() + 2 <= MAX_REX_APC_BYTES {
                        buf.push(0x1b);
                        buf.push(b);
                        let owned = std::mem::take(buf);
                        self.state = FramerState::InPayload(owned);
                    } else {
                        self.spoofed_count += 1;
                        self.state = FramerState::Ground;
                    }
                }
            }
        }

        if !vt_buf.is_empty() {
            out.push(FramedChunk::VtBytes(vt_buf.freeze()));
        }
        out
    }

    /// Flush any pending partial prefix bytes at EOF.
    pub fn finish(&mut self) -> Vec<FramedChunk> {
        let mut out = Vec::new();
        if let FramerState::MatchingPrefix(matched) = self.state {
            out.push(FramedChunk::VtBytes(Bytes::copy_from_slice(
                &REX_APC_PREFIX[..matched],
            )));
            self.state = FramerState::Ground;
        }
        out
    }

    fn verify_and_parse(&mut self, payload: &[u8]) -> Option<RexControlMessage> {
        let text = std::str::from_utf8(payload).ok()?;
        let (nonce_str, json_str) = text.split_once(';')?;
        let parsed_nonce = nonce_str.parse::<u64>().ok();
        if parsed_nonce != Some(self.expected_nonce) {
            self.spoofed_count += 1;
            tracing::warn!(
                expected = self.expected_nonce,
                got = ?parsed_nonce,
                "rejected spoofed/unauthenticated rex1 APC"
            );
            return None;
        }
        match RexControlMessage::parse_json(json_str) {
            Ok(msg) => Some(msg),
            Err(err) => {
                tracing::warn!(error = %err, "failed to parse authenticated rex1 APC JSON");
                None
            }
        }
    }
}

// ============================================================================
// Live Terminal & Bidirectional Bytes Data Streams
// ============================================================================

/// Events produced by [`TerminalDataStream::next_chunk`].
#[derive(Debug, Clone)]
pub enum TerminalStreamChunk {
    /// Raw stream bytes (Phase 1 `GHOSTSNP` snapshot bytes, followed by Phase 2 VT + `rex1;` APC bytes).
    /// Total consumed bytes is updated atomically when yielded.
    Bytes(Bytes),
    /// Stream seamlessly resumed after a transient disconnect (`channel_nonce` unchanged).
    Resumed {
        bytes_received: u64,
        welcome: DataWelcome,
    },
    /// Stream closed and cannot resume automatically.
    Closed(DataCloseAction),
}

/// Active `rex.data.terminal.v1` WebSocket stream tracking exact consumed bytes for seamless `RESUME`.
pub struct TerminalDataStream {
    endpoint: RexEndpoint,
    session_id: String,
    block_id: String,
    data_ref_id: String,
    ws: RexWsStream,
    welcome: DataWelcome,
    bytes_received: u64,
}

impl TerminalDataStream {
    /// Open a new terminal stream using a single-use 22-char base64url ticket from `data.connect`
    /// or `session.bootstrap`.
    pub async fn connect_with_ticket(
        endpoint: RexEndpoint,
        session_id: impl Into<String>,
        block_id: impl Into<String>,
        data_ref_id: impl Into<String>,
        ticket: &str,
    ) -> Result<Self, ProtoError> {
        let ticket_bytes = decode_ticket_16(ticket)?;
        let mut ws = dial_websocket(&endpoint, "/api/data", DATA_TERMINAL_SUBPROTOCOL).await?;
        let hello = encode_hello(ticket_bytes);
        ws.send_message(Message::Binary(hello.to_vec().into()))
            .await?;
        let welcome = recv_data_handshake(&mut ws).await?;

        Ok(Self {
            endpoint,
            session_id: session_id.into(),
            block_id: block_id.into(),
            data_ref_id: data_ref_id.into(),
            ws,
            welcome,
            bytes_received: 0,
        })
    }

    /// Fetch a fresh ticket via `control.data_connect` and open the terminal data stream.
    pub async fn connect(
        control: &ControlClient,
        session_id: impl Into<String>,
        block_id: impl Into<String>,
    ) -> Result<Self, ProtoError> {
        let session_id = session_id.into();
        let block_id = block_id.into();
        let (ticket_res, _) = control
            .data_connect(&DataConnectRequest {
                session_id: session_id.clone(),
                block_id: block_id.clone(),
                data_ref_id: "terminal".into(),
                subprotocol: Some(DATA_TERMINAL_SUBPROTOCOL.into()),
            })
            .await?;
        Self::connect_with_ticket(
            control.endpoint().clone(),
            session_id,
            block_id,
            "terminal",
            &ticket_res.ticket,
        )
        .await
    }

    pub fn welcome(&self) -> DataWelcome {
        self.welcome
    }

    pub fn channel_nonce(&self) -> u64 {
        self.welcome.channel_nonce
    }

    /// Exact count of post-`WELCOME` stream bytes yielded to the caller across all connections.
    pub fn bytes_received(&self) -> u64 {
        self.bytes_received
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn block_id(&self) -> &str {
        &self.block_id
    }

    /// Read the next stream chunk. Automatically handles WebSocket keepalive pings.
    ///
    /// Does **not** automatically reconnect; call [`Self::resume`] on [`DataCloseAction::Resume`]
    /// or re-connect via [`Self::connect`] on [`DataCloseAction::ReconnectFreshSnapshot`].
    pub async fn next_chunk(&mut self) -> Result<TerminalStreamChunk, ProtoError> {
        loop {
            match self.ws.next_message().await {
                Some(Ok(Message::Binary(data))) => {
                    self.bytes_received = self
                        .bytes_received
                        .saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
                    return Ok(TerminalStreamChunk::Bytes(data));
                }
                Some(Ok(Message::Ping(payload))) => {
                    self.ws.send_message(Message::Pong(payload)).await?;
                }
                Some(Ok(Message::Pong(_))) => continue,
                Some(Ok(Message::Close(cf))) => {
                    let (code, reason) = cf
                        .as_ref()
                        .map(|c| (u16::from(c.code), c.reason.as_str()))
                        .unwrap_or((1006, "unexpected close"));
                    return Ok(TerminalStreamChunk::Closed(classify_data_close(
                        code, reason,
                    )));
                }
                Some(Ok(Message::Text(_))) | Some(Ok(Message::Frame(_))) => continue,
                Some(Err(err)) => {
                    return Ok(TerminalStreamChunk::Closed(DataCloseAction::Resume {
                        reason: err.to_string(),
                    }));
                }
                None => {
                    return Ok(TerminalStreamChunk::Closed(DataCloseAction::Resume {
                        reason: "data socket EOF".into(),
                    }));
                }
            }
        }
    }

    /// Attempt to seamlessly resume this stream on a brand-new WebSocket using the latest
    /// `reconnect_token` and exact `bytes_received` count (requires no control connection).
    pub async fn resume(&mut self) -> Result<DataWelcome, ProtoError> {
        let mut new_ws =
            dial_websocket(&self.endpoint, "/api/data", DATA_TERMINAL_SUBPROTOCOL).await?;
        let resume_frame = encode_resume(self.welcome.reconnect_token, self.bytes_received);
        new_ws
            .send_message(Message::Binary(resume_frame.to_vec().into()))
            .await?;
        let new_welcome = recv_data_handshake(&mut new_ws).await?;
        self.ws = new_ws;
        self.welcome = new_welcome;
        Ok(new_welcome)
    }

    /// Attempt [`Self::resume`] first; if the server rejects the resume or the buffer overflowed,
    /// fall back to requesting a fresh ticket via `control.data_connect` (caller should reset its
    /// `GHOSTSNP` decoder when `ResumedFreshSnapshot` is returned!).
    pub async fn recover_or_reopen(
        &mut self,
        control: &ControlClient,
    ) -> Result<RecoveryOutcome, ProtoError> {
        match self.resume().await {
            Ok(welcome) => Ok(RecoveryOutcome::ResumedInPlace {
                bytes_received: self.bytes_received,
                welcome,
            }),
            Err(err) => {
                tracing::info!(
                    block_id = %self.block_id,
                    error = %err,
                    "data stream RESUME declined; requesting fresh ticket"
                );
                let (ticket_res, _) = control
                    .data_connect(&DataConnectRequest {
                        session_id: self.session_id.clone(),
                        block_id: self.block_id.clone(),
                        data_ref_id: self.data_ref_id.clone(),
                        subprotocol: Some(DATA_TERMINAL_SUBPROTOCOL.into()),
                    })
                    .await?;
                let reopened = Self::connect_with_ticket(
                    self.endpoint.clone(),
                    self.session_id.clone(),
                    self.block_id.clone(),
                    self.data_ref_id.clone(),
                    &ticket_res.ticket,
                )
                .await?;
                self.ws = reopened.ws;
                self.welcome = reopened.welcome;
                self.bytes_received = 0;
                Ok(RecoveryOutcome::ReopenedFreshSnapshot {
                    welcome: self.welcome,
                })
            }
        }
    }

    /// Close the WebSocket cleanly with status `1000` (terminates the server-side stream immediately).
    pub async fn close_clean(&mut self) -> Result<(), ProtoError> {
        self.ws
            .close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "".into(),
            }))
            .await
    }
}

/// Result of [`TerminalDataStream::recover_or_reopen`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// Stream resumed seamlessly at the exact byte offset (no new snapshot).
    ResumedInPlace {
        bytes_received: u64,
        welcome: DataWelcome,
    },
    /// Stream reopened with a brand-new ticket (starts with a new `GHOSTSNP` snapshot and nonce).
    ReopenedFreshSnapshot { welcome: DataWelcome },
}

/// Bidirectional `rex.data.bytes.v1` data stream (used for file uploads, downloads, fs, tunnels).
pub struct BytesDataStream {
    ws: RexWsStream,
    welcome: DataWelcome,
    bytes_received: u64,
}

impl BytesDataStream {
    /// Connect to `/api/data` with subprotocol `rex.data.bytes.v1` and complete the `HELLO`/`WELCOME` handshake.
    pub async fn connect_with_ticket(
        endpoint: &RexEndpoint,
        ticket: &str,
    ) -> Result<Self, ProtoError> {
        let ticket_bytes = decode_ticket_16(ticket)?;
        let mut ws = dial_websocket(endpoint, "/api/data", DATA_BYTES_SUBPROTOCOL).await?;
        let hello = encode_hello(ticket_bytes);
        ws.send_message(Message::Binary(hello.to_vec().into()))
            .await?;
        let welcome = recv_data_handshake(&mut ws).await?;
        Ok(Self {
            ws,
            welcome,
            bytes_received: 0,
        })
    }

    pub fn welcome(&self) -> DataWelcome {
        self.welcome
    }

    pub fn bytes_received(&self) -> u64 {
        self.bytes_received
    }

    /// Send one non-empty binary chunk (backpressure-aware: flushes per chunk).
    pub async fn send_chunk(&mut self, chunk: Bytes) -> Result<(), ProtoError> {
        if chunk.is_empty() {
            return Ok(());
        }
        self.ws.send_message(Message::Binary(chunk)).await
    }

    /// Signals client-to-server EOF (`CloseWrite`) on `rex.data.bytes.v1` by sending a
    /// zero-length binary WebSocket message without closing the connection, allowing the server
    /// to finish processing (e.g. a multi-file upload batch) and reply with its final close status.
    pub async fn half_close(&mut self) -> Result<(), ProtoError> {
        self.ws.send_message(Message::Binary(Bytes::new())).await
    }

    /// Waits for the server to close this `rex.data.bytes.v1` stream after `half_close()`,
    /// answering WebSocket pings along the way. Returns `Ok(())` only on normal close (`1000`).
    pub async fn wait_for_server_close(&mut self) -> Result<(), ProtoError> {
        loop {
            match self.ws.next_message().await {
                Some(Ok(Message::Ping(payload))) => {
                    self.ws.send_message(Message::Pong(payload)).await?;
                }
                Some(Ok(Message::Pong(_))) => continue,
                Some(Ok(Message::Close(cf))) => {
                    let code = cf.as_ref().map(|c| u16::from(c.code)).unwrap_or(1000);
                    if code == 1000 {
                        return Ok(());
                    }
                    let reason = cf
                        .as_ref()
                        .map(|c| c.reason.to_string())
                        .unwrap_or_default();
                    return Err(ProtoError::Closed(format!(
                        "bytes stream rejected by server ({code}): {reason}"
                    )));
                }
                Some(Ok(_)) => continue,
                Some(Err(err)) => return Err(ProtoError::Transport(err.to_string())),
                None => return Ok(()),
            }
        }
    }

    /// Receive the next binary chunk, returning `Ok(None)` cleanly on zero-length half-close EOF
    /// or WebSocket status `1000`.
    pub async fn recv_chunk(&mut self) -> Result<Option<Bytes>, ProtoError> {
        loop {
            match self.ws.next_message().await {
                Some(Ok(Message::Binary(data))) => {
                    if data.is_empty() {
                        return Ok(None);
                    }
                    self.bytes_received = self
                        .bytes_received
                        .saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
                    return Ok(Some(data));
                }
                Some(Ok(Message::Ping(payload))) => {
                    self.ws.send_message(Message::Pong(payload)).await?;
                }
                Some(Ok(Message::Pong(_))) => continue,
                Some(Ok(Message::Close(cf))) => {
                    let code = cf.as_ref().map(|c| u16::from(c.code)).unwrap_or(1000);
                    if code == 1000 {
                        return Ok(None);
                    }
                    let reason = cf
                        .as_ref()
                        .map(|c| c.reason.to_string())
                        .unwrap_or_default();
                    return Err(ProtoError::Closed(format!(
                        "bytes stream closed abnormally ({code}): {reason}"
                    )));
                }
                Some(Ok(_)) => continue,
                Some(Err(err)) => return Err(ProtoError::Transport(err.to_string())),
                None => return Ok(None),
            }
        }
    }

    /// Cleanly terminate the stream with WebSocket status `1000`.
    pub async fn close_clean(&mut self) -> Result<(), ProtoError> {
        self.ws
            .close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "".into(),
            }))
            .await
    }
}

async fn recv_data_handshake(ws: &mut RexWsStream) -> Result<DataWelcome, ProtoError> {
    tokio::time::timeout(DATA_HANDSHAKE_TIMEOUT, async {
        loop {
            match ws.next_message().await {
                Some(Ok(Message::Binary(data))) => return decode_server_handshake(&data),
                Some(Ok(Message::Ping(p))) => {
                    ws.send_message(Message::Pong(p)).await?;
                }
                Some(Ok(Message::Pong(_))) => continue,
                Some(Ok(Message::Close(cf))) => {
                    return Err(ProtoError::Closed(format!(
                        "data WS closed during handshake: {cf:?}"
                    )));
                }
                Some(Ok(other)) => {
                    return Err(ProtoError::Protocol(format!(
                        "unexpected non-binary frame during data handshake: {other:?}"
                    )));
                }
                Some(Err(err)) => return Err(ProtoError::Transport(err.to_string())),
                None => {
                    return Err(ProtoError::Closed("data WS EOF during handshake".into()));
                }
            }
        }
    })
    .await
    .map_err(|_| ProtoError::Timeout("timed out waiting for data WELCOME".into()))?
}

// ============================================================================
// Zero-Dependency RFC 4648 Unpadded Base64url Codec
// ============================================================================

const B64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn encode_base64url(input: &[u8]) -> String {
    let mut out = String::with_capacity((input.len() * 4).div_ceil(3));
    let mut i = 0;
    while i + 3 <= input.len() {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | (input[i + 2] as u32);
        out.push(B64URL_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push(B64URL_ALPHABET[(n & 0x3f) as usize] as char);
        i += 3;
    }
    match input.len() - i {
        1 => {
            let n = (input[i] as u32) << 16;
            out.push(B64URL_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(B64URL_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        }
        2 => {
            let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
            out.push(B64URL_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(B64URL_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push(B64URL_ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        }
        _ => {}
    }
    out
}

fn decode_base64url(input: &str) -> Result<Vec<u8>, ProtoError> {
    let trimmed = input.trim_end_matches('=');
    let bytes = trimmed.as_bytes();
    if bytes.len() % 4 == 1 {
        return Err(ProtoError::Protocol("invalid base64url length".into()));
    }
    let mut out = Vec::with_capacity((bytes.len() * 3) / 4);
    let mut val = 0u32;
    let mut bits = 0u8;
    for &b in bytes {
        let sextet = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => {
                return Err(ProtoError::Protocol(format!(
                    "invalid base64url character 0x{b:02x}"
                )));
            }
        };
        val = (val << 6) | u32::from(sextet);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((val >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

/// Helper struct for testing serialization of rex1 control payloads.
#[derive(Debug, Clone, serde::Serialize)]
#[cfg(test)]
struct TestResizePayload {
    #[serde(rename = "type")]
    msg_type: &'static str,
    cols: u16,
    rows: u16,
    cell_width_px: u32,
    cell_height_px: u32,
    owner: Option<&'static str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_handshake_frame_layouts_are_byte_exact() {
        let ticket = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let encoded_ticket = encode_ticket_16(&ticket);
        assert_eq!(encoded_ticket.len(), 22);
        assert_eq!(decode_ticket_16(&encoded_ticket).unwrap(), ticket);

        // HELLO: 17 bytes = [0x01][16-byte ticket]
        let hello = encode_hello(ticket);
        assert_eq!(hello.len(), 17);
        assert_eq!(hello[0], 0x01);
        assert_eq!(&hello[1..17], &ticket);

        // RESUME: 25 bytes = [0x02][16-byte token][u64 LE bytes_received]
        let tok = DataReconnectToken::from_bytes(ticket);
        assert!(!format!("{tok:?}").contains("0x11"));
        let resume = encode_resume(tok, 0x0102_0304_0506_0708);
        assert_eq!(resume.len(), 25);
        assert_eq!(resume[0], 0x02);
        assert_eq!(&resume[1..17], &ticket);
        assert_eq!(
            &resume[17..25],
            &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
        );

        // WELCOME: 32 bytes = [0x81][16-byte token][0,0,0][u32 LE ttl][u64 LE nonce]
        let mut welcome_raw = [0u8; 32];
        welcome_raw[0] = 0x81;
        welcome_raw[1..17].copy_from_slice(&ticket);
        welcome_raw[20..24].copy_from_slice(&60_000u32.to_le_bytes());
        welcome_raw[24..32].copy_from_slice(&0xdead_beef_cafe_babe_u64.to_le_bytes());
        let welcome = decode_server_handshake(&welcome_raw).unwrap();
        assert_eq!(welcome.reconnect_token.expose_bytes(), ticket);
        assert_eq!(welcome.resume_ttl_ms, 60_000);
        assert_eq!(welcome.channel_nonce, 0xdead_beef_cafe_babe);

        // Zero nonce must be rejected
        let mut zero_nonce = welcome_raw;
        zero_nonce[24..32].fill(0);
        assert!(decode_server_handshake(&zero_nonce).is_err());

        // REJECT: 2 bytes = [0x82][reason]
        for (byte, expected) in [
            (0x01, DataRejectReason::InvalidOrExpiredTicket),
            (0x02, DataRejectReason::DuplicateStream),
            (0x03, DataRejectReason::StreamUnavailableOrCountTooLow),
            (0x04, DataRejectReason::LimitExceeded),
            (0x05, DataRejectReason::MalformedOrCountTooHigh),
            (0x06, DataRejectReason::ServerError),
            (0x07, DataRejectReason::SubprotocolMismatch),
            (0xfe, DataRejectReason::Unknown(0xfe)),
        ] {
            match decode_server_handshake(&[0x82, byte]) {
                Err(ProtoError::DataRejected(r)) => assert_eq!(r, expected),
                other => panic!("unexpected result: {other:?}"),
            }
        }
    }

    #[test]
    fn close_code_classifier_matches_spec() {
        assert_eq!(classify_data_close(1000, ""), DataCloseAction::StreamEnded);
        assert_eq!(
            classify_data_close(4000, "block closed"),
            DataCloseAction::BlockClosed
        );
        assert!(matches!(
            classify_data_close(1001, "keepalive timeout"),
            DataCloseAction::Resume { .. }
        ));
        assert_eq!(
            classify_data_close(1001, "data provider canceled"),
            DataCloseAction::ProviderCanceled
        );
        assert_eq!(
            classify_data_close(1013, "data reconnect required"),
            DataCloseAction::ReconnectFreshSnapshot
        );
        assert_eq!(
            classify_data_close(1008, "data provider unavailable"),
            DataCloseAction::ProviderUnavailable
        );
        assert_eq!(
            classify_data_close(1011, "data provider failed"),
            DataCloseAction::ProviderFailed
        );
    }

    #[test]
    fn rex1_apc_framer_handles_arbitrary_chunk_boundaries_and_rejects_spoofs() {
        let nonce = 123_456_789_012_345_u64;
        let resize_json = serde_json::to_string(&TestResizePayload {
            msg_type: "resize",
            cols: 98,
            rows: 32,
            cell_width_px: 8,
            cell_height_px: 16,
            owner: None,
        })
        .unwrap();

        let kitty_img = b"\x1b_Gf=100,t=d;iVBORw0KGgo=\x1b\\";
        let valid_resize = format!("\x1b_rex1;{nonce};{resize_json}\x1b\\");
        let spoofed_exit = "\x1b_rex1;999999;{\"type\":\"child_exited\",\"exit_code\":42}\x1b\\";
        let valid_theme = format!(
            "\x1b_rex1;{nonce};{{\"type\":\"theme\",\"foreground\":\"#ffffff\",\"background\":\"#121212\",\"scheme\":\"dark\"}}\x1b\\"
        );

        let mut stream = Vec::new();
        stream.extend_from_slice(b"hello ");
        stream.extend_from_slice(kitty_img);
        stream.extend_from_slice(valid_resize.as_bytes());
        stream.extend_from_slice(b"world");
        stream.extend_from_slice(spoofed_exit.as_bytes());
        stream.extend_from_slice(valid_theme.as_bytes());
        stream.extend_from_slice(b"!");

        // Test every chunk size from 1 byte (worst-case split) through full buffer.
        for chunk_size in [1usize, 2, 3, 5, 7, 13, 64, stream.len()] {
            let mut framer = RexApcFramer::new(nonce);
            let mut vt_collected = Vec::new();
            let mut controls = Vec::new();

            for chunk in stream.chunks(chunk_size) {
                for item in framer.feed(chunk) {
                    match item {
                        FramedChunk::VtBytes(b) => vt_collected.extend_from_slice(&b),
                        FramedChunk::Control(c) => controls.push(c),
                    }
                }
            }
            for item in framer.finish() {
                if let FramedChunk::VtBytes(b) = item {
                    vt_collected.extend_from_slice(&b);
                }
            }

            // Kitty Graphics APC and plain text must pass through untouched!
            let mut expected_vt = Vec::new();
            expected_vt.extend_from_slice(b"hello ");
            expected_vt.extend_from_slice(kitty_img);
            expected_vt.extend_from_slice(b"world!");
            assert_eq!(vt_collected, expected_vt, "chunk_size={chunk_size}");

            // Only the two nonce-authenticated control messages should appear!
            assert_eq!(controls.len(), 2, "chunk_size={chunk_size}");
            assert_eq!(
                controls[0],
                RexControlMessage::Resize {
                    cols: 98,
                    rows: 32,
                    cell_width_px: 8,
                    cell_height_px: 16,
                    owner: None,
                }
            );
            assert_eq!(
                controls[1],
                RexControlMessage::Theme {
                    foreground: Some("#ffffff".into()),
                    background: Some("#121212".into()),
                    cursor: None,
                    palette: vec![],
                    scheme: Some(ThemeScheme::Dark),
                }
            );
            assert_eq!(framer.spoofed_apc_count(), 1, "chunk_size={chunk_size}");
        }
    }
}
