use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::time::{Duration, Instant};

use omarchy_theme::{NotificationRequest, NotificationTarget, NotificationUrgency, ThemeEvent};
use rex_proto::DownloadResult;
use rex_proto::types::{
    ClipboardWrittenEvent, InitialWindowSpec, ServerEvent, SessionCreateRequest, SessionEvent,
    SessionSummary, TerminalPortsResponse,
};
use tr3x_bridge::{
    ProtoPromptOutcome, apply_proto_keymap, build_proto_keymap_request,
    dispatch_proto_queued_actions, handle_proto_perform_request, keymap_changed_needs_refresh,
    on_proto_prompt_ended, on_proto_prompt_requested, plan_proto_remote_call, proxy_reply_error,
};
use ui_chrome::{FilePickerEntry, FilePickerPurpose, Osc94Progress, Osc94State, ToastSeverity};
use vt_engine::{ProgramStatusState, ProgressState, TerminalEvent};

use crate::audio_chime::ChimeKind;

use crate::host_manager::{
    DataStreamEvent, DataStreamEventKind, HostCommand, LOCAL_HOST_ID, RuntimeEvent,
};
use crate::workspace::{
    BlockKey, HostEntry, HostStatus, OwnershipDecision, OwnershipTrigger, SessionKey,
    SessionWarmth, theme_palette_to_rex_args,
};

use super::*;

impl OmarexController {
    // =========================================================================
    // 2. RuntimeEvent Reducer (`HostManager` -> Controller)
    // =========================================================================

    /// Processes an asynchronous [`RuntimeEvent`] emitted by [`HostManager`].
    pub fn process_runtime_event(&mut self, event: RuntimeEvent, now: Instant) {
        self.process_runtime_event_multi(event, &mut [], None, now);
    }

