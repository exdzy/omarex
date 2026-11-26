//! Chunked, backpressure-aware file uploads (`upload_begin`) and downloads (`download_begin`)
//! over `rex.data.bytes.v1` with bounded memory (`64 KiB` streaming chunks) and atomic cache writes.

use std::path::{Path, PathBuf};

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use crate::ProtoError;
use crate::control::ControlClient;
use crate::data_stream::BytesDataStream;
use crate::types::{TerminalDownloadBeginArgs, TerminalUploadBeginArgs};

/// Streaming chunk size (`64 KiB` — matches Rex's maximum data WebSocket frame payload).
pub const TRANSFER_CHUNK_BYTES: usize = 64 * 1024;

/// Default maximum download size (`256 MiB`) to bound disk/cache usage.
pub const DEFAULT_MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// Optional multi-file drag-and-drop batch metadata for `upload_begin`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadBatch {
    pub batch_id: String,
    pub index: u32,
    pub count: u32,
}

/// Progress snapshot reported during upload or download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferProgress {
    pub bytes_transferred: u64,
    pub total_bytes: u64,
}

/// Outcome of a completed file upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadResult {
    pub sanitized_name: String,
    pub bytes_uploaded: u64,
}

/// Outcome of a file download request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadResult {
    /// Cached copy with `if_none_match` version is still fresh; no bytes downloaded.
    NotModified {
        remote_path: String,
        version: Option<String>,
    },
    /// File streamed to `local_path` inside the cache directory.
    Downloaded {
        remote_path: String,
        name: String,
        local_path: PathBuf,
        bytes_downloaded: u64,
        modified_at: String,
        /// `None` if file changed within last second or server couldn't compute version (do not cache).
        version: Option<String>,
    },
}

/// Sanitize a local filename for `upload_begin` (last component only, control characters stripped).
pub fn sanitize_upload_filename(path: &Path) -> Result<String, ProtoError> {
    let raw = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| ProtoError::Endpoint(format!("invalid file name: {}", path.display())))?;
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\')
        .collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return Err(ProtoError::Endpoint(format!(
            "cannot upload path with unsafe filename `{raw}`"
        )));
    }
    Ok(cleaned)
}

/// Stream a local file to a Rex terminal block via `com.superlogical.terminal.upload_begin`
/// and `rex.data.bytes.v1` using bounded `64 KiB` chunks (never loads entire file into RAM).
pub async fn upload_file<F>(
    control: &ControlClient,
    session_id: &str,
    block_id: &str,
    local_path: &Path,
    batch: Option<UploadBatch>,
    mut on_progress: F,
) -> Result<UploadResult, ProtoError>
where
    F: FnMut(TransferProgress),
{
    let sanitized_name = sanitize_upload_filename(local_path)?;
    let meta = tokio::fs::metadata(local_path).await?;
    if !meta.is_file() {
        return Err(ProtoError::Protocol(format!(
            "upload path is not a regular file: {}",
            local_path.display()
        )));
    }
    let total_bytes = meta.len();

    let (batch_id, index, count) = match batch {
        Some(b) => (Some(b.batch_id), Some(b.index), Some(b.count)),
        None => (None, None, None),
    };

    let data_ref = format!("upload:{}", Uuid::new_v4());
    let (begin, _) = control
        .terminal_upload_begin(
            session_id,
            block_id,
            TerminalUploadBeginArgs {
                data_ref,
                name: sanitized_name.clone(),
                size: total_bytes,
                batch: batch_id,
                index,
                count,
            },
        )
        .await?;

    let mut stream =
        BytesDataStream::connect_with_ticket(control.endpoint(), &begin.ticket).await?;
    let mut file = tokio::fs::File::open(local_path).await?;
    let mut buf = vec![0u8; TRANSFER_CHUNK_BYTES];
    let mut uploaded: u64 = 0;

    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        stream.send_chunk(Bytes::copy_from_slice(&buf[..n])).await?;
        uploaded = uploaded.saturating_add(n as u64);
        on_progress(TransferProgress {
            bytes_transferred: uploaded,
            total_bytes,
        });
    }

    stream.half_close().await?;
    stream.wait_for_server_close().await?;

    Ok(UploadResult {
        sanitized_name,
        bytes_uploaded: uploaded,
    })
}

