//! Zero-polling `inotify` watcher for Omarchy's live theme directory.
//!
//! `omarchy-theme-set` atomically swaps symlinks and files under
//! `~/.local/state/omarchy/current/theme/`. This watcher monitors the target file,
//! its parent directories, and the nearest existing ancestor, re-arming watches
//! after every debounced change so symlink replacements never orphan watching.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::{ThemeError, ThemeEvent, ThemePalette};

/// Default debounce window coalescing bursts of file writes and symlink swaps.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(50);

/// Configuration for [`spawn_theme_watcher`].
#[derive(Debug, Clone)]
pub struct WatcherConfig {
    /// Directory corresponding to `~/.local/state/omarchy/current`.
    pub omarchy_current_dir: PathBuf,
    /// Debounce duration after filesystem notifications before reloading.
    pub debounce: Duration,
}

impl WatcherConfig {
    pub fn new(omarchy_current_dir: PathBuf) -> Self {
        Self {
            omarchy_current_dir,
            debounce: DEFAULT_DEBOUNCE,
        }
    }

    pub fn default_omarchy() -> Result<Self, ThemeError> {
        let dir = ThemePalette::default_omarchy_dir().ok_or_else(|| {
            ThemeError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "could not resolve user state directory for Omarchy theme",
            ))
        })?;
        Ok(Self::new(dir))
    }
}

/// Keeps the background `inotify` watcher alive; dropping it stops watching cleanly.
pub struct ThemeWatcherHandle {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for ThemeWatcherHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Spawns a zero-polling `inotify` watcher that emits [`ThemeEvent::PaletteChanged`]
/// whenever `colors.toml` changes (including across atomic symlink/directory swaps).
///
/// Returns the initially loaded [`ThemePalette`] (or dark fallback) along with a handle
/// that keeps the watcher running.
pub fn spawn_theme_watcher(
    config: WatcherConfig,
    events: mpsc::Sender<ThemeEvent>,
) -> Result<(ThemePalette, ThemeWatcherHandle), ThemeError> {
    let initial = ThemePalette::load_from_omarchy_dir(&config.omarchy_current_dir)
        .unwrap_or_else(|_| ThemePalette::default_dark());

    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<()>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| match res {
        Ok(_) => {
            let _ = raw_tx.send(());
        }
        Err(err) => {
            tracing::warn!(error = %err, "omarchy theme inotify watch error");
        }
    })
    .map_err(|e| ThemeError::Watch(e.to_string()))?;

    let mut watched_paths = HashSet::new();
    rearm_watches(
        &mut watcher,
        &config.omarchy_current_dir,
        &mut watched_paths,
    );

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
    let initial_for_task = initial.clone();
    let task = tokio::spawn(async move {
        let mut last_palette = initial_for_task;
        let debounce = config.debounce;

        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                maybe_ev = raw_rx.recv() => {
                    if maybe_ev.is_none() {
                        break;
                    }
                    // Coalesce rapid write/rename/symlink bursts within the debounce window.
                    let deadline = tokio::time::Instant::now() + debounce;
                    loop {
                        tokio::select! {
                            _ = &mut shutdown_rx => return,
                            _ = tokio::time::sleep_until(deadline) => break,
                            more = raw_rx.recv() => {
                                if more.is_none() {
                                    return;
                                }
                            }
                        }
                    }

                    // Re-arm watches in case `current` or `current/theme` symlink target changed.
                    rearm_watches(&mut watcher, &config.omarchy_current_dir, &mut watched_paths);

                    match ThemePalette::load_from_omarchy_dir(&config.omarchy_current_dir) {
                        Ok(next) if next != last_palette => {
                            last_palette = next.clone();
                            if events.send(ThemeEvent::PaletteChanged(next)).await.is_err() {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(err) => {
                            tracing::debug!(
                                error = %err,
                                dir = %config.omarchy_current_dir.display(),
                                "ignoring transient colors.toml reload error"
                            );
                        }
                    }
                }
            }
        }
    });

    Ok((
        initial,
        ThemeWatcherHandle {
            shutdown: Some(shutdown_tx),
            task: Some(task),
        },
    ))
}

fn candidate_watch_paths(current_dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(6);

    // Nearest existing ancestor ensures launching before Omarchy writes its state still works.
    let mut ancestor = current_dir.to_path_buf();
    while !ancestor.exists() {
        let Some(parent) = ancestor.parent() else {
            break;
        };
        if parent == ancestor {
            break;
        }
        ancestor = parent.to_path_buf();
    }
    if ancestor.exists() {
        paths.push(ancestor.clone());
    }
    if let Some(parent) = ancestor.parent()
        && parent.exists()
    {
        paths.push(parent.to_path_buf());
    }

    let theme_dir = current_dir.join("theme");
    let colors_file = theme_dir.join("colors.toml");
    for p in [current_dir.to_path_buf(), theme_dir.clone(), colors_file] {
        if p.exists() {
            paths.push(p);
        }
    }
    // Also watch the canonical resolved target of `theme` if it is a symlink.
    if let Ok(canon) = std::fs::canonicalize(&theme_dir)
        && canon.exists()
    {
        paths.push(canon);
    }

    paths
}

fn rearm_watches(
    watcher: &mut RecommendedWatcher,
    current_dir: &Path,
    tracked: &mut HashSet<PathBuf>,
) {
    for old in tracked.drain() {
        let _ = watcher.unwatch(&old);
    }
    for path in candidate_watch_paths(current_dir) {
        if tracked.contains(&path) {
            continue;
        }
        if watcher.watch(&path, RecursiveMode::NonRecursive).is_ok() {
            tracked.insert(path);
        }
    }
}
