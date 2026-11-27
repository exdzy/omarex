use std::collections::HashMap;
use std::time::Instant;

use omarchy_theme::ThemeEvent;
use rex_proto::RexEndpoint;
use rex_proto::types::{
    ClientActionRequest, ClientReplyRequest, OwnMode, SessionFloatBlockRequest,
    SessionNewSplitRequest, SessionNewWindowRequest, SessionSetLayerStateRequest,
    SessionZoomBlockRequest,
};
use tr3x_bridge::{
    ActionKind, ClientCommand, KeyChord, PaneDirection, SplitDirection, is_t3_session,
    parse_client_command, reply_to_proto,
};
use ui_chrome::{
    FilePickerPurpose, FocusTarget, HitTarget, IconId, ModalKind, ModalState, PaletteItem,
    PaneDensity, PaneHeaderButton, PaneHeaderMode, SettingChange, SettingsViewSnapshot,
    ToastSeverity, ToolbarBehavior, ToolbarPosition, UiAction, ViewerAction, ViewerContent,
    WhichKeyState, classify_path, parse_path_with_location,
};
use vt_engine::ScrollViewport;

use crate::config::{
    LayoutDensity, SavedHost, SavedPaneHeaderMode, SidebarBehavior, SidebarPosition, WindowMaterial,
};
use crate::host_manager::{HostCommand, LOCAL_HOST_ID, list_dir_local_fs};
use crate::workspace::{
    BlockKey, HostEntry, HostStatus, OwnershipTrigger, SessionKey, format_port_http_url,
    normalize_tunnel_host,
};

use super::*;

impl OmarexController {
    // =========================================================================
    // 3. UiAction & ClientCommand Dispatcher
    // =========================================================================