/// Download a remote file from a Rex terminal block via `com.superlogical.terminal.download_begin`
/// and `rex.data.bytes.v1` into `cache_dir` using bounded `64 KiB` streaming writes and atomic rename.
#[allow(clippy::too_many_arguments)]
pub async fn download_file<F>(
    control: &ControlClient,
    session_id: &str,
    block_id: &str,
    remote_path: &str,
    cache_dir: &Path,
    if_none_match: Option<String>,
    max_bytes: u64,
    mut on_progress: F,
) -> Result<DownloadResult, ProtoError>
where
    F: FnMut(TransferProgress),
{
    let data_ref = format!("download:{}", Uuid::new_v4());
    let (begin, _) = control
        .terminal_download_begin(
            session_id,
            block_id,
            TerminalDownloadBeginArgs {
                data_ref,
                path: remote_path.to_string(),
                if_none_match,
            },
        )
        .await?;

    if begin.not_modified {
        return Ok(DownloadResult::NotModified {
            remote_path: begin.path,
            version: begin.version,
        });
    }

    let ticket = begin.ticket.as_deref().ok_or_else(|| {
        ProtoError::Protocol("download_begin returned neither not_modified nor ticket".into())
    })?;

    if begin.size > max_bytes {
        return Err(ProtoError::Protocol(format!(
            "remote file size {} exceeds max download limit {max_bytes}",
            begin.size
        )));
    }

    tokio::fs::create_dir_all(cache_dir).await?;
    let safe_name = sanitize_upload_filename(Path::new(&begin.name))
        .unwrap_or_else(|_| "download.bin".to_string());
    let tmp_path = cache_dir.join(format!(".{}.{}.part", Uuid::new_v4(), safe_name));
    let final_name = match &begin.version {
        Some(ver) => format!("{}-{}", sanitize_cache_key(ver), safe_name),
        None => format!("uncached-{}-{}", Uuid::new_v4(), safe_name),
    };
    let final_path = cache_dir.join(final_name);

    let mut stream = BytesDataStream::connect_with_ticket(control.endpoint(), ticket).await?;
    let mut file = tokio::fs::File::create(&tmp_path).await?;
    let mut downloaded: u64 = 0;

    let stream_res: Result<(), ProtoError> = async {
        while let Some(chunk) = stream.recv_chunk().await? {
            let next_total = downloaded.saturating_add(chunk.len() as u64);
            if next_total > max_bytes {
                return Err(ProtoError::Protocol(format!(
                    "download stream exceeded max limit {max_bytes} bytes"
                )));
            }
            file.write_all(&chunk).await?;
            downloaded = next_total;
            on_progress(TransferProgress {
                bytes_transferred: downloaded,
                total_bytes: begin.size,
            });
        }
        file.flush().await?;
        Ok(())
    }
    .await;

    if let Err(err) = stream_res {
        drop(file);
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(err);
    }

    drop(file);
    tokio::fs::rename(&tmp_path, &final_path).await?;

    Ok(DownloadResult::Downloaded {
        remote_path: begin.path,
        name: begin.name,
        local_path: final_path,
        bytes_downloaded: downloaded,
        modified_at: begin.modified_at,
        version: begin.version,
    })
}

