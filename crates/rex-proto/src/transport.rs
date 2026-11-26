//! Async WebSocket transport over Unix domain sockets, TCP, rustls TLS (`https://`), and `ssh://` (`rex __internal dial-stdio`).
//!
//! Note: `tokio-tungstenite` does NOT enable `permessage-deflate`, strictly enforcing Rex's
//! requirement that `hello`, frames `< 512B`, and `/api/data` streams are never compressed.

use std::io;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Once};
use std::task::{Context, Poll};

use futures_util::{SinkExt, StreamExt};
use rustls_platform_verifier::BuilderVerifierExt;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpStream, UnixStream};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Message, WebSocketConfig};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use crate::ProtoError;
use crate::endpoint::RexEndpoint;

/// Maximum incoming WebSocket message size (16 MiB, generously above Rex's 1 MiB control limit).
const MAX_WS_MESSAGE_SIZE: usize = 16 * 1024 * 1024;
/// Maximum incoming WebSocket frame size (4 MiB).
const MAX_WS_FRAME_SIZE: usize = 4 * 1024 * 1024;

static CRYPTO_PROVIDER_INIT: Once = Once::new();

fn ensure_crypto_provider() {
    CRYPTO_PROVIDER_INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn build_tls_connector() -> Result<Connector, ProtoError> {
    ensure_crypto_provider();
    let cfg = match rustls::ClientConfig::builder().with_platform_verifier() {
        Ok(builder) => builder.with_no_client_auth(),
        Err(err) => {
            tracing::warn!(
                error = %err,
                "platform TLS verifier unavailable; falling back to webpki-roots"
            );
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth()
        }
    };
    Ok(Connector::Rustls(Arc::new(cfg)))
}

fn rex_ws_config() -> WebSocketConfig {
    let mut cfg = WebSocketConfig::default();
    cfg.max_message_size = Some(MAX_WS_MESSAGE_SIZE);
    cfg.max_frame_size = Some(MAX_WS_FRAME_SIZE);
    cfg
}

/// Async bidirectional byte stream backed by `ssh <target> rex __internal dial-stdio`.
pub struct SshStdioStream {
    _child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

impl std::fmt::Debug for SshStdioStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshStdioStream").finish_non_exhaustive()
    }
}

impl AsyncRead for SshStdioStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stdout).poll_read(cx, buf)
    }
}

impl AsyncWrite for SshStdioStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stdin).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stdin).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stdin).poll_shutdown(cx)
    }
}

/// Unified WebSocket stream over Unix socket, TCP/TLS, SSH stdio, or in-memory duplex (for tests).
pub enum RexWsStream {
    Unix(Box<WebSocketStream<UnixStream>>),
    Tcp(Box<WebSocketStream<MaybeTlsStream<TcpStream>>>),
    Ssh(Box<WebSocketStream<SshStdioStream>>),
    Duplex(Box<WebSocketStream<tokio::io::DuplexStream>>),
}

impl RexWsStream {
    /// Wrap an in-memory `DuplexStream` WebSocket (for unit tests).
    pub fn from_duplex(ws: WebSocketStream<tokio::io::DuplexStream>) -> Self {
        Self::Duplex(Box::new(ws))
    }

    /// Receive the next WebSocket message. Cancel-safe across all variants.
    pub async fn next_message(
        &mut self,
    ) -> Option<Result<Message, tokio_tungstenite::tungstenite::Error>> {
        match self {
            Self::Unix(ws) => ws.next().await,
            Self::Tcp(ws) => ws.next().await,
            Self::Ssh(ws) => ws.next().await,
            Self::Duplex(ws) => ws.next().await,
        }
    }

    /// Send a WebSocket message and flush.
    pub async fn send_message(&mut self, msg: Message) -> Result<(), ProtoError> {
        let res = match self {
            Self::Unix(ws) => ws.send(msg).await,
            Self::Tcp(ws) => ws.send(msg).await,
            Self::Ssh(ws) => ws.send(msg).await,
            Self::Duplex(ws) => ws.send(msg).await,
        };
        res.map_err(|e| ProtoError::Transport(e.to_string()))
    }

