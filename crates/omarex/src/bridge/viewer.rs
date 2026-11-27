use std::path::{Path, PathBuf};
use std::time::Instant;

use ui_chrome::{
    HoverPreviewState, ImageViewerState, ResolvedTarget, TargetKind, ToastSeverity, UiAction,
    ViewerContent, classify_uri, detect_link_at_char_col,
};
use vt_engine::RowCellSpan;

use crate::host_manager::HostCommand;
use crate::media_viewer::{MediaKind, RgbaFrame, texture_key_for_tab, viewer_remote_cache_dir};
use crate::workspace::BlockKey;

use super::*;

impl OmarexController {
    fn osc8_row_span(&self, key: &BlockKey, col: u16, row: u32, uri: &str) -> RowCellSpan {
        let mut start = col;
        while start > 0 {
            if let Ok(Some(prev)) = self.pane_store.hyperlink_at(key, start - 1, row)
                && prev == uri
            {
                start -= 1;
            } else {
                break;
            }
        }
        let mut end = col.saturating_add(1);
        let max_scan = col.saturating_add(256);
        while end < max_scan {
            if let Ok(Some(next)) = self.pane_store.hyperlink_at(key, end, row)
                && next == uri
            {
                end = end.saturating_add(1);
            } else {
                break;
            }
        }
        RowCellSpan {
            row,
            start_col: start,
            end_col: end,
        }
    }

    pub(super) fn resolve_terminal_target(
        &self,
        key: &BlockKey,
        col: u16,
        row: u32,
    ) -> Option<(HoveredTerminalTarget, Vec<RowCellSpan>)> {
        let is_local = self.is_block_local(key);
        let block_pwd = self
            .workspace
            .block(key)
            .and_then(|b| b.pwd.as_deref())
            .and_then(normalize_osc7_pwd)
            .or_else(|| {
                self.pane_store
                    .pwd(key)
                    .as_deref()
                    .and_then(normalize_osc7_pwd)
            });

        // 1. Explicit OSC 8 terminal hyperlink always takes priority.
        if let Ok(Some(uri)) = self.pane_store.hyperlink_at(key, col, row) {
            let trimmed = uri.trim();
            if !trimmed.is_empty() {
                let spans = vec![self.osc8_row_span(key, col, row, &uri)];
                let classified = classify_uri(trimmed);
                match &classified {
                    ResolvedTarget::ThreadLink { uri: thread_uri } => {
                        return Some((
                            HoveredTerminalTarget::ThreadLink(thread_uri.clone()),
                            spans,
                        ));
                    }
                    ResolvedTarget::ExternalUrl { url } => {
                        return Some((HoveredTerminalTarget::Url(url.clone()), spans));
                    }
                    _ => {
                        if let Some(raw_path) = classified.path() {
                            let line = classified.line();
                            let column = classified.col();
                            if is_local {
                                if let Some(resolved) =
                                    self.resolve_local_file_path(raw_path, block_pwd.as_deref())
                                    && resolved.exists()
                                {
                                    return Some((
                                        HoveredTerminalTarget::LocalFile {
                                            resolved_path: resolved,
                                            display_path: raw_path.to_owned(),
                                            target: classified,
                                            line,
                                            column,
                                            is_osc8: true,
                                        },
                                        spans,
                                    ));
                                }
                            } else {
                                return Some((
                                    HoveredTerminalTarget::RemoteFile {
                                        remote_path: raw_path.to_owned(),
                                        target: classified,
                                        line,
                                        column,
                                        is_osc8: true,
                                    },
                                    spans,
                                ));
                            }
                        }
                        if trimmed.contains("://") {
                            return Some((HoveredTerminalTarget::Url(trimmed.to_owned()), spans));
                        }
                    }
                }
            }
        }

        // 2. Plain terminal text across VT soft-wraps AND TUI gutter continuations (`│ ` / `┃ `),
        //    plus bare filename resolution against visible viewport paths & CWD.
        if let Ok(Some(unwrapped)) = self
            .pane_store
            .logical_line_with_tui_unwrap_at(key, col, row)
        {
            let viewport_lines = self.pane_store.viewport_text_lines(key).unwrap_or_default();
            let cwd_path = if is_local {
                block_pwd
                    .as_deref()
                    .or(self.default_working_dir.as_deref())
                    .map(PathBuf::from)
            } else {
                None
            };
            if let Some(link) = detect_link_at_char_col(
                &unwrapped.text,
                unwrapped.hover_char_col,
                &viewport_lines,
                cwd_path.as_deref(),
            ) {
                let spans = unwrapped.cell_spans_for_char_range(link.char_span);
                match &link.target {
                    ResolvedTarget::ThreadLink { uri } => {
                        return Some((HoveredTerminalTarget::ThreadLink(uri.clone()), spans));
                    }
                    ResolvedTarget::ExternalUrl { url } => {
                        return Some((HoveredTerminalTarget::Url(url.clone()), spans));
                    }
                    _ => {
                        if let Some(raw_path) = link.target.path() {
                            let line = link.target.line();
                            let column = link.target.col();
                            if is_local {
                                if let Some(resolved) =
                                    self.resolve_local_file_path(raw_path, block_pwd.as_deref())
                                    && resolved.exists()
                                {
                                    return Some((
                                        HoveredTerminalTarget::LocalFile {
                                            resolved_path: resolved,
                                            display_path: raw_path.to_owned(),
                                            target: link.target,
                                            line,
                                            column,
                                            is_osc8: false,
                                        },
                                        spans,
                                    ));
                                }
                            } else if looks_like_remote_file_path(raw_path) {
                                return Some((
                                    HoveredTerminalTarget::RemoteFile {
                                        remote_path: raw_path.to_owned(),
                                        target: link.target,
                                        line,
                                        column,
                                        is_osc8: false,
                                    },
                                    spans,
                                ));
                            }
                        }
                    }
                }
            }
        }

        // 3. Inline Kitty graphics placement hit-test (click inline image to open in full zoom viewer!).
        if let Some(placement) = self.pane_store.kitty_placement_at(key, col, row) {
            let source_path = if is_local {
                self.find_adjacent_kitty_caption_path(key, &placement, block_pwd.as_deref())
            } else {
                None
            };
            return Some((
                HoveredTerminalTarget::KittyImage {
                    image_id: placement.image_id,
                    width: placement.pixel_width,
                    height: placement.pixel_height,
                    source_path,
                },
                Vec::new(),
            ));
        }

        None
    }

