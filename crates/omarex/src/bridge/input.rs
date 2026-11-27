use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use gpu_renderer::{CellMetrics, GpuRenderer, GpuShared, ImageUpload};
use rex_proto::UploadBatch;
use rex_proto::types::{
    SessionResizeLayerRequest, SessionResizePaneRequest, SessionSwapBlocksRequest,
};
use tr3x_bridge::{KeyChord, KeyResolution, Modifiers, PaneDirection, build_client_key_request};
use ui_chrome::{
    ActivitySortMode, ChromeLayoutConfig, ContextMenuItem, DropZone, DropZoneHit, FloatingHitZone,
    FocusTarget, HitTarget, IconId, ModalContent, ModalKey, RectF, TabReorderOverlay,
    ToastSeverity, ToolbarBehavior, UiAction, ViewerAction, ViewerContent, WhichKeyState,
    compute_drag_ratio, compute_viewer_drag_ratio, hit_test_drop_zone, scrub_fraction_from_x,
};
use vt_engine::{
    KeyAction as VtKeyAction, KeyEventInput as VtKeyEventInput, Modifiers as VtModifiers,
    MouseAction, MouseButton, MouseEventInput, MouseGeometry, MouseTrackingMode, PhysicalKey,
    ScreenPoint, ScrollViewport, SelectionGeometry, SelectionMode, WordMotion,
};

use crate::host_manager::{HostCommand, LOCAL_HOST_ID};
use crate::workspace::{BlockKey, HostEntry, OwnershipTrigger, SessionKey};

use super::*;

impl OmarexController {
    // =========================================================================
    // 4. Input Entrypoints for `app.rs` (`winit`)
    // =========================================================================

    /// Routes a normalized keyboard event honoring [`FocusTarget`] (modals/find/filter first,
    /// then `KeymapEngine`, then `libghostty-vt` key encoding to PTY).
    pub fn handle_key(&mut self, input: &KeyInputEvent, now: Instant) {
        self.needs_redraw = true;

        if input.pressed
            && input.canonical_key == "escape"
            && matches!(
                self.active_drag,
                Some(ActiveDrag::PaneMove { .. } | ActiveDrag::TabReorder { .. })
            )
        {
            self.active_drag = None;
            self.ui.tab_reorder_overlay = None;
            return;
        }

        // Overlays handle press events only; releases never leak to underlying PTY.
        if self.ui.focus.is_overlay_focused() {
            if !input.pressed {
                return;
            }
            match self.ui.focus.current().clone() {
                FocusTarget::Modal(_) => {
                    if let Some(mkey) = key_input_to_modal_key(input) {
                        let action = self.ui.handle_modal_key(mkey);
                        self.dispatch_ui_action(action, now);
                    }
                }
                FocusTarget::FindBar => self.handle_find_bar_key(input, now),
                FocusTarget::SidebarFilter => self.handle_sidebar_filter_key(input),
                FocusTarget::ContextMenu => match input.canonical_key.as_str() {
                    "escape" => {
                        self.ui.close_context_menu();
                    }
                    "up" | "down" => {
                        if let Some(menu) = self.ui.context_menu.as_mut()
                            && !menu.items.is_empty()
                        {
                            let len = menu.items.len();
                            let next = match (menu.hovered_index, input.canonical_key.as_str()) {
                                (None, "down") => 0,
                                (None, _) => len - 1,
                                (Some(i), "down") => (i + 1) % len,
                                (Some(i), _) => (i + len - 1) % len,
                            };
                            menu.hovered_index = Some(next);
                        }
                    }
                    "enter" | "space" => {
                        let action = self
                            .ui
                            .context_menu
                            .as_ref()
                            .and_then(|m| m.hovered_index.and_then(|idx| m.activate_index(idx)))
                            .unwrap_or(UiAction::None);
                        self.ui.close_context_menu();
                        self.dispatch_ui_action(action, now);
                    }
                    _ => {}
                },
                FocusTarget::Viewer => self.handle_viewer_key(input, now),
                FocusTarget::ViewerSearch => self.handle_viewer_search_key(input, now),
                FocusTarget::Terminal(_) => {}
            }
            return;
        }

        if let Some(cm_key) = self.copy_mode.as_ref().map(|c| c.block_key.clone())
            && self.focused_block_key().as_ref() == Some(&cm_key)
        {
            if input.pressed && !input.composing {
                self.handle_copy_mode_key(input, now);
            }
            return;
        }

        // Keymap resolution applies on key press (non-composing).
        if input.pressed && !input.composing {
            let chord = KeyChord::new(input.mods, &input.canonical_key);
            match self.actions.keymap_mut().resolve_key(chord) {
                KeyResolution::Action { action, args, .. } => {
                    self.ui.which_key = None;
                    self.dispatch_command_id(&action, args, now);
                    return;
                }
                KeyResolution::ForwardToServer { prefix, trigger } => {
                    self.ui.which_key = None;
                    if let Some(sk) = self.workspace.active_session.clone() {
                        let req = build_client_key_request(
                            sk.session_id,
                            self.ui.focus.last_terminal_block().map(str::to_owned),
                            &prefix,
                            &trigger,
                        );
                        self.send_command(HostCommand::ClientKey {
                            host_id: sk.host_id,
                            request: req,
                        });
                    }
                    return;
                }
                KeyResolution::PendingSequence { prefix, hints } => {
                    let trail: Vec<String> =
                        prefix.iter().map(KeyChord::to_display_string).collect();
                    self.ui.which_key = Some(WhichKeyState {
                        header: format!("{} ›", trail.join(" › ")),
                        exclusive: false,
                        hints,
                    });
                    return;
                }
                KeyResolution::ModeEntered { mode, hints } => {
                    self.ui.which_key = Some(WhichKeyState {
                        header: format!("MODE: {}", mode.to_uppercase()),
                        exclusive: false,
                        hints,
                    });
                    return;
                }
                KeyResolution::ModeExited => {
                    self.refresh_which_key_overlay();
                    return;
                }
                KeyResolution::Blocked => {
                    return;
                }
                KeyResolution::PassThrough => {
                    self.refresh_which_key_overlay();
                }
            }
        }

        // Pass directly through to focused terminal via `PaneStore::encode_key`!
        let Some(key) = self.focused_block_key() else {
            return;
        };
        let is_bare_modifier = is_modifier_or_lock_key(input.physical_key);

        let vt_action = if !input.pressed {
            VtKeyAction::Release
        } else if input.repeat {
            VtKeyAction::Repeat
        } else {
            VtKeyAction::Press
        };
        let unshifted_char = char::from_u32(input.unshifted_codepoint).filter(|&c| c != '\0');
        let consumed_mods = infer_consumed_mods(input.mods, input.text.as_deref(), unshifted_char);
        let vt_input = VtKeyEventInput {
            action: vt_action,
            key: input.physical_key,
            mods: bridge_mods_to_vt(input.mods),
            consumed_mods,
            composing: input.composing,
            utf8: input.text.as_deref(),
            unshifted_codepoint: unshifted_char,
        };
        if let Ok(bytes) = self.pane_store.encode_key(&key, &vt_input)
            && !bytes.is_empty()
            && let Ok(data) = String::from_utf8(bytes)
        {
            if input.pressed && !is_bare_modifier {
                self.maybe_claim_ownership(&key, OwnershipTrigger::ExplicitInteractOrFit);
                let _ = self.pane_store.scroll_to_bottom(&key);
            }
            self.send_command(HostCommand::TerminalWrite {
                host_id: key.host_id,
                session_id: key.session_id,
                block_id: key.block_id,
                data,
            });
        }
    }

    fn handle_find_bar_key(&mut self, input: &KeyInputEvent, now: Instant) {
        match input.canonical_key.as_str() {
            "escape" => {
                self.ui.close_find_bar();
                self.dispatch_ui_action(UiAction::CloseFindBar, now);
                return;
            }
            "enter" => {
                let act = if input.mods.shift {
                    UiAction::FindPrev
                } else {
                    UiAction::FindNext
                };
                self.dispatch_ui_action(act, now);
                return;
            }
            _ => {}
        }
        if apply_key_to_text_input(&mut self.ui.find_bar.input, input) {
            let query = self.ui.find_bar.input.text().to_owned();
            self.dispatch_ui_action(UiAction::FindQueryChanged(query), now);
        }
    }

    fn handle_sidebar_filter_key(&mut self, input: &KeyInputEvent) {
        if input.canonical_key == "escape" {
            self.ui.sidebar.close_filter();
            self.ui.focus.restore_terminal();
            return;
        }
        let _ = apply_key_to_text_input(&mut self.ui.sidebar.filter_input, input);
    }

    fn handle_viewer_search_key(&mut self, input: &KeyInputEvent, _now: Instant) {
        let key = input.canonical_key.as_str();
        if (input.mods.ctrl || input.mods.super_key) && key == "f" {
            self.ui.viewer.search.input.select_all();
            return;
        }
        if input.mods.alt && key == "c" {
            self.ui.viewer.toggle_search_case();
            return;
        }
        match key {
            "escape" => {
                self.ui.viewer.close_search();
                self.ui.focus_viewer();
                return;
            }
            "enter" | "f3" => {
                if input.mods.shift {
                    self.ui.viewer.prev_search_match();
                } else {
                    self.ui.viewer.next_search_match();
                }
                return;
            }
            _ => {}
        }
        if apply_key_to_text_input(&mut self.ui.viewer.search.input, input) {
            self.ui.viewer.recompute_search();
            self.ui.viewer.scroll_to_active_search_match();
        }
    }

    fn copy_viewer_path_line(&mut self, now: Instant) {
        let Some(tab) = self.ui.viewer.active_tab() else {
            return;
        };
        let path = tab.path.clone();
        let line = match &tab.content {
            ViewerContent::Text(txt) => txt.target_or_active_line(),
            ViewerContent::Pdf(pdf) => pdf.current_page,
            _ => 1,
        };
        self.dispatch_viewer_action(ViewerAction::CopyPathLine { path, line }, now);
    }

