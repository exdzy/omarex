//! Zero-polling Hyprland IPC client for live `decoration:rounding` tracking.
//!
//! Queries `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock` once at
//! startup and subscribes to `.socket2.sock` for `configreloaded>>` events — zero
//! subprocess forks and zero idle polling.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::{ThemeError, ThemeEvent};

const ROUNDING_QUERY: &[u8] = b"j/getoption decoration:rounding";
const IPC_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Deserialize)]
struct HyprOptionInt {
    #[serde(default)]
    int: Option<i64>,
}

/// Resolves the active Hyprland IPC socket directory from environment variables.
///
/// Returns `None` when not running under a Hyprland session.
pub fn hyprland_socket_dir() -> Option<PathBuf> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    if sig.trim().is_empty() {
        return None;
    }
    let runtime = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| dirs::runtime_dir().map(|p| p.to_string_lossy().into_owned()))?;
    let dir = PathBuf::from(runtime).join("hypr").join(sig);
    dir.exists().then_some(dir)
}

/// Parses a Hyprland `j/getoption decoration:rounding` JSON response into a non-negative `f32`.
pub fn parse_rounding_json(raw: &str) -> Result<f32, ThemeError> {
    let parsed: HyprOptionInt =
        serde_json::from_str(raw).map_err(|e| ThemeError::Hyprland(e.to_string()))?;
    let val = parsed
        .int
        .ok_or_else(|| ThemeError::Hyprland("missing `int` field in hyprland option".to_owned()))?;
    Ok(val.max(0) as f32)
}

/// Synchronously queries `decoration:rounding` from `<socket_dir>/.socket.sock`.
pub fn query_rounding_sync(socket_dir: &Path) -> Result<f32, ThemeError> {
    let sock_path = socket_dir.join(".socket.sock");
    let mut stream = StdUnixStream::connect(&sock_path)?;
    stream.set_read_timeout(Some(IPC_TIMEOUT))?;
    stream.set_write_timeout(Some(IPC_TIMEOUT))?;
    stream.write_all(ROUNDING_QUERY)?;
    stream.shutdown(std::net::Shutdown::Write)?;

    let mut buf = String::new();
    stream.read_to_string(&mut buf)?;
    parse_rounding_json(&buf)
}

/// Asynchronously queries `decoration:rounding` from `<socket_dir>/.socket.sock`.
pub async fn query_rounding(socket_dir: &Path) -> Result<f32, ThemeError> {
    let sock_path = socket_dir.join(".socket.sock");
    let fut = async move {
        let mut stream = UnixStream::connect(&sock_path).await?;
        stream.write_all(ROUNDING_QUERY).await?;
        stream.shutdown().await?;
        let mut buf = String::new();
        stream.read_to_string(&mut buf).await?;
        parse_rounding_json(&buf)
    };
    tokio::time::timeout(IPC_TIMEOUT, fut)
        .await
        .map_err(|_| ThemeError::Hyprland("timed out querying Hyprland socket".to_owned()))?
}

/// Queries the current Hyprland `decoration:rounding`, cleanly falling back to `0.0`
/// if outside Hyprland or if the socket is unavailable.
pub fn query_current_rounding_or_zero() -> f32 {
    let Some(dir) = hyprland_socket_dir() else {
        return 0.0;
    };
    query_rounding_sync(&dir).unwrap_or(0.0)
}

/// Keeps the Hyprland `.socket2.sock` event listener alive; dropping cancels it.
pub struct HyprlandWatcherHandle {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for HyprlandWatcherHandle {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Starts listening on Hyprland's live `.socket2.sock` if running under Hyprland.
///
/// Returns `(initial_rounding, Option<HyprlandWatcherHandle>)`. Outside Hyprland,
/// cleanly returns `(0.0, None)` without spawning any task.
pub fn spawn_hyprland_rounding_watcher(
    events: mpsc::Sender<ThemeEvent>,
) -> (f32, Option<HyprlandWatcherHandle>) {
    let Some(dir) = hyprland_socket_dir() else {
        return (0.0, None);
    };
    let initial = query_rounding_sync(&dir).unwrap_or(0.0);
    let handle = spawn_hyprland_rounding_watcher_in(dir, initial, events);
    (initial, Some(handle))
}

/// Testable entry point targeting an explicit Hyprland socket directory.
pub fn spawn_hyprland_rounding_watcher_in(
    socket_dir: PathBuf,
    initial_rounding: f32,
    events: mpsc::Sender<ThemeEvent>,
) -> HyprlandWatcherHandle {
    let task = tokio::spawn(async move {
        let mut last_rounding = initial_rounding;
        let mut backoff = Duration::from_millis(250);
        let event_sock = socket_dir.join(".socket2.sock");

        loop {
            match UnixStream::connect(&event_sock).await {
                Ok(stream) => {
                    backoff = Duration::from_millis(250);
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) => break, // EOF — Hyprland closed or restarted socket2
                            Ok(_) => {
                                if is_config_reloaded_event(&line)
                                    && let Ok(next) = query_rounding(&socket_dir).await
                                    && (next - last_rounding).abs() > f32::EPSILON
                                {
                                    last_rounding = next;
                                    if events
                                        .send(ThemeEvent::RoundingChanged(next))
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                            Err(err) => {
                                tracing::debug!(error = %err, "hyprland socket2 read error");
                                break;
                            }
                        }
                    }
                }
                Err(err) => {
                    tracing::debug!(
                        error = %err,
                        path = %event_sock.display(),
                        "hyprland socket2 connect failed; backing off"
                    );
                }
            }

            if events.is_closed() {
                return;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    });

    HyprlandWatcherHandle { task: Some(task) }
}

fn is_config_reloaded_event(line: &str) -> bool {
    line.starts_with("configreloaded>>")
}