fn sanitize_cache_key(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    if cleaned.is_empty() {
        "v".into()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_upload_filenames_and_cache_keys() {
        assert_eq!(
            sanitize_upload_filename(Path::new("/tmp/foo/screenshot.png")).unwrap(),
            "screenshot.png"
        );
        assert_eq!(
            sanitize_upload_filename(Path::new("bad\nname\x07.txt")).unwrap(),
            "badname.txt"
        );
        assert!(sanitize_upload_filename(Path::new("/")).is_err());
        assert_eq!(sanitize_cache_key("v1:abc/def"), "v1_abc_def");
    }

    /// Opt-in live roundtrip verification (`OMAREX_LIVE_TRANSFER_TEST=1`) against local Rex:
    /// spawns a temporary shell session, uploads an image + video batch via `upload_file`,
    /// downloads a file via `download_file` (including `NotModified`), and destroys the temp session.
    #[tokio::test]
    async fn live_upload_and_download_roundtrip_when_enabled() {
        if std::env::var("OMAREX_LIVE_TRANSFER_TEST").as_deref() != Ok("1") {
            return;
        }
        use crate::control::{ControlClient, ControlConfig};
        use crate::endpoint::RexEndpoint;
        use crate::types::{
            BlockCreateSpec, InitialWindowSpec, LayoutCreateSpec, SessionCreateRequest,
        };

        let ep = RexEndpoint::discover_default();
        let mut cfg = ControlConfig::new(ep);
        cfg.auto_reconnect = false;
        cfg.client_info.label = Some("omarex-transfer-test".into());
        let (client, _welcome, _events) = ControlClient::connect(cfg)
            .await
            .expect("connect to local rex");

        let (created, _) = client
            .session_create(&SessionCreateRequest {
                label: Some("omarex-transfer-verify".into()),
                initial_windows: Some(vec![InitialWindowSpec {
                    window_label: Some("verify".into()),
                    layout: LayoutCreateSpec {
                        block: Some(BlockCreateSpec {
                            flavor: "com.superlogical.terminal.shell".into(),
                            label: Some("verify".into()),
                            options: None,
                        }),
                        split: None,
                    },
                }]),
            })
            .await
            .expect("create temporary session");
        let session_id = created.session_id.clone();
        let block_id = created.initial_windows[0].block_ids[0].clone();

        let _ = client.session_attach(&session_id).await.expect("attach");

        let tmp_dir = std::env::temp_dir().join(format!("omarex-live-xfer-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp_dir);
        let img_path = tmp_dir.join("test shot.png");
        let vid_path = tmp_dir.join("clip.mp4");
        std::fs::write(&img_path, b"\x89PNG\r\n\x1a\nfake-png-data").unwrap();
        std::fs::write(&vid_path, vec![0x42u8; 150_000]).unwrap();

        let batch_id = Uuid::new_v4().to_string();
        let (u0, u1) = tokio::join!(
            upload_file(
                &client,
                &session_id,
                &block_id,
                &img_path,
                Some(UploadBatch {
                    batch_id: batch_id.clone(),
                    index: 0,
                    count: 2,
                }),
                |_| {},
            ),
            upload_file(
                &client,
                &session_id,
                &block_id,
                &vid_path,
                Some(UploadBatch {
                    batch_id,
                    index: 1,
                    count: 2,
                }),
                |_| {},
            )
        );

        let dl_cache = tmp_dir.join("dl-cache");
        let dl1 = download_file(
            &client,
            &session_id,
            &block_id,
            &img_path.to_string_lossy(),
            &dl_cache,
            None,
            DEFAULT_MAX_DOWNLOAD_BYTES,
            |_| {},
        )
        .await;

        // Always destroy our temporary session before asserting!
        let _ = client.session_destroy(&session_id).await;
        let _ = std::fs::remove_dir_all(&tmp_dir);

        let r0 = u0.expect("upload_file #0 (png) should succeed");
        let r1 = u1.expect("upload_file #1 (150KB mp4) should succeed");
        assert_eq!(r0.sanitized_name, "test shot.png");
        assert_eq!(r1.bytes_uploaded, 150_000);
        match dl1.expect("download_file should succeed") {
            DownloadResult::Downloaded {
                bytes_downloaded, ..
            } => {
                assert_eq!(bytes_downloaded, 21);
            }
            other => panic!("expected Downloaded, got {other:?}"),
        }
    }
}