    fn find_adjacent_kitty_caption_path(
        &self,
        key: &BlockKey,
        placement: &vt_engine::KittyPlacement,
        block_pwd: Option<&str>,
    ) -> Option<PathBuf> {
        let top_row = placement.viewport_row.max(0) as u32;
        let bottom_row = top_row.saturating_add(placement.grid_rows.max(1));
        let candidate_rows = [top_row.saturating_sub(1), bottom_row];
        let start_col = placement.viewport_col.max(0) as u16;
        let end_col = start_col.saturating_add(placement.grid_cols.min(120) as u16);

        for r in candidate_rows {
            for c in start_col..=end_col {
                if let Ok(Some(uri)) = self.pane_store.hyperlink_at(key, c, r) {
                    let classified = classify_uri(uri.trim());
                    if let Some(raw) = classified.path()
                        && let Some(resolved) = self.resolve_local_file_path(raw, block_pwd)
                        && resolved.exists()
                    {
                        return Some(resolved);
                    }
                }
            }
        }
        None
    }

    pub(super) fn resolve_local_file_path(
        &self,
        raw: &str,
        block_pwd: Option<&str>,
    ) -> Option<PathBuf> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        if trimmed.starts_with("file://") {
            return normalize_osc7_pwd(trimmed).map(PathBuf::from);
        }
        if trimmed == "~" {
            return dirs::home_dir();
        }
        if let Some(rest) = trimmed.strip_prefix("~/") {
            return dirs::home_dir().map(|home| home.join(rest));
        }
        let path = Path::new(trimmed);
        if path.is_absolute() {
            return Some(path.to_path_buf());
        }
        if let Some(pwd) = block_pwd {
            return Some(Path::new(pwd).join(path));
        }
        if let Some(cwd) = &self.default_working_dir {
            return Some(Path::new(cwd).join(path));
        }
        std::env::current_dir().ok().map(|cwd| cwd.join(path))
    }

    pub(super) fn update_hovered_file_path(
        &mut self,
        key: &BlockKey,
        col: u16,
        row: u32,
        px: f32,
        py: f32,
    ) {
        if let Some((target, spans)) = self.resolve_terminal_target(key, col, row) {
            match &target {
                HoveredTerminalTarget::Url(url) | HoveredTerminalTarget::ThreadLink(url) => {
                    self.pending_hover_image = None;
                    self.ui.hover_preview = Some(HoverPreviewState::FilePath {
                        path: url.clone(),
                        line: None,
                        column: None,
                        anchor_x: px,
                        anchor_y: py,
                    });
                }
                HoveredTerminalTarget::LocalFile {
                    resolved_path,
                    display_path,
                    line,
                    column,
                    ..
                } => {
                    let preview_dims = if is_previewable_image_path(resolved_path)
                        && std::fs::metadata(resolved_path)
                            .ok()
                            .is_some_and(|m| m.is_file() && m.len() <= MAX_HOVER_IMAGE_BYTES)
                    {
                        image::image_dimensions(resolved_path)
                            .ok()
                            .filter(|&(w, h)| w > 0 && h > 0)
                    } else {
                        None
                    };

                    if let Some((image_width, image_height)) = preview_dims {
                        let caption = resolved_path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or(display_path.as_str())
                            .to_owned();
                        self.ui.hover_preview = Some(HoverPreviewState::ImageCard {
                            caption,
                            image_width,
                            image_height,
                            anchor_x: px,
                            anchor_y: py,
                        });
                        if self
                            .loaded_hover_image
                            .as_ref()
                            .is_some_and(|(p, _)| p == resolved_path)
                        {
                            self.hover_image_id = Some(HOVER_PREVIEW_IMAGE_GPU_ID);
                            self.pending_hover_image = None;
                        } else {
                            self.hover_image_gen = self.hover_image_gen.saturating_add(1);
                            self.pending_hover_image =
                                Some((resolved_path.clone(), self.hover_image_gen));
                        }
                    } else {
                        self.pending_hover_image = None;
                        self.ui.hover_preview = Some(HoverPreviewState::FilePath {
                            path: display_path.clone(),
                            line: *line,
                            column: *column,
                            anchor_x: px,
                            anchor_y: py,
                        });
                    }
                }
                HoveredTerminalTarget::RemoteFile {
                    remote_path,
                    line,
                    column,
                    ..
                } => {
                    self.pending_hover_image = None;
                    self.ui.hover_preview = Some(HoverPreviewState::FilePath {
                        path: remote_path.clone(),
                        line: *line,
                        column: *column,
                        anchor_x: px,
                        anchor_y: py,
                    });
                }
                HoveredTerminalTarget::KittyImage {
                    width,
                    height,
                    source_path,
                    ..
                } => {
                    self.pending_hover_image = None;
                    let label = source_path
                        .as_ref()
                        .and_then(|p| p.file_name().and_then(|n| n.to_str()))
                        .map(|n| format!("{n} ({width}×{height}) — Click to open viewer"))
                        .unwrap_or_else(|| {
                            format!("Inline image ({width}×{height}) — Click to open viewer")
                        });
                    self.ui.hover_preview = Some(HoverPreviewState::FilePath {
                        path: label,
                        line: None,
                        column: None,
                        anchor_x: px,
                        anchor_y: py,
                    });
                }
            }
            self.hovered_link_spans = (!spans.is_empty()).then(|| (key.clone(), spans));
            self.hovered_terminal_target = Some((key.clone(), target));
            self.needs_redraw = true;
        } else {
            self.hovered_terminal_target = None;
            if self.hovered_link_spans.take().is_some() {
                self.needs_redraw = true;
            }
            self.pending_hover_image = None;
            if self.ui.hover_preview.take().is_some() {
                self.needs_redraw = true;
            }
        }
    }

    fn media_kind_for_target(kind: Option<TargetKind>) -> MediaKind {
        match kind {
            Some(TargetKind::Image) => MediaKind::Image,
            Some(TargetKind::Video) => MediaKind::Video,
            Some(TargetKind::Audio) => MediaKind::Audio,
            Some(TargetKind::Pdf) => MediaKind::Pdf,
            Some(TargetKind::TextCode | TargetKind::Markdown) => MediaKind::Text,
            _ => MediaKind::Auto,
        }
    }

    pub(super) fn open_local_path_in_viewer(
        &mut self,
        resolved_path: PathBuf,
        display_path: String,
        line: Option<u32>,
        column: Option<u32>,
        kind: Option<TargetKind>,
    ) -> u64 {
        let path_key = resolved_path.to_string_lossy().into_owned();
        let tab_label = if display_path.is_empty() {
            path_key.clone()
        } else {
            display_path
        };

        // If this exact local file is already loaded in a viewer tab, focus & jump immediately.
        if let Some(existing) = self
            .ui
            .viewer
            .tabs
            .iter()
            .find(|t| t.remote_host.is_none() && t.path == path_key)
            .map(|t| {
                (
                    t.id,
                    matches!(t.content, ViewerContent::Text(_)),
                    matches!(t.content, ViewerContent::Pdf(_)),
                )
            })
        {
            let (tab_id, is_text, is_pdf) = existing;
            self.ui.viewer.select_tab_by_id(tab_id);
            if let Some(l) = line {
                if is_text {
                    self.ui
                        .viewer
                        .jump_active_to_line(l as usize, column.map(|c| c as usize));
                } else if is_pdf {
                    if self.media_backend.pdf_goto_page(tab_id, l as usize)
                        && let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                        && let ViewerContent::Pdf(pdf) = &mut tab.content
                    {
                        pdf.loading_page = true;
                    }
                } else {
                    self.pending_tab_jump_lines.insert(tab_id, (l, column));
                }
            }
            self.ui.focus_viewer();
            self.force_immediate_resize = true;
            self.needs_redraw = true;
            return tab_id;
        }

        let tab_id =
            self.ui
                .viewer
                .open_loading_tab(path_key, None, format!("Loading {tab_label}…"));
        let media_kind = Self::media_kind_for_target(kind);
        let resolved_media = media_kind.resolve(&resolved_path);
        let initial_page = if resolved_media == MediaKind::Pdf {
            line.map(|p| p as usize)
        } else {
            if let Some(l) = line {
                self.pending_tab_jump_lines.insert(tab_id, (l, column));
            }
            None
        };
        if let Some(page) = initial_page {
            self.media_backend
                .request_load_local_at(tab_id, resolved_path, media_kind, Some(page));
        } else {
            self.media_backend
                .request_load_local(tab_id, resolved_path, media_kind);
        }
        self.ui.focus_viewer();
        self.force_immediate_resize = true;
        self.needs_redraw = true;
        tab_id
    }

    pub(super) fn open_remote_path_in_viewer(
        &mut self,
        key: &BlockKey,
        remote_path: &str,
        kind: TargetKind,
        line: Option<u32>,
        column: Option<u32>,
    ) -> u64 {
        let host_label = self
            .workspace
            .host(&key.host_id)
            .map(|h| h.label.clone())
            .unwrap_or_else(|| key.host_id.clone());
        let tab_id = self.ui.viewer.open_loading_tab(
            remote_path.to_owned(),
            Some(host_label),
            format!("Downloading {remote_path}…"),
        );
        if let Some(l) = line {
            self.pending_tab_jump_lines.insert(tab_id, (l, column));
        }

        let media_kind = Self::media_kind_for_target(Some(kind));
        let if_none_match = self
            .downloaded_file_cache
            .get(&(key.host_id.clone(), remote_path.to_owned()))
            .and_then(|(path, ver)| path.exists().then(|| ver.clone()).flatten());
        let transfer_id = format!("viewer-dl-{}", self.next_transfer_id);
        self.next_transfer_id = self.next_transfer_id.saturating_add(1);
        self.pending_viewer_downloads.insert(
            transfer_id.clone(),
            PendingRemoteViewerDownload {
                tab_id,
                remote_path: remote_path.to_owned(),
                kind: media_kind,
            },
        );
        self.send_command(HostCommand::DownloadFile {
            host_id: key.host_id.clone(),
            session_id: key.session_id.clone(),
            block_id: key.block_id.clone(),
            transfer_id,
            remote_path: remote_path.to_owned(),
            cache_dir: viewer_remote_cache_dir(),
            if_none_match,
            max_bytes: None,
        });
        self.ui.focus_viewer();
        self.force_immediate_resize = true;
        self.needs_redraw = true;
        tab_id
    }

    pub(super) fn resolve_remote_viewer_host_id(&self, host_ref: &str) -> String {
        if self.workspace.host(host_ref).is_some() {
            host_ref.to_owned()
        } else {
            self.workspace
                .hosts
                .values()
                .find(|h| h.label == host_ref)
                .map(|h| h.id.clone())
                .unwrap_or_else(|| host_ref.to_owned())
        }
    }

    pub(super) fn download_remote_file_for_external(&mut self, key: &BlockKey, remote_path: &str) {
        let if_none_match = self
            .downloaded_file_cache
            .get(&(key.host_id.clone(), remote_path.to_owned()))
            .and_then(|(path, ver)| path.exists().then(|| ver.clone()).flatten());
        let transfer_id = format!("dl-{}", self.next_transfer_id);
        self.next_transfer_id = self.next_transfer_id.saturating_add(1);
        self.push_toast(
            ToastSeverity::Info,
            "Downloading remote file",
            remote_path.to_owned(),
        );
        self.send_command(HostCommand::DownloadFile {
            host_id: key.host_id.clone(),
            session_id: key.session_id.clone(),
            block_id: key.block_id.clone(),
            transfer_id,
            remote_path: remote_path.to_owned(),
            cache_dir: remote_download_cache_dir(),
            if_none_match,
            max_bytes: None,
        });
    }

    fn try_focus_thread_uri(&mut self, uri: &str, now: Instant) -> bool {
        let stripped = uri
            .trim()
            .strip_prefix("t3-thread://v2/")
            .or_else(|| uri.trim().strip_prefix("t3-thread://"))
            .or_else(|| uri.trim().strip_prefix("t3code://threads/"))
            .or_else(|| uri.trim().strip_prefix("t3code://"))
            .unwrap_or("");
        let needle = stripped.trim_matches('/');
        if needle.is_empty() {
            return false;
        }
        let thread_tail = needle.rsplit('/').next().unwrap_or(needle);

        let mut matched: Option<(String, String, Option<String>, Option<String>)> = None;
        'hosts: for (host_id, host) in &self.workspace.hosts {
            for (session_id, sess) in &host.sessions {
                if let Some(view) = &sess.view {
                    for win in &view.windows {
                        let win_matches = win.label.contains(needle)
                            || win.label.contains(thread_tail)
                            || win.window_id.as_deref().is_some_and(|id| id == thread_tail);
                        if win_matches {
                            matched = Some((
                                host_id.clone(),
                                session_id.clone(),
                                win.window_id.clone(),
                                primary_block_id_of_window(win).map(str::to_owned),
                            ));
                            break 'hosts;
                        }
                        for layer in &win.layers {
                            for blk in &layer.blocks {
                                if blk.label.contains(needle)
                                    || blk.label.contains(thread_tail)
                                    || blk.block_id.as_deref().is_some_and(|id| id == thread_tail)
                                {
                                    matched = Some((
                                        host_id.clone(),
                                        session_id.clone(),
                                        win.window_id.clone(),
                                        blk.block_id.clone(),
                                    ));
                                    break 'hosts;
                                }
                            }
                        }
                    }
                }
            }
        }

        if let Some((host_id, session_id, window_id, block_id)) = matched {
            if let Some(wid) = window_id {
                self.dispatch_ui_action(
                    UiAction::SelectTab {
                        host_id,
                        session_id,
                        window_id: wid,
                    },
                    now,
                );
            } else {
                self.dispatch_ui_action(
                    UiAction::SelectSession {
                        host_id,
                        session_id,
                    },
                    now,
                );
            }
            if let Some(bid) = block_id {
                self.dispatch_ui_action(UiAction::FocusPane { block_id: bid }, now);
            }
            return true;
        }
        false
    }

    fn open_thread_link(&mut self, uri: &str, open_external: bool, now: Instant) {
        let trimmed = uri.trim();
        let is_internal_t3_thread = trimmed
            .get(..12)
            .is_some_and(|p| p.eq_ignore_ascii_case("t3-thread://"));
        // Keep `t3code://` title links opening T3 Code directly; only try focusing an already
        // matching Rex tab for internal `t3-thread://` references when not explicitly external.
        if !open_external && is_internal_t3_thread && self.try_focus_thread_uri(trimmed, now) {
            return;
        }

        let targets = resolve_thread_open_targets(trimmed);
        if let Some(desktop_uri) = targets.desktop_uri.as_deref() {
            let scheme = desktop_uri
                .split_once("://")
                .map(|(s, _)| s)
                .unwrap_or("t3code");
            if has_registered_scheme_handler(scheme) {
                if let Err(err) = open_uri_detached(desktop_uri) {
                    self.push_toast_at(
                        ToastSeverity::Warning,
                        "Open thread link failed",
                        format!("{desktop_uri} ({err})"),
                        now,
                    );
                }
                return;
            }
        }

        if let Some(web_url) = targets.web_fallback_url.as_deref() {
            if let Err(err) = open_uri_detached(web_url) {
                self.push_toast_at(
                    ToastSeverity::Warning,
                    "Open thread link failed",
                    format!("{web_url} ({err})"),
                    now,
                );
            }
            return;
        }

        self.push_toast_at(
            ToastSeverity::Warning,
            "Open thread link failed",
            format!("No T3 Code handler found for {trimmed}"),
            now,
        );
    }

    pub(super) fn open_terminal_target(
        &mut self,
        key: &BlockKey,
        target: &HoveredTerminalTarget,
        open_external: bool,
        now: Instant,
    ) {
        match target {
            HoveredTerminalTarget::Url(uri) => {
                if let Err(err) = open::that_detached(uri) {
                    self.push_toast_at(
                        ToastSeverity::Warning,
                        "Open link failed",
                        format!("{uri} ({err})"),
                        now,
                    );
                }
            }
            HoveredTerminalTarget::ThreadLink(uri) => {
                self.open_thread_link(uri, open_external, now);
            }
            HoveredTerminalTarget::LocalFile {
                resolved_path,
                display_path,
                target: resolved_target,
                line,
                column,
                ..
            } => {
                if open_external {
                    if let Err(err) = open::that_detached(resolved_path) {
                        self.push_toast_at(
                            ToastSeverity::Warning,
                            "Open file failed",
                            format!("{} ({err})", resolved_path.display()),
                            now,
                        );
                    }
                } else {
                    self.open_local_path_in_viewer(
                        resolved_path.clone(),
                        display_path.clone(),
                        *line,
                        *column,
                        Some(resolved_target.kind()),
                    );
                }
            }
            HoveredTerminalTarget::RemoteFile {
                remote_path,
                target: resolved_target,
                line,
                column,
                ..
            } => {
                if open_external {
                    self.download_remote_file_for_external(key, remote_path);
                } else {
                    self.open_remote_path_in_viewer(
                        key,
                        remote_path,
                        resolved_target.kind(),
                        *line,
                        *column,
                    );
                }
            }
            HoveredTerminalTarget::KittyImage {
                image_id,
                width,
                height,
                source_path,
            } => {
                if let Some(path) = source_path
                    && path.exists()
                {
                    if open_external {
                        if let Err(err) = open::that_detached(path) {
                            self.push_toast_at(
                                ToastSeverity::Warning,
                                "Open image failed",
                                format!("{} ({err})", path.display()),
                                now,
                            );
                        }
                    } else {
                        let display = path.to_string_lossy().into_owned();
                        self.open_local_path_in_viewer(
                            path.clone(),
                            display,
                            None,
                            None,
                            Some(TargetKind::Image),
                        );
                    }
                    return;
                }

                match self.pane_store.read_kitty_image(key, *image_id) {
                    Ok(Some(pixels)) if pixels.width > 0 && pixels.height > 0 => {
                        let title = format!("inline-image-{image_id}.png");
                        let tab_id = self.ui.viewer.open_loading_tab(
                            title,
                            None,
                            format!("Opening inline image ({width}×{height})…"),
                        );
                        let texture_key = texture_key_for_tab(tab_id);
                        self.viewer_upload_gen = self.viewer_upload_gen.saturating_add(1);
                        let byte_len = pixels.rgba8.len() as u64;
                        let img_state = ImageViewerState::new(
                            texture_key,
                            pixels.width,
                            pixels.height,
                            "Inline RGBA",
                            Some(byte_len),
                            true,
                        );
                        self.ui
                            .viewer
                            .set_tab_content(tab_id, ViewerContent::Image(img_state));
                        self.pending_viewer_uploads.push(PendingViewerUpload {
                            texture_key,
                            generation: self.viewer_upload_gen,
                            frame: RgbaFrame {
                                width: pixels.width,
                                height: pixels.height,
                                rgba: pixels.rgba8,
                            },
                        });
                        self.ui.focus_viewer();
                        self.force_immediate_resize = true;
                        self.needs_redraw = true;
                    }
                    Ok(_) => {
                        self.push_toast_at(
                            ToastSeverity::Warning,
                            "Inline image unavailable",
                            format!("Kitty image #{image_id} has no decoded pixels"),
                            now,
                        );
                    }
                    Err(err) => {
                        self.push_toast_at(
                            ToastSeverity::Warning,
                            "Inline image read failed",
                            err.to_string(),
                            now,
                        );
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ThreadOpenTargets {
    pub(super) desktop_uri: Option<String>,
    pub(super) web_fallback_url: Option<String>,
}

/// Encodes a single T3 thread/environment URI path segment (`A-Za-z0-9-_` verbatim, all other
/// bytes percent-encoded uppercase). Matches T3 Code's `hostedSegment` / `encodeURIComponent`
/// so literal `%` and `:` inside delegated thread IDs round-trip cleanly through T3 Code's router.
fn encode_t3_uri_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
            out.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Maps internal `t3-thread://v2/<env>/<thread>` links (which OS browsers/XDG do not handle)
/// into T3 Code Desktop's registered `t3code://threads/<env>/<thread>` deep link plus T3's
/// hosted web fallback `https://app.t3.codes/threads/<env>/<thread>`.
pub(super) fn resolve_thread_open_targets(uri: &str) -> ThreadOpenTargets {
    let trimmed = uri.trim();
    let without_fragment = trimmed.split('#').next().unwrap_or(trimmed);
    let (base, query_suffix) = match without_fragment.split_once('?') {
        Some((b, q)) if !q.is_empty() => (b, format!("?{q}")),
        Some((b, _)) => (b, String::new()),
        None => (without_fragment, String::new()),
    };

    // 1. Internal Markdown thread chip URI: `t3-thread://v2/<env_id>/<thread_id>`
    if let Some(rest) = base
        .get(..15)
        .filter(|p| p.eq_ignore_ascii_case("t3-thread://v2/"))
        .map(|_| &base[15..])
    {
        let clean = rest.trim_end_matches('/');
        if let Some((env_id, thread_id)) = clean.split_once('/')
            && !env_id.is_empty()
            && !thread_id.is_empty()
            && !thread_id.contains('/')
        {
            let enc_env = encode_t3_uri_segment(env_id);
            let enc_thread = encode_t3_uri_segment(thread_id);
            return ThreadOpenTargets {
                desktop_uri: Some(format!(
                    "t3code://threads/{enc_env}/{enc_thread}{query_suffix}"
                )),
                web_fallback_url: Some(format!(
                    "https://app.t3.codes/threads/{enc_env}/{enc_thread}{query_suffix}"
                )),
            };
        }
        return ThreadOpenTargets {
            desktop_uri: None,
            web_fallback_url: None,
        };
    }

    // Legacy `t3-thread://v1/...` or malformed `t3-thread://` links cannot be handled by browsers.
    if base
        .get(..12)
        .is_some_and(|p| p.eq_ignore_ascii_case("t3-thread://"))
    {
        return ThreadOpenTargets {
            desktop_uri: None,
            web_fallback_url: None,
        };
    }

    // 2. Native T3 Code desktop URI (`t3code://...` or `t3code-dev://...`).
    if let Some((scheme, rest)) = base.split_once("://")
        && (scheme.eq_ignore_ascii_case("t3code")
            || scheme
                .get(..7)
                .is_some_and(|p| p.eq_ignore_ascii_case("t3code-")))
    {
        let web_fallback_url = rest
            .get(..8)
            .filter(|p| p.eq_ignore_ascii_case("threads/"))
            .map(|_| rest[8..].trim_end_matches('/'))
            .and_then(|pair| {
                let (env_seg, thread_seg) = pair.split_once('/')?;
                if env_seg.is_empty() || thread_seg.is_empty() || thread_seg.contains('/') {
                    return None;
                }
                Some(format!(
                    "https://app.t3.codes/threads/{env_seg}/{thread_seg}{query_suffix}"
                ))
            });
        return ThreadOpenTargets {
            desktop_uri: Some(trimmed.to_owned()),
            web_fallback_url,
        };
    }

    ThreadOpenTargets {
        desktop_uri: None,
        web_fallback_url: None,
    }
}

#[cfg(target_os = "linux")]
fn has_registered_scheme_handler(scheme: &str) -> bool {
    let mime = format!("x-scheme-handler/{scheme}");
    let Ok(output) = std::process::Command::new("xdg-mime")
        .args(["query", "default", &mime])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let desktop = String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_ascii_lowercase();
    !desktop.is_empty()
        && !desktop.contains("chromium")
        && !desktop.contains("chrome")
        && !desktop.contains("firefox")
        && !desktop.contains("brave")
        && !desktop.contains("vivaldi")
        && !desktop.contains("epiphany")
        && !desktop.contains("librewolf")
}

#[cfg(not(target_os = "linux"))]
fn has_registered_scheme_handler(_scheme: &str) -> bool {
    true
}

fn open_uri_detached(uri: &str) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::process::{Command, Stdio};
        Command::new("xdg-open")
            .arg(uri)
            .env_remove("ELECTRON_RUN_AS_NODE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        open::that_detached(uri)
    }
}

fn looks_like_remote_file_path(path: &str) -> bool {
    if path.starts_with('/')
        || path.starts_with("~/")
        || path.starts_with("./")
        || path.starts_with("../")
    {
        return true;
    }
    // For bare relative paths on a remote host, require a realistic filename extension in the
    // final component so natural prose like `iPhone/Mac` never triggers a remote file hover.
    let last = path.rsplit('/').next().unwrap_or(path);
    if let Some((stem, ext)) = last.rsplit_once('.') {
        !stem.is_empty()
            && (1..=10).contains(&ext.len())
            && ext.bytes().all(|b| b.is_ascii_alphanumeric())
    } else {
        false
    }
}

fn is_previewable_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp"
            )
        })
}

fn remote_download_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("omarex")
        .join("downloads")
}