    /// Gracefully close the WebSocket with an optional close frame.
    pub async fn close(&mut self, frame: Option<CloseFrame>) -> Result<(), ProtoError> {
        let res = match self {
            Self::Unix(ws) => ws.close(frame).await,
            Self::Tcp(ws) => ws.close(frame).await,
            Self::Ssh(ws) => ws.close(frame).await,
            Self::Duplex(ws) => ws.close(frame).await,
        };
        res.map_err(|e| ProtoError::Transport(e.to_string()))
    }
}

/// Dial a Rex WebSocket endpoint (`/api/control` or `/api/data`) with the required subprotocol.
pub async fn dial_websocket(
    endpoint: &RexEndpoint,
    api_path: &str,
    subprotocol: &str,
) -> Result<RexWsStream, ProtoError> {
    let uri = endpoint.ws_uri(api_path);
    let mut request = uri
        .as_str()
        .into_client_request()
        .map_err(|e| ProtoError::Endpoint(format!("invalid websocket URI `{uri}`: {e}")))?;

    let proto_hdr = HeaderValue::from_str(subprotocol)
        .map_err(|e| ProtoError::Protocol(format!("invalid subprotocol `{subprotocol}`: {e}")))?;
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", proto_hdr);

    let ws_config = Some(rex_ws_config());

    match endpoint {
        RexEndpoint::Unix(path) => {
            let stream = UnixStream::connect(path).await.map_err(|e| {
                ProtoError::Transport(format!("failed to connect to {}: {e}", path.display()))
            })?;
            let (ws, _resp) =
                tokio_tungstenite::client_async_with_config(request, stream, ws_config)
                    .await
                    .map_err(|e| ProtoError::Transport(format!("unix WS handshake failed: {e}")))?;
            Ok(RexWsStream::Unix(Box::new(ws)))
        }
        RexEndpoint::Tcp {
            host, port, tls, ..
        } => {
            let tcp = TcpStream::connect((host.as_str(), *port))
                .await
                .map_err(|e| ProtoError::Transport(format!("TCP connect {host}:{port}: {e}")))?;
            let _ = tcp.set_nodelay(true);
            let connector = if *tls {
                Some(build_tls_connector()?)
            } else {
                Some(Connector::Plain)
            };
            let (ws, _resp) =
                tokio_tungstenite::client_async_tls_with_config(request, tcp, ws_config, connector)
                    .await
                    .map_err(|e| {
                        ProtoError::Transport(format!("TCP/TLS WS handshake failed: {e}"))
                    })?;
            Ok(RexWsStream::Tcp(Box::new(ws)))
        }
        RexEndpoint::Ssh { user, host, port } => {
            let mut cmd = Command::new("ssh");
            cmd.arg("-o").arg("BatchMode=yes");
            cmd.arg("-T");
            if let Some(p) = port {
                cmd.arg("-p").arg(p.to_string());
            }
            if let Some(u) = user {
                cmd.arg("-l").arg(u);
            }
            cmd.arg(host);
            cmd.arg("rex").arg("__internal").arg("dial-stdio");
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);

            let mut child = cmd.spawn().map_err(|e| {
                ProtoError::Transport(format!("failed to spawn ssh tunnel to `{host}`: {e}"))
            })?;
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| ProtoError::Transport("missing ssh stdin pipe".into()))?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| ProtoError::Transport("missing ssh stdout pipe".into()))?;

            let stdio = SshStdioStream {
                _child: child,
                stdin,
                stdout,
            };
            let (ws, _resp) =
                tokio_tungstenite::client_async_with_config(request, stdio, ws_config)
                    .await
                    .map_err(|e| ProtoError::Transport(format!("ssh WS handshake failed: {e}")))?;
            Ok(RexWsStream::Ssh(Box::new(ws)))
        }
    }
}