    /// Processes an asynchronous [`RuntimeEvent`] across shared controller state and all open
    /// [`WindowContext`]s, routing window-scoped completions to their initiating window and
    /// broadcasting global state/theme/keymap updates across every open window.
    pub fn process_runtime_event_multi(
        &mut self,
        event: RuntimeEvent,
        contexts: &mut [&mut WindowContext],
        preferred_idx: Option<usize>,
        now: Instant,
    ) {
        let pref_idx = if contexts.is_empty() {
            None
        } else {
            Some(preferred_idx.filter(|&i| i < contexts.len()).unwrap_or(0))
        };
        if !matches!(
            event,
            RuntimeEvent::DataStream(_)
                | RuntimeEvent::ClientsListed { .. }
                | RuntimeEvent::HostsListUpdated { .. }
                | RuntimeEvent::TransferProgress { .. }
                | RuntimeEvent::DirListed { .. }
                | RuntimeEvent::DownloadFinished { .. }
                | RuntimeEvent::PromptRequested { .. }
                | RuntimeEvent::PromptEnded { .. }
                | RuntimeEvent::NotificationClicked(_)
        ) {
            self.mark_redraw_all(contexts);
        } else if matches!(event, RuntimeEvent::HostsListUpdated { .. }) {
            // Diagnostics HUD / sidebar host list may be open on any window.
            self.mark_redraw_all(contexts);
        }
        match event {
            RuntimeEvent::HostConnected {
                host_id,
                client_id,
                resumed,
                api_revision,
            } => {
                let host = self.ensure_host_entry(&host_id);
                let prev_server_host = host.server_host_id().map(str::to_owned);
                host.status = HostStatus::Connected {
                    client_id,
                    server_host_id: prev_server_host,
                    api_revision,
                    resumed,
                };
            }

            RuntimeEvent::HostDisconnected {
                host_id,
                will_reconnect,
                reason,
            } => {
                let host_label = self.host_display_label(&host_id);
                if let Some(host) = self.workspace.host_mut(&host_id) {
                    host.status = if will_reconnect {
                        HostStatus::Reconnecting {
                            reason: reason.clone(),
                        }
                    } else {
                        HostStatus::Disconnected
                    };
                }
                if !will_reconnect {
                    self.teardown_tunnels_for_host(&host_id);
                    self.push_toast_on_window(
                        contexts,
                        pref_idx,
                        ToastSeverity::Warning,
                        format!("Disconnected from {host_label}"),
                        reason,
                    );
                }
            }

            RuntimeEvent::HostError {
                host_id,
                context,
                message,
            } => {
                tracing::warn!(host = %host_id, %context, %message, "host runtime error");
                let host_label = self.host_display_label(&host_id);
                if context == "com.superlogical.terminal.resize" {
                    self.workspace.invalidate_host_sent_resizes(&host_id);
                }
                if let Some(host) = self.workspace.host_mut(&host_id)
                    && matches!(
                        host.status,
                        HostStatus::Disconnected | HostStatus::Connecting
                    )
                {
                    host.status = HostStatus::Error(message.clone());
                }
                self.push_toast_on_window(
                    contexts,
                    pref_idx,
                    ToastSeverity::Error,
                    format!("{host_label}: {context}"),
                    message,
                );
            }

            RuntimeEvent::ClientsListed { host_id, clients } => {
                let my_client_id = self
                    .workspace
                    .host(&host_id)
                    .and_then(HostEntry::client_id)
                    .map(str::to_owned);
                let mut attached = HashSet::new();
                for client in clients.clients {
                    if my_client_id.as_deref() == Some(client.client_id.as_str()) {
                        continue;
                    }
                    // Track other GUI app windows (ignore headless CLI/daemon/sync helpers).
                    let is_app = client
                        .info
                        .as_ref()
                        .and_then(|i| i.kind.as_deref())
                        .is_none_or(|k| k.eq_ignore_ascii_case("app"));
                    if is_app {
                        for sid in client.session_ids {
                            attached.insert(sid);
                        }
                    }
                }
                self.other_app_attached_sessions.insert(host_id, attached);
            }

            RuntimeEvent::BootstrapReady {
                host_id, bootstrap, ..
            } => {
                let host = self.ensure_host_entry(&host_id);

                let api_rev = match &host.status {
                    HostStatus::Connected { api_revision, .. } => *api_revision,
                    _ => rex_proto::TARGET_API_REVISION,
                };

                // Preserve pre-issued data tickets before consuming `bootstrap`.
                let mut tickets: HashMap<(String, String), String> = HashMap::new();
                for grant in &bootstrap.data_tickets {
                    tickets.insert(
                        (grant.session_id.clone(), grant.block_id.clone()),
                        grant.ticket.clone(),
                    );
                }

                let selected_id = bootstrap
                    .selected_session
                    .as_ref()
                    .map(|s| s.session.session_id.clone());
                let updated_session_ids: Vec<String> = bootstrap
                    .selected_session
                    .iter()
                    .chain(bootstrap.attached_sessions.iter())
                    .map(|s| s.session.session_id.clone())
                    .collect();
                host.apply_bootstrap(*bootstrap, api_rev);
                let updated_set: HashSet<SessionKey> = updated_session_ids
                    .iter()
                    .map(|sid| SessionKey::new(host_id.clone(), sid.clone()))
                    .collect();
                for sk in &updated_set {
                    self.prune_stale_session_blocks(sk);
                }

                // Standalone `-e <cmd>` mode: spawn an ephemeral Rex session for the command
                // instead of attaching to the user's workspace sessions.
                if let Some(spec) = self.command_launch.clone()
                    && host_id == LOCAL_HOST_ID
                {
                    if !self.ephemeral_spawn_requested {
                        self.ephemeral_spawn_requested = true;
                        let label = spec
                            .command
                            .first()
                            .and_then(|s| Path::new(s).file_name())
                            .and_then(|s| s.to_str())
                            .map(|prog| format!("omarex-run-{prog}"))
                            .unwrap_or_else(|| "omarex-run".to_owned());
                        let layout = self
                            .build_shell_layout_spec(Some(&spec.command), spec.wait_after_command);
                        let origin_window_id = pref_idx.map(|i| contexts[i].id);
                        self.send_command(HostCommand::CreateCommandSession {
                            host_id: host_id.clone(),
                            request: SessionCreateRequest {
                                label: Some(label),
                                initial_windows: Some(vec![InitialWindowSpec {
                                    window_label: None,
                                    layout,
                                }]),
                            },
                            origin_window_id,
                        });
                    }
                } else {
                    let no_active_session = if contexts.is_empty() {
                        self.workspace.active_session.is_none()
                    } else {
                        contexts.iter().all(|c| c.active_session.is_none())
                    };
                    if no_active_session {
                        let target_idx = if contexts.is_empty() {
                            None
                        } else {
                            contexts
                                .iter()
                                .position(|c| c.active_session.is_none())
                                .or(pref_idx)
                        };
                        self.with_target_window_or_self(contexts, target_idx, |ctrl| {
                            // Explicit `--session <name>` filter takes top priority (preserves CLI/headless).
                            if ctrl.preferred_session_filter.is_some() {
                                if let Some(chosen_sess) =
                                    ctrl.pick_initial_session_id(&host_id, selected_id.as_deref())
                                {
                                    let sk = SessionKey::new(host_id.clone(), chosen_sess);
                                    ctrl.activate_session(sk.clone(), &tickets);
                                    ctrl.maybe_hydrate_session_metadata(&sk);
                                }
                            } else if let Some(launch_cwd) = ctrl.default_working_dir.clone()
                                && host_id == LOCAL_HOST_ID
                                && !ctrl.read_only
                            {
                                let home_dir = std::env::var("HOME").ok();
                                let is_home_cwd = home_dir
                                    .as_deref()
                                    .is_some_and(|h| Path::new(&launch_cwd) == Path::new(h));
                                let other_attached = ctrl.other_app_attached_sessions.get(&host_id);
                                let reusable = if is_home_cwd {
                                    ctrl.find_preferred_terminal_session(&host_id, other_attached)
                                } else {
                                    None
                                };

                                if let Some(chosen_sess) = reusable {
                                    let sk = SessionKey::new(host_id.clone(), chosen_sess);
                                    ctrl.activate_session(sk.clone(), &tickets);
                                    ctrl.maybe_hydrate_session_metadata(&sk);
                                } else {
                                    let label = ctrl.new_terminal_session_label(
                                        &host_id,
                                        Some(&launch_cwd),
                                        true,
                                    );
                                    let layout = ctrl.build_shell_layout_spec_with_cwd(
                                        Some(&launch_cwd),
                                        None,
                                        false,
                                    );
                                    ctrl.send_command(HostCommand::NewSession {
                                        host_id: host_id.clone(),
                                        request: SessionCreateRequest {
                                            label,
                                            initial_windows: Some(vec![InitialWindowSpec {
                                                window_label: None,
                                                layout,
                                            }]),
                                        },
                                        open_in_new_window: false,
                                        origin_window_id: None,
                                    });
                                }
                            } else if let Some(chosen_sess) =
                                ctrl.pick_initial_session_id(&host_id, selected_id.as_deref())
                            {
                                let sk = SessionKey::new(host_id.clone(), chosen_sess);
                                ctrl.activate_session(sk.clone(), &tickets);
                                ctrl.maybe_hydrate_session_metadata(&sk);
                            } else if host_id == LOCAL_HOST_ID && !ctrl.read_only {
                                // Empty local server: spin up an initial shell session (named by the server).
                                let label = ctrl.new_terminal_session_label(&host_id, None, false);
                                let layout = ctrl.build_shell_layout_spec(None, false);
                                ctrl.send_command(HostCommand::NewSession {
                                    host_id: host_id.clone(),
                                    request: SessionCreateRequest {
                                        label,
                                        initial_windows: Some(vec![InitialWindowSpec {
                                            window_label: None,
                                            layout,
                                        }]),
                                    },
                                    open_in_new_window: false,
                                    origin_window_id: None,
                                });
                            }
                        });
                    } else if !contexts.is_empty() {
                        // Sync any open windows already viewing sessions updated by this bootstrap.
                        for ctx in contexts.iter_mut() {
                            if let Some(sk) = ctx.active_session.clone()
                                && updated_set.contains(&sk)
                            {
                                let focused = self
                                    .workspace
                                    .session(&sk)
                                    .and_then(|s| s.view.as_ref())
                                    .and_then(active_window_of_view)
                                    .and_then(primary_block_id_of_window)
                                    .map(str::to_owned);
                                self.with_window_context(ctx, |ctrl| {
                                    ctrl.ui.on_session_view_changed(focused);
                                    ctrl.open_active_window_streams(&sk, &tickets);
                                });
                            }
                        }
                    }
                }
            }

            RuntimeEvent::SessionCreatedAndAttached {
                host_id,
                session_id,
                view,
                open_in_new_window,
                origin_window_id,
                ..
            } => {
                let sk = SessionKey::new(host_id.clone(), session_id.clone());
                let focused_block = active_window_of_view(&view)
                    .and_then(primary_block_id_of_window)
                    .map(str::to_owned);
                let label = view.label.clone();
                if let Some(host) = self.workspace.host_mut(&host_id) {
                    host.on_session_created(SessionSummary {
                        session_id: session_id.clone(),
                        label,
                    });
                }
                if let Some(sess) = self.workspace.session_mut(&sk) {
                    sess.apply_view(*view);
                    if open_in_new_window {
                        sess.warmth = SessionWarmth::Active;
                    }
                }
                self.prune_stale_session_blocks(&sk);
                self.collapsed_hosts.remove(&host_id);
                self.set_session_collapsed(&host_id, &session_id, false);
                if open_in_new_window {
                    self.workspace.pinned_sessions.insert(sk.clone());
                    self.open_active_window_streams(&sk, &HashMap::new());
                    self.maybe_hydrate_session_metadata(&sk);
                    if let Some(bid) = &focused_block {
                        let key = BlockKey::new(host_id, session_id, bid.clone());
                        self.workspace.invalidate_sent_resize(&key);
                        self.maybe_claim_ownership(&key, OwnershipTrigger::ExplicitInteractOrFit);
                    }
                    self.pending_new_windows.push(PendingNewWindow::Session {
                        session: sk,
                        focused_block,
                    });
                } else {
                    let target_idx = if contexts.is_empty() {
                        None
                    } else if let Some(oid) = origin_window_id {
                        // Never hijack an unrelated focused window if the requester closed!
                        contexts
                            .iter()
                            .position(|c| c.id == oid)
                            .or_else(|| contexts.iter().position(|c| c.active_session.is_none()))
                    } else {
                        contexts
                            .iter()
                            .position(|c| c.active_session.is_none())
                            .or(pref_idx)
                    };
                    if let Some(idx) = target_idx {
                        let sk_c = sk.clone();
                        let focused_c = focused_block.clone();
                        self.with_window_context(contexts[idx], |ctrl| {
                            ctrl.activate_session(sk_c, &HashMap::new());
                            ctrl.ui.focus.focus_terminal(focused_c);
                            ctrl.force_immediate_resize = true;
                        });
                    } else if contexts.is_empty() {
                        self.activate_session(sk.clone(), &HashMap::new());
                        self.ui.focus.focus_terminal(focused_block.clone());
                        self.force_immediate_resize = true;
                    }
                    self.open_active_window_streams(&sk, &HashMap::new());
                    self.maybe_hydrate_session_metadata(&sk);
                    if let Some(bid) = focused_block {
                        let key = BlockKey::new(host_id, session_id, bid);
                        self.workspace.invalidate_sent_resize(&key);
                        self.maybe_claim_ownership(&key, OwnershipTrigger::ExplicitInteractOrFit);
                    }
                }
            }

            RuntimeEvent::TabTransferStarting {
                host_id,
                source_session_id,
                block_ids,
                ..
            } => {
                for bid in block_ids {
                    self.pending_transfer_blocks.insert(BlockKey::new(
                        host_id.clone(),
                        source_session_id.clone(),
                        bid,
                    ));
                }
            }

            RuntimeEvent::TabTransferFailed {
                host_id,
                source_session_id,
                block_ids,
                ..
            } => {
                for bid in block_ids {
                    let key = BlockKey::new(host_id.clone(), source_session_id.clone(), bid);
                    self.pending_transfer_blocks.remove(&key);
                    if self.transfer_closed_source_blocks.remove(&key)
                        && self.workspace.block(&key).is_none()
                    {
                        self.remove_closed_block(&key, contexts);
                    }
                }
            }

            RuntimeEvent::TabTransferredToNewWindow {
                host_id,
                source_session_id,
                target_session_id,
                window_id: _,
                target_view,
                source_view,
                ..
            } => {
                let target_sk = SessionKey::new(host_id.clone(), target_session_id.clone());
                let source_sk = SessionKey::new(host_id.clone(), source_session_id.clone());
                let focused_block = active_window_of_view(&target_view)
                    .and_then(primary_block_id_of_window)
                    .map(str::to_owned);
                let transferred_block_ids: Vec<String> = target_view
                    .windows
                    .iter()
                    .flat_map(|w| w.layers.iter())
                    .flat_map(|l| l.blocks.iter())
                    .filter_map(|b| b.block_id.clone())
                    .collect();

                let label = target_view.label.clone();
                if let Some(host) = self.workspace.host_mut(&host_id) {
                    host.on_session_created(SessionSummary {
                        session_id: target_session_id.clone(),
                        label,
                    });
                }
                if let Some(sess) = self.workspace.session_mut(&target_sk) {
                    sess.apply_view(*target_view);
                    sess.warmth = SessionWarmth::Active;
                }
                if let Some(src_view) = source_view {
                    if let Some(sess) = self.workspace.session_mut(&source_sk) {
                        sess.apply_view(*src_view);
                    }
                    // Keep pending_transfer_blocks intact during prune_stale_session_blocks so
                    // transferred blocks are not prematurely evicted before `rekey_block` runs.
                    self.prune_stale_session_blocks(&source_sk);
                }

                for bid in transferred_block_ids {
                    let old_key =
                        BlockKey::new(host_id.clone(), source_session_id.clone(), bid.clone());
                    let new_key =
                        BlockKey::new(host_id.clone(), target_session_id.clone(), bid.clone());
                    self.pending_transfer_blocks.remove(&old_key);
                    self.transfer_closed_source_blocks.remove(&old_key);
                    let _ = self.pane_store.rekey_block(&old_key, new_key.clone());
                    self.opened_streams.remove(&old_key);
                    self.hydrated_blocks.remove(&old_key);
                    self.ports_queried_blocks.remove(&old_key);
                    if let Some(v) = self.block_activity_at.remove(&old_key) {
                        self.block_activity_at.insert(new_key.clone(), v);
                    }
                    if let Some(v) = self.block_updated_unix.remove(&old_key) {
                        self.block_updated_unix.insert(new_key.clone(), v);
                    }
                    if let Some(v) = self.block_osc94.remove(&old_key) {
                        self.block_osc94.insert(new_key.clone(), v);
                    }
                    if let Some(v) = self.program_chime_state.remove(&old_key) {
                        self.program_chime_state.insert(new_key.clone(), v);
                    }
                    for tun in &mut self.active_tunnels {
                        if tun.source_block == old_key {
                            tun.source_block = new_key.clone();
                        }
                    }
                    if contexts.is_empty() {
                        if let Some(cm) = self.copy_mode.as_mut()
                            && cm.block_key == old_key
                        {
                            cm.block_key = new_key.clone();
                        }
                    } else {
                        for ctx in contexts.iter_mut() {
                            if let Some(cm) = ctx.copy_mode.as_mut()
                                && cm.block_key == old_key
                            {
                                cm.block_key = new_key.clone();
                            }
                        }
                    }
                    // `session.transfer_window` closes streams on the source session; reopen on `target_session_id`.
                    if self.opened_streams.insert(new_key.clone()) {
                        self.send_command(HostCommand::OpenBlockStream {
                            host_id: new_key.host_id.clone(),
                            session_id: new_key.session_id.clone(),
                            block_id: new_key.block_id.clone(),
                            ticket: None,
                        });
                    }
                    self.workspace.invalidate_sent_resize(&new_key);
                    self.maybe_claim_ownership(&new_key, OwnershipTrigger::ExplicitInteractOrFit);
                }

                self.collapsed_hosts.remove(&host_id);
                self.set_session_collapsed(&host_id, &target_session_id, false);
                self.workspace.pinned_sessions.insert(target_sk.clone());
                self.maybe_hydrate_session_metadata(&target_sk);

                let src_focused = self
                    .workspace
                    .session(&source_sk)
                    .and_then(|s| s.view.as_ref())
                    .and_then(active_window_of_view)
                    .and_then(primary_block_id_of_window)
                    .map(str::to_owned);
                if contexts.is_empty() {
                    if self.workspace.active_session.as_ref() == Some(&source_sk) {
                        self.ui.on_session_view_changed(src_focused);
                    }
                } else {
                    for ctx in contexts.iter_mut() {
                        if ctx.active_session.as_ref() == Some(&source_sk) {
                            ctx.ui.on_session_view_changed(src_focused.clone());
                        }
                    }
                }

                self.pending_new_windows.push(PendingNewWindow::Session {
                    session: target_sk,
                    focused_block,
                });
            }

            RuntimeEvent::WindowCreated {
                host_id,
                session_id,
                window_id,
                block_ids,
                view,
                origin_window_id,
                ..
            } => {
                let sk = SessionKey::new(host_id.clone(), session_id.clone());
                self.collapsed_hosts.remove(&host_id);
                self.set_session_collapsed(&host_id, &session_id, false);

                let focused_from_view = if let Some(v) = view {
                    let primary = active_window_of_view(&v)
                        .and_then(primary_block_id_of_window)
                        .map(str::to_owned);
                    if let Some(sess) = self.workspace.session_mut(&sk) {
                        sess.apply_view(*v);
                    }
                    self.prune_stale_session_blocks(&sk);
                    primary
                } else {
                    if let Some(sess) = self.workspace.session_mut(&sk)
                        && let Some(v) = &mut sess.view
                    {
                        v.active_window_id = Some(window_id);
                    }
                    self.send_command(HostCommand::FetchView {
                        host_id: host_id.clone(),
                        session_id: session_id.clone(),
                    });
                    None
                };

                let target_block = block_ids.first().cloned().or(focused_from_view);
                if contexts.is_empty() {
                    if self.workspace.active_session.as_ref() != Some(&sk) {
                        self.activate_session(sk.clone(), &HashMap::new());
                    }
                    if target_block.is_some() {
                        self.ui.focus.focus_terminal(target_block.clone());
                    }
                    self.force_immediate_resize = true;
                } else {
                    let req_idx = if let Some(oid) = origin_window_id {
                        contexts
                            .iter()
                            .position(|c| c.id == oid)
                            .or_else(|| find_window_viewing_session(contexts, &sk, pref_idx))
                    } else {
                        find_window_viewing_session(contexts, &sk, pref_idx)
                            .or_else(|| contexts.iter().position(|c| c.active_session.is_none()))
                            .or_else(|| (contexts.len() == 1).then_some(0))
                    };
                    for (idx, ctx) in contexts.iter_mut().enumerate() {
                        if Some(idx) == req_idx {
                            let sk_c = sk.clone();
                            let tb = target_block.clone();
                            self.with_window_context(ctx, |ctrl| {
                                if ctrl.workspace.active_session.as_ref() != Some(&sk_c) {
                                    ctrl.activate_session(sk_c, &HashMap::new());
                                }
                                if tb.is_some() {
                                    ctrl.ui.focus.focus_terminal(tb);
                                }
                                ctrl.force_immediate_resize = true;
                            });
                        } else if ctx.active_session.as_ref() == Some(&sk) {
                            ctx.ui.on_session_view_changed(target_block.clone());
                            ctx.force_immediate_resize = true;
                        }
                    }
                }
                self.open_active_window_streams(&sk, &HashMap::new());
                for bid in &block_ids {
                    let key = BlockKey::new(host_id.clone(), session_id.clone(), bid.clone());
                    if self.opened_streams.insert(key.clone()) {
                        self.send_command(HostCommand::OpenBlockStream {
                            host_id: key.host_id.clone(),
                            session_id: key.session_id.clone(),
                            block_id: key.block_id.clone(),
                            ticket: None,
                        });
                    }
                    self.workspace.invalidate_sent_resize(&key);
                    self.maybe_claim_ownership(&key, OwnershipTrigger::ExplicitInteractOrFit);
                }
                if block_ids.is_empty()
                    && let Some(bid) = target_block
                {
                    let key = BlockKey::new(host_id, session_id, bid);
                    self.workspace.invalidate_sent_resize(&key);
                    self.maybe_claim_ownership(&key, OwnershipTrigger::ExplicitInteractOrFit);
                }
                self.maybe_hydrate_session_metadata(&sk);
            }

            RuntimeEvent::CommandSessionReady {
                host_id,
                session_id,
                view,
                origin_window_id,
            } => {
                let sk = SessionKey::new(host_id.clone(), session_id.clone());
                let focused_block = active_window_of_view(&view)
                    .and_then(primary_block_id_of_window)
                    .map(str::to_owned);
                let label = view.label.clone();
                if let Some(host) = self.workspace.host_mut(&host_id) {
                    host.on_session_created(SessionSummary {
                        session_id: session_id.clone(),
                        label,
                    });
                }
                if let Some(sess) = self.workspace.session_mut(&sk) {
                    sess.apply_view(*view);
                }
                self.prune_stale_session_blocks(&sk);
                let target_idx = if contexts.is_empty() {
                    None
                } else if let Some(oid) = origin_window_id {
                    contexts
                        .iter()
                        .position(|c| c.id == oid)
                        .or_else(|| contexts.iter().position(|c| c.active_session.is_none()))
                        .or(pref_idx)
                } else {
                    contexts
                        .iter()
                        .position(|c| c.active_session.is_none())
                        .or(pref_idx)
                };
                if let Some(idx) = target_idx {
                    let sk_c = sk.clone();
                    let fb = focused_block.clone();
                    self.with_window_context(contexts[idx], |ctrl| {
                        ctrl.ephemeral_session = Some(sk_c.clone());
                        ctrl.activate_session(sk_c, &HashMap::new());
                        ctrl.ui.on_session_view_changed(fb);
                    });
                } else {
                    self.ephemeral_session = Some(sk.clone());
                    self.activate_session(sk.clone(), &HashMap::new());
                    self.ui.on_session_view_changed(focused_block.clone());
                }
                self.open_active_window_streams(&sk, &HashMap::new());
                if let Some(bid) = focused_block {
                    self.maybe_claim_ownership(
                        &BlockKey::new(host_id, session_id, bid),
                        OwnershipTrigger::ExplicitInteractOrFit,
                    );
                }
            }

            RuntimeEvent::SessionViewUpdated {
                host_id,
                session_id,
                view,
                ..
            } => {
                let sk = SessionKey::new(host_id.clone(), session_id.clone());
                let focused_block = active_window_of_view(&view)
                    .and_then(primary_block_id_of_window)
                    .map(str::to_owned);

                let needs_catchup = self
                    .workspace
                    .session_mut(&sk)
                    .is_some_and(|s| s.apply_view(*view));
                self.prune_stale_session_blocks(&sk);
                if needs_catchup {
                    self.send_command(HostCommand::FetchView {
                        host_id: host_id.clone(),
                        session_id: session_id.clone(),
                    });
                }

                // Batch-hydrate initial OSC 7501 `program_status` for any newly discovered blocks.
                self.maybe_hydrate_session_metadata(&sk);

                let mut any_viewing = false;
                if contexts.is_empty() {
                    if self.workspace.active_session.as_ref() == Some(&sk) {
                        self.ui.on_session_view_changed(focused_block.clone());
                        any_viewing = true;
                    }
                } else {
                    for ctx in contexts.iter_mut() {
                        if ctx.active_session.as_ref() == Some(&sk) {
                            ctx.ui.on_session_view_changed(focused_block.clone());
                            any_viewing = true;
                        }
                    }
                }
                if any_viewing {
                    self.open_active_window_streams(&sk, &HashMap::new());
                    if let Some(bid) = focused_block {
                        self.maybe_claim_ownership(
                            &BlockKey::new(host_id, session_id, bid),
                            OwnershipTrigger::AttachOrView,
                        );
                    }
                }
            }

            RuntimeEvent::SessionViewFetchFailed {
                host_id,
                session_id,
                session_not_found,
            } => {
                self.workspace
                    .clear_view_fetch_in_flight(&host_id, &session_id);
                if session_not_found {
                    self.cleanup_removed_session_multi(&host_id, &session_id, contexts);
                }
            }

            RuntimeEvent::SessionMetadataHydrated {
                host_id,
                session_id,
                blocks,
            } => {
                for meta in &blocks {
                    let key = BlockKey::new(&host_id, &session_id, &meta.block_id);
                    if let Some(latest_unix) = meta
                        .program_statuses
                        .iter()
                        .filter_map(|r| parse_rfc3339_unix_secs(&r.updated_at))
                        .max()
                    {
                        self.block_updated_unix.insert(key.clone(), latest_unix);
                    }
                    let records: Vec<tr3x_bridge::ProgramStatusRecord> = meta
                        .program_statuses
                        .iter()
                        .map(|r| program_status_from_proto(r, false))
                        .collect();
                    if let Some(rolled) = rollup_records(&records) {
                        let st = map_tr3x_state_to_vt(rolled.state);
                        self.program_chime_state.entry(key).or_insert((st, now));
                    }
                }
                let updated = self
                    .workspace
                    .apply_hydrated_metadata(&host_id, &session_id, blocks);
                self.metadata_hydrated_count = self.metadata_hydrated_count.saturating_add(updated);
            }

            RuntimeEvent::KeymapUpdated { keymap, .. } => {
                self.keymap_diagnostics = keymap.diagnostics.clone();
                apply_proto_keymap(&mut self.actions, &keymap);
                if contexts.is_empty() {
                    self.refresh_which_key_overlay();
                } else {
                    for ctx in contexts.iter_mut() {
                        self.with_window_context(ctx, |ctrl| {
                            ctrl.refresh_which_key_overlay();
                            ctrl.needs_redraw = true;
                        });
                    }
                }
            }

            RuntimeEvent::HostsListUpdated { .. } => {}

            RuntimeEvent::ControlEvent { host_id, event, .. } => {
                self.handle_control_event_multi(&host_id, *event, contexts, pref_idx, now);
            }

            RuntimeEvent::PromptRequested {
                host_id, request, ..
            } => match on_proto_prompt_requested(&mut self.prompts, &request, now) {
                ProtoPromptOutcome::Opened => {
                    if let Some(session) = self.prompts.active().cloned() {
                        self.active_prompt_host = Some(host_id);
                        self.with_target_window_or_self(contexts, pref_idx, |ctrl| {
                            ctrl.ui.open_rex_prompt(session);
                            ctrl.needs_redraw = true;
                        });
                    }
                }
                ProtoPromptOutcome::ImmediateReply(reply) => {
                    self.send_command(HostCommand::ClientReply {
                        host_id,
                        request: reply,
                    });
                }
            },

            RuntimeEvent::PromptEnded { event, .. } => {
                on_proto_prompt_ended(&mut self.prompts, &event);
                if contexts.is_empty() {
                    if self.ui.dismiss_rex_prompt_if_matches(&event.call_id) {
                        self.active_prompt_host = None;
                        self.needs_redraw = true;
                    }
                } else {
                    let mut dismissed_any = false;
                    for ctx in contexts.iter_mut() {
                        if ctx.ui.dismiss_rex_prompt_if_matches(&event.call_id) {
                            ctx.needs_redraw = true;
                            dismissed_any = true;
                        }
                    }
                    if dismissed_any {
                        self.active_prompt_host = None;
                    }
                }
            }

            RuntimeEvent::PerformRequested {
                host_id, request, ..
            } => {
                let local_uid = current_process_uid();
                let (cmd_opt, reply) = handle_proto_perform_request(&request, local_uid);
                if let Some(cmd) = cmd_opt {
                    self.with_target_window_or_self(contexts, pref_idx, |ctrl| {
                        ctrl.execute_client_command(&host_id, cmd, now);
                        ctrl.needs_redraw = true;
                    });
                }
                self.send_command(HostCommand::ClientReply {
                    host_id,
                    request: reply,
                });
            }

            RuntimeEvent::CallRequested {
                host_id, request, ..
            } => {
                let connected: Vec<String> = self
                    .workspace
                    .hosts
                    .values()
                    .filter(|h| matches!(h.status, HostStatus::Connected { .. }))
                    .map(|h| h.id.clone())
                    .collect();
                let refs: Vec<&str> = connected.iter().map(String::as_str).collect();
                let reply = match plan_proto_remote_call(&request, &refs) {
                    Ok(plan) => proxy_reply_error(
                        &plan,
                        "not_implemented",
                        "remote multi-server proxy dispatch is not available in this session",
                    ),
                    Err(immediate_err) => immediate_err,
                };
                self.send_command(HostCommand::ClientReply {
                    host_id,
                    request: reply,
                });
            }

            RuntimeEvent::ClientActionCompleted {
                host_id, response, ..
            } => {
                let queued = dispatch_proto_queued_actions(&response);
                if !queued.is_empty() {
                    self.with_target_window_or_self(contexts, pref_idx, |ctrl| {
                        for cmd in queued {
                            ctrl.execute_client_command(&host_id, cmd, now);
                        }
                        ctrl.needs_redraw = true;
                    });
                }
            }

            RuntimeEvent::DataStream(stream_ev) => {
                self.handle_data_stream_event_multi(stream_ev, contexts, now);
            }

            RuntimeEvent::TransferProgress { .. } => {}

            RuntimeEvent::UploadFinished {
                host_id,
                session_id,
                block_id,
                result,
                ..
            } => {
                let sk = SessionKey::new(host_id, session_id);
                let toast_idx = find_window_viewing_session(contexts, &sk, pref_idx).or(pref_idx);
                match result {
                    Ok(res) => self.push_toast_on_window(
                        contexts,
                        toast_idx,
                        ToastSeverity::Success,
                        "File uploaded",
                        format!(
                            "Uploaded {} ({}) to {block_id}",
                            res.sanitized_name,
                            format_byte_size(res.bytes_uploaded),
                        ),
                    ),
                    Err(err) => self.push_toast_on_window(
                        contexts,
                        toast_idx,
                        ToastSeverity::Error,
                        "Upload failed",
                        err,
                    ),
                }
            }

            RuntimeEvent::DownloadFinished {
                host_id,
                transfer_id,
                result,
                ..
            } => {
                let claiming_idx = if contexts.is_empty() {
                    None
                } else {
                    contexts
                        .iter()
                        .position(|c| c.pending_viewer_downloads.contains_key(&transfer_id))
                };
                if let Some(idx) = claiming_idx {
                    self.with_window_context(contexts[idx], |ctrl| {
                        if let Some(pending) = ctrl.pending_viewer_downloads.remove(&transfer_id) {
                            ctrl.apply_viewer_download_result(host_id, pending, result);
                            ctrl.needs_redraw = true;
                        }
                    });
                    return;
                }
                if contexts.is_empty()
                    && let Some(pending) = self.pending_viewer_downloads.remove(&transfer_id)
                {
                    self.apply_viewer_download_result(host_id, pending, result);
                    return;
                }
                match result {
                    Ok(DownloadResult::Downloaded {
                        remote_path,
                        local_path,
                        version,
                        ..
                    }) => {
                        self.downloaded_file_cache
                            .insert((host_id, remote_path), (local_path.clone(), version));
                        if let Err(err) = open::that_detached(&local_path) {
                            self.push_toast_on_window(
                                contexts,
                                pref_idx,
                                ToastSeverity::Warning,
                                "Downloaded file",
                                format!("Saved to {} (open failed: {err})", local_path.display()),
                            );
                        }
                    }
                    Ok(DownloadResult::NotModified { remote_path, .. }) => {
                        if let Some((cached_path, _)) = self
                            .downloaded_file_cache
                            .get(&(host_id, remote_path.clone()))
                            .cloned()
                            && cached_path.exists()
                        {
                            if let Err(err) = open::that_detached(&cached_path) {
                                self.push_toast_on_window(
                                    contexts,
                                    pref_idx,
                                    ToastSeverity::Warning,
                                    "Open cached file failed",
                                    format!("{} ({err})", cached_path.display()),
                                );
                            }
                        } else {
                            self.push_toast_on_window(
                                contexts,
                                pref_idx,
                                ToastSeverity::Info,
                                "File unchanged",
                                format!("Cached copy of {remote_path} is up to date"),
                            );
                        }
                    }
                    Err(err) => self.push_toast_on_window(
                        contexts,
                        pref_idx,
                        ToastSeverity::Error,
                        "Download failed",
                        err,
                    ),
                }
            }

            RuntimeEvent::ThemeChanged(ev) => {
                let pinned_theme =
                    self.settings.theme_name.as_deref().is_some_and(|n| {
                        !n.is_empty() && n != omarchy_theme::OMARCHY_LIVE_THEME_NAME
                    });
                if matches!(ev, ThemeEvent::PaletteChanged(_)) && pinned_theme {
                    return;
                }
                if self.theme.apply_event(ev) {
                    let rounding = self
                        .settings
                        .adaptive_rounding
                        .then_some(self.theme.rounding);
                    let prev_ui_gen = self.shared_ui_gen;
                    self.shared_ui_gen = self.shared_ui_gen.saturating_add(1);
                    if contexts.is_empty() {
                        self.ui.layout_cfg.hyprland_rounding = rounding;
                        if self.current_window_synced_ui_gen == prev_ui_gen {
                            self.current_window_synced_ui_gen = self.shared_ui_gen;
                        }
                        self.needs_redraw = true;
                    } else {
                        for ctx in contexts.iter_mut() {
                            ctx.ui.layout_cfg.hyprland_rounding = rounding;
                            if ctx.synced_ui_gen == prev_ui_gen {
                                ctx.synced_ui_gen = self.shared_ui_gen;
                            }
                            ctx.needs_redraw = true;
                        }
                    }
                    let mode_2031 = self.pane_store.apply_theme_all(&self.theme.palette);
                    if !self.read_only {
                        for (key, report_bytes) in mode_2031 {
                            // Never push Mode 2031 reports to closed/stale blocks that are no
                            // longer tracked in the live workspace.
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
                        if contexts.is_empty() {
                            self.push_theme_to_owned_blocks();
                        } else {
                            let mut seen = HashSet::new();
                            let sessions: Vec<SessionKey> = contexts
                                .iter()
                                .filter_map(|c| c.active_session.clone())
                                .filter(|sk| seen.insert(sk.clone()))
                                .collect();
                            for sk in sessions {
                                self.push_theme_to_owned_blocks_for_session(&sk);
                            }
                        }
                    }
                }
            }

            RuntimeEvent::NotificationClicked(target) => {
                let target_sk = SessionKey::new(target.host.clone(), target.session_id.clone());
                let target_idx =
                    find_window_viewing_session(contexts, &target_sk, pref_idx).or(pref_idx);
                self.with_target_window_or_self(contexts, target_idx, |ctrl| {
                    ctrl.focus_target_block(&target);
                    ctrl.needs_redraw = true;
                });
            }

            RuntimeEvent::DirListed {
                key: _,
                request_id,
                result,
            } => {
                let target_idx = if contexts.is_empty() {
                    None
                } else {
                    contexts.iter().position(|c| {
                        c.file_picker_session
                            .as_ref()
                            .is_some_and(|s| s.request_id == request_id)
                    })
                };
                if let Some(idx) = target_idx {
                    self.with_window_context(contexts[idx], |ctrl| {
                        ctrl.apply_dir_listed_result(request_id, result);
                        ctrl.needs_redraw = true;
                    });
                } else if contexts.is_empty() {
                    self.apply_dir_listed_result(request_id, result);
                }
            }

            RuntimeEvent::ClipboardReadFinished { key, id: _, result } => {
                if !self.clipboard_enabled || self.read_only {
                    return;
                }
                let sk = SessionKey::new(key.host_id.clone(), key.session_id.clone());
                let toast_idx = find_window_viewing_session(contexts, &sk, pref_idx).or(pref_idx);
                match result {
                    Ok(text) => {
                        if self.apply_clipboard_with_dedup(&key, &text, now) {
                            self.push_toast_on_window(
                                contexts,
                                toast_idx,
                                ToastSeverity::Info,
                                "Remote clipboard copied",
                                format!("Copied {} bytes via OSC 52", text.len()),
                            );
                        }
                    }
                    Err(err) => {
                        self.push_toast_on_window(
                            contexts,
                            toast_idx,
                            ToastSeverity::Warning,
                            "Clipboard read failed",
                            err,
                        );
                    }
                }
            }

            RuntimeEvent::PortsListed { key, result } => {
                if let Ok(resp) = result
                    && let Some(blk) = self.workspace.block_mut(&key)
                {
                    blk.apply_ports_response(resp);
                }
            }

            RuntimeEvent::TunnelStarted {
                tunnel_id,
                key,
                tunnel_block_id,
                remote_port,
                local_port,
            } => {
                if let Some(tun) = self
                    .active_tunnels
                    .iter_mut()
                    .find(|t| t.tunnel_id == tunnel_id)
                {
                    tun.tunnel_block_id = Some(tunnel_block_id);
                    tun.local_port = Some(local_port);
                    tun.error = None;
                }
                let url = format!("http://127.0.0.1:{local_port}");
                if !self.read_only {
                    let _ = open::that_detached(&url);
                }
                let sk = SessionKey::new(key.host_id, key.session_id);
                let toast_idx = find_window_viewing_session(contexts, &sk, pref_idx).or(pref_idx);
                self.push_toast_on_window(
                    contexts,
                    toast_idx,
                    ToastSeverity::Info,
                    "Port forwarded",
                    format!("127.0.0.1:{local_port} → remote :{remote_port}"),
                );
            }

            RuntimeEvent::TunnelFailed {
                tunnel_id,
                key,
                remote_port,
                error,
            } => {
                self.active_tunnels.retain(|t| t.tunnel_id != tunnel_id);
                let sk = SessionKey::new(key.host_id, key.session_id);
                let toast_idx = find_window_viewing_session(contexts, &sk, pref_idx).or(pref_idx);
                self.push_toast_on_window(
                    contexts,
                    toast_idx,
                    ToastSeverity::Warning,
                    format!("Forward :{remote_port} failed"),
                    error,
                );
            }
        }
    }

    pub(super) fn apply_viewer_download_result(
        &mut self,
        host_id: String,
        pending: PendingRemoteViewerDownload,
        result: Result<DownloadResult, String>,
    ) {
        match result {
            Ok(DownloadResult::Downloaded {
                remote_path,
                local_path,
                version,
                ..
            }) => {
                self.downloaded_file_cache.insert(
                    (host_id, remote_path.clone()),
                    (local_path.clone(), version),
                );
                self.media_backend.ingest_remote_file(
                    pending.tab_id,
                    remote_path,
                    local_path,
                    pending.kind,
                );
            }
            Ok(DownloadResult::NotModified { remote_path, .. }) => {
                if let Some((cached_path, _)) = self
                    .downloaded_file_cache
                    .get(&(host_id, remote_path.clone()))
                    .cloned()
                    && cached_path.exists()
                {
                    self.media_backend.ingest_remote_file(
                        pending.tab_id,
                        remote_path,
                        cached_path,
                        pending.kind,
                    );
                } else {
                    self.ui.viewer.set_tab_error(
                        pending.tab_id,
                        format!("Cached remote file missing for {}", pending.remote_path),
                    );
                }
            }
            Err(err) => {
                self.ui.viewer.set_tab_error(pending.tab_id, err.clone());
                self.push_toast(ToastSeverity::Error, "Download failed", err);
            }
        }
    }

    fn apply_dir_listed_result(
        &mut self,
        request_id: u64,
        result: Result<rex_proto::types::TerminalListDirResponse, String>,
    ) {
        let Some(sess) = self.file_picker_session.as_mut() else {
            return;
        };
        if sess.request_id != request_id {
            return;
        }
        let purpose = sess.purpose;
        match result {
            Ok(resp) => {
                if let Some(err) = list_dir_status_error(resp.status) {
                    if let Some(modal) = &mut self.ui.modal {
                        modal.set_file_picker_error(err);
                    }
                } else {
                    let resolved_dir = if resp.path.is_empty() {
                        sess.current_dir.clone()
                    } else {
                        resp.path.clone()
                    };
                    sess.current_dir = resolved_dir.clone();
                    let entries = file_picker_entries_from_list_dir(&resolved_dir, &resp, purpose);
                    if let Some(modal) = &mut self.ui.modal {
                        modal.set_file_picker_entries(resolved_dir, entries, None);
                    }
                }
            }
            Err(err) => {
                if let Some(modal) = &mut self.ui.modal {
                    modal.set_file_picker_error(err);
                }
            }
        }
    }

    fn mark_redraw_all(&mut self, contexts: &mut [&mut WindowContext]) {
        if contexts.is_empty() {
            self.needs_redraw = true;
        } else {
            for ctx in contexts {
                ctx.needs_redraw = true;
            }
        }
    }

    fn push_toast_on_window(
        &mut self,
        contexts: &mut [&mut WindowContext],
        target_idx: Option<usize>,
        severity: ToastSeverity,
        title: impl Into<String>,
        body: impl Into<String>,
    ) {
        let title = title.into();
        let body = body.into();
        if let Some(idx) = target_idx.filter(|&i| i < contexts.len()) {
            self.with_window_context(contexts[idx], |ctrl| {
                ctrl.push_toast(severity, title, body);
                ctrl.needs_redraw = true;
            });
        } else if contexts.is_empty() {
            self.push_toast(severity, title, body);
        }
    }

    fn with_target_window_or_self<R>(
        &mut self,
        contexts: &mut [&mut WindowContext],
        target_idx: Option<usize>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> Option<R> {
        if let Some(idx) = target_idx.filter(|&i| i < contexts.len()) {
            Some(self.with_window_context(contexts[idx], f))
        } else if contexts.is_empty() {
            Some(f(self))
        } else {
            None
        }
    }

    fn remove_closed_block(&mut self, key: &BlockKey, contexts: &mut [&mut WindowContext]) {
        self.pane_store.remove_block(key, None);
        let had_stream = self.opened_streams.remove(key);
        self.hydrated_blocks.remove(key);
        self.block_activity_at.remove(key);
        self.block_updated_unix.remove(key);
        self.block_osc94.remove(key);
        self.program_chime_state.remove(key);
        self.ports_queried_blocks.remove(key);
        self.teardown_tunnels_for_block(key);
        if had_stream {
            self.send_command(HostCommand::CloseBlockStream {
                host_id: key.host_id.clone(),
                session_id: key.session_id.clone(),
                block_id: key.block_id.clone(),
            });
        }
        let wait_after = self
            .command_launch
            .as_ref()
            .is_some_and(|c| c.wait_after_command);
        if contexts.is_empty() {
            if self
                .copy_mode
                .as_ref()
                .is_some_and(|cm| &cm.block_key == key)
            {
                self.copy_mode = None;
            }
            if self
                .ephemeral_session
                .as_ref()
                .is_some_and(|sk| sk.host_id == key.host_id && sk.session_id == key.session_id)
                && !wait_after
            {
                self.should_exit = true;
            }
        } else {
            for ctx in contexts.iter_mut() {
                if ctx
                    .copy_mode
                    .as_ref()
                    .is_some_and(|cm| &cm.block_key == key)
                {
                    ctx.copy_mode = None;
                }
                if ctx
                    .ephemeral_session
                    .as_ref()
                    .is_some_and(|sk| sk.host_id == key.host_id && sk.session_id == key.session_id)
                    && !wait_after
                {
                    ctx.close_requested = true;
                }
            }
        }
    }

    fn handle_control_event_multi(
        &mut self,
        host_id: &str,
        event: ServerEvent,
        contexts: &mut [&mut WindowContext],
        pref_idx: Option<usize>,
        now: Instant,
    ) {
        match event {
            ServerEvent::SessionCreated { session } => {
                let new_sid = session.session_id.clone();
                if let Some(host) = self.workspace.host_mut(host_id) {
                    host.on_session_created(session);
                }
                let no_active = if contexts.is_empty() {
                    self.workspace.active_session.is_none()
                } else {
                    contexts.iter().all(|c| c.active_session.is_none())
                };
                if no_active && self.command_launch.is_none() {
                    let target_idx = if contexts.is_empty() {
                        None
                    } else {
                        contexts
                            .iter()
                            .position(|c| c.active_session.is_none())
                            .or(pref_idx)
                    };
                    self.with_target_window_or_self(contexts, target_idx, |ctrl| {
                        ctrl.activate_session(
                            SessionKey::new(host_id.to_owned(), new_sid),
                            &HashMap::new(),
                        );
                    });
                }
            }
            ServerEvent::SessionDestroyed { session_id } => {
                self.cleanup_removed_session_multi(host_id, &session_id, contexts);
            }
            ServerEvent::SessionMoved {
                session_id,
                before_session_id,
            } => {
                if let Some(host) = self.workspace.host_mut(host_id) {
                    host.on_session_moved(&session_id, before_session_id.as_deref());
                }
            }
            ServerEvent::KeymapChanged { generation, hash } => {
                if keymap_changed_needs_refresh(&self.actions, generation, &hash) {
                    self.send_command(HostCommand::RefreshKeymap {
                        host_id: host_id.to_owned(),
                        request: build_proto_keymap_request(&self.actions),
                    });
                }
            }
            ServerEvent::SessionEvent {
                session_id,
                event: sess_ev,
            } => {
                if let SessionEvent::BlockClosed { ref block_id } = sess_ev {
                    let key = BlockKey::new(host_id, &session_id, block_id);
                    if self.pending_transfer_blocks.contains(&key) {
                        self.transfer_closed_source_blocks.insert(key);
                    } else {
                        self.remove_closed_block(&key, contexts);
                    }
                }
                if let SessionEvent::BlockEvent(ref blk_ev) = sess_ev {
                    let key = BlockKey::new(host_id, &session_id, &blk_ev.block_id);
                    self.block_activity_at.insert(key.clone(), now);
                    if blk_ev.name == "child_exited" {
                        let wait_after = self
                            .command_launch
                            .as_ref()
                            .is_some_and(|c| c.wait_after_command);
                        if !wait_after {
                            if contexts.is_empty() {
                                if self.ephemeral_session.as_ref().is_some_and(|sk| {
                                    sk.host_id == host_id && sk.session_id == session_id
                                }) {
                                    self.should_exit = true;
                                }
                            } else {
                                for ctx in contexts.iter_mut() {
                                    if ctx.ephemeral_session.as_ref().is_some_and(|sk| {
                                        sk.host_id == host_id && sk.session_id == session_id
                                    }) {
                                        ctx.close_requested = true;
                                    }
                                }
                            }
                        }
                    } else if blk_ev.name == "clipboard_written" {
                        self.handle_clipboard_written_block_event(&key, &blk_ev.payload, now);
                    } else if blk_ev.name == "ports_changed"
                        && let Ok(resp) =
                            serde_json::from_value::<TerminalPortsResponse>(blk_ev.payload.clone())
                        && resp.truncated.unwrap_or(false)
                        && let Some(blob_id) = resp.id
                    {
                        self.send_command(HostCommand::ReadPortsBlob {
                            key: key.clone(),
                            blob_id,
                        });
                    }
                }
                let owner_change = match &sess_ev {
                    SessionEvent::BlockOwnerChanged {
                        block_id, owner, ..
                    } => Some((block_id.clone(), owner.clone())),
                    _ => None,
                };
                let program_status_block_key = match &sess_ev {
                    SessionEvent::BlockEvent(blk_ev) if blk_ev.name == "program_status_changed" => {
                        Some(BlockKey::new(host_id, &session_id, &blk_ev.block_id))
                    }
                    _ => None,
                };
                let needs_view = self
                    .workspace
                    .apply_session_event(host_id, &session_id, &sess_ev);
                if let Some(ps_key) = program_status_block_key
                    && let Some(blk) = self.workspace.block(&ps_key)
                {
                    let records: Vec<tr3x_bridge::ProgramStatusRecord> = blk
                        .program_statuses
                        .iter()
                        .map(|r| program_status_from_proto(r, false))
                        .collect();
                    if let Some(rolled) = rollup_records(&records) {
                        let st = map_tr3x_state_to_vt(rolled.state);
                        let muted = self.settings.is_muted(
                            &ps_key.host_id,
                            &ps_key.session_id,
                            &ps_key.block_id,
                        );
                        self.maybe_trigger_osc7501_chime(&ps_key, st, muted, now);
                    }
                }
                if let Some((block_id, new_owner)) = owner_change {
                    let key = BlockKey::new(host_id, &session_id, &block_id);
                    let my_client_id = self
                        .workspace
                        .host(host_id)
                        .and_then(HostEntry::client_id)
                        .map(str::to_owned);
                    if contexts.is_empty() {
                        let is_visible = self.visible_pane_keys.contains(&key)
                            || self.focused_block_key().as_ref() == Some(&key);
                        if is_visible {
                            self.apply_owner_change_for_visible(
                                &key,
                                my_client_id.as_deref(),
                                new_owner.as_deref(),
                            );
                        }
                    } else {
                        for ctx in contexts.iter_mut() {
                            let is_visible = ctx.visible_pane_keys.contains(&key)
                                || focused_block_key_for_ctx(&self.workspace, ctx).as_ref()
                                    == Some(&key);
                            if is_visible {
                                self.with_window_context(ctx, |ctrl| {
                                    ctrl.apply_owner_change_for_visible(
                                        &key,
                                        my_client_id.as_deref(),
                                        new_owner.as_deref(),
                                    );
                                });
                            }
                        }
                    }
                }
                if needs_view {
                    self.send_command(HostCommand::FetchView {
                        host_id: host_id.to_owned(),
                        session_id,
                    });
                }
            }
            ServerEvent::TailscaleStatusChanged(ts) => {
                if let Some(host) = self.workspace.host_mut(host_id) {
                    host.tailscale_status = Some(ts);
                }
            }
            ServerEvent::EventDropped { .. } => {
                if let Some(host) = self.workspace.host_mut(host_id) {
                    host.dropped_events_count = host.dropped_events_count.saturating_add(1);
                }
            }
            _ => {}
        }
    }

    fn apply_owner_change_for_visible(
        &mut self,
        key: &BlockKey,
        my_client_id: Option<&str>,
        new_owner: Option<&str>,
    ) {
        if my_client_id.is_some() && new_owner == my_client_id {
            // Ownership just transferred to us: immediately reconcile grid size & theme!
            self.workspace.invalidate_sent_resize(key);
            self.force_immediate_resize = true;
            if !self.read_only {
                self.send_command(HostCommand::SetTheme {
                    host_id: key.host_id.clone(),
                    session_id: key.session_id.clone(),
                    block_id: key.block_id.clone(),
                    args: theme_palette_to_rex_args(&self.theme.palette),
                });
            }
        } else if new_owner.is_none() && !self.read_only {
            // Another window closed/disowned this visible block: immediately reclaim!
            self.workspace.invalidate_sent_resize(key);
            self.force_immediate_resize = true;
            let trigger = if self.window_focused {
                OwnershipTrigger::ExplicitInteractOrFit
            } else {
                OwnershipTrigger::AttachOrView
            };
            self.maybe_claim_ownership(key, trigger);
        }
    }

    fn handle_data_stream_event_multi(
        &mut self,
        event: DataStreamEvent,
        contexts: &mut [&mut WindowContext],
        now: Instant,
    ) {
        let key = BlockKey::new(
            event.host_id.clone(),
            event.session_id.clone(),
            event.block_id.clone(),
        );
        let sk = SessionKey::new(key.host_id.clone(), key.session_id.clone());
        if matches!(
            event.kind,
            DataStreamEventKind::LiveVtChunk { .. } | DataStreamEventKind::SnapshotChunk { .. }
        ) {
            self.block_activity_at.insert(key.clone(), now);
        }

        match self
            .pane_store
            .handle_data_stream_event(event, &mut self.workspace)
        {
            Ok(outcome) => {
                let has_important_event = !outcome.terminal_events.is_empty()
                    || outcome.became_ready
                    || outcome.stream_closed.is_some()
                    || outcome.rex_apc.is_some()
                    || outcome.stream_error.is_some();
                if contexts.is_empty() {
                    let is_visible =
                        self.visible_pane_keys.is_empty() || self.visible_pane_keys.contains(&key);
                    if is_visible || has_important_event {
                        self.needs_redraw = true;
                    }
                } else {
                    for ctx in contexts.iter_mut() {
                        let is_visible = ctx.visible_pane_keys.is_empty()
                            || ctx.visible_pane_keys.contains(&key);
                        if is_visible
                            || (has_important_event
                                && (ctx.active_session.as_ref() == Some(&sk)
                                    || ctx.visible_pane_keys.is_empty()))
                        {
                            ctx.needs_redraw = true;
                        }
                    }
                }
                if let Some(resize_args) = outcome.pending_resize
                    && !self.read_only
                {
                    // Request only (see `flush_ready_resizes`): never steal from another device.
                    self.maybe_claim_ownership(&key, OwnershipTrigger::AttachOrView);
                    self.send_command(HostCommand::TerminalResize {
                        host_id: key.host_id.clone(),
                        session_id: key.session_id.clone(),
                        block_id: key.block_id.clone(),
                        args: resize_args,
                    });
                }
                if let Some(vt_engine::RexControlApc::Theme {
                    background, scheme, ..
                }) = outcome.rex_apc.as_ref()
                {
                    let apc_is_dark = match scheme.as_deref() {
                        Some(s) if s.eq_ignore_ascii_case("light") => Some(false),
                        Some(s) if s.eq_ignore_ascii_case("dark") => Some(true),
                        _ => background
                            .as_deref()
                            .and_then(|hex| omarchy_theme::Rgb::from_hex(hex).ok())
                            .map(|rgb| rgb.relative_luminance() < 0.25),
                    };
                    if self.read_only || apc_is_dark.is_some_and(|dark| dark != self.theme.is_dark)
                    {
                        let _ = self.pane_store.apply_theme_all(&self.theme.palette);
                    }
                }
                if let Some(vt_engine::RexControlApc::ChildExited { .. }) = outcome.rex_apc.as_ref()
                    && self
                        .command_launch
                        .as_ref()
                        .is_none_or(|c| !c.wait_after_command)
                {
                    if contexts.is_empty() {
                        if self.ephemeral_session.as_ref() == Some(&sk) {
                            self.should_exit = true;
                        }
                    } else {
                        for ctx in contexts.iter_mut() {
                            if ctx.ephemeral_session.as_ref() == Some(&sk) {
                                ctx.close_requested = true;
                            }
                        }
                    }
                }
                if outcome.became_ready && self.read_only {
                    let _ = self.pane_store.apply_theme_all(&self.theme.palette);
                }
                if !outcome.terminal_events.is_empty() {
                    let is_focused_anywhere = if contexts.is_empty() {
                        self.window_focused && self.focused_block_key().as_ref() == Some(&key)
                    } else {
                        contexts.iter().any(|c| {
                            c.window_focused
                                && focused_block_key_for_ctx(&self.workspace, c).as_ref()
                                    == Some(&key)
                        })
                    };
                    self.handle_terminal_events_with_focus(
                        &key,
                        &outcome.terminal_events,
                        is_focused_anywhere,
                        now,
                    );
                }
                if outcome.dirty || outcome.rex_apc.is_some() {
                    if contexts.is_empty() {
                        if self
                            .copy_mode
                            .as_ref()
                            .is_some_and(|cm| cm.block_key == key)
                        {
                            let _ = self.clamp_copy_mode_to_bounds();
                        }
                    } else {
                        for ctx in contexts.iter_mut() {
                            if ctx.copy_mode.as_ref().is_some_and(|cm| cm.block_key == key) {
                                self.with_window_context(ctx, |ctrl| {
                                    let _ = ctrl.clamp_copy_mode_to_bounds();
                                });
                            }
                        }
                    }
                }
                if outcome.stream_closed.is_some() {
                    self.opened_streams.remove(&key);
                }
                if let Some(err) = outcome.stream_error {
                    tracing::warn!(block = %key.block_id, error = %err, "data stream error");
                    // Always release `opened_streams` on stream error (and drop any non-ready
                    // placeholder pane) so a transient failure never permanently wedges the pane
                    // into a black screen!
                    self.opened_streams.remove(&key);
                    if !self.pane_store.is_ready(&key) {
                        self.pane_store.remove_block(&key, None);
                    }
                    self.mark_redraw_all(contexts);
                }
            }
            Err(err) => {
                tracing::warn!(?err, block = %key.block_id, "PaneStore failed to process DataStreamEvent");
            }
        }
    }

    pub(super) fn handle_terminal_events(
        &mut self,
        key: &BlockKey,
        events: &[TerminalEvent],
        now: Instant,
    ) {
        let is_focused_pane = self.window_focused && self.focused_block_key().as_ref() == Some(key);
        self.handle_terminal_events_with_focus(key, events, is_focused_pane, now);
    }

    fn handle_terminal_events_with_focus(
        &mut self,
        key: &BlockKey,
        events: &[TerminalEvent],
        is_focused_pane: bool,
        now: Instant,
    ) {
        let muted = self
            .settings
            .is_muted(&key.host_id, &key.session_id, &key.block_id);

        for ev in events {
            match ev {
                TerminalEvent::ProgressReport { state, progress } => match state {
                    ProgressState::Remove => {
                        self.block_osc94.remove(key);
                    }
                    ProgressState::Set => {
                        self.block_osc94.insert(
                            key.clone(),
                            Osc94Progress {
                                state: Osc94State::Normal,
                                percent: progress.unwrap_or(0),
                            },
                        );
                    }
                    ProgressState::Error => {
                        self.block_osc94.insert(
                            key.clone(),
                            Osc94Progress {
                                state: Osc94State::Error,
                                percent: progress.unwrap_or(100),
                            },
                        );
                    }
                    ProgressState::Indeterminate => {
                        self.block_osc94.insert(
                            key.clone(),
                            Osc94Progress {
                                state: Osc94State::Indeterminate,
                                percent: 0,
                            },
                        );
                    }
                    ProgressState::Pause => {
                        self.block_osc94.insert(
                            key.clone(),
                            Osc94Progress {
                                state: Osc94State::Paused,
                                percent: progress.unwrap_or(0),
                            },
                        );
                    }
                },
                TerminalEvent::DesktopNotification { title, body } => {
                    if !muted && !is_focused_pane && !self.read_only {
                        let session_label = self
                            .workspace
                            .session(&SessionKey::new(&key.host_id, &key.session_id))
                            .map(|s| s.label.clone())
                            .unwrap_or_else(|| "omarex".to_owned());
                        let formatted_title = if title.is_empty() {
                            session_label
                        } else {
                            format!("{session_label} · {title}")
                        };
                        self.send_command(HostCommand::SendNotification(NotificationRequest::new(
                            NotificationTarget {
                                host: key.host_id.clone(),
                                session_id: key.session_id.clone(),
                                block_id: key.block_id.clone(),
                            },
                            formatted_title,
                            body.clone(),
                            NotificationUrgency::Normal,
                        )));
                    }
                }
                TerminalEvent::ProgramStatus(report) => {
                    self.block_activity_at.insert(key.clone(), now);
                    self.maybe_trigger_osc7501_chime(key, report.state, muted, now);
                    if self.settings.desktop_notifications
                        && matches!(
                            report.state,
                            vt_engine::ProgramStatusState::Blocked
                                | vt_engine::ProgramStatusState::Error
                        )
                        && !muted
                        && !is_focused_pane
                        && !self.read_only
                    {
                        let urgency = if report.state == vt_engine::ProgramStatusState::Error {
                            NotificationUrgency::Critical
                        } else {
                            NotificationUrgency::Normal
                        };
                        let session_label = self
                            .workspace
                            .session(&SessionKey::new(&key.host_id, &key.session_id))
                            .map(|s| s.label.clone())
                            .unwrap_or_else(|| "omarex".to_owned());
                        let headline = if report.title.is_empty() {
                            "Needs attention"
                        } else {
                            report.title.as_str()
                        };
                        self.send_command(HostCommand::SendNotification(NotificationRequest::new(
                            NotificationTarget {
                                host: key.host_id.clone(),
                                session_id: key.session_id.clone(),
                                block_id: key.block_id.clone(),
                            },
                            format!("{session_label} · {headline}"),
                            report.message.clone(),
                            urgency,
                        )));
                    }
                }
                TerminalEvent::ClipboardWrite { entries, .. } => {
                    if self.clipboard_enabled
                        && !self.read_only
                        && let Some(text_entry) =
                            entries.iter().find(|e| e.mime.starts_with("text/"))
                        && let Ok(text) = std::str::from_utf8(&text_entry.data)
                    {
                        self.apply_clipboard_with_dedup(key, text, now);
                    }
                }
                _ => {}
            }
        }
    }

    fn pick_initial_session_id(
        &self,
        host_id: &str,
        server_selected: Option<&str>,
    ) -> Option<String> {
        let host = self.workspace.host(host_id)?;
        // 1. Explicit CLI `--session <name|id>` filter.
        if let Some(pref) = &self.preferred_session_filter
            && let Some(found) = host
                .sessions
                .values()
                .find(|s| s.session_id == *pref || s.label.eq_ignore_ascii_case(pref))
        {
            return Some(found.session_id.clone());
        }
        // 2. Last session the user actively worked in (session continuity).
        if let Some(last_sid) = &self.settings.last_session_id
            && host.sessions.contains_key(last_sid)
        {
            return Some(last_sid.clone());
        }
        // 3. Normal (non-T3) Rex terminal session first — Rex is a terminal multiplexer first.
        if let Some(normal_sid) = self.find_preferred_terminal_session(host_id, None) {
            return Some(normal_sid);
        }
        // 4. Server-selected session, or fallback to first session (including `T3 Code`).
        if let Some(sel) = server_selected
            && host.sessions.contains_key(sel)
        {
            return Some(sel.to_owned());
        }
        host.session_order.first().cloned()
    }

    pub(super) fn activate_session(
        &mut self,
        target: SessionKey,
        tickets: &HashMap<(String, String), String>,
    ) {
        let plan = self.workspace.select_session(target.clone());
        for evicted in plan.evicted_sessions {
            self.pane_store
                .evict_session(&evicted.host_id, &evicted.session_id, None);
            self.opened_streams
                .retain(|k| !(k.host_id == evicted.host_id && k.session_id == evicted.session_id));
            self.send_command(HostCommand::EvictSessionStreams {
                host_id: evicted.host_id,
                session_id: evicted.session_id,
                disown: !self.read_only,
            });
        }

        let has_view = self
            .workspace
            .session(&target)
            .is_some_and(|s| s.view.is_some());
        if plan.needs_attach || !has_view {
            self.send_command(HostCommand::SelectSession {
                host_id: target.host_id.clone(),
                session_id: target.session_id.clone(),
            });
        }

        self.settings.last_host_id = Some(target.host_id.clone());
        self.settings.last_session_id = Some(target.session_id.clone());

        let focused = self
            .workspace
            .session(&target)
            .and_then(|s| s.view.as_ref())
            .and_then(active_window_of_view)
            .and_then(primary_block_id_of_window)
            .map(str::to_owned);
        self.ui.on_session_view_changed(focused.clone());
        self.record_nav(target.host_id.clone(), target.session_id.clone(), focused);
        let has_ticket_for_target = tickets.keys().any(|(sid, _)| sid == &target.session_id);
        if !plan.needs_attach || has_ticket_for_target {
            self.open_active_window_streams(&target, tickets);
        }
    }

    /// Evicts any zombie panes/streams in `sk` that were pruned from `SessionView` (preventing
    /// `TerminalWrite` 404 errors on theme changes and stale data-stream retries).
    fn prune_stale_session_blocks(&mut self, sk: &SessionKey) {
        let Some(live_ids): Option<HashSet<String>> = self
            .workspace
            .session(sk)
            .map(|s| s.blocks.keys().cloned().collect())
        else {
            return;
        };

        let mut stale: Vec<BlockKey> = self
            .pane_store
            .pane_keys()
            .into_iter()
            .filter(|k| {
                k.host_id == sk.host_id
                    && k.session_id == sk.session_id
                    && !live_ids.contains(&k.block_id)
                    && !self.pending_transfer_blocks.contains(k)
            })
            .collect();
        for k in &self.opened_streams {
            if k.host_id == sk.host_id
                && k.session_id == sk.session_id
                && !live_ids.contains(&k.block_id)
                && !self.pending_transfer_blocks.contains(k)
                && !stale.contains(k)
            {
                stale.push(k.clone());
            }
        }

        for key in stale {
            self.pane_store.remove_block(&key, None);
            let had_stream = self.opened_streams.remove(&key);
            self.hydrated_blocks.remove(&key);
            self.block_activity_at.remove(&key);
            self.block_updated_unix.remove(&key);
            self.block_osc94.remove(&key);
            if had_stream {
                self.send_command(HostCommand::CloseBlockStream {
                    host_id: key.host_id,
                    session_id: key.session_id,
                    block_id: key.block_id,
                });
            }
        }
    }

    /// Cleans up all local panes, stream tasks, and metadata for a destroyed or missing session.
    fn cleanup_removed_session_multi(
        &mut self,
        host_id: &str,
        session_id: &str,
        contexts: &mut [&mut WindowContext],
    ) {
        self.collapsed_sessions
            .remove(&(host_id.to_owned(), session_id.to_owned()));
        let wait_after = self
            .command_launch
            .as_ref()
            .is_some_and(|c| c.wait_after_command);
        if contexts.is_empty() {
            if self
                .ephemeral_session
                .as_ref()
                .is_some_and(|sk| sk.host_id == host_id && sk.session_id == session_id)
            {
                self.ephemeral_session = None;
                if !wait_after {
                    self.should_exit = true;
                }
            }
            if self.copy_mode.as_ref().is_some_and(|cm| {
                cm.block_key.host_id == host_id && cm.block_key.session_id == session_id
            }) {
                self.copy_mode = None;
            }
            self.needs_redraw = true;
        } else {
            for ctx in contexts.iter_mut() {
                if ctx
                    .ephemeral_session
                    .as_ref()
                    .is_some_and(|sk| sk.host_id == host_id && sk.session_id == session_id)
                {
                    ctx.ephemeral_session = None;
                    if !wait_after {
                        ctx.close_requested = true;
                    }
                }
                if ctx.copy_mode.as_ref().is_some_and(|cm| {
                    cm.block_key.host_id == host_id && cm.block_key.session_id == session_id
                }) {
                    ctx.copy_mode = None;
                }
                if ctx
                    .active_session
                    .as_ref()
                    .is_some_and(|sk| sk.host_id == host_id && sk.session_id == session_id)
                {
                    ctx.active_session = None;
                }
                ctx.needs_redraw = true;
            }
        }
        let had_streams = self
            .opened_streams
            .iter()
            .any(|k| k.host_id == host_id && k.session_id == session_id);
        self.pane_store.evict_session(host_id, session_id, None);
        self.opened_streams
            .retain(|k| !(k.host_id == host_id && k.session_id == session_id));
        self.hydrated_blocks
            .retain(|k| !(k.host_id == host_id && k.session_id == session_id));
        self.block_activity_at
            .retain(|k, _| !(k.host_id == host_id && k.session_id == session_id));
        self.block_updated_unix
            .retain(|k, _| !(k.host_id == host_id && k.session_id == session_id));
        self.block_osc94
            .retain(|k, _| !(k.host_id == host_id && k.session_id == session_id));
        self.program_chime_state
            .retain(|k, _| !(k.host_id == host_id && k.session_id == session_id));
        self.ports_queried_blocks
            .retain(|k| !(k.host_id == host_id && k.session_id == session_id));
        self.pending_transfer_blocks
            .retain(|k| !(k.host_id == host_id && k.session_id == session_id));
        self.transfer_closed_source_blocks
            .retain(|k| !(k.host_id == host_id && k.session_id == session_id));
        self.teardown_tunnels_for_session(host_id, session_id);
        if had_streams {
            self.send_command(HostCommand::EvictSessionStreams {
                host_id: host_id.to_owned(),
                session_id: session_id.to_owned(),
                disown: false,
            });
        }
        self.workspace.on_session_destroyed(host_id, session_id);
    }

    pub(super) fn open_active_window_streams(
        &mut self,
        sk: &SessionKey,
        tickets: &HashMap<(String, String), String>,
    ) {
        let block_ids: Vec<String> = self
            .workspace
            .session(sk)
            .and_then(|s| s.view.as_ref())
            .and_then(active_window_of_view)
            .map(|win| {
                win.layers
                    .iter()
                    .flat_map(|l| l.blocks.iter())
                    .filter_map(|b| b.block_id.clone())
                    .collect()
            })
            .unwrap_or_default();

        for block_id in block_ids {
            let key = BlockKey::new(sk.host_id.clone(), sk.session_id.clone(), block_id.clone());
            if self.opened_streams.insert(key.clone()) {
                let ticket = tickets
                    .get(&(sk.session_id.clone(), block_id.clone()))
                    .cloned();
                self.send_command(HostCommand::OpenBlockStream {
                    host_id: key.host_id,
                    session_id: key.session_id,
                    block_id: key.block_id,
                    ticket,
                });
            }
        }
    }

    fn maybe_hydrate_session_metadata(&mut self, sk: &SessionKey) {
        let Some(sess) = self.workspace.session(sk) else {
            return;
        };
        let mut to_hydrate = Vec::new();
        for block_id in sess.all_block_ids() {
            let key = BlockKey::new(sk.host_id.clone(), sk.session_id.clone(), block_id.clone());
            if self.hydrated_blocks.insert(key) {
                to_hydrate.push(block_id);
            }
        }
        if !to_hydrate.is_empty() {
            self.send_command(HostCommand::HydrateSessionMetadata {
                host_id: sk.host_id.clone(),
                session_id: sk.session_id.clone(),
                block_ids: to_hydrate,
            });
        }
    }

    #[track_caller]
    pub(super) fn maybe_claim_ownership(&mut self, key: &BlockKey, trigger: OwnershipTrigger) {
        if self.read_only {
            return;
        }
        if let OwnershipDecision::Own { mode } = self.workspace.decide_ownership(key, trigger) {
            tracing::debug!(
                target: "omarex::ownership",
                block = %key.block_id,
                ?mode,
                ?trigger,
                caller = %std::panic::Location::caller(),
                window_focused = self.window_focused,
                "claiming block ownership"
            );
            self.send_command(HostCommand::OwnBlock {
                host_id: key.host_id.clone(),
                session_id: key.session_id.clone(),
                block_ids: Some(vec![key.block_id.clone()]),
                mode,
            });
        }
    }

    /// Local pointer activity over a pane (motion, wheel, click) means the user is at this
    /// device now: take the block back from another device, like Rex.app does on hover.
    /// Without this, a block last used on the Mac keeps the Mac's grid size here until the
    /// next keypress. Throttled to one `Take` per block per [`POINTER_TAKE_THROTTLE`] so a
    /// stream of motion events never floods the server while the owner change is in flight.
    #[track_caller]
    pub(super) fn reclaim_on_pointer_activity(&mut self, key: &BlockKey, now: Instant) {
        if self.read_only {
            return;
        }
        if self.last_pointer_take.as_ref().is_some_and(|(k, t)| {
            k == key && now.saturating_duration_since(*t) < POINTER_TAKE_THROTTLE
        }) {
            return;
        }
        if matches!(
            self.workspace
                .decide_ownership(key, OwnershipTrigger::ExplicitInteractOrFit),
            OwnershipDecision::Own { .. }
        ) {
            self.last_pointer_take = Some((key.clone(), now));
            self.maybe_claim_ownership(key, OwnershipTrigger::ExplicitInteractOrFit);
        }
    }

    pub(super) fn push_theme_to_owned_blocks(&mut self) {
        if let Some(sk) = self.workspace.active_session.clone() {
            self.push_theme_to_owned_blocks_for_session(&sk);
        }
    }

    fn push_theme_to_owned_blocks_for_session(&mut self, sk: &SessionKey) {
        let args = theme_palette_to_rex_args(&self.theme.palette);
        let client_id = self
            .workspace
            .host(&sk.host_id)
            .and_then(HostEntry::client_id)
            .map(str::to_owned);
        let targets: Vec<String> = self
            .workspace
            .session(sk)
            .map(|s| {
                s.blocks
                    .iter()
                    .filter(|&(_, b)| {
                        b.owner.is_none() || (client_id.is_some() && b.owner == client_id)
                    })
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default();

        for block_id in targets {
            self.send_command(HostCommand::SetTheme {
                host_id: sk.host_id.clone(),
                session_id: sk.session_id.clone(),
                block_id,
                args: args.clone(),
            });
        }
    }

    fn focus_target_block(&mut self, target: &NotificationTarget) {
        self.set_session_collapsed(&target.host, &target.session_id, false);
        let sk = SessionKey::new(target.host.clone(), target.session_id.clone());
        self.activate_session(sk, &HashMap::new());
        self.send_command(HostCommand::FocusBlock {
            host_id: target.host.clone(),
            session_id: target.session_id.clone(),
            block_id: target.block_id.clone(),
        });
        self.ui.focus.focus_terminal(Some(target.block_id.clone()));
        self.force_immediate_resize = true;
        let key = BlockKey::new(&target.host, &target.session_id, &target.block_id);
        self.maybe_claim_ownership(&key, OwnershipTrigger::ExplicitInteractOrFit);
    }

    fn record_nav(&mut self, host_id: String, session_id: String, block_id: Option<String>) {
        let entry = (host_id, session_id, block_id);
        if self.nav_history.get(self.nav_index) == Some(&entry) {
            return;
        }
        if self.nav_index + 1 < self.nav_history.len() {
            self.nav_history.truncate(self.nav_index + 1);
        }
        self.nav_history.push(entry);
        self.nav_index = self.nav_history.len().saturating_sub(1);
    }

    pub(super) fn apply_clipboard_with_dedup(
        &mut self,
        key: &BlockKey,
        text: &str,
        now: Instant,
    ) -> bool {
        if !self.clipboard_enabled || self.read_only {
            return false;
        }
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        let hash = hasher.finish();
        if let Some((prev_key, prev_hash, prev_at)) = &self.last_clipboard_write
            && prev_key == key
            && *prev_hash == hash
            && now.saturating_duration_since(*prev_at) < Duration::from_millis(250)
        {
            return false;
        }
        self.last_clipboard_write = Some((key.clone(), hash, now));
        set_system_clipboard_text(text);
        true
    }

    pub(super) fn handle_clipboard_written_block_event(
        &mut self,
        key: &BlockKey,
        payload: &serde_json::Value,
        now: Instant,
    ) {
        if !self.clipboard_enabled || self.read_only {
            return;
        }
        let Ok(ev) = serde_json::from_value::<ClipboardWrittenEvent>(payload.clone()) else {
            return;
        };
        if let Some(text) = ev.text {
            let _ = self.apply_clipboard_with_dedup(key, &text, now);
        } else if let Some(id) = ev.id {
            let bytes = ev.bytes.unwrap_or(0);
            if bytes > 0 {
                self.send_command(HostCommand::ReadClipboard {
                    key: key.clone(),
                    clipboard_id: id,
                    expected_bytes: bytes,
                });
            }
        }
    }

    pub(super) fn maybe_trigger_osc7501_chime(
        &mut self,
        key: &BlockKey,
        state: ProgramStatusState,
        muted: bool,
        now: Instant,
    ) {
        let prev = self.program_chime_state.get(key).copied();
        if prev.is_some_and(|(prev_state, _)| prev_state == state) {
            return;
        }
        let kind = match state {
            ProgramStatusState::Done => Some(ChimeKind::Done),
            ProgramStatusState::Blocked | ProgramStatusState::Error => Some(ChimeKind::Attention),
            ProgramStatusState::Idle | ProgramStatusState::Working | ProgramStatusState::Clear => {
                None
            }
        };
        let last_chime_at = prev.map(|(_, t)| t);
        let cooldown_ok = last_chime_at
            .is_none_or(|t| now.saturating_duration_since(t) >= Duration::from_secs(2));
        let should_play = kind.is_some()
            && !self.read_only
            && self.settings.chime_enabled
            && !muted
            && cooldown_ok;

        if should_play && let Some(chime) = kind {
            self.program_chime_state.insert(key.clone(), (state, now));
            self.chime_dispatcher.play(chime);
        } else {
            let ts = last_chime_at.unwrap_or_else(|| now - Duration::from_secs(10));
            self.program_chime_state.insert(key.clone(), (state, ts));
        }
    }

    fn teardown_tunnels_for_block(&mut self, key: &BlockKey) {
        let mut to_stop = Vec::new();
        self.active_tunnels.retain(|t| {
            if &t.source_block == key {
                to_stop.push((
                    t.source_block.host_id.clone(),
                    t.tunnel_session_id.clone(),
                    t.tunnel_block_id.clone(),
                    t.tunnel_id,
                ));
                false
            } else {
                true
            }
        });
        for (host_id, session_id, tunnel_block_id, tunnel_id) in to_stop {
            self.send_command(HostCommand::StopPortTunnel {
                host_id,
                session_id,
                tunnel_block_id,
                tunnel_id,
            });
        }
    }

    fn teardown_tunnels_for_host(&mut self, host_id: &str) {
        let mut to_stop = Vec::new();
        self.active_tunnels.retain(|t| {
            if t.source_block.host_id == host_id {
                to_stop.push((
                    t.source_block.host_id.clone(),
                    t.tunnel_session_id.clone(),
                    t.tunnel_block_id.clone(),
                    t.tunnel_id,
                ));
                false
            } else {
                true
            }
        });
        for (hid, session_id, tunnel_block_id, tunnel_id) in to_stop {
            self.send_command(HostCommand::StopPortTunnel {
                host_id: hid,
                session_id,
                tunnel_block_id,
                tunnel_id,
            });
        }
    }

    fn teardown_tunnels_for_session(&mut self, host_id: &str, session_id: &str) {
        let mut to_stop = Vec::new();
        self.active_tunnels.retain(|t| {
            if t.source_block.host_id == host_id
                && (t.source_block.session_id == session_id || t.tunnel_session_id == session_id)
            {
                to_stop.push((
                    t.source_block.host_id.clone(),
                    t.tunnel_session_id.clone(),
                    t.tunnel_block_id.clone(),
                    t.tunnel_id,
                ));
                false
            } else {
                true
            }
        });
        for (hid, sid, tunnel_block_id, tunnel_id) in to_stop {
            self.send_command(HostCommand::StopPortTunnel {
                host_id: hid,
                session_id: sid,
                tunnel_block_id,
                tunnel_id,
            });
        }
    }
}

fn find_window_viewing_session(
    contexts: &[&mut WindowContext],
    sk: &SessionKey,
    preferred_idx: Option<usize>,
) -> Option<usize> {
    if let Some(pidx) = preferred_idx.filter(|&i| i < contexts.len())
        && contexts[pidx].active_session.as_ref() == Some(sk)
    {
        return Some(pidx);
    }
    contexts
        .iter()
        .position(|c| c.active_session.as_ref() == Some(sk))
}

fn focused_block_key_for_ctx(workspace: &WorkspaceState, ctx: &WindowContext) -> Option<BlockKey> {
    let sk = ctx.active_session.as_ref()?;
    if let Some(block_id) = ctx.ui.focus.last_terminal_block() {
        return Some(BlockKey::new(
            sk.host_id.clone(),
            sk.session_id.clone(),
            block_id.to_owned(),
        ));
    }
    let view = workspace.session(sk)?.view.as_ref()?;
    let win = active_window_of_view(view)?;
    let bid = primary_block_id_of_window(win)?;
    Some(BlockKey::new(
        sk.host_id.clone(),
        sk.session_id.clone(),
        bid.to_owned(),
    ))
}

pub(super) fn list_dir_status_error(status: rex_proto::types::ListDirStatus) -> Option<String> {
    match status {
        rex_proto::types::ListDirStatus::Ok => None,
        rex_proto::types::ListDirStatus::Denied => Some("Permission denied".to_owned()),
        rex_proto::types::ListDirStatus::Missing => Some("Directory does not exist".to_owned()),
        rex_proto::types::ListDirStatus::NotDir => Some("Not a directory".to_owned()),
        rex_proto::types::ListDirStatus::Unknown => {
            Some("Directory listing unavailable".to_owned())
        }
    }
}

fn map_tr3x_state_to_vt(s: tr3x_bridge::ProgramState) -> ProgramStatusState {
    match s {
        tr3x_bridge::ProgramState::Blocked => ProgramStatusState::Blocked,
        tr3x_bridge::ProgramState::Error => ProgramStatusState::Error,
        tr3x_bridge::ProgramState::Done => ProgramStatusState::Done,
        tr3x_bridge::ProgramState::Working | tr3x_bridge::ProgramState::Waiting => {
            ProgramStatusState::Working
        }
        tr3x_bridge::ProgramState::Idle => ProgramStatusState::Idle,
    }
}

pub(super) fn file_picker_entries_from_list_dir(
    base_dir: &str,
    resp: &rex_proto::types::TerminalListDirResponse,
    purpose: FilePickerPurpose,
) -> Vec<FilePickerEntry> {
    let base = Path::new(base_dir);
    let mut entries = Vec::with_capacity(resp.entries.len());
    for e in &resp.entries {
        let is_dir = !e.file;
        if purpose == FilePickerPurpose::GoToDirectory && !is_dir {
            continue;
        }
        let full = if base_dir == "/" {
            format!("/{}", e.name)
        } else {
            base.join(&e.name).to_string_lossy().into_owned()
        };
        entries.push(FilePickerEntry {
            name: e.name.clone(),
            path: full,
            is_dir,
            is_symlink: e.symlink,
        });
    }
    entries
}

/// Zero-dependency RFC 3339 timestamp parser (`YYYY-MM-DDTHH:MM:SS[.frac](Z|+HH:MM|-HH:MM)`),
/// used to convert Rex OSC 7501 `updated_at` strings into exact Unix epoch seconds.
pub(super) fn parse_rfc3339_unix_secs(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.len() < 20 || !s.is_ascii() {
        return None;
    }
    let b = s.as_bytes();
    if b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b't' && b[10] != b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: i64 = s.get(11..13)?.parse().ok()?;
    let minute: i64 = s.get(14..16)?.parse().ok()?;
    let second: i64 = s.get(17..19)?.parse().ok()?;

    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // Find timezone suffix (`Z`, `z`, `+HH:MM`, or `-HH:MM`) after optional fractional seconds.
    let mut tz_idx = 19usize;
    if tz_idx < b.len() && b[tz_idx] == b'.' {
        tz_idx += 1;
        while tz_idx < b.len() && b[tz_idx].is_ascii_digit() {
            tz_idx += 1;
        }
    }
    let tz_offset_secs: i64 = if tz_idx >= b.len() || b[tz_idx] == b'Z' || b[tz_idx] == b'z' {
        0
    } else if (b[tz_idx] == b'+' || b[tz_idx] == b'-')
        && tz_idx + 6 <= b.len()
        && b[tz_idx + 1].is_ascii_digit()
        && b[tz_idx + 2].is_ascii_digit()
        && b[tz_idx + 3] == b':'
        && b[tz_idx + 4].is_ascii_digit()
        && b[tz_idx + 5].is_ascii_digit()
    {
        let sign = if b[tz_idx] == b'+' { 1 } else { -1 };
        let tz_h: i64 = s.get(tz_idx + 1..tz_idx + 3)?.parse().ok()?;
        let tz_m: i64 = s.get(tz_idx + 4..tz_idx + 6)?.parse().ok()?;
        sign * (tz_h * 3600 + tz_m * 60)
    } else {
        return None;
    };

    // Howard Hinnant's civil-to-days algorithm (exact Gregorian days since 1970-01-01).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u64;
    let m = u64::from(month);
    let d = u64::from(day);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + (doe as i64) - 719_468;

    Some(days * 86_400 + hour * 3600 + minute * 60 + second - tz_offset_secs)
}