    /// Executes any high-level [`UiAction`] emitted by `UiTree` (clicks, modals, menus, shortcuts).
    pub fn dispatch_ui_action(&mut self, action: UiAction, now: Instant) {
        self.needs_redraw = true;
        match action {
            UiAction::None | UiAction::CloseModal => {}

            UiAction::ToggleSidebar => {
                // UiTree::handle_click already toggled `sidebar_collapsed` for button click;
                // snap visible terminal grids immediately to the new viewport width.
                self.force_immediate_resize = true;
            }

            UiAction::ToggleActivityMode | UiAction::ToggleActivitySort => {}

            UiAction::ToggleHostCollapse { host_id } => {
                if !self.collapsed_hosts.remove(&host_id) {
                    self.collapsed_hosts.insert(host_id);
                }
            }

            UiAction::ToggleSessionCollapse {
                host_id,
                session_id,
            } => {
                let currently_collapsed = self
                    .collapsed_sessions
                    .contains(&(host_id.clone(), session_id.clone()));
                self.set_session_collapsed(&host_id, &session_id, !currently_collapsed);
            }

            UiAction::SelectSession {
                host_id,
                session_id,
            } => {
                self.activate_session(SessionKey::new(host_id, session_id), &HashMap::new());
                self.force_immediate_resize = true;
            }

            UiAction::ToggleSessionMute {
                host_id,
                session_id,
            } => {
                self.settings.toggle_mute_session(&host_id, &session_id);
                self.save_settings_best_effort();
            }

            UiAction::NewSession { host_id } => {
                self.open_real_shell_terminal(host_id, true);
            }

            UiAction::NewTerminal => {
                self.open_real_shell_terminal(None, false);
            }

            UiAction::SelectTab {
                host_id,
                session_id,
                window_id,
            } => {
                self.unread_windows.remove(&(
                    host_id.clone(),
                    session_id.clone(),
                    window_id.clone(),
                ));
                self.set_session_collapsed(&host_id, &session_id, false);
                let sk = SessionKey::new(host_id.clone(), session_id.clone());
                if self.workspace.active_session.as_ref() != Some(&sk) {
                    self.activate_session(sk.clone(), &HashMap::new());
                }
                let (primary_block, window_known) = self
                    .workspace
                    .session_mut(&sk)
                    .and_then(|sess| sess.view.as_mut())
                    .map(|view| {
                        view.active_window_id = Some(window_id.clone());
                        let found = view
                            .windows
                            .iter()
                            .find(|w| w.window_id.as_deref() == Some(window_id.as_str()));
                        (
                            found
                                .and_then(primary_block_id_of_window)
                                .map(str::to_owned),
                            found.is_some(),
                        )
                    })
                    .unwrap_or((None, false));
                self.send_command(HostCommand::FocusWindow {
                    host_id: host_id.clone(),
                    session_id: session_id.clone(),
                    window_id,
                });
                if !window_known {
                    self.send_command(HostCommand::FetchView {
                        host_id: host_id.clone(),
                        session_id: session_id.clone(),
                    });
                }
                self.open_active_window_streams(&sk, &HashMap::new());
                self.force_immediate_resize = true;
                if let Some(bid) = primary_block {
                    self.ui.focus.focus_terminal(Some(bid.clone()));
                    let key = BlockKey::new(host_id, session_id, bid);
                    self.workspace.invalidate_sent_resize(&key);
                    self.maybe_claim_ownership(&key, OwnershipTrigger::ExplicitInteractOrFit);
                }
            }

            UiAction::CloseTab {
                host_id,
                session_id,
                window_id,
            } => {
                self.send_command(HostCommand::CloseWindow {
                    host_id,
                    session_id,
                    window_id,
                    undoable: true,
                });
            }

            UiAction::NewTab {
                host_id,
                session_id,
            } => {
                let resolved_sk = match (host_id.as_deref(), session_id.as_deref()) {
                    (Some(hid), Some(sid)) => Some(SessionKey::new(hid, sid)),
                    (None, Some(sid)) => self.resolve_session_key(Some(sid)),
                    _ => None,
                };
                if let Some(sk) = resolved_sk {
                    let is_t3 = self
                        .workspace
                        .session(&sk)
                        .is_some_and(|s| is_t3_session(&s.label));
                    if is_t3 {
                        self.dispatch_ui_action(
                            UiAction::NewT3Thread {
                                host_id: sk.host_id,
                                session_id: sk.session_id,
                            },
                            now,
                        );
                    } else {
                        self.collapsed_hosts.remove(&sk.host_id);
                        self.set_session_collapsed(&sk.host_id, &sk.session_id, false);
                        if self.workspace.active_session.as_ref() != Some(&sk) {
                            self.activate_session(sk.clone(), &HashMap::new());
                        }
                        let cwd = self.resolve_shell_cwd_for_host(&sk.host_id, None, false);
                        let layout =
                            self.build_shell_layout_spec_with_cwd(cwd.as_deref(), None, false);
                        self.send_command(HostCommand::NewWindow {
                            host_id: sk.host_id,
                            request: SessionNewWindowRequest {
                                session_id: sk.session_id,
                                window_label: None,
                                layout: Some(layout),
                                focus: Some(true),
                            },
                            origin_window_id: None,
                        });
                    }
                } else {
                    self.open_real_shell_terminal(host_id, false);
                }
            }

            UiAction::NewT3Thread {
                host_id,
                session_id,
            } => {
                self.collapsed_hosts.remove(&host_id);
                self.set_session_collapsed(&host_id, &session_id, false);
                let sk = SessionKey::new(host_id.clone(), session_id.clone());
                if self.workspace.active_session.as_ref() != Some(&sk) {
                    self.activate_session(sk, &HashMap::new());
                    self.force_immediate_resize = true;
                }
                self.send_server_action_for_session(&host_id, Some(&session_id), "t3_new", None);
            }

            UiAction::StartRenameHost { host_id } => {
                if host_id != LOCAL_HOST_ID {
                    self.pending_rename_target = Some(PendingRenameTarget::Host { host_id });
                    self.open_modal_kind(ModalKind::Rename);
                }
            }

            UiAction::DisconnectHost { host_id } => {
                if host_id != LOCAL_HOST_ID {
                    let sids: Vec<String> = self
                        .workspace
                        .host(&host_id)
                        .map(|h| h.session_order.clone())
                        .unwrap_or_default();
                    for sid in &sids {
                        self.pane_store.evict_session(&host_id, sid, None);
                    }
                    self.opened_streams.retain(|k| k.host_id != host_id);
                    self.hydrated_blocks.retain(|k| k.host_id != host_id);
                    self.block_activity_at.retain(|k, _| k.host_id != host_id);
                    self.block_updated_unix.retain(|k, _| k.host_id != host_id);
                    self.block_osc94.retain(|k, _| k.host_id != host_id);
                    self.other_app_attached_sessions.remove(&host_id);
                    if let Some(host) = self.workspace.host_mut(&host_id) {
                        host.status = HostStatus::Disconnected;
                        host.sessions.clear();
                        host.session_order.clear();
                    }
                    if self
                        .workspace
                        .active_session
                        .as_ref()
                        .is_some_and(|k| k.host_id == host_id)
                    {
                        self.workspace.active_session = None;
                        if let Some(fallback_sk) =
                            self.workspace.host_order.iter().find_map(|hid| {
                                let host = self.workspace.host(hid)?;
                                let sid = host.session_order.first()?;
                                Some(SessionKey::new(hid.clone(), sid.clone()))
                            })
                        {
                            self.activate_session(fallback_sk, &HashMap::new());
                            self.force_immediate_resize = true;
                        }
                    }
                    self.send_command(HostCommand::RemoveHost { host_id });
                }
            }

            UiAction::ReconnectHost { host_id } => {
                if host_id != LOCAL_HOST_ID {
                    let saved = self
                        .settings
                        .saved_hosts
                        .iter()
                        .find(|h| h.id == host_id)
                        .cloned();
                    let label = self.host_display_label(&host_id);
                    let endpoint = saved
                        .as_ref()
                        .and_then(|s| RexEndpoint::parse(&s.endpoint).ok())
                        .or_else(|| self.workspace.host(&host_id).map(|h| h.endpoint.clone()));
                    if let Some(ep) = endpoint {
                        if let Some(host) = self.workspace.host_mut(&host_id) {
                            host.status = HostStatus::Reconnecting {
                                reason: "reconnecting…".into(),
                            };
                        }
                        self.send_command(HostCommand::RemoveHost {
                            host_id: host_id.clone(),
                        });
                        self.send_command(HostCommand::AddHost {
                            host_id,
                            label,
                            endpoint: ep,
                            preferred_session_id: None,
                        });
                    }
                }
            }

            UiAction::RemoveHost { host_id } => {
                if host_id != LOCAL_HOST_ID {
                    let removed_sessions = self.workspace.remove_host(&host_id);
                    for sk in &removed_sessions {
                        self.pane_store
                            .evict_session(&sk.host_id, &sk.session_id, None);
                    }
                    self.collapsed_hosts.remove(&host_id);
                    self.collapsed_sessions.retain(|(h, _)| h != &host_id);
                    self.unread_windows.retain(|(h, _, _)| h != &host_id);
                    self.opened_streams.retain(|k| k.host_id != host_id);
                    self.hydrated_blocks.retain(|k| k.host_id != host_id);
                    self.block_activity_at.retain(|k, _| k.host_id != host_id);
                    self.block_updated_unix.retain(|k, _| k.host_id != host_id);
                    self.block_osc94.retain(|k, _| k.host_id != host_id);
                    self.other_app_attached_sessions.remove(&host_id);

                    self.settings.saved_hosts.retain(|h| h.id != host_id);
                    self.save_settings_best_effort();
                    self.send_command(HostCommand::RemoveHost { host_id });

                    if self.workspace.active_session.is_none()
                        && let Some(fallback_sk) =
                            self.workspace.host_order.iter().find_map(|hid| {
                                let host = self.workspace.host(hid)?;
                                let sid = host.session_order.first()?;
                                Some(SessionKey::new(hid.clone(), sid.clone()))
                            })
                    {
                        self.activate_session(fallback_sk, &HashMap::new());
                        self.force_immediate_resize = true;
                    }
                }
            }

            UiAction::FocusPane { block_id } => {
                if let Some(sk) = self.workspace.active_session.clone() {
                    let key =
                        BlockKey::new(sk.host_id.clone(), sk.session_id.clone(), block_id.clone());
                    self.ui.focus.focus_terminal(Some(block_id.clone()));
                    self.send_command(HostCommand::FocusBlock {
                        host_id: sk.host_id,
                        session_id: sk.session_id,
                        block_id,
                    });
                    self.maybe_claim_ownership(&key, OwnershipTrigger::FocusPane);
                }
            }

            UiAction::PaneButton { block_id, button } => {
                self.handle_pane_header_button(&block_id, button);
            }

            UiAction::UnzoomPane { block_id } => {
                if let Some(sk) = self.workspace.active_session.clone() {
                    self.send_command(HostCommand::ZoomBlock {
                        host_id: sk.host_id,
                        request: SessionZoomBlockRequest {
                            session_id: sk.session_id,
                            block_id: Some(block_id),
                            window_id: None,
                            zoom: Some(false),
                        },
                    });
                }
            }

            UiAction::RestoreMinimizedFloating { block_id } => {
                if let Some((sk, layer_id)) = self.find_layer_for_block(&block_id) {
                    self.send_command(HostCommand::SetLayerState {
                        host_id: sk.host_id,
                        request: SessionSetLayerStateRequest {
                            session_id: sk.session_id,
                            layer_id,
                            state: "normal".to_owned(),
                            focus: Some(true),
                        },
                    });
                }
            }

            UiAction::CloseMinimizedFloating { block_id } => {
                if let Some(sk) = self.workspace.active_session.clone() {
                    self.send_command(HostCommand::CloseBlock {
                        host_id: sk.host_id,
                        session_id: sk.session_id,
                        block_id,
                        undoable: true,
                    });
                }
            }

            UiAction::BeginSidebarResize => {
                self.active_drag = Some(ActiveDrag::SidebarResize {
                    on_right: self.ui.layout_cfg.toolbar_position == ToolbarPosition::Right,
                });
            }

            UiAction::BeginSplitDividerDrag { split_path } => {
                if let Some(sk) = self.workspace.active_session.clone()
                    && let Some(out) = &self.last_ui_output
                    && let Some(div) = out
                        .dividers
                        .iter()
                        .find(|d| d.split_path == split_path)
                        .cloned()
                {
                    let split_target = self.resolve_split_target_at_path(&sk, &split_path);
                    self.active_drag = Some(ActiveDrag::SplitDivider {
                        host_id: sk.host_id,
                        session_id: sk.session_id,
                        divider: div,
                        split_target,
                    });
                }
            }

            UiAction::BeginFloatingDrag { block_id, zone } => {
                if let Some((sk, layer_id)) = self.find_layer_for_block(&block_id)
                    && let Some(bounds) = self.find_layer_bounds(&sk, &layer_id)
                    && let Some(out) = &self.last_ui_output
                {
                    self.active_drag = Some(ActiveDrag::FloatingLayer {
                        host_id: sk.host_id,
                        session_id: sk.session_id,
                        layer_id,
                        zone,
                        start_px: self.pointer_pos,
                        start_bounds: bounds,
                        workspace: out.chrome.workspace,
                    });
                }
            }

            UiAction::OpenModal(kind) => {
                self.open_modal_kind(kind);
            }

            UiAction::RunCommand { action_id } => match action_id.as_str() {
                "__viewer_open_ctx_target" => {
                    if let Some((key, target)) = self.context_menu_terminal_target.take() {
                        self.open_terminal_target(&key, &target, false, now);
                    }
                }
                "__viewer_open_ctx_external" => {
                    if let Some((key, target)) = self.context_menu_terminal_target.take() {
                        self.open_terminal_target(&key, &target, true, now);
                    }
                }
                "__viewer_copy_ctx_target" => {
                    if let Some((_, target)) = self.context_menu_terminal_target.take()
                        && let Some(text) = target.copyable_string()
                    {
                        if self.clipboard_enabled {
                            set_system_clipboard_text(&text);
                        }
                        self.push_toast_at(ToastSeverity::Info, "Copied to clipboard", text, now);
                    }
                }
                _ => self.dispatch_command_id(&action_id, None, now),
            },

            UiAction::ReplyRexPrompt(reply) => {
                let call_id = reply.call_id.clone();
                self.prompts
                    .on_prompt_ended(&tr3x_bridge::PromptEndedEvent {
                        call_id,
                        reason: "replied".to_owned(),
                    });
                let host_id = self
                    .active_prompt_host
                    .take()
                    .or_else(|| {
                        self.workspace
                            .active_session
                            .as_ref()
                            .map(|s| s.host_id.clone())
                    })
                    .unwrap_or_else(|| LOCAL_HOST_ID.to_owned());
                let proto_reply: ClientReplyRequest = reply_to_proto(reply);
                self.send_command(HostCommand::ClientReply {
                    host_id,
                    request: proto_reply,
                });
            }

            UiAction::SubmitRename {
                target_id,
                new_label,
            } => {
                let trimmed = new_label.trim().to_owned();
                if !trimmed.is_empty() {
                    if let Some(hid) = target_id.strip_prefix("__rename_host:") {
                        let (new_host_label, new_endpoint_opt) = match trimmed.split_once('=') {
                            Some((lhs, rhs))
                                if !lhs.trim().is_empty()
                                    && !lhs.contains("://")
                                    && !rhs.trim().is_empty() =>
                            {
                                (lhs.trim().to_owned(), Some(rhs.trim().to_owned()))
                            }
                            _ => (trimmed.clone(), None),
                        };
                        let mut reconnect_ep: Option<RexEndpoint> = None;
                        if let Some(ref raw_ep) = new_endpoint_opt {
                            match RexEndpoint::parse(raw_ep) {
                                Ok(parsed) => {
                                    reconnect_ep = Some(parsed);
                                }
                                Err(err) => {
                                    self.push_toast(
                                        ToastSeverity::Error,
                                        "Invalid host endpoint",
                                        err.to_string(),
                                    );
                                    return;
                                }
                            }
                        }
                        if let Some(host) = self.workspace.host_mut(hid) {
                            host.label = new_host_label.clone();
                            if let Some(ref ep) = reconnect_ep {
                                host.endpoint = ep.clone();
                                host.status = HostStatus::Reconnecting {
                                    reason: "endpoint updated".into(),
                                };
                            }
                        }
                        if let Some(saved) =
                            self.settings.saved_hosts.iter_mut().find(|h| h.id == hid)
                        {
                            saved.label = new_host_label.clone();
                            if let Some(raw_ep) = new_endpoint_opt {
                                saved.endpoint = raw_ep;
                            }
                            self.save_settings_best_effort();
                        }
                        if let Some(ep) = reconnect_ep {
                            self.send_command(HostCommand::RemoveHost {
                                host_id: hid.to_owned(),
                            });
                            self.send_command(HostCommand::AddHost {
                                host_id: hid.to_owned(),
                                label: new_host_label,
                                endpoint: ep,
                                preferred_session_id: None,
                            });
                        }
                    } else if let Some(rest) = target_id.strip_prefix("__rename_window:") {
                        let mut parts = rest.splitn(3, '|');
                        if let (Some(h), Some(s), Some(w)) =
                            (parts.next(), parts.next(), parts.next())
                        {
                            self.send_command(HostCommand::RenameWindow {
                                host_id: h.to_owned(),
                                session_id: s.to_owned(),
                                window_id: w.to_owned(),
                                label: trimmed,
                            });
                        }
                    } else if let Some(rest) = target_id.strip_prefix("__rename_block:") {
                        let mut parts = rest.splitn(3, '|');
                        if let (Some(h), Some(s), Some(b)) =
                            (parts.next(), parts.next(), parts.next())
                        {
                            self.send_command(HostCommand::RenameBlock {
                                host_id: h.to_owned(),
                                session_id: s.to_owned(),
                                block_id: b.to_owned(),
                                label: trimmed,
                            });
                        }
                    } else if let Some(rest) = target_id.strip_prefix("__rename_session:")
                        && let Some((h, s)) = rest.split_once('|')
                    {
                        self.send_command(HostCommand::RenameSession {
                            host_id: h.to_owned(),
                            session_id: s.to_owned(),
                            label: trimmed,
                        });
                    } else if let Some(sk) = self.workspace.active_session.clone() {
                        if let Some(win_id) = target_id.strip_prefix("window:") {
                            self.send_command(HostCommand::RenameWindow {
                                host_id: sk.host_id,
                                session_id: sk.session_id,
                                window_id: format!("window:{win_id}"),
                                label: trimmed,
                            });
                        } else if target_id.starts_with("block:") {
                            self.send_command(HostCommand::RenameBlock {
                                host_id: sk.host_id,
                                session_id: sk.session_id,
                                block_id: target_id,
                                label: trimmed,
                            });
                        } else if target_id.starts_with("session:") {
                            self.send_command(HostCommand::RenameSession {
                                host_id: sk.host_id,
                                session_id: target_id,
                                label: trimmed,
                            });
                        } else {
                            self.send_command(HostCommand::RenameSession {
                                host_id: sk.host_id,
                                session_id: sk.session_id,
                                label: trimmed,
                            });
                        }
                    }
                }
            }

            UiAction::SubmitAddHost { endpoint } => {
                let raw = endpoint.trim();
                if !raw.is_empty() {
                    let (explicit_label, raw_ep) = match raw.split_once('=') {
                        Some((lhs, rhs))
                            if !lhs.trim().is_empty()
                                && !lhs.contains("://")
                                && !rhs.trim().is_empty() =>
                        {
                            (Some(lhs.trim().to_owned()), rhs.trim())
                        }
                        _ => (None, raw),
                    };
                    match RexEndpoint::parse(raw_ep) {
                        Ok(parsed) => {
                            let host_id = format!("remote:{}", sanitize_host_id(raw_ep));
                            let label = explicit_label.unwrap_or_else(|| derive_host_label(raw_ep));
                            if let Some(existing) = self
                                .settings
                                .saved_hosts
                                .iter_mut()
                                .find(|h| h.id == host_id || h.endpoint == raw_ep)
                            {
                                existing.label = label.clone();
                                existing.endpoint = raw_ep.to_owned();
                                existing.auto_connect = true;
                            } else {
                                self.settings.saved_hosts.push(SavedHost {
                                    id: host_id.clone(),
                                    label: label.clone(),
                                    endpoint: raw_ep.to_owned(),
                                    auto_connect: true,
                                });
                            }
                            self.save_settings_best_effort();

                            let mut entry =
                                HostEntry::new(host_id.clone(), label.clone(), parsed.clone());
                            entry.is_local = false;
                            entry.status = HostStatus::Connecting;
                            if let Some(existing) = self.workspace.host_mut(&host_id) {
                                existing.label = label.clone();
                                existing.endpoint = parsed.clone();
                                existing.is_local = false;
                                existing.status = HostStatus::Connecting;
                            } else {
                                self.workspace.add_host(entry);
                            }

                            self.send_command(HostCommand::AddHost {
                                host_id,
                                label,
                                endpoint: parsed,
                                preferred_session_id: None,
                            });
                        }
                        Err(err) => {
                            self.push_toast(
                                ToastSeverity::Error,
                                "Invalid host endpoint",
                                err.to_string(),
                            );
                        }
                    }
                }
            }

            UiAction::UpdateSetting(change) => {
                self.apply_setting_change(change);
            }

            UiAction::FindQueryChanged(query) => {
                if let Some(key) = self.focused_block_key()
                    && let Ok(summary) = self.pane_store.search_query(&key, &query)
                {
                    self.ui.find_bar.total_matches = summary.total_matches;
                    self.ui.find_bar.current_match = summary.selected_index.map_or(0, |i| i + 1);
                }
            }

            UiAction::FindNext => {
                if matches!(
                    self.ui.focus.current(),
                    FocusTarget::Viewer | FocusTarget::ViewerSearch
                ) && self.ui.viewer.search.open
                {
                    self.ui.viewer.next_search_match();
                } else if let Some(key) = self.focused_block_key() {
                    let _ = self.pane_store.search_next(&key);
                }
            }

            UiAction::FindPrev => {
                if matches!(
                    self.ui.focus.current(),
                    FocusTarget::Viewer | FocusTarget::ViewerSearch
                ) && self.ui.viewer.search.open
                {
                    self.ui.viewer.prev_search_match();
                } else if let Some(key) = self.focused_block_key() {
                    let _ = self.pane_store.search_prev(&key);
                }
            }

            UiAction::FindToggleCase | UiAction::FindToggleRegex => {
                let query = self.ui.find_bar.input.text().to_owned();
                if !query.is_empty()
                    && let Some(key) = self.focused_block_key()
                    && let Ok(summary) = self.pane_store.search_query(&key, &query)
                {
                    self.ui.find_bar.total_matches = summary.total_matches;
                    self.ui.find_bar.current_match = summary.selected_index.map_or(0, |i| i + 1);
                }
            }

            UiAction::CloseFindBar => {
                if let Some(key) = self.focused_block_key() {
                    self.pane_store.search_close(&key);
                }
            }

            UiAction::ActivateToast { id } | UiAction::DismissToast { id } => {
                self.ui.toasts.retain(|t| t.id != id);
                self.toast_deadlines.remove(&id);
            }

            UiAction::CopySelection { block_id } => {
                let target = block_id
                    .and_then(|bid| {
                        let sk = self.workspace.active_session.as_ref()?;
                        Some(BlockKey::new(&sk.host_id, &sk.session_id, bid))
                    })
                    .or_else(|| self.focused_block_key());
                if let Some(key) = target
                    && let Ok(Some(text)) = self.pane_store.copy_selection_text(&key, true, true)
                {
                    if !text.is_empty() {
                        self.trigger_copy_shine_for_block(&key, now);
                    }
                    if self.clipboard_enabled {
                        set_system_clipboard_text(&text);
                    }
                }
            }

            UiAction::PasteClipboard { block_id } => {
                self.handle_clipboard_paste_or_image(block_id, now);
            }

            UiAction::CloseOtherTabs {
                session_id,
                keep_window_id,
            } => {
                if let Some(sk) = self.resolve_session_key(Some(&session_id)) {
                    let to_close: Vec<String> = self
                        .workspace
                        .session(&sk)
                        .and_then(|s| s.view.as_ref())
                        .map(|v| {
                            v.windows
                                .iter()
                                .filter_map(|w| w.window_id.clone())
                                .filter(|id| *id != keep_window_id)
                                .collect()
                        })
                        .unwrap_or_default();
                    for window_id in to_close {
                        self.send_command(HostCommand::CloseWindow {
                            host_id: sk.host_id.clone(),
                            session_id: sk.session_id.clone(),
                            window_id,
                            undoable: true,
                        });
                    }
                }
            }

            UiAction::MarkTabUnread { window_id } => {
                if let Some(sk) = self.workspace.active_session.clone() {
                    let tuple = (sk.host_id, sk.session_id, window_id);
                    if !self.unread_windows.remove(&tuple) {
                        self.unread_windows.insert(tuple);
                    }
                }
            }

            UiAction::MoveTabToNewWindow { window_id } => {
                let target =
                    self.workspace
                        .active_session
                        .clone()
                        .filter(|sk| {
                            self.workspace
                                .session(sk)
                                .and_then(|s| s.view.as_ref())
                                .is_some_and(|v| {
                                    v.windows
                                        .iter()
                                        .any(|w| w.window_id.as_deref() == Some(window_id.as_str()))
                                })
                        })
                        .or_else(|| {
                            for hid in &self.workspace.host_order {
                                let Some(host) = self.workspace.host(hid) else {
                                    continue;
                                };
                                for sid in &host.session_order {
                                    let Some(sess) = host.sessions.get(sid) else {
                                        continue;
                                    };
                                    if let Some(view) = &sess.view
                                        && view.windows.iter().any(|w| {
                                            w.window_id.as_deref() == Some(window_id.as_str())
                                        })
                                    {
                                        return Some(SessionKey::new(hid.clone(), sid.clone()));
                                    }
                                }
                            }
                            None
                        });
                if let Some(sk) = target {
                    let block_ids: Vec<String> = self
                        .workspace
                        .session(&sk)
                        .and_then(|s| s.view.as_ref())
                        .and_then(|v| {
                            v.windows
                                .iter()
                                .find(|w| w.window_id.as_deref() == Some(window_id.as_str()))
                        })
                        .map(|w| {
                            w.layers
                                .iter()
                                .flat_map(|l| l.blocks.iter())
                                .filter_map(|b| b.block_id.clone())
                                .collect()
                        })
                        .unwrap_or_default();
                    self.send_command(HostCommand::TransferTabToNewWindow {
                        host_id: sk.host_id,
                        source_session_id: sk.session_id,
                        window_id,
                        block_ids,
                    });
                }
            }

            UiAction::CloseSession { session_id } => {
                if let Some(sk) = self.resolve_session_key(Some(&session_id)) {
                    self.set_session_collapsed(&sk.host_id, &sk.session_id, false);
                    self.send_command(HostCommand::DestroySession {
                        host_id: sk.host_id,
                        session_id: sk.session_id,
                    });
                }
            }

            UiAction::CollapseOtherSessions { keep_session_id } => {
                let target_host = self
                    .resolve_session_key(Some(&keep_session_id))
                    .map(|sk| sk.host_id)
                    .or_else(|| {
                        self.workspace
                            .active_session
                            .as_ref()
                            .map(|sk| sk.host_id.clone())
                    });
                if let Some(host_id) = target_host
                    && let Some(session_ids) = self
                        .workspace
                        .host(&host_id)
                        .map(|h| h.session_order.clone())
                {
                    for sid in session_ids {
                        let should_collapse = sid != keep_session_id;
                        self.set_session_collapsed(&host_id, &sid, should_collapse);
                    }
                }
            }

            UiAction::ScrollPaneToFraction { block_id, fraction } => {
                if let Some(sk) = self.workspace.active_session.clone() {
                    let key = BlockKey::new(sk.host_id, sk.session_id, block_id);
                    if let Some(sb) = self.pane_store.scrollbar(&key) {
                        let max_off = sb.total.saturating_sub(sb.len);
                        let row = ((max_off as f32) * fraction.clamp(0.0, 1.0)).round() as usize;
                        let _ = self
                            .pane_store
                            .scroll_viewport(&key, ScrollViewport::Row(row));
                    }
                }
            }

            UiAction::ScrollPaneToBottom { block_id } => {
                if let Some(sk) = self.workspace.active_session.clone() {
                    let key = BlockKey::new(sk.host_id, sk.session_id, block_id);
                    if self.pane_store.scroll_to_bottom(&key).is_ok() {
                        self.needs_redraw = true;
                    }
                }
            }

            UiAction::OpenPortAction { block_id, port } => {
                if let Some(sk) = self.workspace.active_session.clone() {
                    let key = BlockKey::new(sk.host_id, sk.session_id, block_id);
                    self.open_port_action(key, port);
                }
            }

            UiAction::FilePickerDescend { path } => {
                self.request_file_picker_dir(path);
            }

            UiAction::FilePickerParent => {
                if let Some(sess) = &self.file_picker_session {
                    let cur = Path::new(&sess.current_dir);
                    let parent = cur
                        .parent()
                        .map(|p| p.to_string_lossy().into_owned())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "/".to_owned());
                    if parent != sess.current_dir {
                        self.request_file_picker_dir(parent);
                    }
                }
            }

            UiAction::FilePickerAccept { path } => {
                if let Some(sess) = self.file_picker_session.take() {
                    let escaped = super::input::shell_escape_path(Path::new(&path));
                    match sess.purpose {
                        FilePickerPurpose::InsertPath => {
                            if !self.read_only {
                                self.paste_into_block(&sess.key, &escaped);
                            }
                        }
                        FilePickerPurpose::GoToDirectory => {
                            if !self.read_only {
                                self.send_command(HostCommand::TerminalWrite {
                                    host_id: sess.key.host_id,
                                    session_id: sess.key.session_id,
                                    block_id: sess.key.block_id,
                                    data: format!("cd {escaped}\n"),
                                });
                            }
                        }
                    }
                }
            }

            UiAction::ThemePickerPreview { name } => {
                self.preview_theme_by_name(&name);
            }

            UiAction::ThemePickerCommit { name } => {
                self.commit_theme_by_name(&name);
            }

            UiAction::ThemePickerCancel => {
                if let Some((prev_name, prev_palette)) = self.theme_preview_rollback.take() {
                    self.settings.theme_name = prev_name;
                    self.apply_theme_palette(prev_palette);
                }
                self.theme_preview_cache.clear();
            }

            UiAction::CopyDiagnosticsBundle => {
                let bundle = self.format_debug_bundle();
                if self.clipboard_enabled {
                    set_system_clipboard_text(&bundle);
                }
                self.push_toast_at(
                    ToastSeverity::Info,
                    "Copied debug bundle",
                    format!("{} bytes copied to clipboard", bundle.len()),
                    now,
                );
            }

            UiAction::Viewer(viewer_action) => {
                self.dispatch_viewer_action(viewer_action, now);
            }
        }
    }

    pub(super) fn dispatch_viewer_action(&mut self, action: ViewerAction, now: Instant) {
        match action {
            ViewerAction::Focus => {
                self.ui.focus_viewer();
            }
            ViewerAction::BeginDividerDrag => {
                if let Some(workspace) = self.last_ui_output.as_ref().map(|o| o.chrome.workspace) {
                    self.active_drag = Some(ActiveDrag::ViewerDivider { workspace });
                }
            }
            ViewerAction::SelectTab { tab_id } => {
                self.ui.viewer.select_tab_by_id(tab_id);
                self.ui.focus_viewer();
            }
            ViewerAction::CloseTab { tab_id } => {
                self.media_backend.close_tab(tab_id);
                self.release_viewer_tab_textures(tab_id);
                self.pending_tab_jump_lines.remove(&tab_id);
                self.ui.close_viewer_tab(tab_id);
                if !self.ui.viewer.open {
                    if matches!(
                        self.ui.focus.current(),
                        FocusTarget::Viewer | FocusTarget::ViewerSearch
                    ) {
                        self.ui.focus.restore_terminal();
                    }
                    self.force_immediate_resize = true;
                }
            }
            ViewerAction::CopyPath { path } => {
                if self.clipboard_enabled {
                    set_system_clipboard_text(&path);
                }
                if let Some(bc_rect) = self
                    .last_ui_output
                    .as_ref()
                    .and_then(|o| o.viewer_metrics.breadcrumb_path_rect)
                {
                    let clip = rectf_to_gpu(bc_rect.inset(-4.0, -4.0, -4.0, -4.0)).to_clip_rect();
                    self.trigger_copy_shine_at_layer(
                        vec![rectf_to_gpu(bc_rect)],
                        clip,
                        layers::MODAL - 1,
                        now,
                    );
                }
                self.push_toast_at(ToastSeverity::Info, "Copied path", path, now);
            }
            ViewerAction::CopyPathLine { path, line } => {
                let path_line = format!("{path}:{line}");
                if self.clipboard_enabled {
                    set_system_clipboard_text(&path_line);
                }
                if let Some(bc_rect) = self
                    .last_ui_output
                    .as_ref()
                    .and_then(|o| o.viewer_metrics.breadcrumb_path_rect)
                {
                    let clip = rectf_to_gpu(bc_rect.inset(-4.0, -4.0, -4.0, -4.0)).to_clip_rect();
                    self.trigger_copy_shine_at_layer(
                        vec![rectf_to_gpu(bc_rect)],
                        clip,
                        layers::MODAL - 1,
                        now,
                    );
                }
                self.push_toast_at(ToastSeverity::Info, "Copied path:line", path_line, now);
            }
            ViewerAction::OpenExternal { path, remote_host } => {
                if let Some(host_ref) = remote_host {
                    let resolved_host_id = self.resolve_remote_viewer_host_id(&host_ref);
                    let cached_path = self
                        .downloaded_file_cache
                        .get(&(resolved_host_id.clone(), path.clone()))
                        .or_else(|| {
                            self.downloaded_file_cache
                                .get(&(host_ref.clone(), path.clone()))
                        })
                        .map(|(p, _)| p)
                        .filter(|p| p.exists())
                        .cloned();
                    if let Some(cached) = cached_path {
                        if let Err(err) = open::that_detached(&cached) {
                            self.push_toast_at(
                                ToastSeverity::Warning,
                                "Open external failed",
                                format!("{} ({err})", cached.display()),
                                now,
                            );
                        }
                    } else if let Some(key) = self.focused_block_key().map(|k| {
                        if k.host_id == resolved_host_id {
                            k
                        } else {
                            BlockKey::new(resolved_host_id.clone(), k.session_id, k.block_id)
                        }
                    }) {
                        self.download_remote_file_for_external(&key, &path);
                    } else {
                        self.push_toast_at(
                            ToastSeverity::Warning,
                            "Cannot open remote file externally",
                            format!("No active session for {host_ref}:{path}"),
                            now,
                        );
                    }
                } else if let Err(err) = open::that_detached(&path) {
                    self.push_toast_at(
                        ToastSeverity::Warning,
                        "Open external failed",
                        format!("{path} ({err})"),
                        now,
                    );
                }
            }
            ViewerAction::ToggleMaximize => {
                self.ui.viewer.toggle_maximized();
                self.force_immediate_resize = true;
            }
            ViewerAction::ClosePane => {
                for tab in &mut self.ui.viewer.tabs {
                    if let ViewerContent::Video(vid) = &mut tab.content
                        && vid.playing
                    {
                        vid.playing = false;
                        self.media_backend.video_pause(tab.id);
                    } else if let ViewerContent::Audio(aud) = &mut tab.content
                        && aud.playing
                    {
                        aud.playing = false;
                        self.media_backend.audio_pause(tab.id);
                    }
                }
                self.ui.close_viewer();
                self.force_immediate_resize = true;
            }
            ViewerAction::ToggleWrap => {
                self.ui.viewer.toggle_wrap();
            }
            ViewerAction::ToggleRawMarkdown => {
                self.ui.viewer.toggle_raw_markdown();
            }
            ViewerAction::ToggleDiff => {
                self.ui.viewer.toggle_diff_mode();
                self.ui.focus_viewer();
            }
            ViewerAction::OpenLink { url, base_path } => {
                if url.starts_with("http://") || url.starts_with("https://") {
                    if let Err(err) = open::that_detached(&url) {
                        self.push_toast_at(
                            ToastSeverity::Warning,
                            "Open link failed",
                            format!("{url} ({err})"),
                            now,
                        );
                    }
                } else {
                    let (raw_rel, frag_line) = if let Some((p, frag)) = url.split_once("#L") {
                        let line_num = frag.split('-').next().and_then(|s| s.parse::<u32>().ok());
                        (p.to_owned(), line_num)
                    } else {
                        let parsed = parse_path_with_location(&url);
                        (parsed.path, parsed.line)
                    };
                    let candidate = Path::new(&raw_rel);
                    let resolved = if candidate.is_absolute() {
                        candidate.to_path_buf()
                    } else {
                        Path::new(&base_path)
                            .parent()
                            .unwrap_or_else(|| Path::new("."))
                            .join(candidate)
                    };
                    if resolved.exists() {
                        let kind = ui_chrome::classify_extension(&resolved.to_string_lossy());
                        let display = resolved.display().to_string();
                        self.open_local_path_in_viewer(
                            resolved,
                            display,
                            frag_line,
                            None,
                            Some(kind),
                        );
                    } else {
                        self.push_toast_at(
                            ToastSeverity::Warning,
                            "Link target not found",
                            url,
                            now,
                        );
                    }
                }
            }
            ViewerAction::SearchOpen => {
                self.ui.viewer.open_search();
                self.ui.focus.focus_viewer_search();
            }
            ViewerAction::SearchPrev => {
                self.ui.viewer.prev_search_match();
            }
            ViewerAction::SearchNext => {
                self.ui.viewer.next_search_match();
            }
            ViewerAction::SearchToggleCase => {
                self.ui.viewer.toggle_search_case();
            }
            ViewerAction::SearchClose => {
                self.ui.viewer.close_search();
                self.ui.focus_viewer();
            }
            ViewerAction::ZoomIn => {
                self.ui.viewer.zoom_in();
            }
            ViewerAction::ZoomOut => {
                self.ui.viewer.zoom_out();
            }
            ViewerAction::ZoomReset => {
                self.ui.viewer.zoom_reset();
            }
            ViewerAction::ZoomFit => {
                self.ui.viewer.zoom_fit();
            }
            ViewerAction::BeginPan => {
                self.ui.focus_viewer();
                self.active_drag = Some(ActiveDrag::ViewerPan {
                    last_px: self.pointer_pos,
                });
            }
            ViewerAction::ToggleVideoPlayPause { tab_id } => {
                self.ui.focus_viewer();
                let toggled_backend = self.media_backend.video_toggle_play(tab_id);
                if let Some(state) = self.media_backend.video_state(tab_id) {
                    if let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                        && let ViewerContent::Video(vid) = &mut tab.content
                    {
                        vid.playing = state.playing;
                        vid.position_secs = state.position_secs;
                        vid.muted = state.muted;
                        vid.looping = state.looping;
                    }
                } else if !toggled_backend
                    && let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                    && let ViewerContent::Video(vid) = &mut tab.content
                {
                    vid.playing = !vid.playing;
                }
            }
            ViewerAction::SeekVideo { tab_id, fraction } => {
                self.ui.focus_viewer();
                if let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                    && let ViewerContent::Video(vid) = &mut tab.content
                {
                    let secs = vid.seek_to_fraction(fraction);
                    self.media_backend.video_seek(tab_id, secs);
                }
                if let Some(bar_rect) = self
                    .last_ui_output
                    .as_ref()
                    .and_then(|o| o.viewer_metrics.seek_bar_rect)
                {
                    self.active_drag = Some(ActiveDrag::ViewerVideoSeek { tab_id, bar_rect });
                }
            }
            ViewerAction::ToggleVideoMute { tab_id } => {
                if let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                    && let ViewerContent::Video(vid) = &mut tab.content
                {
                    vid.muted = !vid.muted;
                    self.media_backend.video_set_muted(tab_id, vid.muted);
                }
            }
            ViewerAction::ToggleVideoLoop { tab_id } => {
                if let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                    && let ViewerContent::Video(vid) = &mut tab.content
                {
                    vid.looping = !vid.looping;
                    self.media_backend.video_set_loop(tab_id, vid.looping);
                }
            }
            ViewerAction::ToggleAudioPlayPause { tab_id } => {
                self.ui.focus_viewer();
                let toggled_backend = self.media_backend.audio_toggle_play(tab_id);
                if let Some(state) = self.media_backend.audio_state(tab_id) {
                    if let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                        && let ViewerContent::Audio(aud) = &mut tab.content
                    {
                        aud.playing = state.playing;
                        aud.position_secs = state.position_secs;
                        aud.muted = state.muted;
                        aud.looping = state.looping;
                    }
                } else if !toggled_backend
                    && let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                    && let ViewerContent::Audio(aud) = &mut tab.content
                {
                    aud.playing = !aud.playing;
                }
            }
            ViewerAction::SeekAudio { tab_id, fraction } => {
                self.ui.focus_viewer();
                if let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                    && let ViewerContent::Audio(aud) = &mut tab.content
                {
                    let secs = aud.seek_to_fraction(fraction);
                    self.media_backend.audio_seek(tab_id, secs);
                }
                if let Some(bar_rect) = self
                    .last_ui_output
                    .as_ref()
                    .and_then(|o| o.viewer_metrics.seek_bar_rect)
                {
                    self.active_drag = Some(ActiveDrag::ViewerAudioSeek { tab_id, bar_rect });
                }
            }
            ViewerAction::ToggleAudioMute { tab_id } => {
                if let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                    && let ViewerContent::Audio(aud) = &mut tab.content
                {
                    aud.muted = !aud.muted;
                    self.media_backend.audio_set_muted(tab_id, aud.muted);
                }
            }
            ViewerAction::ToggleAudioLoop { tab_id } => {
                if let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                    && let ViewerContent::Audio(aud) = &mut tab.content
                {
                    aud.looping = !aud.looping;
                    self.media_backend.audio_set_loop(tab_id, aud.looping);
                }
            }
            ViewerAction::PdfPrevPage { tab_id } => {
                self.ui.focus_viewer();
                if let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                    && let ViewerContent::Pdf(pdf) = &mut tab.content
                    && pdf.current_page > 1
                {
                    let target = pdf.current_page - 1;
                    pdf.loading_page = true;
                    if !self.media_backend.pdf_goto_page(tab_id, target) {
                        pdf.current_page = target;
                        pdf.loading_page = false;
                    }
                }
            }
            ViewerAction::PdfNextPage { tab_id } => {
                self.ui.focus_viewer();
                if let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                    && let ViewerContent::Pdf(pdf) = &mut tab.content
                    && pdf.current_page < pdf.page_count
                {
                    let target = pdf.current_page + 1;
                    pdf.loading_page = true;
                    if !self.media_backend.pdf_goto_page(tab_id, target) {
                        pdf.current_page = target;
                        pdf.loading_page = false;
                    }
                }
            }
            ViewerAction::ScrollToFraction { tab_id, fraction } => {
                self.ui.focus_viewer();
                if let Some(tab) = self.ui.viewer.tab_by_id_mut(tab_id)
                    && let ViewerContent::Text(txt) = &mut tab.content
                {
                    txt.scroll_to_fraction(fraction);
                }
                if let Some(track_rect) = self.last_ui_output.as_ref().and_then(|out| {
                    out.draw_list
                        .hits
                        .iter()
                        .find(|h| matches!(h.target, HitTarget::ViewerScrollbarTrack))
                        .map(|h| h.rect)
                }) {
                    self.active_drag = Some(ActiveDrag::ViewerScrollbar { track_rect });
                }
            }
        }
    }

    fn handle_pane_header_button(&mut self, block_id: &str, button: PaneHeaderButton) {
        let Some(sk) = self.workspace.active_session.clone() else {
            return;
        };
        let anchor_key = BlockKey::new(
            sk.host_id.clone(),
            sk.session_id.clone(),
            block_id.to_owned(),
        );
        match button {
            PaneHeaderButton::SplitRight => {
                let cwd = self.resolve_shell_cwd_for_host(&sk.host_id, Some(&anchor_key), true);
                let layout = self.build_shell_layout_spec_with_cwd(cwd.as_deref(), None, false);
                self.send_command(HostCommand::SplitPane {
                    host_id: sk.host_id,
                    request: SessionNewSplitRequest {
                        session_id: sk.session_id,
                        anchor_id: Some(block_id.to_owned()),
                        window_id: None,
                        direction: "horizontal".to_owned(),
                        side: "after".to_owned(),
                        ratio: None,
                        layout,
                        focus: Some(true),
                    },
                });
            }
            PaneHeaderButton::SplitDown => {
                let cwd = self.resolve_shell_cwd_for_host(&sk.host_id, Some(&anchor_key), true);
                let layout = self.build_shell_layout_spec_with_cwd(cwd.as_deref(), None, false);
                self.send_command(HostCommand::SplitPane {
                    host_id: sk.host_id,
                    request: SessionNewSplitRequest {
                        session_id: sk.session_id,
                        anchor_id: Some(block_id.to_owned()),
                        window_id: None,
                        direction: "vertical".to_owned(),
                        side: "after".to_owned(),
                        ratio: None,
                        layout,
                        focus: Some(true),
                    },
                });
            }
            PaneHeaderButton::FloatOrDock => {
                let is_floating = self.is_block_in_floating_layer(&sk, block_id);
                if is_floating {
                    if let Some(tiled_anchor) = self.find_tiled_anchor_block(&sk, block_id) {
                        self.send_command(HostCommand::MoveBlock {
                            host_id: sk.host_id,
                            session_id: sk.session_id,
                            block_id: block_id.to_owned(),
                            anchor_block_id: tiled_anchor,
                            direction: "horizontal".to_owned(),
                            side: "after".to_owned(),
                            ratio: Some(0.5),
                        });
                    } else {
                        let (window_label, before_window_id) = self
                            .workspace
                            .session(&sk)
                            .and_then(|s| s.view.as_ref())
                            .map(|v| {
                                let active_idx = v
                                    .active_window_id
                                    .as_ref()
                                    .and_then(|aid| {
                                        v.windows.iter().position(|w| {
                                            w.window_id.as_deref() == Some(aid.as_str())
                                        })
                                    })
                                    .unwrap_or(0);
                                let label = v
                                    .windows
                                    .get(active_idx)
                                    .map(|w| w.label.trim().to_owned())
                                    .filter(|l| !l.is_empty());
                                let before = v
                                    .windows
                                    .get(active_idx + 1)
                                    .and_then(|w| w.window_id.clone());
                                (label, before)
                            })
                            .unwrap_or((None, None));
                        self.send_command(HostCommand::DockFloatingBlock {
                            host_id: sk.host_id,
                            session_id: sk.session_id,
                            block_id: block_id.to_owned(),
                            window_label,
                            before_window_id,
                        });
                    }
                } else {
                    self.send_command(HostCommand::FloatBlock {
                        host_id: sk.host_id,
                        request: SessionFloatBlockRequest {
                            session_id: sk.session_id,
                            block_id: block_id.to_owned(),
                            bounds: [0.15, 0.15, 0.70, 0.70],
                        },
                    });
                }
            }
            PaneHeaderButton::Zoom => {
                self.send_command(HostCommand::ZoomBlock {
                    host_id: sk.host_id,
                    request: SessionZoomBlockRequest {
                        session_id: sk.session_id,
                        block_id: Some(block_id.to_owned()),
                        window_id: None,
                        zoom: None,
                    },
                });
            }
            PaneHeaderButton::Close => {
                self.send_command(HostCommand::CloseBlock {
                    host_id: sk.host_id,
                    session_id: sk.session_id,
                    block_id: block_id.to_owned(),
                    undoable: true,
                });
            }
        }
    }

    fn apply_setting_change(&mut self, change: SettingChange) {
        match change {
            SettingChange::ToolbarPosition(pos) => {
                self.ui.layout_cfg.toolbar_position = pos;
                self.settings.sidebar_position = match pos {
                    ToolbarPosition::Left => SidebarPosition::Left,
                    ToolbarPosition::Right => SidebarPosition::Right,
                    ToolbarPosition::Top => SidebarPosition::Top,
                    ToolbarPosition::Bottom => SidebarPosition::Bottom,
                };
                self.force_immediate_resize = true;
            }
            SettingChange::ToolbarBehavior(beh) => {
                self.ui.layout_cfg.toolbar_behavior = beh;
                self.settings.sidebar_behavior = match beh {
                    ToolbarBehavior::Always => SidebarBehavior::Always,
                    ToolbarBehavior::OnHover => SidebarBehavior::OnHover,
                    ToolbarBehavior::Hidden => SidebarBehavior::Hidden,
                };
                self.force_immediate_resize = true;
            }
            SettingChange::PaneDensity(den) => {
                self.ui.layout_cfg.density = den;
                self.settings.density = match den {
                    PaneDensity::Comfortable => LayoutDensity::Comfortable,
                    PaneDensity::Compact => LayoutDensity::Compact,
                };
                self.force_immediate_resize = true;
            }
            SettingChange::PaneHeaderMode(mode) => {
                self.ui.layout_cfg.pane_header_mode = mode;
                self.settings.pane_header_mode = match mode {
                    PaneHeaderMode::Always => SavedPaneHeaderMode::Always,
                    PaneHeaderMode::OnHover => SavedPaneHeaderMode::OnHover,
                    PaneHeaderMode::Hidden => SavedPaneHeaderMode::Hidden,
                };
                self.force_immediate_resize = true;
            }
            SettingChange::ToggleTranslucentMaterial => {
                self.settings.material = match self.settings.material {
                    WindowMaterial::Classic => WindowMaterial::Translucent { alpha: 0.90 },
                    WindowMaterial::Translucent { .. } => WindowMaterial::Classic,
                };
                self.theme.material = self.settings.material.to_theme_material();
            }
            SettingChange::ToggleActivePaneBorder => {
                self.ui.pane_options.highlight_active_border =
                    !self.ui.pane_options.highlight_active_border;
                self.settings.highlight_active_border =
                    self.ui.pane_options.highlight_active_border;
            }
            SettingChange::ToggleFadeInactivePanes => {
                self.ui.pane_options.fade_inactive_panes =
                    !self.ui.pane_options.fade_inactive_panes;
                self.settings.fade_inactive_panes = self.ui.pane_options.fade_inactive_panes;
            }
            SettingChange::ToggleAdaptiveRounding => {
                self.settings.adaptive_rounding = !self.settings.adaptive_rounding;
                self.ui.layout_cfg.hyprland_rounding = if self.settings.adaptive_rounding {
                    Some(self.theme.rounding)
                } else {
                    None
                };
            }
            SettingChange::ToggleProgramColorHeader => {
                self.ui.pane_options.program_color_header =
                    !self.ui.pane_options.program_color_header;
                self.settings.program_color_header = self.ui.pane_options.program_color_header;
            }
            SettingChange::ToggleChimeEnabled => {
                self.settings.chime_enabled = !self.settings.chime_enabled;
            }
            SettingChange::ToggleDesktopNotifications => {
                self.settings.desktop_notifications = !self.settings.desktop_notifications;
            }
            SettingChange::FontSizePt(pt) => {
                self.settings.font_size_pt = pt;
                self.settings.sanitize();
                self.force_immediate_resize = true;
            }
            SettingChange::OpenThemePicker => {
                self.open_modal_kind(ModalKind::ThemePicker);
                return;
            }
        }

        self.mark_shared_settings_changed(true);
        self.save_settings_best_effort();
    }

    pub(super) fn save_settings_best_effort(&self) {
        if let Err(err) = self.settings.save_to(&self.settings_path) {
            tracing::warn!(path = %self.settings_path.display(), error = %err, "failed to save settings");
        }
    }

    pub(super) fn open_modal_kind(&mut self, kind: ModalKind) {
        match kind {
            ModalKind::CommandPalette => {
                let items: Vec<PaletteItem> = self
                    .actions
                    .actions()
                    .iter()
                    .map(|a| {
                        let mut item = PaletteItem::new(a.name.clone(), a.title.clone());
                        item.category = (!a.category.is_empty()).then(|| a.category.clone());
                        item.keywords = (!a.keywords.is_empty()).then(|| a.keywords.clone());
                        item.shortcut = a.shortcut_hint.clone();
                        item.icon = Some(if a.is_tr3x {
                            IconId::T3
                        } else {
                            IconId::Terminal
                        });
                        item
                    })
                    .collect();
                self.ui.open_modal(ModalState::open_list(
                    ModalKind::CommandPalette,
                    "Command Palette",
                    "Type a command…",
                    Some("Ctrl+Shift+P".to_owned()),
                    items,
                ));
            }
            ModalKind::SwitchSession => {
                let mut items = Vec::new();
                for host_id in &self.workspace.host_order {
                    let Some(host) = self.workspace.host(host_id) else {
                        continue;
                    };
                    for sid in &host.session_order {
                        if let Some(sess) = host.sessions.get(sid) {
                            let win_count = sess.view.as_ref().map_or(0, |v| v.windows.len());
                            let mut item = PaletteItem::new(
                                format!("__select_session:{host_id}|{sid}"),
                                sess.label.clone(),
                            );
                            item.category = Some(host.label.clone());
                            item.detail = Some(format!("{win_count} tabs"));
                            item.icon = Some(if is_t3_session(&sess.label) {
                                IconId::T3
                            } else {
                                IconId::Server
                            });
                            items.push(item);
                        }
                    }
                }
                self.ui.open_modal(ModalState::open_list(
                    ModalKind::SwitchSession,
                    "Switch Session",
                    "Filter sessions…",
                    Some("Ctrl+Shift+K".to_owned()),
                    items,
                ));
            }
            ModalKind::GoToAnything | ModalKind::ActivityPicker => {
                let mut items = Vec::new();
                for host_id in &self.workspace.host_order {
                    let Some(host) = self.workspace.host(host_id) else {
                        continue;
                    };
                    for sid in &host.session_order {
                        let Some(sess) = host.sessions.get(sid) else {
                            continue;
                        };
                        if let Some(view) = &sess.view {
                            for win in &view.windows {
                                if let Some(wid) = &win.window_id {
                                    let mut item = PaletteItem::new(
                                        format!("__select_tab:{host_id}|{sid}|{wid}"),
                                        if win.label.is_empty() {
                                            "Untitled Tab".to_owned()
                                        } else {
                                            win.label.clone()
                                        },
                                    );
                                    item.category = Some(sess.label.clone());
                                    item.subtitle = Some(host.label.clone());
                                    item.icon = Some(IconId::Terminal);
                                    items.push(item);
                                }
                            }
                        }
                    }
                }
                let title = if kind == ModalKind::ActivityPicker {
                    "Activity"
                } else {
                    "Go to Anything"
                };
                self.ui.open_modal(ModalState::open_list(
                    kind,
                    title,
                    "Jump to tab, session, or pane…",
                    Some("Ctrl+Alt+P".to_owned()),
                    items,
                ));
            }
            ModalKind::Settings => {
                let snap = self.settings_view_snapshot();
                self.ui.open_modal(ModalState::open_settings(snap));
            }
            ModalKind::Diagnostics => {
                let snap = self.build_diagnostics_snapshot();
                self.ui.open_modal(ModalState::open_diagnostics(snap));
            }
            ModalKind::ThemePicker => {
                self.open_theme_picker_modal();
            }
            ModalKind::FilePicker => {
                self.open_file_picker(FilePickerPurpose::InsertPath);
            }
            ModalKind::Rename => {
                let (target_id, title, current) = match self.pending_rename_target.take() {
                    Some(PendingRenameTarget::Host { host_id }) => {
                        let cur = self.host_display_label(&host_id);
                        (format!("__rename_host:{host_id}"), "Edit Host", cur)
                    }
                    Some(PendingRenameTarget::Window {
                        host_id,
                        session_id,
                        window_id,
                    }) => {
                        let cur = self
                            .workspace
                            .session(&SessionKey::new(&host_id, &session_id))
                            .and_then(|s| s.view.as_ref())
                            .and_then(|v| {
                                v.windows
                                    .iter()
                                    .find(|w| w.window_id.as_deref() == Some(window_id.as_str()))
                            })
                            .map(|w| w.label.clone())
                            .unwrap_or_default();
                        (
                            format!("__rename_window:{host_id}|{session_id}|{window_id}"),
                            "Rename Tab",
                            cur,
                        )
                    }
                    Some(PendingRenameTarget::Block {
                        host_id,
                        session_id,
                        block_id,
                    }) => {
                        let cur = self
                            .workspace
                            .session(&SessionKey::new(&host_id, &session_id))
                            .and_then(|s| s.view.as_ref())
                            .and_then(|v| {
                                v.windows
                                    .iter()
                                    .flat_map(|w| w.layers.iter())
                                    .flat_map(|l| l.blocks.iter())
                                    .find(|b| b.block_id.as_deref() == Some(block_id.as_str()))
                            })
                            .map(|b| b.label.clone())
                            .unwrap_or_default();
                        (
                            format!("__rename_block:{host_id}|{session_id}|{block_id}"),
                            "Rename Pane",
                            cur,
                        )
                    }
                    Some(PendingRenameTarget::Session {
                        host_id,
                        session_id,
                    }) => {
                        let cur = self
                            .workspace
                            .session(&SessionKey::new(&host_id, &session_id))
                            .map(|s| s.label.clone())
                            .unwrap_or_default();
                        (
                            format!("__rename_session:{host_id}|{session_id}"),
                            "Rename Session",
                            cur,
                        )
                    }
                    None => self
                        .workspace
                        .active_session
                        .as_ref()
                        .and_then(|sk| self.workspace.session(sk).map(|s| (sk, s)))
                        .map(|(sk, s)| {
                            (
                                format!("__rename_session:{}|{}", sk.host_id, sk.session_id),
                                "Rename Session",
                                s.label.clone(),
                            )
                        })
                        .unwrap_or_else(|| (String::new(), "Rename Session", String::new())),
                };
                self.ui
                    .open_modal(ModalState::open_rename(target_id, title, current));
            }
            ModalKind::AddHost => {
                self.ui.open_modal(ModalState::open_add_host());
            }
            ModalKind::ShortcutsHelp => {
                self.ui.open_modal(ModalState::open_shortcuts_help());
            }
            ModalKind::RexPrompt => {}
        }
    }

    pub(super) fn dispatch_command_id(
        &mut self,
        action_id: &str,
        args: Option<serde_json::Value>,
        now: Instant,
    ) {
        // Internal modal jump tokens
        if let Some(rest) = action_id.strip_prefix("__select_session:")
            && let Some((host_id, session_id)) = rest.split_once('|')
        {
            self.dispatch_ui_action(
                UiAction::SelectSession {
                    host_id: host_id.to_owned(),
                    session_id: session_id.to_owned(),
                },
                now,
            );
            return;
        }
        if let Some(rest) = action_id.strip_prefix("__select_tab:") {
            let mut parts = rest.splitn(3, '|');
            if let (Some(h), Some(s), Some(w)) = (parts.next(), parts.next(), parts.next()) {
                self.dispatch_ui_action(
                    UiAction::SelectTab {
                        host_id: h.to_owned(),
                        session_id: s.to_owned(),
                        window_id: w.to_owned(),
                    },
                    now,
                );
                return;
            }
        }
        if let Some(target_host_id) = action_id.strip_prefix("__new_session_on_host:") {
            self.dispatch_ui_action(
                UiAction::NewSession {
                    host_id: Some(target_host_id.to_owned()),
                },
                now,
            );
            return;
        }

        let host_id = self
            .workspace
            .active_session
            .as_ref()
            .map(|s| s.host_id.clone())
            .unwrap_or_else(|| LOCAL_HOST_ID.to_owned());

        // Check if registered as a server Lua/block action first.
        if let Some(entry) = self.actions.get(action_id)
            && entry.kind != ActionKind::Client
        {
            if action_id == "t3_fit" {
                self.fit_active_session_to_device();
            }
            self.send_server_action(&host_id, action_id, args);
            return;
        }

        let fallback_sid = self
            .workspace
            .active_session
            .as_ref()
            .map(|s| s.session_id.as_str());
        let fallback_bid = self.ui.focus.last_terminal_block();
        match parse_client_command(action_id, args.as_ref(), fallback_sid, fallback_bid) {
            Ok(cmd) => self.execute_client_command(&host_id, cmd, now),
            Err(_) => self.send_server_action(&host_id, action_id, args),
        }
    }

    fn send_server_action(&mut self, host_id: &str, action: &str, args: Option<serde_json::Value>) {
        self.send_server_action_for_session(host_id, None, action, args);
    }

    fn send_server_action_for_session(
        &mut self,
        host_id: &str,
        target_session_id: Option<&str>,
        action: &str,
        args: Option<serde_json::Value>,
    ) {
        let session_id = target_session_id.map(str::to_owned).or_else(|| {
            self.workspace
                .active_session
                .as_ref()
                .filter(|s| s.host_id == host_id)
                .map(|s| s.session_id.clone())
        });
        let block_id = self
            .focused_block_key()
            .filter(|k| {
                k.host_id == host_id && session_id.as_deref().is_none_or(|sid| k.session_id == sid)
            })
            .map(|k| k.block_id);
        self.send_command(HostCommand::ClientAction {
            host_id: host_id.to_owned(),
            request: ClientActionRequest {
                action: action.to_owned(),
                session_id,
                block_id,
                args,
                origin: None,
                servers: None,
                server: None,
            },
        });
    }

    fn fit_active_session_to_device(&mut self) {
        if self.read_only {
            return;
        }
        if let Some(sk) = self.workspace.active_session.clone() {
            tracing::debug!(target: "omarex::ownership", "fit_active_session_to_device: take all");
            self.send_command(HostCommand::OwnBlock {
                host_id: sk.host_id,
                session_id: sk.session_id,
                block_ids: None,
                mode: OwnMode::Take,
            });
            self.push_theme_to_owned_blocks();
        }
    }

    pub(super) fn execute_client_command(
        &mut self,
        host_id: &str,
        cmd: ClientCommand,
        now: Instant,
    ) {
        match cmd {
            ClientCommand::SelectSession {
                session_id,
                window_id,
                block_id,
            } => {
                if let Some(sid) = session_id {
                    let sk = SessionKey::new(host_id.to_owned(), sid.clone());
                    self.activate_session(sk, &HashMap::new());
                    if let Some(wid) = window_id {
                        self.dispatch_ui_action(
                            UiAction::SelectTab {
                                host_id: host_id.to_owned(),
                                session_id: sid.clone(),
                                window_id: wid,
                            },
                            now,
                        );
                    }
                    if let Some(bid) = block_id {
                        self.dispatch_ui_action(UiAction::FocusPane { block_id: bid }, now);
                    }
                } else if let Some(bid) = block_id {
                    self.dispatch_ui_action(UiAction::FocusPane { block_id: bid }, now);
                }
            }
            ClientCommand::Paste { block_id, .. } => {
                self.dispatch_ui_action(UiAction::PasteClipboard { block_id }, now);
            }
            ClientCommand::CopySelection => {
                self.dispatch_ui_action(UiAction::CopySelection { block_id: None }, now);
            }
            ClientCommand::TogglePalette => {
                if matches!(
                    self.ui.focus.current(),
                    FocusTarget::Modal(ModalKind::CommandPalette)
                ) {
                    self.ui.close_modal();
                } else {
                    self.open_modal_kind(ModalKind::CommandPalette);
                }
            }
            ClientCommand::OpenSessionSwitcher => self.open_modal_kind(ModalKind::SwitchSession),
            ClientCommand::OpenGoTo => self.open_modal_kind(ModalKind::GoToAnything),
            ClientCommand::OpenActivity => self.open_modal_kind(ModalKind::ActivityPicker),
            ClientCommand::OpenFind => {
                if matches!(
                    self.ui.focus.current(),
                    FocusTarget::Viewer | FocusTarget::ViewerSearch
                ) && self.ui.viewer.open
                {
                    self.ui.viewer.open_search();
                    self.ui.focus.focus_viewer_search();
                } else {
                    self.ui.open_find_bar();
                }
            }
            ClientCommand::FindNext => self.dispatch_ui_action(UiAction::FindNext, now),
            ClientCommand::FindPrevious => self.dispatch_ui_action(UiAction::FindPrev, now),
            ClientCommand::OpenSettings => self.open_modal_kind(ModalKind::Settings),
            ClientCommand::OpenDiagnostics => {
                self.open_modal_kind(ModalKind::Diagnostics);
            }
            ClientCommand::EnterCopyMode => {
                self.enter_copy_mode();
            }
            ClientCommand::InsertPath => {
                self.open_file_picker(FilePickerPurpose::InsertPath);
            }
            ClientCommand::GoToDirectory => {
                self.open_file_picker(FilePickerPurpose::GoToDirectory);
            }
            ClientCommand::ChangeTheme { name } => {
                if let Some(theme_name) = name {
                    self.commit_theme_by_name(&theme_name);
                } else {
                    self.open_theme_picker_modal();
                }
            }
            ClientCommand::ToggleSidebar => {
                self.ui.layout_cfg.sidebar_collapsed = !self.ui.layout_cfg.sidebar_collapsed;
                self.force_immediate_resize = true;
            }
            ClientCommand::ToggleTabStyle => {
                let next = match self.ui.layout_cfg.toolbar_position {
                    ToolbarPosition::Left => ToolbarPosition::Top,
                    ToolbarPosition::Top => ToolbarPosition::Right,
                    ToolbarPosition::Right => ToolbarPosition::Bottom,
                    ToolbarPosition::Bottom => ToolbarPosition::Left,
                };
                self.apply_setting_change(SettingChange::ToolbarPosition(next));
            }
            ClientCommand::FocusSidebarFilter => {
                self.ui.sidebar.open_filter();
                self.ui.focus.focus_sidebar_filter();
            }
            ClientCommand::RevealToolbar => {
                self.ui.layout_cfg.toolbar_revealed = !self.ui.layout_cfg.toolbar_revealed;
            }
            ClientCommand::RevealPaneHeader => {
                let next = match self.ui.layout_cfg.pane_header_mode {
                    PaneHeaderMode::Always => PaneHeaderMode::OnHover,
                    PaneHeaderMode::OnHover | PaneHeaderMode::Hidden => PaneHeaderMode::Always,
                };
                self.apply_setting_change(SettingChange::PaneHeaderMode(next));
            }
            ClientCommand::DismissNotice => {
                if let Some(dismissed) = self.ui.toasts.pop() {
                    self.toast_deadlines.remove(&dismissed.id);
                }
            }
            ClientCommand::RefreshSessions => {
                if let Some(sk) = self.workspace.active_session.clone() {
                    self.send_command(HostCommand::FetchView {
                        host_id: sk.host_id,
                        session_id: sk.session_id,
                    });
                }
            }
            ClientCommand::NavigateBack => {
                if self.nav_index > 0 {
                    self.nav_index -= 1;
                    if let Some((h, s, b)) = self.nav_history.get(self.nav_index).cloned() {
                        self.activate_session(SessionKey::new(h, s), &HashMap::new());
                        if let Some(bid) = b {
                            self.ui.focus.focus_terminal(Some(bid));
                        }
                    }
                }
            }
            ClientCommand::NavigateForward => {
                if self.nav_index + 1 < self.nav_history.len() {
                    self.nav_index += 1;
                    if let Some((h, s, b)) = self.nav_history.get(self.nav_index).cloned() {
                        self.activate_session(SessionKey::new(h, s), &HashMap::new());
                        if let Some(bid) = b {
                            self.ui.focus.focus_terminal(Some(bid));
                        }
                    }
                }
            }
            ClientCommand::NewSession { on_host } => {
                if on_host && self.workspace.host_order.len() > 1 {
                    let mut items = Vec::new();
                    for hid in &self.workspace.host_order {
                        let Some(host) = self.workspace.host(hid) else {
                            continue;
                        };
                        let mut item = PaletteItem::new(
                            format!("__new_session_on_host:{hid}"),
                            host.label.clone(),
                        );
                        item.category = Some(if host.is_local {
                            "Local Host".to_owned()
                        } else {
                            "Remote Host".to_owned()
                        });
                        item.detail = Some(match &host.status {
                            HostStatus::Connected { .. } => "Online".to_owned(),
                            HostStatus::Connecting => "Connecting".to_owned(),
                            HostStatus::Reconnecting { .. } => "Reconnecting".to_owned(),
                            HostStatus::Disconnected => "Offline".to_owned(),
                            HostStatus::Error(_) => "Error".to_owned(),
                        });
                        item.icon = Some(if host.is_local {
                            IconId::Terminal
                        } else {
                            IconId::Server
                        });
                        items.push(item);
                    }
                    self.ui.open_modal(ModalState::open_list(
                        ModalKind::CommandPalette,
                        "New Session on Host",
                        "Choose target host…",
                        None,
                        items,
                    ));
                } else {
                    self.dispatch_ui_action(
                        UiAction::NewSession {
                            host_id: Some(host_id.to_owned()),
                        },
                        now,
                    );
                }
            }
            ClientCommand::NewWindow => {
                self.open_new_os_window_session(Some(host_id.to_owned()));
            }
            ClientCommand::NewTab => {
                self.dispatch_ui_action(UiAction::NewTerminal, now);
            }
            ClientCommand::CloseCurrent | ClientCommand::ClosePane => {
                if let Some(key) = self.focused_block_key() {
                    self.send_command(HostCommand::CloseBlock {
                        host_id: key.host_id,
                        session_id: key.session_id,
                        block_id: key.block_id,
                        undoable: true,
                    });
                }
            }
            ClientCommand::CloseTab => {
                if let Some(sk) = self.workspace.active_session.clone()
                    && let Some(wid) = self
                        .workspace
                        .session(&sk)
                        .and_then(|s| s.view.as_ref())
                        .and_then(|v| v.active_window_id.clone())
                {
                    self.dispatch_ui_action(
                        UiAction::CloseTab {
                            host_id: sk.host_id,
                            session_id: sk.session_id,
                            window_id: wid,
                        },
                        now,
                    );
                }
            }
            ClientCommand::UndoClose => {
                if let Some(sk) = self.workspace.active_session.clone() {
                    self.send_command(HostCommand::UndoClose {
                        host_id: sk.host_id,
                        session_id: sk.session_id,
                    });
                }
            }
            ClientCommand::NextTab => self.cycle_active_tab(1, now),
            ClientCommand::PrevTab => self.cycle_active_tab(-1, now),
            ClientCommand::GotoTab { index } => self.goto_tab_index(index, now),
            ClientCommand::MoveTab { forward } => self.move_active_tab(forward),
            ClientCommand::MarkTabUnread => {
                if let Some(sk) = self.workspace.active_session.clone()
                    && let Some(wid) = self
                        .workspace
                        .session(&sk)
                        .and_then(|s| s.view.as_ref())
                        .and_then(|v| v.active_window_id.clone())
                {
                    self.dispatch_ui_action(UiAction::MarkTabUnread { window_id: wid }, now);
                }
            }
            ClientCommand::ToggleTabMute => {
                let mut block_keys = Vec::new();
                if let Some(sk) = self.workspace.active_session.clone()
                    && let Some(view) = self.workspace.session(&sk).and_then(|s| s.view.as_ref())
                    && let Some(win) = active_window_of_view(view)
                {
                    for layer in &win.layers {
                        for blk in &layer.blocks {
                            if let Some(bid) = blk.block_id.as_deref() {
                                let k = BlockKey::new(&sk.host_id, &sk.session_id, bid);
                                if !block_keys.contains(&k) {
                                    block_keys.push(k);
                                }
                            }
                        }
                    }
                    if let Some(bid) = win.focused_block_id.as_deref() {
                        let k = BlockKey::new(&sk.host_id, &sk.session_id, bid);
                        if !block_keys.contains(&k) {
                            block_keys.push(k);
                        }
                    }
                }
                if block_keys.is_empty()
                    && let Some(key) = self.focused_block_key()
                {
                    block_keys.push(key);
                }
                if !block_keys.is_empty() {
                    let all_muted = block_keys.iter().all(|k| {
                        self.settings
                            .is_muted(&k.host_id, &k.session_id, &k.block_id)
                    });
                    let target_muted = !all_muted;
                    // If unmuting a tab whose parent session is muted, lift the session-level
                    // mute while preserving mute on every sibling tab's blocks so only this
                    // tab becomes unmuted.
                    if !target_muted {
                        let session_key = self.workspace.active_session.clone().or_else(|| {
                            block_keys
                                .first()
                                .map(|k| SessionKey::new(&k.host_id, &k.session_id))
                        });
                        if let Some(sk) = session_key
                            && self.settings.is_session_muted(&sk.host_id, &sk.session_id)
                        {
                            let mut sibling_block_ids = Vec::new();
                            if let Some(view) =
                                self.workspace.session(&sk).and_then(|s| s.view.as_ref())
                            {
                                for win in &view.windows {
                                    for layer in &win.layers {
                                        for blk in &layer.blocks {
                                            if let Some(bid) = blk.block_id.as_deref()
                                                && !block_keys.iter().any(|k| {
                                                    k.host_id == sk.host_id && k.block_id == bid
                                                })
                                            {
                                                sibling_block_ids.push(bid.to_owned());
                                            }
                                        }
                                    }
                                    if let Some(bid) = win.focused_block_id.as_deref()
                                        && !block_keys
                                            .iter()
                                            .any(|k| k.host_id == sk.host_id && k.block_id == bid)
                                    {
                                        sibling_block_ids.push(bid.to_owned());
                                    }
                                }
                            }
                            for bid in sibling_block_ids {
                                self.settings.set_block_muted(&sk.host_id, &bid, true);
                            }
                            self.settings
                                .set_session_muted(&sk.host_id, &sk.session_id, false);
                        }
                    }
                    for k in &block_keys {
                        self.settings
                            .set_block_muted(&k.host_id, &k.block_id, target_muted);
                    }
                    self.save_settings_best_effort();
                }
            }
            ClientCommand::SplitPane(dir) => {
                if let Some(sk) = self.workspace.active_session.clone() {
                    let (direction, side) = match dir {
                        SplitDirection::Left => ("horizontal", "before"),
                        SplitDirection::Right | SplitDirection::Auto => ("horizontal", "after"),
                        SplitDirection::Up => ("vertical", "before"),
                        SplitDirection::Down => ("vertical", "after"),
                    };
                    let anchor_id = self.ui.focus.last_terminal_block().map(str::to_owned);
                    let anchor_key = anchor_id
                        .as_deref()
                        .map(|bid| BlockKey::new(&sk.host_id, &sk.session_id, bid));
                    let cwd =
                        self.resolve_shell_cwd_for_host(&sk.host_id, anchor_key.as_ref(), true);
                    let layout = self.build_shell_layout_spec_with_cwd(cwd.as_deref(), None, false);
                    self.send_command(HostCommand::SplitPane {
                        host_id: sk.host_id,
                        request: SessionNewSplitRequest {
                            session_id: sk.session_id,
                            anchor_id,
                            window_id: None,
                            direction: direction.to_owned(),
                            side: side.to_owned(),
                            ratio: None,
                            layout,
                            focus: Some(true),
                        },
                    });
                }
            }
            ClientCommand::ZoomPane => {
                if let Some(key) = self.focused_block_key() {
                    self.handle_pane_header_button(&key.block_id, PaneHeaderButton::Zoom);
                }
            }
            ClientCommand::FloatPane => {
                if let Some(key) = self.focused_block_key() {
                    self.handle_pane_header_button(&key.block_id, PaneHeaderButton::FloatOrDock);
                }
            }
            ClientCommand::MinimizeFloating => {
                if let Some(key) = self.focused_block_key()
                    && let Some((sk, layer_id)) = self.find_layer_for_block(&key.block_id)
                {
                    let has_other_visible = self
                        .workspace
                        .session(&sk)
                        .and_then(|s| s.view.as_ref())
                        .and_then(active_window_of_view)
                        .is_some_and(|win| {
                            win.layers.iter().any(|l| {
                                l.layer_id.as_deref() != Some(layer_id.as_str())
                                    && !l.blocks.is_empty()
                                    && !l
                                        .state
                                        .as_deref()
                                        .is_some_and(|s| s.eq_ignore_ascii_case("minimized"))
                            })
                        });
                    if has_other_visible {
                        self.send_command(HostCommand::SetLayerState {
                            host_id: sk.host_id,
                            request: SessionSetLayerStateRequest {
                                session_id: sk.session_id,
                                layer_id,
                                state: "minimized".to_owned(),
                                focus: Some(false),
                            },
                        });
                    } else {
                        self.push_toast_at(
                            ToastSeverity::Info,
                            "Cannot Minimize Pane",
                            "A window must keep at least one visible pane",
                            now,
                        );
                    }
                }
            }
            ClientCommand::MinimizeAllFloating => {
                if let Some(sk) = self.workspace.active_session.clone() {
                    let focused_bid = self.focused_block_key().map(|k| k.block_id);
                    let (has_non_floating_visible, mut float_candidates) = self
                        .workspace
                        .session(&sk)
                        .and_then(|s| s.view.as_ref())
                        .and_then(active_window_of_view)
                        .map(|win| {
                            let non_floating = win.layers.iter().any(|l| {
                                !l.kind.eq_ignore_ascii_case("floating")
                                    && !l.blocks.is_empty()
                                    && !l
                                        .state
                                        .as_deref()
                                        .is_some_and(|s| s.eq_ignore_ascii_case("minimized"))
                            });
                            let floats: Vec<(String, bool, bool)> = win
                                .layers
                                .iter()
                                .filter(|l| {
                                    l.kind.eq_ignore_ascii_case("floating")
                                        && !l
                                            .state
                                            .as_deref()
                                            .is_some_and(|s| s.eq_ignore_ascii_case("minimized"))
                                })
                                .filter_map(|l| {
                                    let lid = l.layer_id.clone()?;
                                    let has_blocks = !l.blocks.is_empty();
                                    let is_focused = focused_bid.as_deref().is_some_and(|fb| {
                                        l.blocks.iter().any(|b| b.block_id.as_deref() == Some(fb))
                                    });
                                    Some((lid, has_blocks, is_focused))
                                })
                                .collect();
                            (non_floating, floats)
                        })
                        .unwrap_or((false, Vec::new()));

                    if !float_candidates.is_empty() {
                        if !has_non_floating_visible {
                            // Preserve at least one floating layer with blocks so the window keeps
                            // its last visible pane (Rex rejects minimizing the last visible layer).
                            let keep_idx = float_candidates
                                .iter()
                                .position(|(_, has_blocks, is_focused)| *has_blocks && *is_focused)
                                .or_else(|| {
                                    float_candidates
                                        .iter()
                                        .position(|(_, has_blocks, _)| *has_blocks)
                                });
                            if let Some(idx) = keep_idx {
                                float_candidates.remove(idx);
                            }
                            if float_candidates.is_empty() {
                                self.push_toast_at(
                                    ToastSeverity::Info,
                                    "Cannot Minimize Pane",
                                    "A window must keep at least one visible pane",
                                    now,
                                );
                            }
                        }
                        for (layer_id, _, _) in float_candidates {
                            self.send_command(HostCommand::SetLayerState {
                                host_id: sk.host_id.clone(),
                                request: SessionSetLayerStateRequest {
                                    session_id: sk.session_id.clone(),
                                    layer_id,
                                    state: "minimized".to_owned(),
                                    focus: Some(false),
                                },
                            });
                        }
                    }
                }
            }
            ClientCommand::MovePane(dir) => {
                self.move_focused_pane(dir);
            }
            ClientCommand::BalancePanes => {
                self.send_server_action(host_id, "pane.balance", None);
            }
            ClientCommand::FontIncrease => {
                self.settings.increase_font();
                self.mark_shared_settings_changed(true);
                self.save_settings_best_effort();
            }
            ClientCommand::FontDecrease => {
                self.settings.decrease_font();
                self.mark_shared_settings_changed(true);
                self.save_settings_best_effort();
            }
            ClientCommand::FontReset => {
                self.settings.reset_font();
                self.mark_shared_settings_changed(true);
                self.save_settings_best_effort();
            }
            ClientCommand::ReloadConfig => {
                self.send_command(HostCommand::ReloadConfig {
                    host_id: host_id.to_owned(),
                });
            }
            ClientCommand::FitToDevice => {
                self.fit_active_session_to_device();
            }
            ClientCommand::PromptJump { next } => {
                if let Some(key) = self.focused_block_key() {
                    let delta = if next { 1 } else { -1 };
                    let _ = self
                        .pane_store
                        .scroll_viewport(&key, ScrollViewport::DeltaPrompt(delta));
                }
            }
            ClientCommand::FocusPane(dir) => {
                self.focus_adjacent_pane(dir, now);
            }
            ClientCommand::FocusNextPane => self.cycle_pane_focus(1, now),
            ClientCommand::FocusPrevPane => self.cycle_pane_focus(-1, now),
            ClientCommand::EnterKeyMode { mode } => {
                let hints = self.actions.keymap_mut().enter_mode(&mode);
                self.ui.which_key = Some(WhichKeyState {
                    header: format!("MODE: {}", mode.to_uppercase()),
                    exclusive: false,
                    hints,
                });
            }
            ClientCommand::ExitKeyMode => {
                self.actions.keymap_mut().exit_mode();
                self.refresh_which_key_overlay();
            }
            ClientCommand::ExitAllKeyModes => {
                self.actions.keymap_mut().exit_all_modes();
                self.ui.which_key = None;
            }
            ClientCommand::ServerAction { action, args } => {
                self.send_server_action(host_id, &action, args);
            }
            ClientCommand::ViewerOpen {
                path,
                line,
                col,
                remote_host,
            } => {
                let effective_host = remote_host
                    .as_deref()
                    .filter(|h| !h.is_empty())
                    .unwrap_or(host_id);
                let parsed = parse_path_with_location(&path);
                let final_line = line.or(parsed.line);
                let final_col = col.or(parsed.column);
                let target = classify_path(&parsed.path, final_line, parsed.end_line, final_col);
                if effective_host == LOCAL_HOST_ID {
                    let block_pwd = self.focused_block_key().and_then(|k| {
                        self.workspace
                            .block(&k)
                            .and_then(|b| b.pwd.as_deref())
                            .and_then(normalize_osc7_pwd)
                            .or_else(|| {
                                self.pane_store
                                    .pwd(&k)
                                    .as_deref()
                                    .and_then(normalize_osc7_pwd)
                            })
                    });
                    if let Some(resolved) =
                        self.resolve_local_file_path(&parsed.path, block_pwd.as_deref())
                    {
                        self.open_local_path_in_viewer(
                            resolved,
                            parsed.path,
                            final_line,
                            final_col,
                            Some(target.kind()),
                        );
                    }
                } else if let Some(key) = self.focused_block_key() {
                    let remote_key = BlockKey::new(effective_host, key.session_id, key.block_id);
                    self.open_remote_path_in_viewer(
                        &remote_key,
                        &parsed.path,
                        target.kind(),
                        final_line,
                        final_col,
                    );
                }
            }
            ClientCommand::QuickLook => {
                if let Some((key, target)) = self.hovered_terminal_target.clone() {
                    self.open_terminal_target(&key, &target, false, now);
                } else if !self.ui.viewer.tabs.is_empty() {
                    if self.ui.viewer.open {
                        self.dispatch_viewer_action(ViewerAction::ClosePane, now);
                    } else {
                        self.ui.viewer.open = true;
                        self.ui.focus_viewer();
                        self.force_immediate_resize = true;
                    }
                }
            }
        }
    }

    fn cycle_active_tab(&mut self, delta: isize, now: Instant) {
        let Some(sk) = self.workspace.active_session.clone() else {
            return;
        };
        let target_wid = self
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
                    .and_then(|id| ids.iter().position(|&w| w == id))
                    .unwrap_or(0);
                let next = (cur as isize + delta).rem_euclid(ids.len() as isize) as usize;
                Some(ids[next].to_owned())
            });
        if let Some(window_id) = target_wid {
            self.dispatch_ui_action(
                UiAction::SelectTab {
                    host_id: sk.host_id,
                    session_id: sk.session_id,
                    window_id,
                },
                now,
            );
        }
    }

    fn goto_tab_index(&mut self, index: i32, now: Instant) {
        let Some(sk) = self.workspace.active_session.clone() else {
            return;
        };
        let target_wid = self
            .workspace
            .session(&sk)
            .and_then(|s| s.view.as_ref())
            .and_then(|view| {
                if view.windows.is_empty() {
                    return None;
                }
                let idx = if index < 0 {
                    view.windows.len() - 1
                } else {
                    (index as usize)
                        .saturating_sub(1)
                        .min(view.windows.len() - 1)
                };
                view.windows[idx].window_id.clone()
            });
        if let Some(window_id) = target_wid {
            self.dispatch_ui_action(
                UiAction::SelectTab {
                    host_id: sk.host_id,
                    session_id: sk.session_id,
                    window_id,
                },
                now,
            );
        }
    }

    fn focus_adjacent_pane(&mut self, dir: PaneDirection, now: Instant) {
        let Some(out) = &self.last_ui_output else {
            return;
        };
        if out.tiled_panes.len() <= 1 {
            return;
        }
        let cur_id = self.ui.focus.last_terminal_block();
        let Some(cur) = out
            .tiled_panes
            .iter()
            .find(|p| Some(p.id.as_str()) == cur_id)
        else {
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
        if let Some((_, next_id)) = best {
            self.dispatch_ui_action(UiAction::FocusPane { block_id: next_id }, now);
        }
    }

    fn cycle_pane_focus(&mut self, delta: isize, now: Instant) {
        let next_id = self.last_ui_output.as_ref().and_then(|out| {
            if out.tiled_panes.len() <= 1 {
                return None;
            }
            let cur = self
                .ui
                .focus
                .last_terminal_block()
                .and_then(|id| out.tiled_panes.iter().position(|p| p.id == id))
                .unwrap_or(0);
            let next = (cur as isize + delta).rem_euclid(out.tiled_panes.len() as isize) as usize;
            Some(out.tiled_panes[next].id.clone())
        });
        if let Some(block_id) = next_id {
            self.dispatch_ui_action(UiAction::FocusPane { block_id }, now);
        }
    }

    pub(super) fn refresh_which_key_overlay(&mut self) {
        let mode = self.actions.keymap().active_mode().map(str::to_owned);
        let pending = self.actions.keymap().pending_sequence().to_vec();
        if mode.is_none() && pending.is_empty() {
            self.ui.which_key = None;
            return;
        }
        let hints = self.actions.keymap().hints_for(mode.as_deref(), &pending);
        let header = if let Some(m) = mode {
            format!("MODE: {}", m.to_uppercase())
        } else {
            let trail: Vec<String> = pending.iter().map(KeyChord::to_display_string).collect();
            format!("{} ›", trail.join(" › "))
        };
        self.ui.which_key = Some(WhichKeyState {
            header,
            exclusive: false,
            hints,
        });
    }

    pub(super) fn settings_view_snapshot(&self) -> SettingsViewSnapshot {
        SettingsViewSnapshot::from_config(
            &self.ui.layout_cfg,
            self.theme.material,
            self.ui.pane_options.highlight_active_border,
            self.ui.pane_options.fade_inactive_panes,
            self.ui.pane_options.program_color_header,
        )
        .with_extras(
            self.settings.theme_name.clone(),
            &self.settings.font_family,
            self.settings.font_size_pt,
            self.settings.chime_enabled,
            self.settings.desktop_notifications,
        )
    }

    pub(super) fn apply_theme_palette(&mut self, palette: omarchy_theme::ThemePalette) {
        if self.theme.apply_event(ThemeEvent::PaletteChanged(palette)) {
            self.ui.layout_cfg.hyprland_rounding = self
                .settings
                .adaptive_rounding
                .then_some(self.theme.rounding);
            let mode_2031 = self.pane_store.apply_theme_all(&self.theme.palette);
            if !self.read_only {
                for (key, report_bytes) in mode_2031 {
                    if self.workspace.block(&key).is_none() {
                        continue;
                    }
                    if let Ok(data) = String::from_utf8(report_bytes) {
                        self.send_command(HostCommand::TerminalWrite {
                            host_id: key.host_id,
                            session_id: key.session_id,
                            block_id: key.block_id,
                            data,
                        });
                    }
                }
                self.push_theme_to_owned_blocks();
            }
            self.mark_shared_settings_changed(true);
        }
    }

    fn load_theme_cached(&mut self, name: &str) -> Result<omarchy_theme::ThemePalette, String> {
        if let Some(pal) = self.theme_preview_cache.get(name) {
            return Ok(pal.clone());
        }
        let pal = omarchy_theme::load_theme_by_name(name).map_err(|e| e.to_string())?;
        self.theme_preview_cache
            .insert(name.to_owned(), pal.clone());
        Ok(pal)
    }

    fn open_theme_picker_modal(&mut self) {
        self.theme_preview_rollback =
            Some((self.settings.theme_name.clone(), self.theme.palette.clone()));
        self.theme_preview_cache.clear();
        let catalog = omarchy_theme::list_available_themes();
        let active_name = self
            .settings
            .theme_name
            .as_deref()
            .filter(|n| !n.is_empty())
            .unwrap_or(omarchy_theme::OMARCHY_LIVE_THEME_NAME);
        let items: Vec<PaletteItem> = catalog
            .into_iter()
            .map(|entry| {
                let mut item = PaletteItem::new(entry.name.clone(), entry.name.clone());
                let mode_tag = match entry.mode {
                    omarchy_theme::AppearanceMode::Dark => "Dark",
                    omarchy_theme::AppearanceMode::Light => "Light",
                };
                item.detail = Some(if entry.name == active_name {
                    format!("{mode_tag} ✓")
                } else {
                    mode_tag.to_owned()
                });
                item.subtitle = if entry.is_builtin {
                    Some("Built-in".to_owned())
                } else if entry.is_user_override {
                    Some("~/.config/omarchy/themes".to_owned())
                } else {
                    Some("/usr/share/omarchy/themes".to_owned())
                };
                item
            })
            .collect();
        self.ui
            .open_modal(ModalState::open_theme_picker(items, Some(active_name)));
    }

    fn preview_theme_by_name(&mut self, name: &str) {
        if let Ok(pal) = self.load_theme_cached(name) {
            self.apply_theme_palette(pal);
        }
    }

    fn commit_theme_by_name(&mut self, name: &str) {
        match self.load_theme_cached(name) {
            Ok(pal) => {
                self.theme_preview_rollback = None;
                self.theme_preview_cache.clear();
                self.settings.theme_name = if name == omarchy_theme::OMARCHY_LIVE_THEME_NAME {
                    None
                } else {
                    Some(name.to_owned())
                };
                self.apply_theme_palette(pal);
                self.mark_shared_settings_changed(false);
                self.save_settings_best_effort();
            }
            Err(err) => {
                self.push_toast(
                    ToastSeverity::Warning,
                    format!("Theme `{name}` failed to load"),
                    err,
                );
            }
        }
    }

    pub(super) fn enter_copy_mode(&mut self) {
        let Some(key) = self.focused_block_key() else {
            return;
        };
        let Some((cursor, _, _, _)) = self.pane_store.copy_mode_initial_cursor(&key) else {
            return;
        };
        let _ = self.pane_store.copy_mode_clear_selection(&key);
        self.copy_mode = Some(CopyModeState {
            block_key: key,
            cursor,
            visual: None,
            search_active: false,
            search_query: String::new(),
            pending_g: false,
        });
        self.needs_redraw = true;
    }

    fn open_file_picker(&mut self, purpose: FilePickerPurpose) {
        let Some(key) = self.focused_block_key() else {
            return;
        };
        let start_dir = self
            .workspace
            .block(&key)
            .and_then(|b| b.pwd.as_deref())
            .and_then(normalize_osc7_pwd)
            .or_else(|| {
                self.pane_store
                    .pwd(&key)
                    .as_deref()
                    .and_then(normalize_osc7_pwd)
            })
            .or_else(|| {
                if key.host_id == LOCAL_HOST_ID {
                    dirs::home_dir().map(|h| h.to_string_lossy().into_owned())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "~".to_owned());

        let remote_host = if key.host_id == LOCAL_HOST_ID {
            None
        } else {
            Some(self.host_display_label(&key.host_id))
        };
        self.next_file_picker_request_id = self.next_file_picker_request_id.saturating_add(1);
        let req_id = self.next_file_picker_request_id;
        self.file_picker_session = Some(FilePickerSession {
            purpose,
            key: key.clone(),
            current_dir: start_dir.clone(),
            request_id: req_id,
        });

        let has_host = self.host_manager.is_some()
            && self
                .workspace
                .host(&key.host_id)
                .is_some_and(|h| matches!(h.status, HostStatus::Connected { .. }));
        if !has_host && key.host_id == LOCAL_HOST_ID {
            match list_dir_local_fs(
                &start_dir,
                None,
                purpose == FilePickerPurpose::InsertPath,
                2000,
            ) {
                Ok(resp) => {
                    if let Some(err) = super::events::list_dir_status_error(resp.status) {
                        let mut modal = ModalState::open_file_picker(
                            purpose,
                            start_dir,
                            remote_host,
                            Vec::new(),
                            false,
                        );
                        modal.set_file_picker_error(err);
                        self.ui.open_modal(modal);
                    } else {
                        let resolved = if resp.path.is_empty() {
                            start_dir
                        } else {
                            resp.path.clone()
                        };
                        if let Some(sess) = &mut self.file_picker_session {
                            sess.current_dir = resolved.clone();
                        }
                        let entries = super::events::file_picker_entries_from_list_dir(
                            &resolved, &resp, purpose,
                        );
                        self.ui.open_modal(ModalState::open_file_picker(
                            purpose,
                            resolved,
                            remote_host,
                            entries,
                            false,
                        ));
                    }
                }
                Err(err) => {
                    let mut modal = ModalState::open_file_picker(
                        purpose,
                        start_dir,
                        remote_host,
                        Vec::new(),
                        false,
                    );
                    modal.set_file_picker_error(err);
                    self.ui.open_modal(modal);
                }
            }
            return;
        }

        self.ui.open_modal(ModalState::open_file_picker(
            purpose,
            start_dir.clone(),
            remote_host,
            Vec::new(),
            true,
        ));
        self.send_command(HostCommand::ListDir {
            key,
            path: start_dir,
            prefix: None,
            files: purpose == FilePickerPurpose::InsertPath,
            limit: 2000,
            request_id: req_id,
        });
    }

    fn request_file_picker_dir(&mut self, dir: String) {
        let Some(sess) = self.file_picker_session.as_mut() else {
            return;
        };
        self.next_file_picker_request_id = self.next_file_picker_request_id.saturating_add(1);
        let req_id = self.next_file_picker_request_id;
        sess.request_id = req_id;
        let purpose = sess.purpose;
        let key = sess.key.clone();

        let has_host = self.host_manager.is_some()
            && self
                .workspace
                .host(&key.host_id)
                .is_some_and(|h| matches!(h.status, HostStatus::Connected { .. }));
        if !has_host && key.host_id == LOCAL_HOST_ID {
            match list_dir_local_fs(&dir, None, purpose == FilePickerPurpose::InsertPath, 2000) {
                Ok(resp) => {
                    if let Some(err) = super::events::list_dir_status_error(resp.status) {
                        if let Some(modal) = &mut self.ui.modal {
                            modal.set_file_picker_error(err);
                        }
                    } else {
                        let resolved = if resp.path.is_empty() {
                            dir
                        } else {
                            resp.path.clone()
                        };
                        if let Some(s) = &mut self.file_picker_session {
                            s.current_dir = resolved.clone();
                        }
                        let entries = super::events::file_picker_entries_from_list_dir(
                            &resolved, &resp, purpose,
                        );
                        if let Some(modal) = &mut self.ui.modal {
                            modal.set_file_picker_entries(resolved, entries, None);
                        }
                    }
                }
                Err(err) => {
                    if let Some(modal) = &mut self.ui.modal {
                        modal.set_file_picker_error(err);
                    }
                }
            }
            return;
        }

        self.send_command(HostCommand::ListDir {
            key,
            path: dir,
            prefix: None,
            files: purpose == FilePickerPurpose::InsertPath,
            limit: 2000,
            request_id: req_id,
        });
    }

    fn open_port_action(&mut self, key: BlockKey, port: u16) {
        let is_local = key.host_id == LOCAL_HOST_ID
            || self
                .workspace
                .host(&key.host_id)
                .is_some_and(|h| h.is_local);
        let raw_addr = self
            .workspace
            .block(&key)
            .and_then(|b| b.listening_ports.iter().find(|p| p.port == port))
            .map(|p| p.address.clone())
            .unwrap_or_else(|| "127.0.0.1".to_owned());
        let host = normalize_tunnel_host(&raw_addr);

        if is_local {
            let url = format_port_http_url(&host, port);
            if !self.read_only {
                let _ = open::that_detached(&url);
            }
            self.push_toast(ToastSeverity::Info, format!("Opening :{port}"), url);
            return;
        }

        if let Some(existing) = self
            .active_tunnels
            .iter()
            .find(|t| t.source_block == key && t.remote_port == port)
        {
            if let Some(lp) = existing.local_port {
                let url = format!("http://127.0.0.1:{lp}");
                if !self.read_only {
                    let _ = open::that_detached(&url);
                }
                self.push_toast(
                    ToastSeverity::Info,
                    format!("Opening forwarded :{port}"),
                    url,
                );
            }
            return;
        }

        if self.read_only {
            return;
        }
        let tunnel_id = self.next_tunnel_id;
        self.next_tunnel_id = self.next_tunnel_id.saturating_add(1);
        self.active_tunnels.push(ActiveTunnel {
            tunnel_id,
            source_block: key.clone(),
            tunnel_session_id: key.session_id.clone(),
            tunnel_block_id: None,
            remote_host: host.clone(),
            remote_port: port,
            local_port: None,
            error: None,
        });
        self.send_command(HostCommand::StartPortTunnel {
            tunnel_id,
            key,
            remote_host: host,
            remote_port: port,
        });
    }
}
