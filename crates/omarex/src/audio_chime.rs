//! Zero-asset PipeWire micro-chime synthesizer for OSC 7501 (`Blocked`/`Error`/`Done`) transitions.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, SyncSender};
use std::time::{Duration, Instant};

/// Sample rate used for synthesized PipeWire raw s16le mono PCM chimes.
pub const CHIME_SAMPLE_RATE: u32 = 48_000;

/// Audible micro-chime flavor for OSC 7501 agent/program status transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChimeKind {
    /// Gentle ascending interval when an agent/task finishes (`Done`).
    Done,
    /// Distinct softer attention interval when an agent needs input (`Blocked` / `Error`).
    Attention,
}

/// Synthesizes a non-clipping 48 kHz mono signed 16-bit little-endian PCM chime buffer.
#[must_use]
pub fn synthesize_chime_pcm(kind: ChimeKind) -> Vec<u8> {
    let notes: &[(f32, u32)] = match kind {
        ChimeKind::Done => &[(523.25, 90), (783.99, 110)],
        ChimeKind::Attention => &[(440.0, 110), (329.63, 130)],
    };
    let amplitude = 0.28_f32 * f32::from(i16::MAX);
    let total_samples: usize = notes
        .iter()
        .map(|(_, ms)| ((CHIME_SAMPLE_RATE as usize) * (*ms as usize)) / 1000)
        .sum();
    let mut pcm = Vec::with_capacity(total_samples * 2);

    for &(freq, ms) in notes {
        let count = ((CHIME_SAMPLE_RATE as usize) * (ms as usize)) / 1000;
        let fade_samples = ((CHIME_SAMPLE_RATE as usize) * 12) / 1000;
        for i in 0..count {
            let t = i as f32 / CHIME_SAMPLE_RATE as f32;
            let env = if i < fade_samples {
                let p = i as f32 / fade_samples.max(1) as f32;
                0.5 * (1.0 - (std::f32::consts::PI * p).cos())
            } else if i + fade_samples >= count {
                let rem = count.saturating_sub(i) as f32 / fade_samples.max(1) as f32;
                0.5 * (1.0 - (std::f32::consts::PI * rem).cos())
            } else {
                1.0
            };
            let s = (2.0 * std::f32::consts::PI * freq * t).sin() * env * amplitude;
            let sample = s.round().clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
    }
    pcm
}

fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Plays raw s16le 48 kHz mono PCM through `pw-play` (falling back to `pw-cat -p`) and
/// guarantees child process reaping on completion or timeout.
pub fn play_pcm_pipewire(pcm: &[u8], volume: f32) -> bool {
    let vol = format!("{:.2}", volume.clamp(0.0, 1.0));
    let candidates: [(&str, &[&str]); 2] = [
        (
            "pw-play",
            &[
                "--raw",
                "--rate",
                "48000",
                "--channels",
                "1",
                "--format",
                "s16",
                "--volume",
            ],
        ),
        (
            "pw-cat",
            &[
                "-p",
                "--raw",
                "--rate",
                "48000",
                "--channels",
                "1",
                "--format",
                "s16",
                "--volume",
            ],
        ),
    ];

    for (bin, args) in candidates {
        match play_pcm_with_bin(bin, args, &vol, pcm) {
            Some(ok) => return ok,
            None => continue,
        }
    }
    false
}

pub(crate) fn play_pcm_with_bin(bin: &str, args: &[&str], vol: &str, pcm: &[u8]) -> Option<bool> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .arg(vol)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(pcm);
            }
            let deadline = Instant::now() + Duration::from_millis(1500);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => return Some(status.success()),
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(15));
                    }
                    _ => {
                        kill_and_reap(&mut child);
                        return Some(false);
                    }
                }
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(false),
    }
}

/// Non-blocking bounded PipeWire chime dispatcher for the UI controller.
pub struct ChimeDispatcher {
    tx: Option<SyncSender<ChimeKind>>,
}

impl Default for ChimeDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl ChimeDispatcher {
    #[must_use]
    pub fn new() -> Self {
        let (tx, rx) = mpsc::sync_channel::<ChimeKind>(2);
        let spawn_res = std::thread::Builder::new()
            .name("omarex-chime".to_owned())
            .spawn(move || {
                while let Ok(kind) = rx.recv() {
                    let pcm = synthesize_chime_pcm(kind);
                    let _ = play_pcm_pipewire(&pcm, 0.35);
                }
            });
        match spawn_res {
            Ok(_) => Self { tx: Some(tx) },
            Err(_) => Self { tx: None },
        }
    }

    /// Creates a no-op dispatcher for headless / test environments.
    #[must_use]
    pub fn disabled() -> Self {
        Self { tx: None }
    }

    /// Enqueues a chime without blocking the UI thread if PipeWire session env vars are present.
    pub fn play(&self, kind: ChimeKind) {
        if std::env::var_os("WAYLAND_DISPLAY").is_none()
            && std::env::var_os("XDG_RUNTIME_DIR").is_none()
        {
            return;
        }
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(kind);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesize_chime_pcm_produces_exact_non_clipping_s16le_bytes() {
        let done = synthesize_chime_pcm(ChimeKind::Done);
        // 90ms + 110ms = 200ms at 48kHz mono s16le = 9600 samples * 2 bytes = 19200 bytes.
        assert_eq!(done.len(), 19_200);
        let attn = synthesize_chime_pcm(ChimeKind::Attention);
        // 110ms + 130ms = 240ms = 23040 bytes.
        assert_eq!(attn.len(), 23_040);

        for &chunk in done.as_chunks::<2>().0 {
            let sample = i16::from_le_bytes(chunk);
            assert!(
                sample.unsigned_abs() <= 10_000,
                "unexpected clipping sample {sample}"
            );
        }
    }
}
