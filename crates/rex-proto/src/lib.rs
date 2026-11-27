//! Rex Control API (revision 24) & Data WebSocket (`rex.data.terminal.v1` / `rex.data.bytes.v1`)
//! protocol client for `omarex`.

pub mod control;
pub mod data_stream;
pub mod endpoint;
pub mod transfer;
pub mod transport;
pub mod types;

use thiserror::Error;

pub use control::{ControlClient, ControlConfig, ControlNotification, RpcOutcome};
pub use data_stream::{
    BytesDataStream, DataCloseAction, DataReconnectToken, DataRejectReason, DataWelcome,
    FramedChunk, RecoveryOutcome, RexApcFramer, RexControlMessage, TerminalDataStream,
    TerminalStreamChunk,
};
pub use endpoint::RexEndpoint;
pub use transfer::{
    DEFAULT_MAX_DOWNLOAD_BYTES, DownloadResult, TRANSFER_CHUNK_BYTES, TransferProgress,
    UploadBatch, UploadResult, download_file, upload_file,
};
pub use types::{
    CONTROL_SUBPROTOCOL, DATA_BYTES_SUBPROTOCOL, DATA_TERMINAL_SUBPROTOCOL, RpcError,
    TARGET_API_REVISION,
};

/// Errors returned by `rex-proto`.
#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("endpoint error: {0}")]
    Endpoint(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("RPC error: {0}")]
    Rpc(RpcError),
    #[error("data handshake rejected: {0}")]
    DataRejected(DataRejectReason),
    #[error("timeout: {0}")]
    Timeout(String),
    #[error("connection closed: {0}")]
    Closed(String),
    #[error("serialization error: {0}")]
    Serde(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<serde_json::Error> for ProtoError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serde(value.to_string())
    }
}

impl RpcError {
    /// Returns `true` when the Rex server reports the target session no longer exists.
    pub fn is_session_not_found(&self) -> bool {
        self.code == "session_not_found" || (self.status == 404 && self.message.contains("session"))
    }

    /// Returns `true` for any 404 / missing-entity RPC error (stale session, window, block, or prompt).
    pub fn is_not_found(&self) -> bool {
        self.status == 404
            || matches!(
                self.code.as_str(),
                "not_found"
                    | "session_not_found"
                    | "block_not_found"
                    | "window_not_found"
                    | "prompt_not_found"
            )
    }

    /// Returns `true` for 409 conflict errors (e.g., `data.connect` racing `session.attach`).
    pub fn is_conflict(&self) -> bool {
        self.status == 409 || self.code == "conflict"
    }

    /// Returns `true` for 400 bad_request errors (e.g., `session.own_block` / `disown_block` on an
    /// unattached or tearing-down session/block).
    pub fn is_bad_request(&self) -> bool {
        self.status == 400 || self.code == "bad_request"
    }
}

impl ProtoError {
    /// Borrow the underlying [`RpcError`] if this is `ProtoError::Rpc`.
    pub fn as_rpc(&self) -> Option<&RpcError> {
        match self {
            Self::Rpc(err) => Some(err),
            _ => None,
        }
    }

    /// Returns `true` when the server rejected the RPC because the target session no longer exists.
    pub fn is_session_not_found(&self) -> bool {
        self.as_rpc().is_some_and(RpcError::is_session_not_found)
    }

    /// Returns `true` when the server rejected the RPC with 404 / missing resource.
    pub fn is_not_found(&self) -> bool {
        self.as_rpc().is_some_and(RpcError::is_not_found)
    }

    /// Returns `true` when the server rejected the RPC with 409 conflict.
    pub fn is_conflict(&self) -> bool {
        self.as_rpc().is_some_and(RpcError::is_conflict)
    }

    /// Returns `true` when the server rejected the RPC with 400 bad_request.
    pub fn is_bad_request(&self) -> bool {
        self.as_rpc().is_some_and(RpcError::is_bad_request)
    }

    /// Returns `true` for transient transport-drop errors expected during host reconnects/shutdown.
    pub fn is_transient_disconnect(&self) -> bool {
        matches!(self, Self::Closed(_) | Self::Timeout(_))
    }
}

#[cfg(test)]
mod error_classification_tests {
    use super::*;

    #[test]
    fn proto_error_helpers_classify_rex_rpc_statuses_and_codes() {
        let sess_404 = ProtoError::Rpc(RpcError {
            code: "session_not_found".into(),
            message: "session session:abc not found".into(),
            status: 404,
        });
        assert!(sess_404.is_session_not_found());
        assert!(sess_404.is_not_found());
        assert!(!sess_404.is_conflict());

        let block_404 = ProtoError::Rpc(RpcError {
            code: "not_found".into(),
            message: "block not found".into(),
            status: 404,
        });
        assert!(!block_404.is_session_not_found());
        assert!(block_404.is_not_found());

        let conflict_409 = ProtoError::Rpc(RpcError {
            code: "conflict".into(),
            message: "client is not attached to session".into(),
            status: 409,
        });
        assert!(conflict_409.is_conflict());
        assert!(!conflict_409.is_not_found());

        let bad_400 = ProtoError::Rpc(RpcError {
            code: "bad_request".into(),
            message: "block_id not in session".into(),
            status: 400,
        });
        assert!(bad_400.is_bad_request());

        assert!(ProtoError::Closed("peer reset".into()).is_transient_disconnect());
        assert!(ProtoError::Timeout("control RPC timed out".into()).is_transient_disconnect());
        assert!(!sess_404.is_transient_disconnect());
    }
}