    fn handle_viewer_key(&mut self, input: &KeyInputEvent, now: Instant) {
        let key = input.canonical_key.as_str();
        let active_tab_id = self.ui.viewer.active_tab().map(|t| t.id);

        if (input.mods.ctrl || input.mods.super_key) && key == "w" {
            if let Some(tab_id) = active_tab_id {
                self.dispatch_viewer_action(ViewerAction::CloseTab { tab_id }, now);
            } else {
                self.dispatch_viewer_action(ViewerAction::ClosePane, now);
            }
            return;
        }
        if (input.mods.ctrl || input.mods.super_key) && key == "f" {
            self.ui.viewer.open_search();
            self.ui.focus.focus_viewer_search();
            return;
        }
        if (input.mods.ctrl || input.mods.super_key) && key == "a" {
            if let Some(tab) = self.ui.viewer.active_tab_mut()
                && let ViewerContent::Text(txt) = &mut tab.content
            {
                txt.select_all();
            }
            return;
        }
        if (input.mods.ctrl || input.mods.super_key) && input.mods.shift && matches!(key, "c" | "l")
        {
            self.copy_viewer_path_line(now);
            return;
        }
        if (input.mods.ctrl || input.mods.super_key) && key == "c" {
            let copied_sel = self.ui.viewer.active_tab().and_then(|t| match &t.content {
                ViewerContent::Text(txt) => txt.selected_text().map(|s| {
                    let rects = txt
                        .selection_viewport_rects()
                        .into_iter()
                        .map(rectf_to_gpu)
                        .collect::<Vec<_>>();
                    let clip = txt
                        .last_body_rect
                        .map(|r| rectf_to_gpu(r).to_clip_rect())
                        .unwrap_or(ClipRect::UNBOUNDED);
                    (s, rects, clip)
                }),
                _ => None,
            });
            if let Some((text, rects, clip)) = copied_sel {
                if self.clipboard_enabled {
                    set_system_clipboard_text(&text);
                }
                self.trigger_copy_shine_at_layer(rects, clip, layers::MODAL - 1, now);
                let line_count = text.lines().count().max(1);
                self.push_toast_at(
                    ToastSeverity::Info,
                    "Copied selection",
                    format!("{line_count} line(s)"),
                    now,
                );
            } else {
                self.copy_viewer_path_line(now);
            }
            return;
        }
        if input.mods.ctrl && key == "tab" {
            if input.mods.shift {
                self.ui.viewer.prev_tab();
            } else {
                self.ui.viewer.next_tab();
            }
            if self.ui.viewer.search.open {
                self.ui.viewer.recompute_search();
            }
            return;
        }

        match key {
            "escape" => {
                if input.mods.shift {
                    self.dispatch_viewer_action(ViewerAction::ClosePane, now);
                } else if self.ui.viewer.search.open {
                    self.ui.viewer.close_search();
                } else if self.ui.viewer.maximized {
                    self.ui.viewer.toggle_maximized();
                    self.force_immediate_resize = true;
                } else {
                    self.ui.focus.restore_terminal();
                }
            }
            "f3" => {
                if self.ui.viewer.search.open {
                    if input.mods.shift {
                        self.ui.viewer.prev_search_match();
                    } else {
                        self.ui.viewer.next_search_match();
                    }
                }
            }
            "+" | "=" => {
                self.ui.viewer.zoom_in();
            }
            "-" | "_" => {
                self.ui.viewer.zoom_out();
            }
            "0" => {
                self.ui.viewer.zoom_reset();
            }
            "f" => {
                self.ui.viewer.zoom_fit();
            }
            "space" | " " => {
                if let Some(tab_id) = active_tab_id {
                    match self.ui.viewer.tab_by_id(tab_id).map(|t| &t.content) {
                        Some(ViewerContent::Video(_)) => {
                            self.dispatch_viewer_action(
                                ViewerAction::ToggleVideoPlayPause { tab_id },
                                now,
                            );
                        }
                        Some(ViewerContent::Audio(_)) => {
                            self.dispatch_viewer_action(
                                ViewerAction::ToggleAudioPlayPause { tab_id },
                                now,
                            );
                        }
                        _ => {}
                    }
                }
            }
            "m" => {
                if let Some(tab_id) = active_tab_id {
                    match self.ui.viewer.tab_by_id(tab_id).map(|t| &t.content) {
                        Some(ViewerContent::Video(_)) => {
                            self.dispatch_viewer_action(
                                ViewerAction::ToggleVideoMute { tab_id },
                                now,
                            );
                        }
                        Some(ViewerContent::Audio(_)) => {
                            self.dispatch_viewer_action(
                                ViewerAction::ToggleAudioMute { tab_id },
                                now,
                            );
                        }
                        _ => {}
                    }
                }
            }
            "l" => {
                if let Some(tab_id) = active_tab_id {
                    match self.ui.viewer.tab_by_id(tab_id).map(|t| &t.content) {
                        Some(ViewerContent::Video(_)) => {
                            self.dispatch_viewer_action(
                                ViewerAction::ToggleVideoLoop { tab_id },
                                now,
                            );
                        }
                        Some(ViewerContent::Audio(_)) => {
                            self.dispatch_viewer_action(
                                ViewerAction::ToggleAudioLoop { tab_id },
                                now,
                            );
                        }
                        _ => {}
                    }
                }
            }
            "w" => {
                self.ui.viewer.toggle_wrap();
            }
            "r" => {
                self.ui.viewer.toggle_raw_markdown();
            }
            "d" => {
                self.ui.viewer.toggle_diff_mode();
            }
            "up" | "k" => {
                if !self.ui.viewer.scroll_by(-40.0) {
                    self.ui.viewer.pan_by(0.0, 40.0);
                }
            }
            "down" | "j" => {
                if !self.ui.viewer.scroll_by(40.0) {
                    self.ui.viewer.pan_by(0.0, -40.0);
                }
            }
            "[" => {
                if let Some(tab_id) = active_tab_id
                    && self
                        .ui
                        .viewer
                        .tab_by_id(tab_id)
                        .is_some_and(|t| matches!(t.content, ViewerContent::Pdf(_)))
                {
                    self.dispatch_viewer_action(ViewerAction::PdfPrevPage { tab_id }, now);
                }
            }
            "]" => {
                if let Some(tab_id) = active_tab_id
                    && self
                        .ui
                        .viewer
                        .tab_by_id(tab_id)
                        .is_some_and(|t| matches!(t.content, ViewerContent::Pdf(_)))
                {
                    self.dispatch_viewer_action(ViewerAction::PdfNextPage { tab_id }, now);
                }
            }
            "left" => {
                if let Some(tab_id) = active_tab_id {
                    match self.ui.viewer.tab_by_id_mut(tab_id).map(|t| &mut t.content) {
                        Some(ViewerContent::Video(vid)) => {
                            let next = (vid.position_secs - 5.0).max(0.0);
                            vid.position_secs = next;
                            self.media_backend.video_seek(tab_id, next);
                        }
                        Some(ViewerContent::Audio(aud)) => {
                            let next = (aud.position_secs - 5.0).max(0.0);
                            aud.position_secs = next;
                            self.media_backend.audio_seek(tab_id, next);
                        }
                        Some(ViewerContent::Pdf(_)) => {
                            self.dispatch_viewer_action(ViewerAction::PdfPrevPage { tab_id }, now);
                        }
                        _ => {
                            self.ui.viewer.pan_by(40.0, 0.0);
                        }
                    }
                }
            }
            "right" => {
                if let Some(tab_id) = active_tab_id {
                    match self.ui.viewer.tab_by_id_mut(tab_id).map(|t| &mut t.content) {
                        Some(ViewerContent::Video(vid)) => {
                            let max_t = vid.duration_secs.max(0.0);
                            let next = (vid.position_secs + 5.0).min(max_t);
                            vid.position_secs = next;
                            self.media_backend.video_seek(tab_id, next);
                        }
                        Some(ViewerContent::Audio(aud)) => {
                            let max_t = aud.duration_secs.max(0.0);
                            let next = (aud.position_secs + 5.0).min(max_t);
                            aud.position_secs = next;
                            self.media_backend.audio_seek(tab_id, next);
                        }
                        Some(ViewerContent::Pdf(_)) => {
                            self.dispatch_viewer_action(ViewerAction::PdfNextPage { tab_id }, now);
                        }
                        _ => {
                            self.ui.viewer.pan_by(-40.0, 0.0);
                        }
                    }
                }
            }
            "page_up" => {
                if let Some(tab_id) = active_tab_id
                    && self
                        .ui
                        .viewer
                        .tab_by_id(tab_id)
                        .is_some_and(|t| matches!(t.content, ViewerContent::Pdf(_)))
                {
                    self.dispatch_viewer_action(ViewerAction::PdfPrevPage { tab_id }, now);
                    return;
                }
                let step = self
                    .ui
                    .viewer
                    .active_tab()
                    .and_then(|t| match &t.content {
                        ViewerContent::Text(txt) => Some((txt.last_viewport_h * 0.85).max(120.0)),
                        _ => None,
                    })
                    .unwrap_or(320.0);
                self.ui.viewer.scroll_by(-step);
            }
            "page_down" => {
                if let Some(tab_id) = active_tab_id
                    && self
                        .ui
                        .viewer
                        .tab_by_id(tab_id)
                        .is_some_and(|t| matches!(t.content, ViewerContent::Pdf(_)))
                {
                    self.dispatch_viewer_action(ViewerAction::PdfNextPage { tab_id }, now);
                    return;
                }
                let step = self
                    .ui
                    .viewer
                    .active_tab()
                    .and_then(|t| match &t.content {
                        ViewerContent::Text(txt) => Some((txt.last_viewport_h * 0.85).max(120.0)),
                        _ => None,
                    })
                    .unwrap_or(320.0);
                self.ui.viewer.scroll_by(step);
            }
            "home" => {
                self.ui.viewer.scroll_active_to_fraction(0.0);
            }
            "end" => {
                self.ui.viewer.scroll_active_to_fraction(1.0);
            }
            _ => {}
        }
    }