#[cfg(test)]
mod live_smoke_tests {
    use std::path::Path;
    use std::time::Duration;

    use crate::control::{ControlClient, ControlConfig};
    use crate::data_stream::{TerminalDataStream, TerminalStreamChunk};
    use crate::endpoint::RexEndpoint;
    use crate::types::{
        ClientKeymapRequest, DataTicketSpec, SessionBootstrapRequest, TARGET_API_REVISION,
    };

    /// Read-only live smoke test against the local Omarchy Rex server socket.
    ///
    /// Skips gracefully if `/run/user/1000/rex/server.sock` (or discovered default socket) is absent.
    /// Strictly read-only: NEVER creates, modifies, focuses, owns, or closes any session/window/block.
    #[tokio::test]
    async fn live_readonly_smoke_bootstrap_keymap_and_snapshot_header() {
        let ep = RexEndpoint::discover_default();
        let sock_path = match &ep {
            RexEndpoint::Unix(p) if p.exists() => p.clone(),
            _ => {
                let fallback = Path::new("/run/user/1000/rex/server.sock");
                if fallback.exists() {
                    fallback.to_path_buf()
                } else {
                    eprintln!("skipping live Rex smoke test: local server.sock not present");
                    return;
                }
            }
        };

        let mut cfg = ControlConfig::new(RexEndpoint::Unix(sock_path));
        cfg.auto_reconnect = false;
        cfg.client_info.label = Some("omarex-proto-smoke-readonly".into());

        let (client, welcome, _events) = ControlClient::connect(cfg)
            .await
            .expect("live Rex control handshake should succeed");
        assert_eq!(welcome.protocol, "rex.control.v1");
        assert_eq!(welcome.api_revision, TARGET_API_REVISION);
        assert!(welcome.client.id.starts_with("client:"));
        assert!(welcome.client.reconnect_token.is_some());

        // 1. Single-RTT read-only session.bootstrap with terminal data tickets
        let (boot, boot_seq) = client
            .bootstrap(&SessionBootstrapRequest {
                event_scope: "all".into(),
                preferred_session_id: None,
                attach_session_ids: None,
                data_tickets: Some(vec![DataTicketSpec::terminal_v1()]),
            })
            .await
            .expect("session.bootstrap should succeed");
        assert_eq!(boot.client_id, welcome.client.id);
        assert!(boot.server_status.pid > 0);
        let _ = boot_seq;

        // 2. Read-only client.keymap query
        let (keymap, _) = client
            .client_keymap(&ClientKeymapRequest::default())
            .await
            .expect("client.keymap should succeed");
        assert!(!keymap.config.instance_id.is_empty());

        // 3. Read-only hosts.list & server.version queries
        let (hosts, _) = client
            .hosts_list()
            .await
            .expect("hosts.list should succeed");
        assert!(!hosts.hash.is_empty());

        let (version, _) = client
            .server_version()
            .await
            .expect("server.version should succeed");
        assert!(!version.rex_version.is_empty());
        assert!(!version.platform.is_empty());

        // 4. If a live terminal data ticket was issued, verify read-only OSC 7577 `program_resume_records`
        //    query AND open /api/data, verify WELCOME & GHOSTSNP magic, then close cleanly (1000).
        if let Some(grant) = boot.data_tickets.first() {
            let (_resume_records, _) = client
                .terminal_program_resume_records(&grant.session_id, &grant.block_id)
                .await
                .expect("com.superlogical.terminal.program_resume_records should succeed");
            let mut stream = TerminalDataStream::connect_with_ticket(
                client.endpoint().clone(),
                grant.session_id.clone(),
                grant.block_id.clone(),
                grant.data_ref_id.clone(),
                &grant.ticket,
            )
            .await
            .expect("live /api/data handshake should succeed");
            assert_ne!(stream.channel_nonce(), 0);
            assert!(stream.welcome().resume_ttl_ms > 0);

            let mut header = Vec::new();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            while header.len() < 10 && tokio::time::Instant::now() < deadline {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                match tokio::time::timeout(remaining, stream.next_chunk()).await {
                    Ok(Ok(TerminalStreamChunk::Bytes(chunk))) => header.extend_from_slice(&chunk),
                    Ok(Ok(other)) => panic!("unexpected stream event before snapshot: {other:?}"),
                    Ok(Err(err)) => panic!("data stream error: {err}"),
                    Err(_) => panic!("timed out waiting for GHOSTSNP header"),
                }
            }
            assert!(header.len() >= 10);
            assert_eq!(
                &header[..8],
                b"GHOSTSNP",
                "terminal stream must begin with GHOSTSNP snapshot magic"
            );
            assert_eq!(
                u16::from_le_bytes([header[8], header[9]]),
                1,
                "expected GHOSTSNP v1"
            );
            assert_eq!(stream.bytes_received(), header.len() as u64);

            stream.close_clean().await.expect("clean WS 1000 close");
        }

        client.shutdown();
    }
}