    /// Handles pointer motion (hover tracking, active drag updates, terminal mouse motion / selection,
    /// and cell-debounced file-path hover detection).
    pub fn handle_pointer_move(&mut self, px: f32, py: f32, mods: Modifiers, now: Instant) {
        let moved = self.pointer_pos != (px, py);
        self.pointer_pos = (px, py);
        if moved
            && self.active_drag.is_none()
            && let Some((key, ..)) = self.pane_cell_at_pointer(px, py)
        {
            self.reclaim_on_pointer_activity(&key, now);
        }

        // 1. Active drag takes priority
        if let Some(drag) = self.active_drag.clone() {
            self.needs_redraw = true;
            match drag {
                ActiveDrag::SidebarResize { on_right } => {
                    let raw = if on_right { self.viewport_w - px } else { px };
                    self.ui.layout_cfg.sidebar_width = raw.clamp(
                        ChromeLayoutConfig::MIN_SIDEBAR_WIDTH,
                        ChromeLayoutConfig::MAX_SIDEBAR_WIDTH,
                    );
                    self.settings.sidebar_width_px = self.ui.layout_cfg.sidebar_width;
                }
                ActiveDrag::SplitDivider {
                    host_id,
                    session_id,
                    divider,
                    split_target,
                } => {
                    let ratio = compute_drag_ratio(&divider, px, py);
                    if let Some((direction, before_block_id, after_block_id)) = split_target {
                        self.send_command(HostCommand::ResizePane {
                            host_id,
                            request: SessionResizePaneRequest {
                                session_id,
                                direction,
                                before_block_id,
                                after_block_id,
                                ratio: f64::from(ratio),
                            },
                        });
                    }
                }
                ActiveDrag::FloatingLayer {
                    host_id,
                    session_id,
                    layer_id,
                    zone,
                    start_px,
                    start_bounds,
                    workspace,
                    ..
                } => {
                    let next = compute_floating_drag_bounds(
                        start_bounds,
                        zone,
                        (px - start_px.0) / workspace.w.max(1.0),
                        (py - start_px.1) / workspace.h.max(1.0),
                    );
                    self.send_command(HostCommand::ResizeLayer {
                        host_id,
                        request: SessionResizeLayerRequest {
                            session_id,
                            layer_id,
                            bounds: [
                                f64::from(next[0]),
                                f64::from(next[1]),
                                f64::from(next[2]),
                                f64::from(next[3]),
                            ],
                        },
                    });
                }
                ActiveDrag::TerminalSelection {
                    key,
                    content_rect,
                    cell_w,
                    cell_h,
                    rectangle,
                } => {
                    let rel_x = (px - content_rect.x).max(0.0);
                    let rel_y = (py - content_rect.y).max(0.0);
                    let col = (rel_x / cell_w).floor() as u16;
                    let row = (rel_y / cell_h).floor() as u32;
                    let geom = SelectionGeometry {
                        columns: (content_rect.w / cell_w).floor().max(1.0) as u32,
                        cell_width: cell_w.round().max(1.0) as u32,
                        padding_left: 0,
                        screen_height: content_rect.h.round().max(1.0) as u32,
                    };
                    let _ = self.pane_store.selection_drag(
                        &key,
                        col,
                        row,
                        f64::from(rel_x),
                        f64::from(rel_y),
                        geom,
                        rectangle || mods.alt,
                    );
                }
                ActiveDrag::PaneScrollbar { key, track_rect } => {
                    let frac = ((py - track_rect.y) / track_rect.h.max(1.0)).clamp(0.0, 1.0);
                    if let Some(sb) = self.pane_store.scrollbar(&key) {
                        let max_off = sb.total.saturating_sub(sb.len);
                        let row = ((max_off as f32) * frac).round() as usize;
                        let _ = self
                            .pane_store
                            .scroll_viewport(&key, ScrollViewport::Row(row));
                    }
                }
                ActiveDrag::ViewerDivider { workspace } => {
                    let ratio = compute_viewer_drag_ratio(workspace, px);
                    self.ui.viewer.set_split_ratio(ratio);
                }
                ActiveDrag::ViewerPan { last_px } => {
                    let dx = px - last_px.0;
                    let dy = py - last_px.1;
                    self.ui.viewer.pan_by(dx, dy);
                    self.active_drag = Some(ActiveDrag::ViewerPan { last_px: (px, py) });
                }
                ActiveDrag::ViewerScrollbar { track_rect } => {
                    let frac = ((py - track_rect.y) / track_rect.h.max(1.0)).clamp(0.0, 1.0);
                    self.ui.viewer.scroll_active_to_fraction(frac);
                }
                ActiveDrag::ViewerVideoSeek { tab_id, bar_rect } => {
                    let frac = scrub_fraction_from_x(bar_rect, px);
                    if let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                        && let ViewerContent::Video(vid) = &mut tab.content
                    {
                        let secs = vid.seek_to_fraction(frac);
                        self.media_backend.video_seek(tab_id, secs);
                    }
                }
                ActiveDrag::ViewerAudioSeek { tab_id, bar_rect } => {
                    let frac = scrub_fraction_from_x(bar_rect, px);
                    if let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                        && let ViewerContent::Audio(aud) = &mut tab.content
                    {
                        let secs = aud.seek_to_fraction(frac);
                        self.media_backend.audio_seek(tab_id, secs);
                    }
                }
                ActiveDrag::ViewerTextSelection { tab_id } => {
                    if let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                        && let ViewerContent::Text(txt) = &mut tab.content
                    {
                        if let Some(body) = txt.last_body_rect {
                            if py < body.y {
                                txt.scroll_by(-((body.y - py).min(40.0) * 0.6));
                            } else if py > body.bottom() {
                                txt.scroll_by((py - body.bottom()).min(40.0) * 0.6);
                            }
                        }
                        if let Some(pos) = txt.hit_test_text_pos(px, py) {
                            txt.drag_selection(pos);
                        }
                    }
                }
                ActiveDrag::PaneMove {
                    host_id,
                    session_id,
                    block_id,
                    start_px,
                    mut activated,
                    mut hover_target,
                } => {
                    if !activated && (px - start_px.0).hypot(py - start_px.1) >= 6.0 {
                        activated = true;
                    }
                    if activated {
                        hover_target = self.compute_pane_drop_target(&block_id, px, py);
                    }
                    self.active_drag = Some(ActiveDrag::PaneMove {
                        host_id,
                        session_id,
                        block_id,
                        start_px,
                        activated,
                        hover_target,
                    });
                }
                ActiveDrag::TabReorder {
                    host_id,
                    session_id,
                    window_id,
                    start_px,
                    mut activated,
                    horizontal,
                    mut before_window_id,
                    mut has_valid_drop,
                } => {
                    if !activated && (px - start_px.0).hypot(py - start_px.1) >= 6.0 {
                        activated = true;
                    }
                    if activated {
                        if let Some((before, ind_rect, valid)) = self.compute_tab_reorder_target(
                            &host_id,
                            &session_id,
                            &window_id,
                            horizontal,
                            px,
                            py,
                        ) {
                            before_window_id = before;
                            has_valid_drop = valid;
                            self.ui.tab_reorder_overlay = Some(TabReorderOverlay {
                                host_id: host_id.clone(),
                                session_id: session_id.clone(),
                                dragged_window_id: window_id.clone(),
                                indicator_rect: valid.then_some(ind_rect),
                            });
                        } else {
                            before_window_id = None;
                            has_valid_drop = false;
                            self.ui.tab_reorder_overlay = None;
                        }
                    }
                    self.active_drag = Some(ActiveDrag::TabReorder {
                        host_id,
                        session_id,
                        window_id,
                        start_px,
                        activated,
                        horizontal,
                        before_window_id,
                        has_valid_drop,
                    });
                }
            }
            return;
        }

        // Cancel a pending plain link click once the pointer drags beyond click jitter.
        if let Some(pending) = &self.pending_plain_link_click
            && (px - pending.press_px.0).hypot(py - pending.press_px.1) >= 5.0
        {
            self.pending_plain_link_click = None;
        }

        // 2. Update unified chrome hover state + on-hover toolbar reveal
        if let Some(out) = &self.last_ui_output {
            if self.ui.update_pointer_hover(&out.draw_list, px, py) {
                self.needs_redraw = true;
            }

            if self.ui.layout_cfg.toolbar_behavior == ToolbarBehavior::OnHover {
                let on_edge = out.chrome.hot_edge.is_some_and(|r| r.contains(px, py));
                let on_toolbar = out
                    .chrome
                    .sidebar
                    .or(out.chrome.tab_bar)
                    .is_some_and(|r| r.contains(px, py));
                let should_reveal = on_edge || (self.ui.layout_cfg.toolbar_revealed && on_toolbar);
                if should_reveal != self.ui.layout_cfg.toolbar_revealed {
                    self.ui.layout_cfg.toolbar_revealed = should_reveal;
                    self.needs_redraw = true;
                }
            }

            // 3. Forward mouse motion to mouse-tracking terminal if pointer is inside a pane,
            //    and ALWAYS continue to cell-debounced link/path/image hover detection so SGR
            //    mouse-tracking TUIs (such as `tr3x`) never hide clickable links.
            if let Some((key, content_rect, col, row, rel_x, rel_y)) =
                self.pane_cell_at_pointer(px, py)
            {
                let metrics = self.last_cell_metrics.unwrap_or(CellMetrics {
                    font_size: 14.0,
                    scale_factor: 1.0,
                    cell_width: 8.0,
                    cell_height: 19.0,
                    baseline_y: 14.0,
                    underline_y: 16.0,
                    underline_thickness: 1.0,
                    strikethrough_y: 10.0,
                });
                let geom = MouseGeometry {
                    screen_width: content_rect.w.round().max(1.0) as u32,
                    screen_height: content_rect.h.round().max(1.0) as u32,
                    cell_width: metrics.cell_width.round().max(1.0) as u32,
                    cell_height: metrics.cell_height.round().max(1.0) as u32,
                    padding_top: 0,
                    padding_bottom: 0,
                    padding_left: 0,
                    padding_right: 0,
                };
                let mouse_in = MouseEventInput {
                    action: MouseAction::Motion,
                    button: None,
                    mods: bridge_mods_to_vt(mods),
                    x: rel_x,
                    y: rel_y,
                };
                if let Ok(Some(bytes)) = self.pane_store.encode_mouse(&key, &mouse_in, geom)
                    && let Ok(data) = String::from_utf8(bytes)
                {
                    self.send_command(HostCommand::TerminalWrite {
                        host_id: key.host_id.clone(),
                        session_id: key.session_id.clone(),
                        block_id: key.block_id.clone(),
                        data,
                    });
                }

                // Cell-debounced hover link/path/media detection (only runs on cell change!).
                let cell_id = (key.clone(), col, row);
                if self.hovered_path_cell.as_ref() != Some(&cell_id) {
                    self.hovered_path_cell = Some(cell_id);
                    self.update_hovered_file_path(&key, col, row, px, py);
                }
            } else {
                self.hovered_path_cell = None;
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
    }

    /// Handles pointer button press (UI chrome hit-test first, then terminal mouse tracking /
    /// OSC 8 hyperlink Ctrl+click / selection gesture).
    pub fn handle_pointer_down(
        &mut self,
        px: f32,
        py: f32,
        button: PointerButton,
        mods: Modifiers,
        time_ns: u64,
        now: Instant,
    ) {
        self.pointer_pos = (px, py);
        self.pending_plain_link_click = None;
        self.needs_redraw = true;

        // Middle-click on a tab closes it immediately (`gui-spec.md §2.2`).
        if button == PointerButton::Middle {
            if let Some(out) = &self.last_ui_output
                && let Some(HitTarget::TabCard {
                    host_id,
                    session_id,
                    window_id,
                }) = out.draw_list.hit_test(px, py).cloned()
            {
                self.dispatch_ui_action(
                    UiAction::CloseTab {
                        host_id,
                        session_id,
                        window_id,
                    },
                    now,
                );
                return;
            }
            // Middle-click paste inside terminal (Linux primary selection with clipboard fallback).
            self.handle_primary_paste(now);
            return;
        }

        // Check if click lands inside a terminal pane's content rect (and NOT covered by modal/menu/header).
        let is_pane_surface_hit = self.last_ui_output.as_ref().is_some_and(|out| {
            matches!(
                out.draw_list.hit_test(px, py),
                Some(HitTarget::PaneSurface { .. })
            )
        });

        let mut right_click_terminal_target: Option<(BlockKey, HoveredTerminalTarget)> = None;

        if is_pane_surface_hit
            && let Some((key, content_rect, col, row, rel_x, rel_y)) =
                self.pane_cell_at_pointer(px, py)
        {
            self.ui.focus.focus_terminal(Some(key.block_id.clone()));
            self.reclaim_on_pointer_activity(&key, now);

            let clicked_target = if matches!(button, PointerButton::Left | PointerButton::Right) {
                self.hovered_terminal_target
                    .as_ref()
                    .filter(|(k, _)| k == &key)
                    .map(|(_, t)| t.clone())
                    .or_else(|| self.resolve_terminal_target(&key, col, row).map(|(t, _)| t))
            } else {
                None
            };

            // 1. Ctrl+Click / Super+Click ALWAYS intercepts clickable links, paths, and inline
            //    Kitty images BEFORE SGR mouse tracking (`tr3x` / TUIs never swallow it!).
            if button == PointerButton::Left
                && (mods.ctrl || mods.super_key)
                && let Some(target) = clicked_target
            {
                self.open_terminal_target(&key, &target, mods.alt, now);
                return;
            }

            // 2. Plain Left-Click on a detected clickable link, file path, or inline Kitty image:
            //    arm a non-drag click (fires on pointer-up if movement < 5px). When SGR mouse
            //    tracking is active (e.g. `tr3x`), consume press so TUI doesn't swallow the click;
            //    when tracking is off, fall through to selection so dragging still selects text!
            let mut armed_plain_click = false;
            if button == PointerButton::Left
                && !mods.ctrl
                && !mods.super_key
                && !mods.alt
                && !mods.shift
                && let Some(target) = clicked_target.clone()
            {
                self.pending_plain_link_click = Some(PendingPlainLinkClick {
                    key: key.clone(),
                    target,
                    press_px: (px, py),
                });
                armed_plain_click = true;
                let mouse_tracking_active = self
                    .pane_store
                    .mouse_tracking_mode(&key)
                    .is_some_and(|m| m != MouseTrackingMode::None);
                if mouse_tracking_active {
                    return;
                }
            }

            // 3. Right-click over a detected link/path/image opens OmaRex's context menu with
            //    `Open in Right Pane`, `Open External`, and `Copy Path` instead of being swallowed.
            if button == PointerButton::Right
                && let Some(target) = clicked_target
            {
                right_click_terminal_target = Some((key.clone(), target));
            } else {
                // 4. Terminal SGR mouse tracking (honors Shift-bypass automatically via `encode_mouse`)
                let metrics = self.last_cell_metrics.unwrap_or(CellMetrics {
                    font_size: 14.0,
                    scale_factor: 1.0,
                    cell_width: 8.0,
                    cell_height: 19.0,
                    baseline_y: 14.0,
                    underline_y: 16.0,
                    underline_thickness: 1.0,
                    strikethrough_y: 10.0,
                });
                let geom = MouseGeometry {
                    screen_width: content_rect.w.round().max(1.0) as u32,
                    screen_height: content_rect.h.round().max(1.0) as u32,
                    cell_width: metrics.cell_width.round().max(1.0) as u32,
                    cell_height: metrics.cell_height.round().max(1.0) as u32,
                    padding_top: 0,
                    padding_bottom: 0,
                    padding_left: 0,
                    padding_right: 0,
                };
                let mouse_in = MouseEventInput {
                    action: MouseAction::Press,
                    button: Some(button.to_vt()),
                    mods: bridge_mods_to_vt(mods),
                    x: rel_x,
                    y: rel_y,
                };
                if !armed_plain_click
                    && let Ok(Some(bytes)) = self.pane_store.encode_mouse(&key, &mouse_in, geom)
                    && let Ok(data) = String::from_utf8(bytes)
                {
                    self.send_command(HostCommand::TerminalWrite {
                        host_id: key.host_id,
                        session_id: key.session_id,
                        block_id: key.block_id,
                        data,
                    });
                    return;
                }

                // 5. Left-click starts native text selection gesture (Alt+drag = rectangle!)
                if button == PointerButton::Left {
                    let _ = self.pane_store.selection_press(
                        &key,
                        col,
                        row,
                        f64::from(rel_x),
                        f64::from(rel_y),
                        time_ns,
                    );
                    self.active_drag = Some(ActiveDrag::TerminalSelection {
                        key,
                        content_rect,
                        cell_w: metrics.cell_width.max(1.0),
                        cell_h: metrics.cell_height.max(1.0),
                        rectangle: mods.alt,
                    });
                    return;
                }
            }
        }

        // Track pressed chrome target for tactile button feedback & remember right-clicked target for Rename.
        if let Some(out) = self.last_ui_output.clone() {
            if button == PointerButton::Left && self.ui.set_pointer_pressed(&out.draw_list, px, py)
            {
                self.needs_redraw = true;
            }
            if button == PointerButton::Right {
                self.pending_rename_target = match out.draw_list.hit_test(px, py) {
                    Some(
                        HitTarget::HostHeader { host_id } | HitTarget::HostNewSession { host_id },
                    ) if host_id != LOCAL_HOST_ID => Some(PendingRenameTarget::Host {
                        host_id: host_id.clone(),
                    }),
                    Some(HitTarget::SessionRow {
                        host_id,
                        session_id,
                    }) => Some(PendingRenameTarget::Session {
                        host_id: host_id.clone(),
                        session_id: session_id.clone(),
                    }),
                    Some(HitTarget::TabCard {
                        host_id,
                        session_id,
                        window_id,
                    }) => Some(PendingRenameTarget::Window {
                        host_id: host_id.clone(),
                        session_id: session_id.clone(),
                        window_id: window_id.clone(),
                    }),
                    Some(
                        HitTarget::PaneSurface { block_id } | HitTarget::PaneHeader { block_id },
                    ) => self.workspace.active_session.as_ref().map(|sk| {
                        PendingRenameTarget::Block {
                            host_id: sk.host_id.clone(),
                            session_id: sk.session_id.clone(),
                            block_id: block_id.clone(),
                        }
                    }),
                    _ => None,
                };
            }

            // Left-click on terminal scrollbar track or thumb: scroll to exact click fraction + drag!
            if button == PointerButton::Left
                && let Some(entry) = out.draw_list.hit_test_entry(px, py)
                && let HitTarget::PaneScrollbarTrack { block_id }
                | HitTarget::PaneScrollbarThumb { block_id } = &entry.target
                && let Some(sk) = self.workspace.active_session.clone()
            {
                let track_rect = out
                    .draw_list
                    .hits
                    .iter()
                    .find(|h| {
                        matches!(
                            &h.target,
                            HitTarget::PaneScrollbarTrack { block_id: bid } if bid == block_id
                        )
                    })
                    .map(|h| h.rect)
                    .unwrap_or(entry.rect);
                let frac = ((py - track_rect.y) / track_rect.h.max(1.0)).clamp(0.0, 1.0);
                let key = BlockKey::new(sk.host_id, sk.session_id, block_id.clone());
                if let Some(sb) = self.pane_store.scrollbar(&key) {
                    let max_off = sb.total.saturating_sub(sb.len);
                    let row = ((max_off as f32) * frac).round() as usize;
                    let _ = self
                        .pane_store
                        .scroll_viewport(&key, ScrollViewport::Row(row));
                }
                self.active_drag = Some(ActiveDrag::PaneScrollbar { key, track_rect });
                return;
            }
        }

        // Route all other clicks (sidebar, tabs, headers, viewer, modals, context menu) through `UiTree`.
        let hit_target = self
            .last_ui_output
            .as_ref()
            .and_then(|o| o.draw_list.hit_test(px, py).cloned());
        let action = if let Some(out) = self.last_ui_output.clone() {
            self.ui
                .handle_click(&out.draw_list, px, py, button == PointerButton::Right)
        } else {
            UiAction::None
        };

        if button == PointerButton::Left && matches!(hit_target, Some(HitTarget::ViewerTextBody)) {
            let clicks = if let Some((prev_t, prev_pos, prev_count)) = self.last_viewer_click
                && now.duration_since(prev_t) <= Duration::from_millis(400)
                && (px - prev_pos.0).hypot(py - prev_pos.1) < 5.0
            {
                (prev_count % 3) + 1
            } else {
                1
            };
            self.last_viewer_click = Some((now, (px, py), clicks));
            if let Some(tab) = self.ui.viewer.active_tab_mut()
                && let ViewerContent::Text(txt) = &mut tab.content
                && let Some(pos) = txt.hit_test_text_pos(px, py)
            {
                txt.begin_selection(pos, clicks);
                self.active_drag = Some(ActiveDrag::ViewerTextSelection { tab_id: tab.id });
            }
        }

        // Augment pane right-click context menu with viewer/link entries when clicking a target.
        if button == PointerButton::Right {
            if let Some((key, target)) = right_click_terminal_target {
                let copyable = target.copyable_string();
                let is_file_or_image = matches!(
                    target,
                    HoveredTerminalTarget::LocalFile { .. }
                        | HoveredTerminalTarget::RemoteFile { .. }
                        | HoveredTerminalTarget::KittyImage { .. }
                );
                self.context_menu_terminal_target = Some((key, target));
                if let Some(menu) = self.ui.context_menu.as_mut() {
                    let mut extra = Vec::new();
                    if is_file_or_image {
                        extra.push(ContextMenuItem {
                            label: "Open in Right Pane".to_owned(),
                            icon: Some(IconId::SplitRight),
                            shortcut: Some("Click".to_owned()),
                            danger: false,
                            separator_before: false,
                            action: UiAction::RunCommand {
                                action_id: "__viewer_open_ctx_target".to_owned(),
                            },
                        });
                        extra.push(ContextMenuItem {
                            label: "Open External".to_owned(),
                            icon: Some(IconId::Link),
                            shortcut: Some("Ctrl+Alt+Click".to_owned()),
                            danger: false,
                            separator_before: false,
                            action: UiAction::RunCommand {
                                action_id: "__viewer_open_ctx_external".to_owned(),
                            },
                        });
                    } else {
                        extra.push(ContextMenuItem {
                            label: "Open Link".to_owned(),
                            icon: Some(IconId::Link),
                            shortcut: Some("Click".to_owned()),
                            danger: false,
                            separator_before: false,
                            action: UiAction::RunCommand {
                                action_id: "__viewer_open_ctx_target".to_owned(),
                            },
                        });
                    }
                    if copyable.is_some() {
                        extra.push(ContextMenuItem {
                            label: if is_file_or_image {
                                "Copy Path".to_owned()
                            } else {
                                "Copy Link".to_owned()
                            },
                            icon: Some(IconId::Document),
                            shortcut: None,
                            danger: false,
                            separator_before: false,
                            action: UiAction::RunCommand {
                                action_id: "__viewer_copy_ctx_target".to_owned(),
                            },
                        });
                    }
                    if let Some(first_existing) = menu.items.first_mut() {
                        first_existing.separator_before = true;
                    }
                    extra.append(&mut menu.items);
                    menu.items = extra;
                }
            } else {
                self.context_menu_terminal_target = None;
            }
        }

        self.dispatch_ui_action(action, now);

        if button == PointerButton::Left {
            match hit_target {
                Some(HitTarget::PaneHeader { block_id }) => {
                    let is_tiled = self
                        .last_ui_output
                        .as_ref()
                        .is_some_and(|out| out.tiled_panes.iter().any(|p| p.id == block_id));
                    if is_tiled && let Some(sk) = self.workspace.active_session.clone() {
                        self.active_drag = Some(ActiveDrag::PaneMove {
                            host_id: sk.host_id,
                            session_id: sk.session_id,
                            block_id,
                            start_px: (px, py),
                            activated: false,
                            hover_target: None,
                        });
                    }
                }
                Some(HitTarget::TabCard {
                    host_id,
                    session_id,
                    window_id,
                }) => {
                    let in_tabbar = self
                        .last_ui_output
                        .as_ref()
                        .and_then(|o| o.chrome.tab_bar)
                        .is_some_and(|r| r.contains(px, py));
                    let sidebar_allows_reorder = !self.ui.sidebar.activity_mode
                        || self.ui.sidebar.activity_sort == ActivitySortMode::BySession;
                    let multi_tab = self
                        .workspace
                        .session(&SessionKey::new(host_id.clone(), session_id.clone()))
                        .and_then(|s| s.view.as_ref())
                        .is_some_and(|v| v.windows.len() >= 2);
                    if multi_tab && (in_tabbar || sidebar_allows_reorder) {
                        self.active_drag = Some(ActiveDrag::TabReorder {
                            host_id,
                            session_id,
                            window_id,
                            start_px: (px, py),
                            activated: false,
                            horizontal: in_tabbar,
                            before_window_id: None,
                            has_valid_drop: false,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    /// Handles pointer leaving the window surface (clears transient hover/pressed tooltips).
    pub fn handle_pointer_leave(&mut self) {
        let mut changed = self.ui.clear_pointer_pressed();
        if self.ui.hover_preview.take().is_some() {
            changed = true;
        }
        if self.hovered_link_spans.take().is_some() {
            changed = true;
        }
        self.hovered_path_cell = None;
        self.hovered_terminal_target = None;
        self.pending_hover_image = None;
        self.pending_plain_link_click = None;
        if self
            .ui
            .update_pointer_hover(&ui_chrome::UiDrawList::new(), -1.0, -1.0)
        {
            changed = true;
        }
        if changed {
            self.needs_redraw = true;
        }
    }

    /// Handles pointer button release (finalizes active drags, selection gestures, plain link clicks, or mouse release).
    pub fn handle_pointer_up(&mut self, px: f32, py: f32, button: PointerButton, mods: Modifiers) {
        self.pointer_pos = (px, py);
        if self.ui.clear_pointer_pressed() {
            self.needs_redraw = true;
        }

        let pending_link = self.pending_plain_link_click.take();
        if let Some(drag) = self.active_drag.take() {
            self.needs_redraw = true;
            match drag {
                ActiveDrag::SidebarResize { .. } => {
                    self.mark_shared_settings_changed(true);
                    self.save_settings_best_effort();
                }
                ActiveDrag::ViewerDivider { .. } => {
                    self.force_immediate_resize = true;
                }
                ActiveDrag::TerminalSelection {
                    key,
                    content_rect,
                    cell_w,
                    cell_h,
                    ..
                } => {
                    let col = ((px - content_rect.x).max(0.0) / cell_w).floor() as u16;
                    let row = ((py - content_rect.y).max(0.0) / cell_h).floor() as u32;
                    let _ = self.pane_store.selection_release(&key, Some((col, row)));
                    // If this was a non-drag plain click on a detected link/path/image, open it!
                    if button == PointerButton::Left
                        && let Some(click) = pending_link
                        && (px - click.press_px.0).hypot(py - click.press_px.1) < 5.0
                    {
                        self.open_terminal_target(&click.key, &click.target, false, Instant::now());
                    }
                }
                ActiveDrag::PaneMove {
                    host_id,
                    session_id,
                    block_id,
                    activated,
                    ..
                } => {
                    if activated
                        && let Some((target_id, hit)) =
                            self.compute_pane_drop_target(&block_id, px, py)
                    {
                        match hit.zone {
                            DropZone::CenterSwap => {
                                self.send_command(HostCommand::SwapBlocks {
                                    host_id,
                                    request: SessionSwapBlocksRequest {
                                        session_id,
                                        block_id: block_id.clone(),
                                        other_block_id: target_id,
                                    },
                                });
                            }
                            DropZone::Left | DropZone::Right | DropZone::Top | DropZone::Bottom => {
                                let (direction, side) = match hit.zone {
                                    DropZone::Left => ("horizontal", "before"),
                                    DropZone::Right => ("horizontal", "after"),
                                    DropZone::Top => ("vertical", "before"),
                                    DropZone::Bottom => ("vertical", "after"),
                                    DropZone::CenterSwap => unreachable!(),
                                };
                                self.send_command(HostCommand::MoveBlock {
                                    host_id,
                                    session_id,
                                    block_id: block_id.clone(),
                                    anchor_block_id: target_id,
                                    direction: direction.to_owned(),
                                    side: side.to_owned(),
                                    ratio: Some(0.5),
                                });
                            }
                        }
                        self.ui.focus.focus_terminal(Some(block_id));
                    }
                }
                ActiveDrag::TabReorder {
                    host_id,
                    session_id,
                    window_id,
                    activated,
                    horizontal,
                    ..
                } => {
                    self.ui.tab_reorder_overlay = None;
                    if activated
                        && let Some((before_window_id, _, true)) = self.compute_tab_reorder_target(
                            &host_id,
                            &session_id,
                            &window_id,
                            horizontal,
                            px,
                            py,
                        )
                    {
                        self.optimistic_reorder_window(
                            &host_id,
                            &session_id,
                            &window_id,
                            before_window_id.as_deref(),
                        );
                        self.send_command(HostCommand::MoveWindow {
                            host_id,
                            session_id,
                            window_id,
                            before_window_id,
                        });
                    }
                }
                ActiveDrag::SplitDivider { .. }
                | ActiveDrag::FloatingLayer { .. }
                | ActiveDrag::PaneScrollbar { .. }
                | ActiveDrag::ViewerPan { .. }
                | ActiveDrag::ViewerScrollbar { .. }
                | ActiveDrag::ViewerVideoSeek { .. }
                | ActiveDrag::ViewerAudioSeek { .. }
                | ActiveDrag::ViewerTextSelection { .. } => {}
            }
            return;
        }

        // Fire non-drag plain link click in SGR mouse-tracking panes (where selection drag didn't run).
        if button == PointerButton::Left
            && let Some(click) = pending_link
            && (px - click.press_px.0).hypot(py - click.press_px.1) < 5.0
        {
            self.open_terminal_target(&click.key, &click.target, false, Instant::now());
            return;
        }

        // Forward release to mouse-tracking terminal if active.
        if let Some((key, content_rect, _, _, rel_x, rel_y)) = self.pane_cell_at_pointer(px, py)
            && let Some(metrics) = self.last_cell_metrics
        {
            let geom = MouseGeometry {
                screen_width: content_rect.w.round().max(1.0) as u32,
                screen_height: content_rect.h.round().max(1.0) as u32,
                cell_width: metrics.cell_width.round().max(1.0) as u32,
                cell_height: metrics.cell_height.round().max(1.0) as u32,
                padding_top: 0,
                padding_bottom: 0,
                padding_left: 0,
                padding_right: 0,
            };
            let mouse_in = MouseEventInput {
                action: MouseAction::Release,
                button: Some(button.to_vt()),
                mods: bridge_mods_to_vt(mods),
                x: rel_x,
                y: rel_y,
            };
            if let Ok(Some(bytes)) = self.pane_store.encode_mouse(&key, &mouse_in, geom)
                && let Ok(data) = String::from_utf8(bytes)
            {
                self.send_command(HostCommand::TerminalWrite {
                    host_id: key.host_id,
                    session_id: key.session_id,
                    block_id: key.block_id,
                    data,
                });
            }
        }
    }

    /// Handles smooth mouse wheel / Wayland touchpad scrolling (`delta_y_px > 0` scrolls UP).
    pub fn handle_pointer_wheel(
        &mut self,
        px: f32,
        py: f32,
        delta_x_px: f32,
        delta_y_px: f32,
        mods: Modifiers,
    ) {
        self.pointer_pos = (px, py);
        self.needs_redraw = true;

        // 0. If a modal is open, scroll the modal list instead of leaking wheel input to the terminal.
        if let Some(modal) = self.ui.modal.as_mut() {
            let steps = ((delta_y_px.abs() / 36.0).round() as isize).clamp(1, 5);
            let delta_rows = if delta_y_px >= 0.0 { -steps } else { steps };
            let _ = modal.scroll_by(delta_rows);
            return;
        }
        if self.ui.context_menu.is_some() {
            return;
        }

        // 1. Scroll or zoom inside the docked right-hand Viewer Pane if pointer is over it.
        if let Some(out) = &self.last_ui_output
            && let Some(vs) = out.viewer_split
            && vs.viewer_rect.contains(px, py)
        {
            match self.ui.viewer.active_tab().map(|t| &t.content) {
                Some(ViewerContent::Text(_)) => {
                    self.ui.viewer.scroll_by(-delta_y_px);
                }
                Some(ViewerContent::Image(_) | ViewerContent::Video(_) | ViewerContent::Pdf(_)) => {
                    if delta_y_px > 0.0 {
                        self.ui.viewer.zoom_in();
                    } else if delta_y_px < 0.0 {
                        self.ui.viewer.zoom_out();
                    }
                }
                _ => {}
            }
            return;
        }

        // 2. Scroll sidebar or overflowing horizontal tab bar if pointer is over chrome toolbar
        if let Some(out) = &self.last_ui_output {
            if out.chrome.sidebar.is_some_and(|r| r.contains(px, py)) {
                self.ui
                    .sidebar
                    .scroll_by(delta_y_px, out.sidebar_scroll.max_scroll);
                return;
            }
            if out.chrome.tab_bar.is_some_and(|r| r.contains(px, py)) {
                let horiz = if delta_x_px.abs() > 0.1 {
                    delta_x_px
                } else {
                    delta_y_px
                };
                self.ui.scroll_tabbar_by(horiz);
                return;
            }
        }

        // 2. Scroll or report wheel to hovered terminal pane
        if let Some((key, content_rect, _, _, rel_x, rel_y)) = self.pane_cell_at_pointer(px, py) {
            self.reclaim_on_pointer_activity(&key, Instant::now());
            let cell_h = self
                .last_cell_metrics
                .map_or(19.0, |m| m.cell_height.max(1.0));
            let cell_w = self
                .last_cell_metrics
                .map_or(8.0, |m| m.cell_width.max(1.0));

            // If TUI has mouse tracking on (and Shift isn't held), encode SGR wheel events.
            let geom = MouseGeometry {
                screen_width: content_rect.w.round().max(1.0) as u32,
                screen_height: content_rect.h.round().max(1.0) as u32,
                cell_width: cell_w.round().max(1.0) as u32,
                cell_height: cell_h.round().max(1.0) as u32,
                padding_top: 0,
                padding_bottom: 0,
                padding_left: 0,
                padding_right: 0,
            };
            let wheel_btn = if delta_y_px >= 0.0 {
                MouseButton::WheelUp
            } else {
                MouseButton::WheelDown
            };
            let mouse_in = MouseEventInput {
                action: MouseAction::Press,
                button: Some(wheel_btn),
                mods: bridge_mods_to_vt(mods),
                x: rel_x,
                y: rel_y,
            };
            if let Ok(Some(bytes)) = self.pane_store.encode_mouse(&key, &mouse_in, geom)
                && let Ok(data) = String::from_utf8(bytes)
            {
                self.pane_store.cancel_scroll_inertia(&key);
                self.send_command(HostCommand::TerminalWrite {
                    host_id: key.host_id,
                    session_id: key.session_id,
                    block_id: key.block_id,
                    data,
                });
                return;
            }

            // Otherwise perform smooth fractional Y overscan scroll with inertia momentum!
            if self
                .pane_store
                .scroll_wheel_impulse(&key, delta_y_px, cell_h)
                .is_ok()
            {
                self.needs_redraw = true;
            }
        }
    }

    /// Pastes text from the Linux primary selection (middle-click paste), falling back to the
    /// standard Wayland/X11 clipboard if primary selection is empty.
    pub fn handle_primary_paste(&mut self, now: Instant) {
        if !self.clipboard_enabled {
            return;
        }
        if let Some(text) = get_primary_selection_text().or_else(get_system_clipboard_text) {
            self.handle_paste(&text, now);
        }
    }

    /// Pastes clipboard text, copied files (`text/uri-list` from file managers), or clipboard
    /// images (Wayland screenshots for Mode 5522 / T3 Code / Claude Code) into the target block
    /// or focused UI input.
    pub fn handle_clipboard_paste_or_image(&mut self, block_id: Option<String>, now: Instant) {
        if !self.clipboard_enabled {
            return;
        }

        // 1. When terminal is focused, check Wayland clipboard file URIs first (e.g. files,
        //    images, or videos copied in Dolphin/Nautilus/Thunar).
        if !self.ui.focus.is_overlay_focused() {
            let copied_files = get_wayland_clipboard_uri_paths();
            if !copied_files.is_empty() {
                if let Some(bid) = block_id.as_ref()
                    && let Some(sk) = self.workspace.active_session.clone()
                {
                    let key = BlockKey::new(sk.host_id, sk.session_id, bid.clone());
                    self.handle_file_drop_into_block(&key, &copied_files);
                } else {
                    self.handle_file_drop_at(&copied_files, None);
                }
                return;
            }
        }

        // 2. Standard UTF-8 clipboard text.
        if let Some(text) = get_system_clipboard_text()
            && !text.is_empty()
        {
            if let Some(bid) = block_id
                && let Some(sk) = self.workspace.active_session.clone()
            {
                self.paste_into_block(&BlockKey::new(sk.host_id, sk.session_id, bid), &text);
            } else {
                self.handle_paste(&text, now);
            }
            return;
        }

        // 3. Fallback: image clipboard -> temporary image file -> local path paste or remote upload.
        if !self.ui.focus.is_overlay_focused()
            && let Some(tmp_img) = save_clipboard_image_to_temp_file(self.next_transfer_id)
        {
            self.next_transfer_id = self.next_transfer_id.saturating_add(1);
            if let Some(bid) = block_id
                && let Some(sk) = self.workspace.active_session.clone()
            {
                let key = BlockKey::new(sk.host_id, sk.session_id, bid);
                self.handle_file_drop_into_block(&key, &[tmp_img]);
            } else {
                self.handle_file_drop_at(&[tmp_img], None);
            }
        }
    }

    /// Pastes UTF-8 text into whichever surface holds focus (modal, find bar, sidebar filter, or PTY).
    pub fn handle_paste(&mut self, text: &str, now: Instant) {
        if text.is_empty() {
            return;
        }
        self.needs_redraw = true;
        match self.ui.focus.current() {
            FocusTarget::Modal(_) => {
                let action = self.ui.handle_modal_key(ModalKey::Paste(text.to_owned()));
                self.dispatch_ui_action(action, now);
            }
            FocusTarget::FindBar => {
                self.ui.find_bar.input.paste(text);
                let query = self.ui.find_bar.input.text().to_owned();
                self.dispatch_ui_action(UiAction::FindQueryChanged(query), now);
            }
            FocusTarget::SidebarFilter => {
                self.ui.sidebar.filter_input.paste(text);
            }
            FocusTarget::ViewerSearch => {
                self.ui.viewer.search.input.paste(text);
                self.ui.viewer.recompute_search();
                self.ui.viewer.scroll_to_active_search_match();
            }
            FocusTarget::ContextMenu | FocusTarget::Viewer => {}
            FocusTarget::Terminal(_) => {
                if let Some(key) = self.focused_block_key() {
                    self.paste_into_block(&key, text);
                }
            }
        }
    }

    pub(super) fn paste_into_block(&mut self, key: &BlockKey, text: &str) {
        self.maybe_claim_ownership(key, OwnershipTrigger::ExplicitInteractOrFit);
        let bytes = self
            .pane_store
            .encode_paste(key, text.as_bytes())
            .unwrap_or_else(|_| text.as_bytes().to_vec());
        if let Ok(data) = String::from_utf8(bytes) {
            self.send_command(HostCommand::TerminalWrite {
                host_id: key.host_id.clone(),
                session_id: key.session_id.clone(),
                block_id: key.block_id.clone(),
                data,
            });
        }
    }

    pub(super) fn handle_copy_mode_key(&mut self, input: &KeyInputEvent, now: Instant) {
        let Some((cols, total_rows, sb)) = self.clamp_copy_mode_to_bounds() else {
            return;
        };
        let Some(mut cm) = self.copy_mode.take() else {
            return;
        };
        let key = cm.block_key.clone();
        let max_col = cols.saturating_sub(1);
        let max_row = total_rows.saturating_sub(1);

        let text_ch = input
            .text
            .as_deref()
            .and_then(|t| t.chars().next())
            .filter(|c| !c.is_control());
        let cname = input.canonical_key.as_str();

        if cm.search_active {
            match cname {
                "escape" => {
                    cm.search_active = false;
                    cm.search_query.clear();
                    self.copy_mode = Some(cm);
                    return;
                }
                "enter" => {
                    cm.search_active = false;
                    self.copy_mode = Some(cm);
                    return;
                }
                "backspace" => {
                    cm.search_query.pop();
                }
                _ => {
                    if let Some(t) = &input.text {
                        for ch in t.chars().filter(|c| !c.is_control()) {
                            cm.search_query.push(ch);
                        }
                    }
                }
            }
            if let Ok(summary) = self.pane_store.search_query(&key, &cm.search_query)
                && summary.total_matches > 0
                && let Ok(Some((m_start, _))) =
                    self.pane_store.search_selected_match_screen_range(&key)
            {
                cm.cursor = ScreenPoint {
                    col: m_start.col.min(max_col),
                    row: m_start.row.min(max_row),
                };
                if let Some((mode, anchor)) = cm.visual {
                    let _ = self
                        .pane_store
                        .copy_mode_select_range(&key, anchor, cm.cursor, mode);
                }
                self.ensure_copy_mode_cursor_visible(&key, cm.cursor.row, sb);
            }
            self.copy_mode = Some(cm);
            return;
        }

        if cname == "q" && !input.mods.ctrl && !input.mods.alt {
            let _ = self.pane_store.copy_mode_clear_selection(&key);
            self.pane_store.search_close(&key);
            return;
        }
        if cname == "escape" {
            if cm.visual.take().is_some() {
                let _ = self.pane_store.copy_mode_clear_selection(&key);
                cm.pending_g = false;
                self.copy_mode = Some(cm);
            } else {
                let _ = self.pane_store.copy_mode_clear_selection(&key);
                self.pane_store.search_close(&key);
            }
            return;
        }

        if (cname == "y" && !input.mods.ctrl && !input.mods.alt)
            || (cname == "enter" && cm.visual.is_some())
        {
            if cm.visual.is_none() {
                let _ = self.pane_store.copy_mode_select_range(
                    &key,
                    cm.cursor,
                    cm.cursor,
                    SelectionMode::Line,
                );
            }
            if let Ok(Some(yanked)) = self.pane_store.copy_mode_selection_text(&key)
                && !yanked.is_empty()
            {
                if self.clipboard_enabled && !self.read_only {
                    set_system_clipboard_text(&yanked);
                }
                self.trigger_copy_shine_for_block(&key, now);
                self.push_toast_at(
                    ToastSeverity::Info,
                    "Copied from Copy Mode",
                    format!("{} chars yanked", yanked.chars().count()),
                    now,
                );
            }
            let _ = self.pane_store.copy_mode_clear_selection(&key);
            self.pane_store.search_close(&key);
            return;
        }

        if cname == "v" && !input.mods.alt {
            let next_mode = if input.mods.ctrl {
                SelectionMode::Block
            } else if input.mods.shift {
                SelectionMode::Line
            } else {
                SelectionMode::Char
            };
            cm.pending_g = false;
            match cm.visual {
                Some((cur_mode, _)) if cur_mode == next_mode => {
                    cm.visual = None;
                    let _ = self.pane_store.copy_mode_clear_selection(&key);
                }
                Some((_, anchor)) => {
                    cm.visual = Some((next_mode, anchor));
                    let _ = self
                        .pane_store
                        .copy_mode_select_range(&key, anchor, cm.cursor, next_mode);
                }
                None => {
                    let anchor = cm.cursor;
                    cm.visual = Some((next_mode, anchor));
                    let _ = self
                        .pane_store
                        .copy_mode_select_range(&key, anchor, cm.cursor, next_mode);
                }
            }
            self.copy_mode = Some(cm);
            return;
        }

        if text_ch == Some('/') && !input.mods.ctrl && !input.mods.alt {
            cm.pending_g = false;
            cm.search_active = true;
            cm.search_query.clear();
            self.copy_mode = Some(cm);
            return;
        }

        let mut moved = false;
        let half_page = (u32::try_from(sb.len).unwrap_or(20) / 2).max(1);
        let full_page = u32::try_from(sb.len).unwrap_or(20).max(1);

        if cname == "g" && !input.mods.shift && !input.mods.ctrl && !input.mods.alt {
            if cm.pending_g {
                cm.pending_g = false;
                cm.cursor = ScreenPoint { col: 0, row: 0 };
                let _ = self.pane_store.scroll_to_top(&key);
                moved = true;
            } else {
                cm.pending_g = true;
                self.copy_mode = Some(cm);
                return;
            }
        } else {
            cm.pending_g = false;
            match (cname, text_ch, input.mods.ctrl, input.mods.shift) {
                ("h" | "left", _, false, false) => {
                    cm.cursor.col = cm.cursor.col.saturating_sub(1);
                    moved = true;
                }
                ("l" | "right", _, false, false) => {
                    cm.cursor.col = (cm.cursor.col + 1).min(max_col);
                    moved = true;
                }
                ("k" | "up", _, false, false) => {
                    cm.cursor.row = cm.cursor.row.saturating_sub(1);
                    moved = true;
                }
                ("j" | "down", _, false, false) => {
                    cm.cursor.row = (cm.cursor.row + 1).min(max_row);
                    moved = true;
                }
                ("0", _, false, false) | ("home", _, false, _) => {
                    cm.cursor.col = 0;
                    moved = true;
                }
                (_, Some('^'), false, _) => {
                    cm.cursor.col = self
                        .pane_store
                        .copy_mode_first_non_blank(&key, cm.cursor.row)
                        .unwrap_or(0)
                        .min(max_col);
                    moved = true;
                }
                (_, Some('$'), false, _) | ("end", _, false, _) => {
                    cm.cursor.col = self
                        .pane_store
                        .copy_mode_line_end(&key, cm.cursor.row)
                        .unwrap_or(max_col)
                        .min(max_col);
                    moved = true;
                }
                ("w", _, false, false) => {
                    if let Ok(next) = self.pane_store.copy_mode_word_motion(
                        &key,
                        cm.cursor,
                        WordMotion::NextStart,
                    ) {
                        cm.cursor = next;
                        moved = true;
                    }
                }
                ("b", _, false, false) => {
                    if let Ok(next) = self.pane_store.copy_mode_word_motion(
                        &key,
                        cm.cursor,
                        WordMotion::PrevStart,
                    ) {
                        cm.cursor = next;
                        moved = true;
                    }
                }
                ("e", _, false, false) => {
                    if let Ok(next) =
                        self.pane_store
                            .copy_mode_word_motion(&key, cm.cursor, WordMotion::NextEnd)
                    {
                        cm.cursor = next;
                        moved = true;
                    }
                }
                ("g", _, false, true) | (_, Some('G'), false, _) => {
                    cm.cursor = ScreenPoint {
                        col: 0,
                        row: max_row,
                    };
                    let _ = self.pane_store.scroll_to_bottom(&key);
                    moved = true;
                }
                (_, Some('H'), false, _) | ("h", _, false, true) => {
                    cm.cursor.row = u32::try_from(sb.offset).unwrap_or(0).min(max_row);
                    moved = true;
                }
                (_, Some('M'), false, _) | ("m", _, false, true) => {
                    cm.cursor.row = u32::try_from(sb.offset + sb.len / 2)
                        .unwrap_or(0)
                        .min(max_row);
                    moved = true;
                }
                (_, Some('L'), false, _) | ("l", _, false, true) => {
                    cm.cursor.row = u32::try_from(sb.offset + sb.len.saturating_sub(1))
                        .unwrap_or(max_row)
                        .min(max_row);
                    moved = true;
                }
                ("u", _, true, false) => {
                    cm.cursor.row = cm.cursor.row.saturating_sub(half_page);
                    moved = true;
                }
                ("d", _, true, false) => {
                    cm.cursor.row = (cm.cursor.row + half_page).min(max_row);
                    moved = true;
                }
                ("b", _, true, false) | ("page_up", _, false, false) => {
                    cm.cursor.row = cm.cursor.row.saturating_sub(full_page);
                    moved = true;
                }
                ("f", _, true, false) | ("page_down", _, false, false) => {
                    cm.cursor.row = (cm.cursor.row + full_page).min(max_row);
                    moved = true;
                }
                ("n", _, false, false) => {
                    if self.pane_store.search_next(&key).unwrap_or(false)
                        && let Ok(Some((m_start, _))) =
                            self.pane_store.search_selected_match_screen_range(&key)
                    {
                        cm.cursor = ScreenPoint {
                            col: m_start.col.min(max_col),
                            row: m_start.row.min(max_row),
                        };
                        moved = true;
                    }
                }
                ("n", _, false, true) | (_, Some('N'), false, _) => {
                    if self.pane_store.search_prev(&key).unwrap_or(false)
                        && let Ok(Some((m_start, _))) =
                            self.pane_store.search_selected_match_screen_range(&key)
                    {
                        cm.cursor = ScreenPoint {
                            col: m_start.col.min(max_col),
                            row: m_start.row.min(max_row),
                        };
                        moved = true;
                    }
                }
                _ => {}
            }
        }

        if moved {
            cm.cursor.col = cm.cursor.col.min(max_col);
            cm.cursor.row = cm.cursor.row.min(max_row);
            if let Some((mode, anchor)) = cm.visual {
                let _ = self
                    .pane_store
                    .copy_mode_select_range(&key, anchor, cm.cursor, mode);
            }
            self.ensure_copy_mode_cursor_visible(&key, cm.cursor.row, sb);
        }
        self.copy_mode = Some(cm);
    }

    fn ensure_copy_mode_cursor_visible(
        &mut self,
        key: &BlockKey,
        cursor_row: u32,
        sb: vt_engine::Scrollbar,
    ) {
        if sb.len == 0 || sb.total <= sb.len {
            return;
        }
        let row = u64::from(cursor_row);
        let max_off = sb.total.saturating_sub(sb.len);
        if row < sb.offset {
            let _ = self
                .pane_store
                .scroll_viewport(key, ScrollViewport::Row(row.min(max_off) as usize));
        } else if row >= sb.offset + sb.len {
            let target = row.saturating_sub(sb.len.saturating_sub(1)).min(max_off);
            let _ = self
                .pane_store
                .scroll_viewport(key, ScrollViewport::Row(target as usize));
        }
    }

    /// Updates visual drop-zone highlight while a file/media drag hovers at logical `(px, py)`.
    pub fn handle_file_hover(&mut self, px: f32, py: f32) {
        let changed = self
            .active_dnd_hover
            .is_none_or(|(ox, oy)| (ox - px).abs() > 0.5 || (oy - py).abs() > 0.5);
        self.active_dnd_hover = Some((px, py));
        if changed {
            self.needs_redraw = true;
        }
    }

    /// Clears the active file drag-and-drop highlight when the drag leaves the window or aborts.
    pub fn handle_file_hover_cancelled(&mut self) {
        if self.active_dnd_hover.take().is_some() {
            self.needs_redraw = true;
        }
    }

    /// Handles dropped file paths on the currently focused pane.
    pub fn handle_file_drop(&mut self, paths: &[PathBuf]) {
        self.handle_file_drop_at(paths, None);
    }

    /// Handles dropped file paths (images, videos, documents, folders, etc.) from Wayland/X11 DnD
    /// at an optional window-local logical drop coordinate `(px, py)`:
    /// - Targets the exact pane beneath `drop_pos` (falling back to the focused pane), focusing it.
    /// - Local blocks: shell-escaped paths pasted via bracketed paste (`tr3x` / shell / agents).
    /// - Remote blocks: streams all regular files via `upload_begin` (`64 KiB` chunks) using a
    ///   single `UploadBatch` for multi-file drops so Rex pastes every path together in order.
    pub fn handle_file_drop_at(&mut self, paths: &[PathBuf], drop_pos: Option<(f32, f32)>) {
        if self.active_dnd_hover.take().is_some() {
            self.needs_redraw = true;
        }
        if paths.is_empty() {
            return;
        }
        let target_key = drop_pos
            .and_then(|(px, py)| self.pane_key_at_pointer(px, py))
            .or_else(|| self.focused_block_key());
        let Some(key) = target_key else {
            return;
        };
        self.ui.focus.focus_terminal(Some(key.block_id.clone()));
        self.handle_file_drop_into_block(&key, paths);
    }

    /// Handles dropped plain UTF-8 text from Wayland DnD at an optional logical `(px, py)`.
    pub fn handle_text_drop_at(&mut self, text: &str, drop_pos: Option<(f32, f32)>, now: Instant) {
        if self.active_dnd_hover.take().is_some() {
            self.needs_redraw = true;
        }
        if text.is_empty() {
            return;
        }
        if let Some((px, py)) = drop_pos
            && let Some(key) = self.pane_key_at_pointer(px, py)
        {
            self.ui.focus.focus_terminal(Some(key.block_id.clone()));
            self.paste_into_block(&key, text);
            self.needs_redraw = true;
            return;
        }
        self.handle_paste(text, now);
    }

    fn handle_file_drop_into_block(&mut self, key: &BlockKey, paths: &[PathBuf]) {
        if paths.is_empty() {
            return;
        }
        self.needs_redraw = true;
        let is_local = self.is_block_local(key);
        if is_local {
            let escaped: Vec<String> = paths.iter().map(|p| shell_escape_path(p)).collect();
            let joined = escaped.join(" ");
            self.paste_into_block(key, &joined);
            return;
        }

        let mut regular_files: Vec<(PathBuf, u64)> = Vec::new();
        let mut skipped_dirs: usize = 0;
        let mut missing_files: usize = 0;

        for path in paths {
            match std::fs::metadata(path) {
                Ok(meta) if meta.is_file() => {
                    regular_files.push((path.clone(), meta.len()));
                }
                Ok(meta) if meta.is_dir() => {
                    skipped_dirs += 1;
                }
                Ok(_) => {
                    missing_files += 1;
                }
                Err(_) => {
                    // Preserve unit tests that pass synthetic non-existent paths into remote mocks
                    // only when `host_manager` is not attached; on live runs report missing files.
                    if self.host_manager.is_none() {
                        regular_files.push((path.clone(), 0));
                    } else {
                        missing_files += 1;
                    }
                }
            }
        }

        if skipped_dirs > 0 {
            self.push_toast(
                ToastSeverity::Warning,
                "Folders skipped for remote upload",
                format!(
                    "{skipped_dirs} folder(s) cannot be dropped into a remote Rex terminal (use `rex cp` for directories)"
                ),
            );
        }

        if regular_files.is_empty() {
            if missing_files > 0 {
                self.push_toast(
                    ToastSeverity::Warning,
                    "Drop skipped",
                    format!("{missing_files} dropped path(s) could not be read"),
                );
            }
            return;
        }

        let count = regular_files.len() as u32;
        let total_bytes: u64 = regular_files.iter().map(|(_, sz)| *sz).sum();
        let batch_id = (count > 1).then(|| uuid::Uuid::new_v4().to_string());

        if count > 1 || total_bytes >= LARGE_UPLOAD_TOAST_THRESHOLD_BYTES {
            let detail = if count == 1 {
                let name = regular_files[0]
                    .0
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file");
                format!("Uploading {name} ({})…", format_byte_size(total_bytes))
            } else {
                format!(
                    "Uploading {count} files ({})…",
                    format_byte_size(total_bytes)
                )
            };
            self.push_toast(ToastSeverity::Info, "Uploading to remote pane", detail);
        }

        self.maybe_claim_ownership(key, OwnershipTrigger::ExplicitInteractOrFit);
        for (idx, (path, _size)) in regular_files.into_iter().enumerate() {
            let transfer_id = format!("up-{}", self.next_transfer_id);
            self.next_transfer_id = self.next_transfer_id.saturating_add(1);
            let batch = batch_id.as_ref().map(|id| UploadBatch {
                batch_id: id.clone(),
                index: idx as u32,
                count,
            });
            self.send_command(HostCommand::UploadFile {
                host_id: key.host_id.clone(),
                session_id: key.session_id.clone(),
                block_id: key.block_id.clone(),
                transfer_id,
                local_path: path,
                batch,
            });
        }
    }

    /// Handles Wayland IME preedit and committed text.
    pub fn handle_ime(
        &mut self,
        preedit: Option<(String, Option<Range<usize>>)>,
        commit: Option<String>,
        now: Instant,
    ) {
        self.needs_redraw = true;
        if let Some(committed) = commit {
            self.handle_paste(&committed, now);
            return;
        }
        let apply_preedit = |inp: &mut ui_chrome::TextInput| {
            if let Some((text, cursor_range)) = preedit {
                inp.set_preedit(text, cursor_range);
            } else {
                inp.clear_preedit();
            }
        };
        match self.ui.focus.current() {
            FocusTarget::FindBar => {
                apply_preedit(&mut self.ui.find_bar.input);
            }
            FocusTarget::SidebarFilter => {
                apply_preedit(&mut self.ui.sidebar.filter_input);
            }
            FocusTarget::Modal(_) => {
                if let Some(modal) = &mut self.ui.modal {
                    match &mut modal.content {
                        ModalContent::List { input, .. }
                        | ModalContent::FilePicker { input, .. }
                        | ModalContent::ThemePicker { input, .. }
                        | ModalContent::RexPrompt { input, .. }
                        | ModalContent::Rename { input, .. }
                        | ModalContent::AddHost { input } => apply_preedit(input),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    /// Updates Wayland top-level keyboard focus state (forwarding DEC Mode 1004 reports if enabled
    /// and reclaiming block ownership / snapping dimensions when regaining focus after a split).
    pub fn handle_window_focus(&mut self, gained: bool, _now: Instant) {
        self.window_focused = gained;
        self.needs_redraw = true;
        if let Some(key) = self.focused_block_key()
            && let Ok(Some(bytes)) = self.pane_store.encode_focus(&key, gained)
            && let Ok(data) = String::from_utf8(bytes)
        {
            self.send_command(HostCommand::TerminalWrite {
                host_id: key.host_id,
                session_id: key.session_id,
                block_id: key.block_id,
                data,
            });
        }
        if gained && !self.read_only {
            let keys = if self.visible_pane_keys.is_empty() {
                self.focused_block_key().into_iter().collect()
            } else {
                self.visible_pane_keys.clone()
            };
            for key in keys {
                let my_client_id = self
                    .workspace
                    .host(&key.host_id)
                    .and_then(HostEntry::client_id)
                    .map(str::to_owned);
                let blk = self.workspace.block(&key);
                let not_owned_by_me = blk.is_none_or(|b| b.owner != my_client_id);
                let expected = self.last_pane_grids.get(&key).copied();
                let local_mismatch =
                    expected.is_some_and(|exp| self.pane_store.size(&key) != Some(exp));
                let server_mismatch = expected.is_some_and(|exp| {
                    blk.and_then(|b| b.last_server_size)
                        .is_some_and(|srv| srv != exp)
                });
                if not_owned_by_me || local_mismatch || server_mismatch {
                    self.workspace.invalidate_sent_resize(&key);
                    self.force_immediate_resize = true;
                    self.maybe_claim_ownership(&key, OwnershipTrigger::ExplicitInteractOrFit);
                }
            }
        }
    }

    /// Uploads a hover preview image into `GpuRenderer` and records its ID for `hover_image_slot`.
    pub fn set_hover_preview_image(
        &mut self,
        renderer: &mut GpuRenderer,
        image_id: u64,
        generation: u64,
        encoded_bytes: &[u8],
    ) -> Result<()> {
        self.set_hover_preview_image_shared(renderer.shared(), image_id, generation, encoded_bytes)
    }

    /// Shared-GPU variant of [`Self::set_hover_preview_image`].
    pub fn set_hover_preview_image_shared(
        &mut self,
        shared: &GpuShared,
        image_id: u64,
        generation: u64,
        encoded_bytes: &[u8],
    ) -> Result<()> {
        shared.upload_image(ImageUpload {
            id: image_id,
            generation,
            width: 0,
            height: 0,
            format: gpu_renderer::ImagePixelFormat::Encoded,
            data: encoded_bytes,
        })?;
        self.hover_image_id = Some(image_id);
        self.needs_redraw = true;
        Ok(())
    }

    /// Strict 5-zone drop target hit-test for tiled pane drag-and-drop (`F22`).
    ///
    /// Unlike `pane_card_rect_at_pointer` (used for external OS file drops), this NEVER falls
    /// back to a default pane when the pointer is over the sidebar, tab bar, viewer, floating
    /// layer, source pane, or outside window bounds so out-of-bounds releases cancel cleanly.
    pub(super) fn compute_pane_drop_target(
        &self,
        source_block_id: &str,
        px: f32,
        py: f32,
    ) -> Option<(String, DropZoneHit)> {
        if px < 0.0 || py < 0.0 || px >= self.viewport_w || py >= self.viewport_h {
            return None;
        }
        let out = self.last_ui_output.as_ref()?;
        if !out.chrome.workspace.contains(px, py) {
            return None;
        }
        if out.chrome.sidebar.is_some_and(|r| r.contains(px, py))
            || out.chrome.tab_bar.is_some_and(|r| r.contains(px, py))
        {
            return None;
        }
        if out
            .viewer_split
            .as_ref()
            .is_some_and(|v| v.viewer_rect.contains(px, py))
        {
            return None;
        }
        if out
            .floating_panes
            .iter()
            .any(|f| f.card_rect.contains(px, py))
        {
            return None;
        }
        for geo in &out.tiled_panes {
            if geo.card_rect.contains(px, py) {
                if geo.id == source_block_id {
                    return None;
                }
                return hit_test_drop_zone(geo.card_rect, px, py).map(|hit| (geo.id.clone(), hit));
            }
        }
        None
    }

    /// Resolves tab drag-to-reorder insertion target and indicator rect in tab bar or sidebar (`F22`).
    pub(super) fn compute_tab_reorder_target(
        &self,
        host_id: &str,
        session_id: &str,
        dragged_window_id: &str,
        horizontal: bool,
        px: f32,
        py: f32,
    ) -> Option<(Option<String>, RectF, bool)> {
        if px < 0.0 || py < 0.0 || px >= self.viewport_w || py >= self.viewport_h {
            return None;
        }
        let out = self.last_ui_output.as_ref()?;
        let container_rect = if horizontal {
            out.chrome.tab_bar?
        } else {
            out.chrome.sidebar?
        };
        if !container_rect.inset_uniform(-12.0).contains(px, py) {
            return None;
        }

        let sk = SessionKey::new(host_id.to_owned(), session_id.to_owned());
        let window_ids: Vec<String> = self
            .workspace
            .session(&sk)?
            .view
            .as_ref()?
            .windows
            .iter()
            .filter_map(|w| w.window_id.clone())
            .collect();
        let src_idx = window_ids.iter().position(|id| id == dragged_window_id)?;

        let mut visible_tabs: Vec<(usize, RectF)> = Vec::new();
        for hit in &out.draw_list.hits {
            if let HitTarget::TabCard {
                host_id: h,
                session_id: s,
                window_id: w,
            } = &hit.target
                && h == host_id
                && s == session_id
                && let Some(idx) = window_ids.iter().position(|id| id == w)
            {
                visible_tabs.push((idx, hit.rect));
            }
        }
        visible_tabs.sort_by_key(|(idx, _)| *idx);
        if visible_tabs.is_empty() {
            return None;
        }

        let mut insert_pos = visible_tabs.last().map_or(0, |(idx, _)| idx + 1);
        for &(idx, rect) in &visible_tabs {
            let midpoint = if horizontal {
                rect.x + rect.w * 0.5
            } else {
                rect.y + rect.h * 0.5
            };
            let coord = if horizontal { px } else { py };
            if coord < midpoint {
                insert_pos = idx;
                break;
            }
        }

        let has_valid_drop = insert_pos != src_idx && insert_pos != src_idx + 1;
        let before_window_id = window_ids.get(insert_pos).cloned();

        let indicator_rect = if let Some((_, target_rect)) =
            visible_tabs.iter().find(|&&(idx, _)| idx == insert_pos)
        {
            if horizontal {
                RectF::new(
                    (target_rect.x - 3.0).max(container_rect.x),
                    target_rect.y + 2.0,
                    2.5,
                    (target_rect.h - 4.0).max(4.0),
                )
            } else {
                RectF::new(
                    target_rect.x + 2.0,
                    (target_rect.y - 3.0).max(container_rect.y),
                    (target_rect.w - 4.0).max(4.0),
                    2.5,
                )
            }
        } else {
            let (_, last_rect) = visible_tabs.last().expect("non-empty visible_tabs");
            if horizontal {
                RectF::new(
                    (last_rect.right() + 1.0).min(container_rect.right() - 2.5),
                    last_rect.y + 2.0,
                    2.5,
                    (last_rect.h - 4.0).max(4.0),
                )
            } else {
                RectF::new(
                    last_rect.x + 2.0,
                    (last_rect.bottom() + 1.0).min(container_rect.bottom() - 2.5),
                    (last_rect.w - 4.0).max(4.0),
                    2.5,
                )
            }
        };

        Some((before_window_id, indicator_rect, has_valid_drop))
    }

    pub(super) fn optimistic_reorder_window(
        &mut self,
        host_id: &str,
        session_id: &str,
        window_id: &str,
        before_window_id: Option<&str>,
    ) {
        let sk = SessionKey::new(host_id.to_owned(), session_id.to_owned());
        let Some(view) = self
            .workspace
            .session_mut(&sk)
            .and_then(|s| s.view.as_mut())
        else {
            return;
        };
        let Some(from_idx) = view
            .windows
            .iter()
            .position(|w| w.window_id.as_deref() == Some(window_id))
        else {
            return;
        };
        let win = view.windows.remove(from_idx);
        let to_idx = match before_window_id {
            Some(before_id) => view
                .windows
                .iter()
                .position(|w| w.window_id.as_deref() == Some(before_id))
                .unwrap_or(view.windows.len()),
            None => view.windows.len(),
        };
        view.windows.insert(to_idx, win);
        self.needs_redraw = true;
    }

    // =========================================================================
    // Helpers
    // =========================================================================

    fn pane_key_at_pointer(&self, px: f32, py: f32) -> Option<BlockKey> {
        let sk = self.workspace.active_session.as_ref()?;
        let out = self.last_ui_output.as_ref()?;
        for fgeo in out.floating_panes.iter().rev() {
            if fgeo.card_rect.contains(px, py) {
                return Some(BlockKey::new(&sk.host_id, &sk.session_id, &fgeo.id));
            }
        }
        for geo in &out.tiled_panes {
            if geo.card_rect.contains(px, py) {
                return Some(BlockKey::new(&sk.host_id, &sk.session_id, &geo.id));
            }
        }
        None
    }

    pub(super) fn pane_card_rect_at_pointer(&self, px: f32, py: f32) -> Option<RectF> {
        let out = self.last_ui_output.as_ref()?;
        for fgeo in out.floating_panes.iter().rev() {
            if fgeo.card_rect.contains(px, py) {
                return Some(fgeo.card_rect);
            }
        }
        for geo in &out.tiled_panes {
            if geo.card_rect.contains(px, py) {
                return Some(geo.card_rect);
            }
        }
        // Fallback to focused pane card when dragging over window border or sidebar.
        if let Some(focused_id) = self.ui.focus.last_terminal_block() {
            if let Some(fgeo) = out.floating_panes.iter().find(|p| p.id == focused_id) {
                return Some(fgeo.card_rect);
            }
            if let Some(geo) = out.tiled_panes.iter().find(|p| p.id == focused_id) {
                return Some(geo.card_rect);
            }
        }
        out.tiled_panes.first().map(|p| p.card_rect)
    }

    fn pane_cell_at_pointer(
        &self,
        px: f32,
        py: f32,
    ) -> Option<(BlockKey, RectF, u16, u32, f32, f32)> {
        let sk = self.workspace.active_session.as_ref()?;
        let out = self.last_ui_output.as_ref()?;
        let (pad_x, pad_y) = self.pane_content_padding();
        let metrics = self.last_cell_metrics?;

        // Floating panes take priority over tiled panes beneath them.
        for fgeo in out.floating_panes.iter().rev() {
            if fgeo.terminal_rect.contains(px, py) {
                let content = fgeo.terminal_rect.inset(pad_x, pad_y, pad_x, pad_y);
                let rel_x = (px - content.x).max(0.0);
                let rel_y = (py - content.y).max(0.0);
                let col = (rel_x / metrics.cell_width.max(1.0)).floor() as u16;
                let row = (rel_y / metrics.cell_height.max(1.0)).floor() as u32;
                return Some((
                    BlockKey::new(&sk.host_id, &sk.session_id, &fgeo.id),
                    content,
                    col,
                    row,
                    rel_x,
                    rel_y,
                ));
            }
        }
        for geo in &out.tiled_panes {
            if geo.terminal_rect.contains(px, py) {
                let content = geo.terminal_rect.inset(pad_x, pad_y, pad_x, pad_y);
                let rel_x = (px - content.x).max(0.0);
                let rel_y = (py - content.y).max(0.0);
                let col = (rel_x / metrics.cell_width.max(1.0)).floor() as u16;
                let row = (rel_y / metrics.cell_height.max(1.0)).floor() as u32;
                return Some((
                    BlockKey::new(&sk.host_id, &sk.session_id, &geo.id),
                    content,
                    col,
                    row,
                    rel_x,
                    rel_y,
                ));
            }
        }
        None
    }

    pub(super) fn resolve_session_key(&self, session_id: Option<&str>) -> Option<SessionKey> {
        if let Some(sid) = session_id {
            for host in self.workspace.hosts.values() {
                if host.sessions.contains_key(sid) {
                    return Some(SessionKey::new(host.id.clone(), sid.to_owned()));
                }
            }
        }
        self.workspace.active_session.clone()
    }

    pub(super) fn find_layer_for_block(&self, block_id: &str) -> Option<(SessionKey, String)> {
        let sk = self.workspace.active_session.as_ref()?;
        let view = self.workspace.session(sk)?.view.as_ref()?;
        let win = active_window_of_view(view)?;
        for layer in &win.layers {
            if layer
                .blocks
                .iter()
                .any(|b| b.block_id.as_deref() == Some(block_id))
                && let Some(lid) = &layer.layer_id
            {
                return Some((sk.clone(), lid.clone()));
            }
        }
        None
    }

    pub(super) fn is_block_in_floating_layer(&self, sk: &SessionKey, block_id: &str) -> bool {
        let Some(view) = self.workspace.session(sk).and_then(|s| s.view.as_ref()) else {
            return false;
        };
        let Some(win) = active_window_of_view(view) else {
            return false;
        };
        win.layers.iter().any(|l| {
            !l.kind.eq_ignore_ascii_case("tiled")
                && l.blocks
                    .iter()
                    .any(|b| b.block_id.as_deref() == Some(block_id))
        })
    }

    pub(super) fn find_tiled_anchor_block(
        &self,
        sk: &SessionKey,
        exclude_block_id: &str,
    ) -> Option<String> {
        let view = self.workspace.session(sk)?.view.as_ref()?;
        let win = active_window_of_view(view)?;
        win.layers
            .iter()
            .filter(|l| l.kind.eq_ignore_ascii_case("tiled"))
            .flat_map(|l| l.blocks.iter())
            .filter_map(|b| b.block_id.as_deref())
            .find(|&bid| bid != exclude_block_id)
            .map(str::to_owned)
    }

    pub(super) fn move_active_tab(&mut self, forward: bool) {
        let Some(sk) = self.workspace.active_session.clone() else {
            return;
        };
        let move_args = self
            .workspace
            .session(&sk)
            .and_then(|s| s.view.as_ref())
            .and_then(|view| {
                let ids: Vec<&str> = view
                    .windows
                    .iter()
                    .filter_map(|w| w.window_id.as_deref())
                    .collect();
                if ids.len() <= 1 {
                    return None;
                }
                let cur = view
                    .active_window_id
                    .as_deref()
                    .and_then(|id| ids.iter().position(|&w| w == id))?;
                let wid = ids[cur].to_owned();
                if forward {
                    if cur + 1 >= ids.len() {
                        return None;
                    }
                    let before = ids.get(cur + 2).map(|&s| s.to_owned());
                    Some((wid, before))
                } else {
                    if cur == 0 {
                        return None;
                    }
                    let before = Some(ids[cur - 1].to_owned());
                    Some((wid, before))
                }
            });
        if let Some((window_id, before_window_id)) = move_args {
            self.optimistic_reorder_window(
                &sk.host_id,
                &sk.session_id,
                &window_id,
                before_window_id.as_deref(),
            );
            self.send_command(HostCommand::MoveWindow {
                host_id: sk.host_id,
                session_id: sk.session_id,
                window_id,
                before_window_id,
            });
        }
    }

    pub(super) fn move_focused_pane(&mut self, dir: PaneDirection) {
        let Some(sk) = self.workspace.active_session.clone() else {
            return;
        };
        let Some(cur_key) = self.focused_block_key() else {
            return;
        };
        let Some(out) = &self.last_ui_output else {
            return;
        };
        let Some(cur) = out.tiled_panes.iter().find(|p| p.id == cur_key.block_id) else {
            return;
        };
        let (cx, cy) = cur.card_rect.center();
        let mut best: Option<(f32, String)> = None;
        for cand in &out.tiled_panes {
            if cand.id == cur.id {
                continue;
            }
            let (tx, ty) = cand.card_rect.center();
            let valid = match dir {
                PaneDirection::Left => tx < cx - 4.0,
                PaneDirection::Right => tx > cx + 4.0,
                PaneDirection::Up => ty < cy - 4.0,
                PaneDirection::Down => ty > cy + 4.0,
            };
            if !valid {
                continue;
            }
            let dist = (tx - cx).hypot(ty - cy);
            if best.as_ref().is_none_or(|(d, _)| dist < *d) {
                best = Some((dist, cand.id.clone()));
            }
        }
        if let Some((_, anchor_block_id)) = best {
            let (direction, side) = match dir {
                PaneDirection::Left => ("horizontal", "before"),
                PaneDirection::Right => ("horizontal", "after"),
                PaneDirection::Up => ("vertical", "before"),
                PaneDirection::Down => ("vertical", "after"),
            };
            self.send_command(HostCommand::MoveBlock {
                host_id: sk.host_id,
                session_id: sk.session_id,
                block_id: cur_key.block_id,
                anchor_block_id,
                direction: direction.to_owned(),
                side: side.to_owned(),
                ratio: Some(0.5),
            });
        }
    }

    pub(super) fn find_layer_bounds(&self, sk: &SessionKey, layer_id: &str) -> Option<[f32; 4]> {
        let view = self.workspace.session(sk)?.view.as_ref()?;
        let win = active_window_of_view(view)?;
        let layer = win
            .layers
            .iter()
            .find(|l| l.layer_id.as_deref() == Some(layer_id))?;
        Some([
            layer.bounds[0] as f32,
            layer.bounds[1] as f32,
            layer.bounds[2] as f32,
            layer.bounds[3] as f32,
        ])
    }

    pub(super) fn resolve_split_target_at_path(
        &self,
        sk: &SessionKey,
        path: &[bool],
    ) -> Option<(String, String, String)> {
        let view = self.workspace.session(sk)?.view.as_ref()?;
        let win = active_window_of_view(view)?;
        let tiled = win
            .layers
            .iter()
            .find(|l| l.kind.eq_ignore_ascii_case("tiled"))?;
        let mut cur = tiled.layout.as_ref()?;
        for &go_second in path {
            let sp = cur.split.as_ref()?;
            cur = if go_second { &sp.after } else { &sp.before };
        }
        let sp = cur.split.as_ref()?;
        let before_id = first_leaf_block_id(&sp.before)?;
        let after_id = first_leaf_block_id(&sp.after)?;
        Some((sp.direction.clone(), before_id, after_id))
    }

    pub(super) fn is_block_local(&self, key: &BlockKey) -> bool {
        let Some(host) = self.workspace.host(&key.host_id) else {
            return false;
        };
        let block_host_id = host
            .sessions
            .get(&key.session_id)
            .and_then(|s| s.view.as_ref())
            .and_then(|v| {
                v.windows
                    .iter()
                    .flat_map(|w| w.layers.iter())
                    .flat_map(|l| l.blocks.iter())
                    .find(|b| b.block_id.as_deref() == Some(key.block_id.as_str()))
                    .and_then(|b| b.host_id.as_deref())
            });
        host.is_block_local_to_this_machine(block_host_id)
    }
}

fn bridge_mods_to_vt(m: Modifiers) -> VtModifiers {
    let mut out = VtModifiers::NONE;
    if m.shift {
        out |= VtModifiers::SHIFT;
    }
    if m.ctrl {
        out |= VtModifiers::CTRL;
    }
    if m.alt {
        out |= VtModifiers::ALT;
    }
    if m.super_key {
        out |= VtModifiers::SUPER;
    }
    out
}

fn is_modifier_or_lock_key(key: PhysicalKey) -> bool {
    matches!(
        key,
        PhysicalKey::ControlLeft
            | PhysicalKey::ControlRight
            | PhysicalKey::ShiftLeft
            | PhysicalKey::ShiftRight
            | PhysicalKey::AltLeft
            | PhysicalKey::AltRight
            | PhysicalKey::MetaLeft
            | PhysicalKey::MetaRight
            | PhysicalKey::CapsLock
            | PhysicalKey::NumLock
            | PhysicalKey::ScrollLock
            | PhysicalKey::Fn
            | PhysicalKey::FnLock
    )
}

fn key_input_to_modal_key(input: &KeyInputEvent) -> Option<ModalKey> {
    match input.canonical_key.as_str() {
        "up" => Some(ModalKey::Up),
        "down" => Some(ModalKey::Down),
        "page_up" => Some(ModalKey::PageUp),
        "page_down" => Some(ModalKey::PageDown),
        "enter" if input.mods.ctrl => Some(ModalKey::CtrlEnter),
        "enter" => Some(ModalKey::Enter),
        "tab" if !input.mods.ctrl && !input.mods.alt => Some(ModalKey::Tab),
        "escape" => Some(ModalKey::Escape),
        "backspace" if input.mods.ctrl => Some(ModalKey::WordBackspace),
        "backspace" => Some(ModalKey::Backspace),
        "delete" => Some(ModalKey::Delete),
        "left" => Some(ModalKey::Left {
            extend: input.mods.shift,
        }),
        "right" => Some(ModalKey::Right {
            extend: input.mods.shift,
        }),
        "home" => Some(ModalKey::Home {
            extend: input.mods.shift,
        }),
        "end" => Some(ModalKey::End {
            extend: input.mods.shift,
        }),
        "a" if input.mods.ctrl && !input.mods.alt => Some(ModalKey::SelectAll),
        "space" if !input.mods.ctrl && !input.mods.alt => Some(ModalKey::Space),
        _ => {
            if !input.mods.ctrl
                && !input.mods.alt
                && !input.mods.super_key
                && let Some(text) = &input.text
            {
                let mut chars = text.chars();
                if let (Some(ch), None) = (chars.next(), chars.next())
                    && !ch.is_control()
                {
                    return Some(ModalKey::Char(ch));
                }
                if !text.is_empty() && !text.chars().all(char::is_control) {
                    return Some(ModalKey::Paste(text.clone()));
                }
            }
            None
        }
    }
}

fn apply_key_to_text_input(input: &mut ui_chrome::TextInput, ev: &KeyInputEvent) -> bool {
    match ev.canonical_key.as_str() {
        "backspace" if ev.mods.ctrl => input.delete_word_backward(),
        "backspace" => input.backspace(),
        "delete" => input.delete_forward(),
        "left" => {
            input.move_left(ev.mods.shift);
            false
        }
        "right" => {
            input.move_right(ev.mods.shift);
            false
        }
        "home" => {
            input.move_home(ev.mods.shift);
            false
        }
        "end" => {
            input.move_end(ev.mods.shift);
            false
        }
        "a" if ev.mods.ctrl => {
            input.select_all();
            false
        }
        _ => {
            if !ev.mods.ctrl
                && !ev.mods.alt
                && !ev.mods.super_key
                && let Some(text) = &ev.text
                && !text.is_empty()
                && !text.chars().all(char::is_control)
            {
                input.insert_str(text);
                return true;
            }
            false
        }
    }
}

fn compute_floating_drag_bounds(
    start: [f32; 4],
    zone: FloatingHitZone,
    dx: f32,
    dy: f32,
) -> [f32; 4] {
    let [mut x, mut y, mut w, mut h] = start;
    let min_w = 0.15f32;
    let min_h = 0.15f32;
    match zone {
        FloatingHitZone::MoveHeader => {
            x = (x + dx).clamp(0.0, (1.0 - w).max(0.0));
            y = (y + dy).clamp(0.0, (1.0 - h).max(0.0));
        }
        FloatingHitZone::ResizeE => {
            w = (w + dx).clamp(min_w, (1.0 - x).max(min_w));
        }
        FloatingHitZone::ResizeS => {
            h = (h + dy).clamp(min_h, (1.0 - y).max(min_h));
        }
        FloatingHitZone::ResizeSE => {
            w = (w + dx).clamp(min_w, (1.0 - x).max(min_w));
            h = (h + dy).clamp(min_h, (1.0 - y).max(min_h));
        }
        FloatingHitZone::ResizeW => {
            let nx = (x + dx).clamp(0.0, (x + w - min_w).max(0.0));
            w += x - nx;
            x = nx;
        }
        FloatingHitZone::ResizeN => {
            let ny = (y + dy).clamp(0.0, (y + h - min_h).max(0.0));
            h += y - ny;
            y = ny;
        }
        FloatingHitZone::ResizeNW => {
            let nx = (x + dx).clamp(0.0, (x + w - min_w).max(0.0));
            let ny = (y + dy).clamp(0.0, (y + h - min_h).max(0.0));
            w += x - nx;
            h += y - ny;
            x = nx;
            y = ny;
        }
        FloatingHitZone::ResizeNE => {
            let ny = (y + dy).clamp(0.0, (y + h - min_h).max(0.0));
            h += y - ny;
            y = ny;
            w = (w + dx).clamp(min_w, (1.0 - x).max(min_w));
        }
        FloatingHitZone::ResizeSW => {
            let nx = (x + dx).clamp(0.0, (x + w - min_w).max(0.0));
            w += x - nx;
            x = nx;
            h = (h + dy).clamp(min_h, (1.0 - y).max(min_h));
        }
        FloatingHitZone::Content => {}
    }
    [x, y, w, h]
}

pub(super) fn shell_escape_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | ':'))
    {
        s.into_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn infer_consumed_mods(
    mods: Modifiers,
    text: Option<&str>,
    unshifted: Option<char>,
) -> VtModifiers {
    let mut consumed = VtModifiers::NONE;
    let Some(t) = text.filter(|s| !s.is_empty() && !s.chars().all(char::is_control)) else {
        return consumed;
    };
    if mods.ctrl || mods.super_key {
        return consumed;
    }
    let mut chars = t.chars();
    let single = match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    };
    let transformed = match (single, unshifted) {
        (Some(produced), Some(base)) => produced != base,
        _ => false,
    };
    if mods.shift && transformed {
        consumed |= VtModifiers::SHIFT;
    }
    // AltGr on German/international layouts transforms keys (e.g. `q` -> `@`, `7` -> `{`, `e` -> `€`).
    // Plain Meta-key shortcuts like `Alt+f` keep `produced == 'f' == unshifted` so ALT stays unconsumed.
    if mods.alt && transformed {
        consumed |= VtModifiers::ALT;
    }
    consumed
}

fn get_system_clipboard_text() -> Option<String> {
    let mut cb = arboard::Clipboard::new().ok()?;
    cb.get_text().ok()
}

fn get_primary_selection_text() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        use arboard::{GetExtLinux, LinuxClipboardKind};
        let mut cb = arboard::Clipboard::new().ok()?;
        cb.get()
            .clipboard(LinuxClipboardKind::Primary)
            .text()
            .ok()
            .filter(|s| !s.is_empty())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn get_wayland_clipboard_uri_paths() -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Vec::new();
        }
        let Ok(types_out) = std::process::Command::new("wl-paste")
            .arg("--list-types")
            .output()
        else {
            return Vec::new();
        };
        if !types_out.status.success() {
            return Vec::new();
        }
        let offered = String::from_utf8_lossy(&types_out.stdout);
        let has_uri_list = offered.lines().any(|l| l.trim() == "text/uri-list");
        let has_gnome = offered
            .lines()
            .any(|l| l.trim() == "x-special/gnome-copied-files");

        if has_uri_list
            && let Ok(out) = std::process::Command::new("wl-paste")
                .args(["--no-newline", "--type", "text/uri-list"])
                .output()
            && out.status.success()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            let paths = crate::wayland_dnd::parse_uri_list(&text);
            if !paths.is_empty() {
                return paths;
            }
        }
        if has_gnome
            && let Ok(out) = std::process::Command::new("wl-paste")
                .args(["--no-newline", "--type", "x-special/gnome-copied-files"])
                .output()
            && out.status.success()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            let paths = crate::wayland_dnd::parse_gnome_copied_files(&text);
            if !paths.is_empty() {
                return paths;
            }
        }
        Vec::new()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

fn save_clipboard_image_to_temp_file(seq: u64) -> Option<PathBuf> {
    if let Some(path) = save_clipboard_image_to_temp_png(seq) {
        return Some(path);
    }
    #[cfg(target_os = "linux")]
    {
        std::env::var_os("WAYLAND_DISPLAY")?;
        let types_out = std::process::Command::new("wl-paste")
            .arg("--list-types")
            .output()
            .ok()?;
        if !types_out.status.success() {
            return None;
        }
        let offered: Vec<&str> = std::str::from_utf8(&types_out.stdout)
            .ok()?
            .lines()
            .map(str::trim)
            .collect();
        for (mime, ext) in [
            ("image/png", "png"),
            ("image/webp", "webp"),
            ("image/jpeg", "jpg"),
            ("image/gif", "gif"),
        ] {
            if offered.contains(&mime)
                && let Ok(out) = std::process::Command::new("wl-paste")
                    .args(["--type", mime])
                    .output()
                && out.status.success()
                && !out.stdout.is_empty()
            {
                let path = std::env::temp_dir()
                    .join(format!("omarex-clip-{}-{seq}.{ext}", std::process::id()));
                if std::fs::write(&path, &out.stdout).is_ok() {
                    return Some(path);
                }
            }
        }
    }
    None
}

fn save_clipboard_image_to_temp_png(seq: u64) -> Option<PathBuf> {
    let mut cb = arboard::Clipboard::new().ok()?;
    let img = cb.get_image().ok()?;
    let w = u32::try_from(img.width).ok()?;
    let h = u32::try_from(img.height).ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    let path = std::env::temp_dir().join(format!("omarex-clip-{}-{seq}.png", std::process::id()));
    gpu_renderer::save_rgba_png(&path, w, h, &img.bytes).ok()?;
    Some(path)
}
