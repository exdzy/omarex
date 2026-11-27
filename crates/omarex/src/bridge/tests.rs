// ============================================================================
// Tests (pure unit + live read-only full omarex window Vulkan PNG capture!)
// ============================================================================

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpu_renderer::{CellMetrics, GpuConfig, GpuRenderer, layers};
use omarchy_theme::Theme;
use rex_proto::types::{
    LayoutNode, OwnMode, ServerEvent, SessionEvent, SessionSummary, SessionView, WindowView,
};
use rex_proto::{RexEndpoint, UploadBatch};
use tr3x_bridge::{ClientCommand, Modifiers};
use ui_chrome::{
    FocusTarget, HoverPreviewState, ModalKind, PaneHeaderButton, SettingChange, TargetKind,
    ToastSeverity, ToolbarPosition, UiAction, ViewerContent,
};
use vt_engine::PhysicalKey;

use crate::config::{SavedHost, SidebarPosition, UserSettings};
use crate::host_manager::{
    DataStreamEvent, DataStreamEventKind, HostCommand, LOCAL_HOST_ID, RuntimeEvent,
};
use crate::workspace::{BlockKey, HostEntry, HostStatus, SessionKey};

use super::viewer::resolve_thread_open_targets;
use super::*;
use omarchy_theme::ThemeEvent;
use rex_proto::types::{
    BlockView, ClientActionResponse, ClientPerformRequestedEvent, ClientPromptEndedEvent,
    ClientPromptRequestedEvent, LayerView, Principal, PromptIcon, PromptItem, PromptKind,
    PromptSpec, QueuedClientAction, RequesterInfo,
};

fn make_test_controller() -> OmarexController {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let cfg_path = std::env::temp_dir()
        .join(format!("omarex-bridge-test-{}-{nanos}", std::process::id()))
        .join("settings.json");
    let mut ctrl = OmarexController::new(UserSettings::default(), cfg_path, Theme::default())
        .expect("create controller");
    ctrl.clipboard_enabled = false;
    ctrl
}

fn seed_single_pane_workspace(ctrl: &mut OmarexController) {
    let mut host = HostEntry::new(
        LOCAL_HOST_ID,
        "This Machine",
        RexEndpoint::parse("unix:///tmp/test.sock").unwrap(),
    );
    host.status = HostStatus::Connected {
        client_id: "client:1".into(),
        server_host_id: Some("host:local".into()),
        api_revision: 24,
        resumed: false,
    };
    host.on_session_created(SessionSummary {
        session_id: "session:t3".into(),
        label: "T3 Code".into(),
    });
    ctrl.workspace.add_host(host);
    ctrl.workspace
        .select_session(SessionKey::new(LOCAL_HOST_ID, "session:t3"));

    let view = SessionView {
        revision: 1,
        session_id: Some("session:t3".into()),
        label: "T3 Code".into(),
        active_window_id: Some("window:1".into()),
        windows: vec![WindowView {
            window_id: Some("window:1".into()),
            label: "Build bridge.rs".into(),
            focused_block_id: Some("block:1".into()),
            zoomed_block_id: None,
            tiled_zoomed_block_id: None,
            layers: vec![LayerView {
                layer_id: Some("layer:tiled".into()),
                kind: "tiled".into(),
                bounds: [0.0, 0.0, 1.0, 1.0],
                state: None,
                layout: Some(LayoutNode {
                    rect: [0.0, 0.0, 1.0, 1.0],
                    block_id: Some("block:1".into()),
                    split: None,
                }),
                blocks: vec![BlockView {
                    block_id: Some("block:1".into()),
                    label: "t3:1baa1939/thread-1".into(),
                    creator_name: Some("com.superlogical.terminal".into()),
                    flavor: Some("com.superlogical.terminal.shell".into()),
                    host_id: Some("host:local".into()),
                    owner: Some("client:1".into()),
                }],
            }],
        }],
    };

    ctrl.process_runtime_event(
        RuntimeEvent::SessionViewUpdated {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:t3".into(),
            view: Box::new(view),
            seq: 1,
        },
        Instant::now(),
    );

    let term = vt_engine::Terminal::new(100, 32, vt_engine::TerminalConfig::default())
        .expect("create test terminal");
    term.write(b"\x1b[1;32m[omarex]\x1b[0m bridge test terminal ready\r\n");
    ctrl.pane_store
        .insert_terminal(BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1"), term)
        .expect("insert test terminal");
    ctrl.drain_pending_commands();
}

#[test]
fn registers_all_40_embedded_icons_and_translates_ui_to_vulkan_frame() {
    let _gpu_guard = crate::lock_headless_gpu_test();
    let mut renderer =
        GpuRenderer::new_headless(GpuConfig::default()).expect("headless Vulkan renderer");
    let count = register_all_ui_icons(&mut renderer.atlas_mut());
    assert_eq!(count, 40);

    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    ctrl.set_viewport(1280, 720, 1.0);
    // Open Command Palette over live pane to verify modal Z-ordering & clip balance.
    ctrl.dispatch_ui_action(
        UiAction::OpenModal(ModalKind::CommandPalette),
        Instant::now(),
    );

    let frame = ctrl
        .render_frame(&mut renderer, Instant::now())
        .expect("build composite SceneFrame");
    assert!(frame.layers.contains_key(&layers::CANVAS));
    assert!(frame.layers.contains_key(&layers::TERMINAL_FG));
    assert!(frame.layers.keys().any(|&z| z >= layers::CHROME));

    let (rgba, stats) = renderer
        .render_to_rgba(&frame)
        .expect("render composite frame on Vulkan");
    assert_eq!(rgba.len(), 1280 * 720 * 4);
    assert!(stats.quad_count > 10);
    assert!(stats.glyph_count > 40);
}

#[test]
fn rex_ui_prompt_lifecycle_focus_guard_and_fast_reply() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();

    // 1. Incoming `rex.ui.pick` opens native modal cleanly.
    ctrl.process_runtime_event(
        RuntimeEvent::PromptRequested {
            host_id: LOCAL_HOST_ID.into(),
            seq: 10,
            request: Box::new(ClientPromptRequestedEvent {
                call_id: "call-42".into(),
                action: Some("t3_answer".into()),
                prompt: PromptSpec {
                    kind: PromptKind::Pick,
                    title: "Approve command?".into(),
                    placeholder: Some("Choose action".into()),
                    value: None,
                    confirm: Some("Confirm".into()),
                    multiple: false,
                    allow_other: true,
                    items: vec![
                        PromptItem {
                            id: "approve".into(),
                            title: "Approve".into(),
                            subtitle: Some("cargo test".into()),
                            icon: Some(PromptIcon::Terminal),
                            detail: Some("Allow".into()),
                            selected: false,
                        },
                        PromptItem {
                            id: "decline".into(),
                            title: "Decline".into(),
                            subtitle: None,
                            icon: None,
                            detail: None,
                            selected: false,
                        },
                    ],
                },
                timeout_ms: 30_000,
            }),
        },
        now,
    );
    assert_eq!(
        ctrl.ui.focus.current(),
        &FocusTarget::Modal(ModalKind::RexPrompt)
    );

    // 2. Background `SessionViewUpdated` MUST NOT steal focus from the prompt modal!
    let mut updated_view = ctrl
        .workspace
        .session(&SessionKey::new(LOCAL_HOST_ID, "session:t3"))
        .unwrap()
        .view
        .clone()
        .unwrap();
    updated_view.revision = 2;
    ctrl.process_runtime_event(
        RuntimeEvent::SessionViewUpdated {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:t3".into(),
            view: Box::new(updated_view),
            seq: 11,
        },
        now,
    );
    assert_eq!(
        ctrl.ui.focus.current(),
        &FocusTarget::Modal(ModalKind::RexPrompt)
    );

    // 3. Pressing Enter submits selected item via unordered `HostCommand::ClientReply`.
    ctrl.handle_key(
        &KeyInputEvent {
            physical_key: PhysicalKey::Enter,
            canonical_key: "enter".into(),
            text: None,
            unshifted_codepoint: 13,
            mods: Modifiers::NONE,
            pressed: true,
            repeat: false,
            composing: false,
        },
        now,
    );
    assert!(matches!(ctrl.ui.focus.current(), FocusTarget::Terminal(_)));
    let cmds = ctrl.drain_pending_commands();
    assert!(cmds.iter().any(|c| matches!(
        c,
        HostCommand::ClientReply { request, .. }
            if request.call_id == "call-42" && request.error.is_none()
    )));

    // 4. Multi-client `PromptEnded` dismisses a second prompt cleanly with zero reply.
    ctrl.process_runtime_event(
        RuntimeEvent::PromptRequested {
            host_id: LOCAL_HOST_ID.into(),
            seq: 12,
            request: Box::new(ClientPromptRequestedEvent {
                call_id: "call-43".into(),
                action: Some("t3_rename".into()),
                prompt: PromptSpec {
                    kind: PromptKind::Input,
                    title: "Rename Thread".into(),
                    placeholder: None,
                    value: Some("Old".into()),
                    confirm: None,
                    multiple: false,
                    allow_other: false,
                    items: vec![],
                },
                timeout_ms: 0,
            }),
        },
        now,
    );
    assert!(ctrl.ui.modal.is_some());
    ctrl.process_runtime_event(
        RuntimeEvent::PromptEnded {
            host_id: LOCAL_HOST_ID.into(),
            seq: 13,
            event: ClientPromptEndedEvent {
                call_id: "call-43".into(),
                reason: "answered_elsewhere".into(),
            },
        },
        now,
    );
    assert!(ctrl.ui.modal.is_none());
    assert!(ctrl.drain_pending_commands().is_empty());
}

#[test]
fn keymap_perform_and_settings_atomic_save_work_end_to_end() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();

    // Ctrl+Shift+P opens Command Palette via KeymapEngine.
    ctrl.handle_key(
        &KeyInputEvent {
            physical_key: PhysicalKey::KeyP,
            canonical_key: "p".into(),
            text: None,
            unshifted_codepoint: b'p' as u32,
            mods: Modifiers::CTRL_SHIFT,
            pressed: true,
            repeat: false,
            composing: false,
        },
        now,
    );
    assert_eq!(
        ctrl.ui.focus.current(),
        &FocusTarget::Modal(ModalKind::CommandPalette)
    );
    ctrl.ui.close_modal();

    // Unbound key passes straight to libghostty-vt -> HostCommand::TerminalWrite.
    ctrl.handle_key(
        &KeyInputEvent {
            physical_key: PhysicalKey::KeyA,
            canonical_key: "a".into(),
            text: Some("a".into()),
            unshifted_codepoint: b'a' as u32,
            mods: Modifiers::NONE,
            pressed: true,
            repeat: false,
            composing: false,
        },
        now,
    );
    let cmds = ctrl.drain_pending_commands();
    assert!(cmds.iter().any(|c| matches!(
        c,
        HostCommand::TerminalWrite { data, .. } if data == "a"
    )));

    // PerformRequested (`session.select`) authorizes local UID and replies immediately.
    ctrl.process_runtime_event(
        RuntimeEvent::PerformRequested {
            host_id: LOCAL_HOST_ID.into(),
            seq: 20,
            request: Box::new(ClientPerformRequestedEvent {
                call_id: "perf-1".into(),
                action: "session.select".into(),
                args: Some(serde_json::json!({ "session_id": "session:t3" })),
                origin: None,
                session_id: Some("session:t3".into()),
                block_id: None,
                requester: RequesterInfo {
                    client_id: "cli:1".into(),
                    info: None,
                    principal: Principal {
                        kind: "peer".into(),
                        transport: "unix".into(),
                        display: "local".into(),
                        remote_addr: None,
                        uid: current_process_uid(),
                        gid: None,
                        pid: None,
                        sid: None,
                        user_id: None,
                        login_name: None,
                        node: None,
                        tags: Vec::new(),
                        path: Vec::new(),
                    },
                },
            }),
        },
        now,
    );
    let perf_cmds = ctrl.drain_pending_commands();
    assert!(perf_cmds.iter().any(|c| matches!(
        c,
        HostCommand::ClientReply { request, .. }
            if request.call_id == "perf-1" && request.error.is_none()
    )));

    // ClientActionCompleted inline `client_actions` executes `session.select`.
    ctrl.process_runtime_event(
        RuntimeEvent::ClientActionCompleted {
            host_id: LOCAL_HOST_ID.into(),
            action: "t3_inbox".into(),
            response: ClientActionResponse {
                result: None,
                client_actions: vec![QueuedClientAction {
                    action: "session.select".into(),
                    args: Some(serde_json::json!({
                        "session_id": "session:t3",
                        "block_id": "block:1"
                    })),
                }],
                ordered_after: None,
            },
            seq: 21,
        },
        now,
    );
    assert_eq!(ctrl.ui.focus.last_terminal_block(), Some("block:1"));

    // SettingChange updates UI & atomically persists settings.json.
    ctrl.dispatch_ui_action(
        UiAction::UpdateSetting(SettingChange::ToolbarPosition(ToolbarPosition::Right)),
        now,
    );
    let reloaded = UserSettings::load_from(&ctrl.settings_path).expect("reload saved settings");
    assert_eq!(reloaded.sidebar_position, SidebarPosition::Right);

    // ThemeChanged propagates to live Theme + PaneStore.
    let mut light_pal = omarchy_theme::ThemePalette::default_dark();
    light_pal.mode = omarchy_theme::AppearanceMode::Light;
    ctrl.process_runtime_event(
        RuntimeEvent::ThemeChanged(ThemeEvent::PaletteChanged(light_pal)),
        now,
    );
    assert!(!ctrl.theme.is_dark);

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn viewport_shrink_and_expand_claims_ownership_and_resizes_without_stale_lockout() {
    let _gpu_guard = crate::lock_headless_gpu_test();
    let mut renderer =
        GpuRenderer::new_headless(GpuConfig::default()).expect("headless Vulkan renderer");
    register_all_ui_icons(&mut renderer.atlas_mut());

    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");
    let _ = ctrl.workspace.mark_snapshot_ready(&key);
    // Simulate remote Mac mini currently owning the block (as observed on live server).
    ctrl.workspace
        .note_server_resize(&key, 80, 32, Some("client:mac-mini".into()));
    let t0 = Instant::now();
    // The app has already painted frames before Hyprland resizes the window, so the visible
    // panes are known (only a real window resize may take ownership from another device).
    let _ = ctrl.render_frame(&mut renderer, t0).expect("initial frame");
    let _ = ctrl.drain_pending_commands();

    // 1. Wide viewport (1920x1080): must claim OwnMode::Take and send wide TerminalResize.
    ctrl.set_viewport(1920, 1080, 1.0);
    ctrl.on_viewport_resized(t0);
    let _ = ctrl.render_frame(&mut renderer, t0).expect("wide frame");
    let wide_cmds = ctrl.drain_pending_commands();
    assert!(
        wide_cmds.iter().any(|c| matches!(
            c,
            HostCommand::OwnBlock {
                mode: OwnMode::Take,
                ..
            }
        )),
        "expected OwnMode::Take when resizing focused window, got {wide_cmds:?}"
    );
    let wide_cols = wide_cmds
        .iter()
        .find_map(|c| match c {
            HostCommand::TerminalResize { args, .. } => Some((args.columns, args.rows)),
            _ => None,
        })
        .expect("expected initial wide TerminalResize");
    assert!(
        wide_cols.0 > 140,
        "expected wide viewport columns > 140, got {wide_cols:?}"
    );

    // Simulate Rex streaming back authoritative RexApc::Resize for wide grid.
    ctrl.process_runtime_event(
        RuntimeEvent::DataStream(DataStreamEvent {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:t3".into(),
            block_id: "block:1".into(),
            kind: DataStreamEventKind::RexApc(rex_proto::RexControlMessage::Resize {
                cols: wide_cols.0,
                rows: wide_cols.1,
                cell_width_px: 9,
                cell_height_px: 18,
                owner: Some("client:1".into()),
            }),
        }),
        t0 + Duration::from_millis(5),
    );
    assert_eq!(ctrl.pane_store.size(&key), Some(wide_cols));

    // 2. Side terminal opens in Hyprland -> viewport shrinks to 960x1080.
    let t1 = t0 + Duration::from_millis(50);
    ctrl.set_viewport(960, 1080, 1.0);
    ctrl.on_viewport_resized(t1);
    let _ = ctrl.render_frame(&mut renderer, t1).expect("narrow frame");
    let narrow_cmds = ctrl.drain_pending_commands();
    let narrow_cols = narrow_cmds
        .iter()
        .find_map(|c| match c {
            HostCommand::TerminalResize { args, .. } => Some((args.columns, args.rows)),
            _ => None,
        })
        .expect("expected narrow TerminalResize after Hyprland split");
    assert!(
        narrow_cols.0 < wide_cols.0,
        "expected narrow cols ({}) < wide cols ({})",
        narrow_cols.0,
        wide_cols.0
    );

    // Simulate server resizing block down to narrow width (and side window taking ownership).
    ctrl.process_runtime_event(
        RuntimeEvent::DataStream(DataStreamEvent {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:t3".into(),
            block_id: "block:1".into(),
            kind: DataStreamEventKind::RexApc(rex_proto::RexControlMessage::Resize {
                cols: narrow_cols.0,
                rows: narrow_cols.1,
                cell_width_px: 9,
                cell_height_px: 18,
                owner: Some("client:side-window".into()),
            }),
        }),
        t1 + Duration::from_millis(5),
    );
    assert_eq!(ctrl.pane_store.size(&key), Some(narrow_cols));

    // 3. Side terminal closes -> viewport expands back to 1920x1080.
    // MUST reclaim ownership and resend exact original wide dimensions (never locked out!).
    let t2 = t1 + Duration::from_millis(50);
    ctrl.set_viewport(1920, 1080, 1.0);
    ctrl.on_viewport_resized(t2);
    let _ = ctrl
        .render_frame(&mut renderer, t2)
        .expect("restored wide frame");
    let restored_cmds = ctrl.drain_pending_commands();
    assert!(
        restored_cmds.iter().any(|c| matches!(
            c,
            HostCommand::OwnBlock {
                mode: OwnMode::Take,
                ..
            }
        )),
        "expected OwnMode::Take when expanding back after side window close"
    );
    let restored_cols = restored_cmds
        .iter()
        .find_map(|c| match c {
            HostCommand::TerminalResize { args, .. } => Some((args.columns, args.rows)),
            _ => None,
        })
        .expect("expected restored wide TerminalResize (must not be dropped by stale cache!)");
    assert_eq!(restored_cols, wide_cols);

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn local_pointer_activity_takes_block_back_from_other_device() {
    let _gpu_guard = crate::lock_headless_gpu_test();
    let mut renderer =
        GpuRenderer::new_headless(GpuConfig::default()).expect("headless Vulkan renderer");
    register_all_ui_icons(&mut renderer.atlas_mut());

    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");
    let _ = ctrl.workspace.mark_snapshot_ready(&key);
    ctrl.set_viewport(1920, 1080, 1.0);
    let t0 = Instant::now();
    // The Mac mini owns the tab at its own (smaller) size.
    ctrl.process_runtime_event(
        RuntimeEvent::DataStream(DataStreamEvent {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:t3".into(),
            block_id: "block:1".into(),
            kind: DataStreamEventKind::RexApc(rex_proto::RexControlMessage::Resize {
                cols: 80,
                rows: 32,
                cell_width_px: 9,
                cell_height_px: 18,
                owner: Some("client:mac-mini".into()),
            }),
        }),
        t0,
    );
    let _ = ctrl.render_frame(&mut renderer, t0).expect("frame");
    let _ = ctrl.drain_pending_commands();
    let is_take = |c: &HostCommand| {
        matches!(
            c,
            HostCommand::OwnBlock {
                mode: OwnMode::Take,
                ..
            }
        )
    };

    // Pointer that did not move (e.g. compositor re-sending its position): no take.
    let (px, py) = ctrl.pointer_pos;
    ctrl.handle_pointer_move(px, py, Modifiers::default(), t0);
    assert!(!ctrl.drain_pending_commands().iter().any(is_take));

    // User comes back and moves the mouse over the pane: exactly one take, even for a burst.
    for i in 0..10u16 {
        let t = t0 + Duration::from_millis(5 * u64::from(i));
        ctrl.handle_pointer_move(900.0 + f32::from(i), 500.0, Modifiers::default(), t);
    }
    let takes = ctrl
        .drain_pending_commands()
        .iter()
        .filter(|c| is_take(c))
        .count();
    assert_eq!(
        takes, 1,
        "pointer activity must send exactly one throttled Take"
    );

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn second_device_owning_same_tab_is_not_fought_over() {
    let _gpu_guard = crate::lock_headless_gpu_test();
    let mut renderer =
        GpuRenderer::new_headless(GpuConfig::default()).expect("headless Vulkan renderer");
    register_all_ui_icons(&mut renderer.atlas_mut());

    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");
    let _ = ctrl.workspace.mark_snapshot_ready(&key);
    ctrl.set_viewport(1920, 1080, 1.0);
    assert!(
        ctrl.window_focused,
        "omarex keeps Hyprland focus while user is on the Mac"
    );

    let mac_resize = |cols: u16, rows: u16| {
        RuntimeEvent::DataStream(DataStreamEvent {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:t3".into(),
            block_id: "block:1".into(),
            kind: DataStreamEventKind::RexApc(rex_proto::RexControlMessage::Resize {
                cols,
                rows,
                cell_width_px: 9,
                cell_height_px: 18,
                owner: Some("client:mac-mini".into()),
            }),
        })
    };
    let is_take = |c: &HostCommand| {
        matches!(
            c,
            HostCommand::OwnBlock {
                mode: OwnMode::Take,
                ..
            }
        )
    };

    // The Mac mini owns the tab at its own (smaller) size.
    let t0 = Instant::now();
    ctrl.process_runtime_event(mac_resize(80, 32), t0);
    assert_eq!(ctrl.pane_store.size(&key), Some((80, 32)));

    // Many frames pass while the Mac keeps owning the block: omarex must never steal it and
    // may only record its desired size once.
    let mut cmds = Vec::new();
    for i in 0..20u64 {
        let t = t0 + Duration::from_millis(20 * i);
        let _ = ctrl.render_frame(&mut renderer, t).expect("frame");
        cmds.extend(ctrl.drain_pending_commands());
    }
    assert!(
        !cmds.iter().any(is_take),
        "must not take ownership from another device on passive frames, got {cmds:?}"
    );
    let resizes = cmds
        .iter()
        .filter(|c| matches!(c, HostCommand::TerminalResize { .. }))
        .count();
    assert!(
        resizes <= 1,
        "desired size must be sent at most once, got {resizes}"
    );

    // The Mac re-asserts its size (user typing there): still no fight from omarex.
    let t1 = t0 + Duration::from_millis(500);
    ctrl.process_runtime_event(mac_resize(80, 32), t1);
    let mut cmds = Vec::new();
    for i in 0..10u64 {
        let t = t1 + Duration::from_millis(20 * i);
        let _ = ctrl.render_frame(&mut renderer, t).expect("frame");
        cmds.extend(ctrl.drain_pending_commands());
    }
    assert!(
        !cmds
            .iter()
            .any(|c| is_take(c) || matches!(c, HostCommand::TerminalResize { .. })),
        "no ownership take or resize while the Mac owns the block, got {cmds:?}"
    );

    // Ownership moves to us (e.g. user typed here): our size is reconciled immediately.
    let t2 = t1 + Duration::from_millis(500);
    ctrl.process_runtime_event(
        RuntimeEvent::ControlEvent {
            host_id: LOCAL_HOST_ID.into(),
            seq: 30,
            event: Box::new(ServerEvent::SessionEvent {
                session_id: "session:t3".into(),
                event: SessionEvent::BlockOwnerChanged {
                    block_id: "block:1".into(),
                    owner: Some("client:1".into()),
                    previous: Some("client:mac-mini".into()),
                    reason: Some("take".into()),
                },
            }),
        },
        t2,
    );
    let _ = ctrl.render_frame(&mut renderer, t2).expect("owned frame");
    let cmds = ctrl.drain_pending_commands();
    let cols = cmds
        .iter()
        .find_map(|c| match c {
            HostCommand::TerminalResize { args, .. } => Some(args.columns),
            _ => None,
        })
        .expect("expected resize to our grid after ownership moved to us");
    assert!(cols > 140, "expected wide columns, got {cols}");

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn background_dirty_pane_does_not_spin_needs_redraw() {
    let _gpu_guard = crate::lock_headless_gpu_test();
    let mut renderer =
        GpuRenderer::new_headless(GpuConfig::default()).expect("headless Vulkan renderer");
    register_all_ui_icons(&mut renderer.atlas_mut());

    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let bg_key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:bg");
    let bg_term = vt_engine::Terminal::new(80, 24, vt_engine::TerminalConfig::default())
        .expect("bg terminal");
    ctrl.pane_store
        .insert_terminal(bg_key.clone(), bg_term)
        .expect("insert bg terminal");

    // Render initial frame so visible pane (`block:1`) clears dirty and `visible_pane_keys` is populated.
    let now = Instant::now();
    ctrl.read_only = true;
    let _ = ctrl
        .render_frame(&mut renderer, now)
        .expect("initial frame");
    assert!(
        !ctrl.needs_redraw(),
        "controller must be idle immediately after rendering visible panes"
    );

    // Background tab receives live VT output -> MUST NOT cause an infinite redraw loop!
    ctrl.process_runtime_event(
        RuntimeEvent::DataStream(DataStreamEvent {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:t3".into(),
            block_id: "block:bg".into(),
            kind: DataStreamEventKind::LiveVtChunk {
                bytes: bytes::Bytes::from_static(b"background log output\r\n"),
                bytes_received: 23,
            },
        }),
        now + Duration::from_millis(10),
    );
    assert!(
        !ctrl.needs_redraw(),
        "dirty output on off-screen background tab must not spin needs_redraw()"
    );

    // Visible pane receives live VT output -> MUST trigger redraw immediately!
    ctrl.process_runtime_event(
        RuntimeEvent::DataStream(DataStreamEvent {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:t3".into(),
            block_id: "block:1".into(),
            kind: DataStreamEventKind::LiveVtChunk {
                bytes: bytes::Bytes::from_static(b"visible output\r\n"),
                bytes_received: 16,
            },
        }),
        now + Duration::from_millis(20),
    );
    assert!(
        ctrl.needs_redraw(),
        "visible pane output must mark controller dirty for repaint"
    );

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn t3_code_session_collapse_toggles_and_persists() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();

    // Collapse `T3 Code` via `ToggleSessionCollapse`.
    ctrl.dispatch_ui_action(
        UiAction::ToggleSessionCollapse {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:t3".into(),
        },
        now,
    );
    let frame_in = ctrl.build_ui_frame_input(now);
    assert!(
        frame_in.hosts[0].sessions[0].collapsed,
        "T3 Code session should be collapsed after ToggleSessionCollapse"
    );
    let saved = UserSettings::load_from(&ctrl.settings_path).expect("reload settings");
    assert!(saved.collapsed_sessions.contains("local/session:t3"));

    // Expand `T3 Code` back cleanly via second `ToggleSessionCollapse`.
    ctrl.dispatch_ui_action(
        UiAction::ToggleSessionCollapse {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:t3".into(),
        },
        now,
    );
    let frame_in2 = ctrl.build_ui_frame_input(now);
    assert!(
        !frame_in2.hosts[0].sessions[0].collapsed,
        "T3 Code session should expand back cleanly"
    );
    let saved2 = UserSettings::load_from(&ctrl.settings_path).expect("reload settings");
    assert!(!saved2.collapsed_sessions.contains("local/session:t3"));

    // Add a second session and verify `CollapseOtherSessions` collapses T3 Code and expands Terminal.
    ctrl.workspace
        .host_mut(LOCAL_HOST_ID)
        .unwrap()
        .on_session_created(SessionSummary {
            session_id: "session:term".into(),
            label: "Terminal".into(),
        });
    ctrl.dispatch_ui_action(
        UiAction::CollapseOtherSessions {
            keep_session_id: "session:term".into(),
        },
        now,
    );
    let frame_in3 = ctrl.build_ui_frame_input(now);
    let t3_sess = frame_in3.hosts[0]
        .sessions
        .iter()
        .find(|s| s.session_id == "session:t3")
        .unwrap();
    let term_sess = frame_in3.hosts[0]
        .sessions
        .iter()
        .find(|s| s.session_id == "session:term")
        .unwrap();
    assert!(t3_sess.collapsed);
    assert!(!term_sess.collapsed);

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn plus_opens_real_shell_terminal_instead_of_t3_thread_and_attaches() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();

    // 1. Inside `T3 Code` (with no normal session yet), pressing `+` (`UiAction::NewTerminal`)
    //    MUST create a real `"Terminal"` shell session — NEVER a window in `T3 Code` or `t3_new`!
    ctrl.dispatch_ui_action(UiAction::NewTerminal, now);
    let cmds = ctrl.drain_pending_commands();
    assert!(
        !cmds.iter().any(
            |c| matches!(c, HostCommand::ClientAction { request, .. } if request.action == "t3_new")
        ),
        "global + must never invoke t3_new"
    );
    let new_sess_req = cmds
        .iter()
        .find_map(|c| match c {
            HostCommand::NewSession { request, .. } => Some(request),
            _ => None,
        })
        .expect("expected HostCommand::NewSession creating a real shell session");
    // Like the Rex app, new sessions are left unlabeled so the server names them.
    assert_eq!(new_sess_req.label, None);
    let init_wins = new_sess_req
        .initial_windows
        .as_ref()
        .expect("NewSession must include initial shell window");
    assert_eq!(init_wins.len(), 1);
    let blk = init_wins[0]
        .layout
        .block
        .as_ref()
        .expect("initial window must contain shell block");
    assert_eq!(blk.flavor, "com.superlogical.terminal.shell");
    assert!(
        blk.options.as_ref().and_then(|o| o.get("theme")).is_some(),
        "new shell must embed live Omarchy theme"
    );

    // 2. When `SessionCreatedAndAttached` returns from `HostManager`, controller MUST switch
    //    active session, focus the new shell block, open its stream, and take ownership!
    let term_view = SessionView {
        revision: 1,
        session_id: Some("session:term1".into()),
        label: "Terminal".into(),
        active_window_id: Some("window:term1".into()),
        windows: vec![WindowView {
            window_id: Some("window:term1".into()),
            label: "zsh".into(),
            focused_block_id: Some("block:term1".into()),
            zoomed_block_id: None,
            tiled_zoomed_block_id: None,
            layers: vec![LayerView {
                layer_id: Some("layer:term1".into()),
                kind: "tiled".into(),
                bounds: [0.0, 0.0, 1.0, 1.0],
                state: None,
                layout: Some(LayoutNode {
                    rect: [0.0, 0.0, 1.0, 1.0],
                    block_id: Some("block:term1".into()),
                    split: None,
                }),
                blocks: vec![BlockView {
                    block_id: Some("block:term1".into()),
                    label: "zsh".into(),
                    creator_name: Some("com.superlogical.terminal".into()),
                    flavor: Some("com.superlogical.terminal.shell".into()),
                    host_id: Some("host:local".into()),
                    owner: None,
                }],
            }],
        }],
    };
    ctrl.process_runtime_event(
        RuntimeEvent::SessionCreatedAndAttached {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:term1".into(),
            view: Box::new(term_view),
            seq: 5,
            open_in_new_window: false,
            origin_window_id: None,
        },
        now,
    );
    assert_eq!(
        ctrl.active_session_key(),
        Some(&SessionKey::new(LOCAL_HOST_ID, "session:term1"))
    );
    assert_eq!(
        ctrl.ui.focus.current(),
        &FocusTarget::Terminal(Some("block:term1".into()))
    );
    let attach_cmds = ctrl.drain_pending_commands();
    assert!(attach_cmds.iter().any(|c| matches!(
        c,
        HostCommand::OpenBlockStream {
            session_id,
            block_id,
            ..
        } if session_id == "session:term1" && block_id == "block:term1"
    )));
    assert!(attach_cmds.iter().any(|c| matches!(
        c,
        HostCommand::OwnBlock {
            session_id,
            mode: OwnMode::Take,
            ..
        } if session_id == "session:term1"
    )));

    // 3. Switch back to `T3 Code` and press global `+` again -> switches to existing `"Terminal"`
    //    session and creates a real shell tab (`NewWindow`) there instead of inside `T3 Code`!
    ctrl.dispatch_ui_action(
        UiAction::SelectSession {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:t3".into(),
        },
        now,
    );
    ctrl.drain_pending_commands();
    ctrl.dispatch_ui_action(UiAction::NewTerminal, now);
    let tab_cmds = ctrl.drain_pending_commands();
    assert!(tab_cmds.iter().any(|c| matches!(
        c,
        HostCommand::NewWindow { request, .. }
            if request.session_id == "session:term1"
                && request
                    .layout
                    .as_ref()
                    .and_then(|l| l.block.as_ref())
                    .is_some_and(|b| b.flavor == "com.superlogical.terminal.shell")
    )));

    // 4. Explicit per-session `+` on `T3 Code` still dispatches explicit `t3_new`.
    ctrl.dispatch_ui_action(
        UiAction::NewTab {
            host_id: Some(LOCAL_HOST_ID.into()),
            session_id: Some("session:t3".into()),
        },
        now,
    );
    let t3_cmds = ctrl.drain_pending_commands();
    assert!(t3_cmds.iter().any(|c| matches!(
        c,
        HostCommand::ClientAction { host_id, request }
            if host_id == LOCAL_HOST_ID
                && request.action == "t3_new"
                && request.session_id.as_deref() == Some("session:t3")
    )));

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn multi_host_labels_remote_new_session_and_targeted_t3_new_thread_work() {
    let dir = std::env::temp_dir().join(format!("omarex-multihost-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let settings_path = dir.join("settings.json");

    let mut settings = UserSettings::default();
    settings.saved_hosts.push(SavedHost {
        id: "remote:https---rex-tail000000-ts-net".into(),
        label: "Studio Mac".into(),
        endpoint: "https://rex.tail000000.ts.net".into(),
        auto_connect: true,
    });
    let mut ctrl = OmarexController::new(settings, settings_path.clone(), Theme::default())
        .expect("multi-host controller");
    ctrl.clipboard_enabled = false;
    ctrl.set_default_working_directory(Some("/home/user/projects/omarex".into()));
    let now = Instant::now();

    // 1. Saved remote host immediately uses human label (NEVER raw `remote:https---...`) and `is_local == false`!
    let mac_id = "remote:https---rex-tail000000-ts-net";
    assert_eq!(ctrl.workspace.host_order, vec![LOCAL_HOST_ID, mac_id]);
    let mac_host = ctrl
        .workspace
        .host(mac_id)
        .expect("pre-registered remote host");
    assert_eq!(mac_host.label, "Studio Mac");
    assert!(!mac_host.is_local);

    // Connect + bootstrap both local (`T3 Code · omarchy`) and remote Mac (`T3 Code`, `a6dd4a`).
    ctrl.process_runtime_event(
        RuntimeEvent::HostConnected {
            host_id: LOCAL_HOST_ID.into(),
            client_id: "c:local".into(),
            resumed: false,
            api_revision: 24,
        },
        now,
    );
    ctrl.process_runtime_event(
        RuntimeEvent::HostConnected {
            host_id: mac_id.into(),
            client_id: "c:mac".into(),
            resumed: false,
            api_revision: 24,
        },
        now,
    );
    let local_boot: rex_proto::types::SessionBootstrapResponse =
            serde_json::from_value(serde_json::json!({
                "client_id": "c:local",
                "server_status": {
                    "pid": 100, "instance_id": "i1", "host_id": "h:local",
                    "started_at": "2026-11-26T10:00:00Z", "exit_empty": false,
                    "session_count": 1, "api_revision": 24, "api_min_revision": 24
                },
                "attach_all": { "sessions": [{ "session_id": "4e6bb7", "label": "T3 Code · omarchy" }] },
                "selected_session": null,
                "attached_sessions": [],
                "data_tickets": []
            }))
            .expect("local boot");
    let mac_boot: rex_proto::types::SessionBootstrapResponse =
        serde_json::from_value(serde_json::json!({
            "client_id": "c:mac",
            "server_status": {
                "pid": 200, "instance_id": "i2", "host_id": "h:mac",
                "started_at": "2026-11-26T10:00:00Z", "exit_empty": false,
                "session_count": 1, "api_revision": 24, "api_min_revision": 24
            },
            "attach_all": { "sessions": [{ "session_id": "a6dd4a", "label": "T3 Code" }] },
            "selected_session": null,
            "attached_sessions": [],
            "data_tickets": []
        }))
        .expect("mac boot");
    ctrl.process_runtime_event(
        RuntimeEvent::BootstrapReady {
            host_id: LOCAL_HOST_ID.into(),
            seq: 1,
            bootstrap: Box::new(local_boot),
        },
        now,
    );
    ctrl.process_runtime_event(
        RuntimeEvent::BootstrapReady {
            host_id: mac_id.into(),
            seq: 1,
            bootstrap: Box::new(mac_boot),
        },
        now,
    );
    // Active session starts on local; focus a local block to verify zero cross-host block leak.
    ctrl.activate_session(SessionKey::new(LOCAL_HOST_ID, "4e6bb7"), &HashMap::new());
    ctrl.ui.focus.focus_terminal(Some("block:local-1".into()));
    ctrl.drain_pending_commands();

    // After bootstrap, remote host STILL keeps its human label and `is_local == false`!
    let mac_host = ctrl
        .workspace
        .host(mac_id)
        .expect("mac host after bootstrap");
    assert_eq!(mac_host.label, "Studio Mac");
    assert!(!mac_host.is_local);

    // 2. Clicking `+` on remote host's `T3 Code` routes `t3_new` strictly to remote host (`a6dd4a`),
    //    switches active session to remote host, and does NOT leak local's `block:local-1`!
    ctrl.dispatch_ui_action(
        UiAction::NewT3Thread {
            host_id: mac_id.into(),
            session_id: "a6dd4a".into(),
        },
        now,
    );
    assert_eq!(
        ctrl.active_session_key(),
        Some(&SessionKey::new(mac_id, "a6dd4a"))
    );
    let t3_cmds = ctrl.drain_pending_commands();
    assert!(t3_cmds.iter().any(|c| matches!(
        c,
        HostCommand::ClientAction { host_id, request }
            if host_id == mac_id
                && request.action == "t3_new"
                && request.session_id.as_deref() == Some("a6dd4a")
                && request.block_id.is_none()
    )));

    // 3. Clicking `+` on remote host header spawns a new real shell session on that host
    //    and NEVER leaks Linux `/home/user/projects/omarex` or `$HOME` in `cwd`!
    ctrl.dispatch_ui_action(
        UiAction::NewSession {
            host_id: Some(mac_id.into()),
        },
        now,
    );
    let sess_cmds = ctrl.drain_pending_commands();
    let mac_new_sess = sess_cmds
        .iter()
        .find_map(|c| match c {
            HostCommand::NewSession {
                host_id, request, ..
            } if host_id == mac_id => Some(request),
            _ => None,
        })
        .expect("NewSession must target remote host");
    assert_eq!(mac_new_sess.label, None);
    let mac_opts = mac_new_sess.initial_windows.as_ref().unwrap()[0]
        .layout
        .block
        .as_ref()
        .unwrap()
        .options
        .as_ref();
    assert!(
        mac_opts.and_then(|o| o.get("cwd")).is_none(),
        "remote macOS session must NOT inherit Linux cwd: {mac_opts:?}"
    );

    // 4. Rename Host updates workspace label + persists into settings.json!
    ctrl.dispatch_ui_action(
        UiAction::StartRenameHost {
            host_id: mac_id.into(),
        },
        now,
    );
    ctrl.dispatch_ui_action(
        UiAction::SubmitRename {
            target_id: format!("__rename_host:{mac_id}"),
            new_label: "Mac mini Studio".into(),
        },
        now,
    );
    assert_eq!(
        ctrl.workspace.host(mac_id).unwrap().label,
        "Mac mini Studio"
    );
    assert_eq!(ctrl.settings.saved_hosts[0].label, "Mac mini Studio");

    // 5. SubmitAddHost supports `Name = url` syntax AND cleans bare Tailscale URLs.
    ctrl.dispatch_ui_action(
        UiAction::SubmitAddHost {
            endpoint: "Work VPS = https://vps-eu.tail000000.ts.net".into(),
        },
        now,
    );
    let vps_id = format!(
        "remote:{}",
        sanitize_host_id("https://vps-eu.tail000000.ts.net")
    );
    assert_eq!(ctrl.workspace.host(&vps_id).unwrap().label, "Work VPS");
    assert!(!ctrl.workspace.host(&vps_id).unwrap().is_local);
    assert_eq!(derive_host_label("https://rex.tail000000.ts.net"), "rex");

    // 6. DisconnectHost disconnects without removing saved host; RemoveHost removes completely.
    ctrl.drain_pending_commands();
    ctrl.dispatch_ui_action(
        UiAction::DisconnectHost {
            host_id: mac_id.into(),
        },
        now,
    );
    assert_eq!(
        ctrl.workspace.host(mac_id).unwrap().status,
        HostStatus::Disconnected
    );
    assert!(ctrl.settings.saved_hosts.iter().any(|h| h.id == mac_id));
    assert!(ctrl.drain_pending_commands().iter().any(|c| matches!(
        c,
        HostCommand::RemoveHost { host_id } if host_id == mac_id
    )));

    ctrl.dispatch_ui_action(
        UiAction::RemoveHost {
            host_id: mac_id.into(),
        },
        now,
    );
    assert!(ctrl.workspace.host(mac_id).is_none());
    assert!(!ctrl.settings.saved_hosts.iter().any(|h| h.id == mac_id));
    assert!(ctrl.drain_pending_commands().iter().any(|c| matches!(
        c,
        HostCommand::RemoveHost { host_id } if host_id == mac_id
    )));
    assert_eq!(
        ctrl.active_session_key(),
        Some(&SessionKey::new(LOCAL_HOST_ID, "4e6bb7"))
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn super_return_working_directory_spawns_independent_shell_session() {
    let mut ctrl = make_test_controller();
    ctrl.set_default_working_directory(Some("/tmp/omarex-proj".into()));
    let now = Instant::now();

    ctrl.process_runtime_event(
        RuntimeEvent::HostConnected {
            host_id: LOCAL_HOST_ID.into(),
            client_id: "client:window-2".into(),
            resumed: false,
            api_revision: 24,
        },
        now,
    );
    // Simulate another live `omarex` window already attached to both `T3 Code` and `Terminal`.
    ctrl.process_runtime_event(
        RuntimeEvent::ClientsListed {
            host_id: LOCAL_HOST_ID.into(),
            clients: rex_proto::types::ClientListResponse {
                clients: vec![rex_proto::types::ConnectedClient {
                    client_id: "client:window-1".into(),
                    info: Some(rex_proto::types::ClientInfo {
                        kind: Some("app".into()),
                        platform: Some("linux".into()),
                        name: Some("omarex".into()),
                        version: None,
                        label: None,
                    }),
                    principal: Principal {
                        kind: "peer".into(),
                        transport: "unix".into(),
                        display: "local".into(),
                        remote_addr: None,
                        uid: None,
                        gid: None,
                        pid: None,
                        sid: None,
                        user_id: None,
                        login_name: None,
                        node: None,
                        tags: Vec::new(),
                        path: Vec::new(),
                    },
                    connected_at: String::new(),
                    connection_state: None,
                    connection_lost_at: None,
                    session_ids: vec!["session:t3".into(), "session:term".into()],
                }],
            },
        },
        now,
    );
    let bootstrap: rex_proto::types::SessionBootstrapResponse =
        serde_json::from_value(serde_json::json!({
            "client_id": "client:window-2",
            "server_status": {
                "pid": 1234,
                "instance_id": "inst-1",
                "host_id": "host:local",
                "started_at": "2026-11-26T10:00:00Z",
                "exit_empty": false,
                "session_count": 2,
                "api_revision": 24,
                "api_min_revision": 24
            },
            "attach_all": {
                "sessions": [
                    { "session_id": "session:t3", "label": "T3 Code" },
                    { "session_id": "session:term", "label": "Terminal" }
                ]
            },
            "selected_session": null,
            "attached_sessions": [],
            "data_tickets": []
        }))
        .expect("parse test bootstrap");
    ctrl.process_runtime_event(
        RuntimeEvent::BootstrapReady {
            host_id: LOCAL_HOST_ID.into(),
            seq: 1,
            bootstrap: Box::new(bootstrap),
        },
        now,
    );

    let cmds = ctrl.drain_pending_commands();
    let new_req = cmds
        .iter()
        .find_map(|c| match c {
            HostCommand::NewSession { request, .. } => Some(request),
            _ => None,
        })
        .expect("Super+Return with --working-directory must spawn a fresh shell session");
    assert_eq!(new_req.label.as_deref(), Some("omarex-proj"));
    let cwd = new_req.initial_windows.as_ref().unwrap()[0]
        .layout
        .block
        .as_ref()
        .unwrap()
        .options
        .as_ref()
        .and_then(|o| o.get("cwd"))
        .and_then(|v| v.as_str());
    assert_eq!(cwd, Some("/tmp/omarex-proj"));

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn close_session_float_dock_and_contextual_rename_wire_correctly() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();

    // 1. `CloseSession` sends `HostCommand::DestroySession` (not `ClientAction`).
    ctrl.dispatch_ui_action(
        UiAction::CloseSession {
            session_id: "session:t3".into(),
        },
        now,
    );
    let close_cmds = ctrl.drain_pending_commands();
    assert!(close_cmds.iter().any(|c| matches!(
        c,
        HostCommand::DestroySession { session_id, .. } if session_id == "session:t3"
    )));

    // 2. `FloatOrDock` floats tiled pane and docks floating pane via `MoveBlock`.
    ctrl.dispatch_ui_action(
        UiAction::PaneButton {
            block_id: "block:1".into(),
            button: PaneHeaderButton::FloatOrDock,
        },
        now,
    );
    assert!(ctrl.drain_pending_commands().iter().any(|c| matches!(
        c,
        HostCommand::FloatBlock { request, .. } if request.block_id == "block:1"
    )));

    // Add a floating block alongside tiled `block:1` and verify `FloatOrDock` docks it!
    let sk = SessionKey::new(LOCAL_HOST_ID, "session:t3");
    if let Some(view) = ctrl
        .workspace
        .session_mut(&sk)
        .and_then(|s| s.view.as_mut())
    {
        view.windows[0].layers.push(LayerView {
            layer_id: Some("layer:float1".into()),
            kind: "floating".into(),
            bounds: [0.2, 0.2, 0.6, 0.6],
            state: Some("normal".into()),
            layout: None,
            blocks: vec![BlockView {
                block_id: Some("block:float1".into()),
                label: "Scratchpad".into(),
                creator_name: Some("com.superlogical.terminal".into()),
                flavor: Some("com.superlogical.terminal.shell".into()),
                host_id: Some("host:local".into()),
                owner: Some("client:1".into()),
            }],
        });
    }
    ctrl.dispatch_ui_action(
        UiAction::PaneButton {
            block_id: "block:float1".into(),
            button: PaneHeaderButton::FloatOrDock,
        },
        now,
    );
    assert!(ctrl.drain_pending_commands().iter().any(|c| matches!(
        c,
        HostCommand::MoveBlock {
            block_id,
            anchor_block_id,
            ..
        } if block_id == "block:float1" && anchor_block_id == "block:1"
    )));

    // 3. Contextual `SubmitRename` handles window, block, and session targets accurately.
    ctrl.dispatch_ui_action(
        UiAction::SubmitRename {
            target_id: "__rename_window:local|session:t3|window:1".into(),
            new_label: "Renamed Tab".into(),
        },
        now,
    );
    ctrl.dispatch_ui_action(
        UiAction::SubmitRename {
            target_id: "__rename_block:local|session:t3|block:1".into(),
            new_label: "Renamed Pane".into(),
        },
        now,
    );
    let ren_cmds = ctrl.drain_pending_commands();
    assert!(ren_cmds.iter().any(|c| matches!(
        c,
        HostCommand::RenameWindow { window_id, label, .. }
            if window_id == "window:1" && label == "Renamed Tab"
    )));
    assert!(ren_cmds.iter().any(|c| matches!(
        c,
        HostCommand::RenameBlock { block_id, label, .. }
            if block_id == "block:1" && label == "Renamed Pane"
    )));

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn plain_filepath_hover_detects_local_image_card_and_ignores_prose() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");

    // Create a real tiny valid 2x2 PNG file on disk so local existence + image_dimensions pass.
    let tmp_dir = std::env::temp_dir().join(format!("omarex-hover-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp_dir);
    let png_path = tmp_dir.join("preview shot.png");
    let rgba = [
        255u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
    ];
    gpu_renderer::save_rgba_png(&png_path, 2, 2, &rgba).expect("write test PNG");

    let note_path = tmp_dir.join("notes.txt");
    std::fs::write(&note_path, "hello").expect("write test text file");

    ctrl.pane_store
        .create_local_test_pane(&key, 120, 24)
        .unwrap();
    let line0 = format!("Check '{}'\r\n", png_path.display());
    let line1 = format!("See {} and iPhone/Mac prose\r\n", note_path.display());
    ctrl.pane_store
        .feed_local_test_bytes(&key, line0.as_bytes())
        .unwrap();
    ctrl.pane_store
        .feed_local_test_bytes(&key, line1.as_bytes())
        .unwrap();

    // 1. Hovering quoted PNG path with space -> `ImageCard` + pending GPU upload!
    let png_col = line0.find("preview").unwrap() as u16;
    ctrl.update_hovered_file_path(&key, png_col, 0, 120.0, 80.0);
    match ctrl.ui.hover_preview.as_ref() {
        Some(HoverPreviewState::ImageCard {
            caption,
            image_width,
            image_height,
            ..
        }) => {
            assert_eq!(caption, "preview shot.png");
            assert_eq!((*image_width, *image_height), (2, 2));
        }
        other => panic!("expected ImageCard hover preview, got {other:?}"),
    }
    assert!(
        ctrl.pending_hover_image
            .as_ref()
            .is_some_and(|(p, _)| p == &png_path)
    );

    // 2. Hovering regular text file -> `FilePath` tooltip.
    let txt_col = line1.find("notes.txt").unwrap() as u16;
    ctrl.update_hovered_file_path(&key, txt_col, 1, 120.0, 100.0);
    assert!(matches!(
        ctrl.ui.hover_preview.as_ref(),
        Some(HoverPreviewState::FilePath { path, .. }) if path.ends_with("notes.txt")
    ));

    // 3. Hovering German/English prose slash `iPhone/Mac` -> ZERO false positives!
    let prose_col = line1.find("iPhone/Mac").unwrap() as u16 + 2;
    ctrl.update_hovered_file_path(&key, prose_col, 1, 200.0, 100.0);
    assert!(
        ctrl.ui.hover_preview.is_none(),
        "prose token `iPhone/Mac` must not trigger local file hover"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn remote_multi_file_media_drop_batches_uploads_and_targets_hovered_split_pane() {
    let mut ctrl = make_test_controller();
    let now = Instant::now();

    // Set up a remote host with a 2-pane horizontal split (left: block:left, right: block:right).
    let mut host = HostEntry::new(
        "remote-mac",
        "Mac",
        RexEndpoint::parse("https://rex.example.ts.net").unwrap(),
    );
    host.is_local = false;
    host.status = HostStatus::Connected {
        client_id: "client:1".into(),
        server_host_id: Some("host:mac".into()),
        api_revision: 24,
        resumed: false,
    };
    host.on_session_created(SessionSummary {
        session_id: "session:mac".into(),
        label: "T3 Code".into(),
    });
    ctrl.workspace.add_host(host);
    ctrl.workspace
        .select_session(SessionKey::new("remote-mac", "session:mac"));
    let view = SessionView {
        revision: 1,
        session_id: Some("session:mac".into()),
        label: "T3 Code".into(),
        active_window_id: Some("window:1".into()),
        windows: vec![WindowView {
            window_id: Some("window:1".into()),
            label: "Split".into(),
            focused_block_id: Some("block:left".into()),
            zoomed_block_id: None,
            tiled_zoomed_block_id: None,
            layers: vec![LayerView {
                layer_id: Some("layer:tiled".into()),
                kind: "tiled".into(),
                bounds: [0.0, 0.0, 1.0, 1.0],
                state: None,
                layout: Some(LayoutNode {
                    rect: [0.0, 0.0, 1.0, 1.0],
                    block_id: None,
                    split: Some(Box::new(rex_proto::types::SplitNode {
                        split_id: Some("split:1".into()),
                        direction: "horizontal".into(),
                        ratio: 0.5,
                        before: LayoutNode {
                            rect: [0.0, 0.0, 0.5, 1.0],
                            block_id: Some("block:left".into()),
                            split: None,
                        },
                        after: LayoutNode {
                            rect: [0.5, 0.0, 0.5, 1.0],
                            block_id: Some("block:right".into()),
                            split: None,
                        },
                    })),
                }),
                blocks: vec![
                    BlockView {
                        block_id: Some("block:left".into()),
                        label: "Left".into(),
                        creator_name: Some("com.superlogical.terminal".into()),
                        flavor: Some("com.superlogical.terminal.shell".into()),
                        host_id: Some("host:mac".into()),
                        owner: Some("client:1".into()),
                    },
                    BlockView {
                        block_id: Some("block:right".into()),
                        label: "Right".into(),
                        creator_name: Some("com.superlogical.terminal".into()),
                        flavor: Some("com.superlogical.terminal.shell".into()),
                        host_id: Some("host:mac".into()),
                        owner: Some("client:1".into()),
                    },
                ],
            }],
        }],
    };
    ctrl.workspace
        .session_mut(&SessionKey::new("remote-mac", "session:mac"))
        .unwrap()
        .apply_view(view);
    ctrl.ui.focus.focus_terminal(Some("block:left".into()));
    ctrl.set_viewport(1440, 900, 1.0);

    // Compute one UI layout pass so `last_ui_output` knows both split pane rects.
    let frame_input = ctrl.build_ui_frame_input(now);
    let ui_out = ctrl.ui.render(&frame_input, &ctrl.theme, now, 0.0);
    let right_card = ui_out
        .tiled_panes
        .iter()
        .find(|p| p.id == "block:right")
        .expect("right pane geometry")
        .card_rect;
    ctrl.last_ui_output = Some(ui_out);

    // 1. Hovering a drag over the right pane produces a crisp `dnd_overlay` on `right_card`.
    let (drop_x, drop_y) = right_card.center();
    ctrl.handle_file_hover(drop_x, drop_y);
    let hover_input = ctrl.build_ui_frame_input(now);
    assert_eq!(
        hover_input.dnd_overlay.map(|d| d.preview_rect),
        Some(right_card),
        "drag hover must highlight the exact pane card under the cursor"
    );

    // 2. Dropping an image + video + folder onto the right pane skips the folder (with toast)
    //    and uploads both media files to `block:right` (NOT `block:left`) in ONE `UploadBatch`!
    let tmp_dir = std::env::temp_dir().join(format!("omarex-dnd-batch-{}", std::process::id()));
    let sub_dir = tmp_dir.join("folder");
    let _ = std::fs::create_dir_all(&sub_dir);
    let img = tmp_dir.join("photo.webp");
    let vid = tmp_dir.join("clip.mp4");
    std::fs::write(&img, b"RIFF....WEBP").unwrap();
    std::fs::write(&vid, b"\x00\x00\x00\x18ftypmp42").unwrap();

    let _ = ctrl.drain_pending_commands();
    ctrl.handle_file_drop_at(
        &[img.clone(), sub_dir.clone(), vid.clone()],
        Some((drop_x, drop_y)),
    );

    // Overlay cleared & focus moved to the target pane.
    assert!(ctrl.active_dnd_hover.is_none());
    assert_eq!(ctrl.ui.focus.last_terminal_block(), Some("block:right"));
    assert!(
        ctrl.ui
            .toasts
            .iter()
            .any(|t| t.title.contains("Folders skipped")),
        "expected warning toast for skipped directory"
    );

    let uploads: Vec<(String, PathBuf, Option<UploadBatch>)> = ctrl
        .drain_pending_commands()
        .into_iter()
        .filter_map(|c| match c {
            HostCommand::UploadFile {
                block_id,
                local_path,
                batch,
                ..
            } => Some((block_id, local_path, batch)),
            _ => None,
        })
        .collect();
    assert_eq!(uploads.len(), 2);
    assert_eq!(uploads[0].0, "block:right");
    assert_eq!(uploads[1].0, "block:right");
    assert_eq!(uploads[0].1, img);
    assert_eq!(uploads[1].1, vid);
    let b0 = uploads[0].2.as_ref().expect("batch on file 0");
    let b1 = uploads[1].2.as_ref().expect("batch on file 1");
    assert_eq!(b0.batch_id, b1.batch_id);
    assert_eq!((b0.index, b0.count), (0, 2));
    assert_eq!((b1.index, b1.count), (1, 2));

    let _ = std::fs::remove_dir_all(&tmp_dir);
    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn local_media_drop_shell_escapes_images_videos_and_bracketed_paste() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");
    ctrl.pane_store
        .create_local_test_pane(&key, 80, 24)
        .unwrap();
    // Enable DEC Mode 2004 bracketed paste like `tr3x` / `zsh` / `claude`.
    ctrl.pane_store
        .feed_local_test_bytes(&key, b"\x1b[?2004h")
        .unwrap();
    let _ = ctrl.drain_pending_commands();

    ctrl.handle_file_drop(&[
        PathBuf::from("/home/user/Pictures/shot.webp"),
        PathBuf::from("/home/user/My Videos/demo clip.mp4"),
    ]);

    let written = ctrl
        .drain_pending_commands()
        .into_iter()
        .find_map(|c| match c {
            HostCommand::TerminalWrite { data, .. } => Some(data),
            _ => None,
        })
        .expect("local drop must paste paths into terminal");
    assert_eq!(
        written,
        "\x1b[200~/home/user/Pictures/shot.webp '/home/user/My Videos/demo clip.mp4'\x1b[201~"
    );

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn session_view_fetch_failed_resets_inflight_and_evicts_not_found_session_without_toast() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();
    let sk = SessionKey::new(LOCAL_HOST_ID, "session:t3");

    // Trigger a pending view fetch.
    ctrl.process_runtime_event(
        RuntimeEvent::ControlEvent {
            host_id: LOCAL_HOST_ID.into(),
            seq: 2,
            event: Box::new(ServerEvent::SessionEvent {
                session_id: "session:t3".into(),
                event: SessionEvent::SessionViewChanged { revision: 2 },
            }),
        },
        now,
    );
    assert!(ctrl.workspace.session(&sk).unwrap().view_fetch_in_flight);
    ctrl.drain_pending_commands();

    // 1. Transient failure clears `view_fetch_in_flight` without destroying session or toasting.
    ctrl.process_runtime_event(
        RuntimeEvent::SessionViewFetchFailed {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:t3".into(),
            session_not_found: false,
        },
        now,
    );
    assert!(!ctrl.workspace.session(&sk).unwrap().view_fetch_in_flight);
    assert!(ctrl.ui.toasts.is_empty());

    // 2. `session_not_found` evicts panes, cancels streams, removes session, and shows zero toasts.
    let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");
    ctrl.opened_streams.insert(key.clone());
    ctrl.process_runtime_event(
        RuntimeEvent::SessionViewFetchFailed {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:t3".into(),
            session_not_found: true,
        },
        now,
    );
    assert!(ctrl.workspace.session(&sk).is_none());
    assert!(!ctrl.pane_store.contains(&key));
    assert!(!ctrl.opened_streams.contains(&key));
    assert!(ctrl.ui.toasts.is_empty());
    let cmds = ctrl.drain_pending_commands();
    assert!(cmds.iter().any(|c| matches!(
        c,
        HostCommand::EvictSessionStreams {
            host_id,
            session_id,
            disown: false,
        } if host_id == LOCAL_HOST_ID && session_id == "session:t3"
    )));

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn apply_view_prunes_zombie_panes_and_theme_change_never_writes_to_dead_blocks() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();

    // Insert a second pane (`block:zombie`) with Mode 2031 enabled, simulating a block that
    // was closed/replaced by `tr3x` and removed from the next `SessionView`.
    let zombie_key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:zombie");
    let zombie_term = vt_engine::Terminal::new(80, 24, vt_engine::TerminalConfig::default())
        .expect("create zombie terminal");
    zombie_term.write(b"\x1b[?2031h");
    ctrl.pane_store
        .insert_terminal(zombie_key.clone(), zombie_term)
        .expect("insert zombie terminal");
    ctrl.opened_streams.insert(zombie_key.clone());

    // Deliver an updated `SessionView` containing ONLY `block:1`.
    let updated_view = SessionView {
        revision: 2,
        session_id: Some("session:t3".into()),
        label: "T3 Code".into(),
        active_window_id: Some("window:1".into()),
        windows: vec![WindowView {
            window_id: Some("window:1".into()),
            label: "Build bridge.rs".into(),
            focused_block_id: Some("block:1".into()),
            zoomed_block_id: None,
            tiled_zoomed_block_id: None,
            layers: vec![LayerView {
                layer_id: Some("layer:tiled".into()),
                kind: "tiled".into(),
                bounds: [0.0, 0.0, 1.0, 1.0],
                state: None,
                layout: Some(LayoutNode {
                    rect: [0.0, 0.0, 1.0, 1.0],
                    block_id: Some("block:1".into()),
                    split: None,
                }),
                blocks: vec![BlockView {
                    block_id: Some("block:1".into()),
                    label: "t3:1baa1939/thread-1".into(),
                    creator_name: Some("com.superlogical.terminal".into()),
                    flavor: Some("com.superlogical.terminal.shell".into()),
                    host_id: Some("host:local".into()),
                    owner: Some("client:1".into()),
                }],
            }],
        }],
    };
    ctrl.process_runtime_event(
        RuntimeEvent::SessionViewUpdated {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:t3".into(),
            view: Box::new(updated_view),
            seq: 2,
        },
        now,
    );

    assert!(
        !ctrl.pane_store.contains(&zombie_key),
        "zombie pane must be evicted from PaneStore when pruned from SessionView"
    );
    assert!(!ctrl.opened_streams.contains(&zombie_key));
    let prune_cmds = ctrl.drain_pending_commands();
    assert!(prune_cmds.iter().any(|c| matches!(
        c,
        HostCommand::CloseBlockStream { block_id, .. } if block_id == "block:zombie"
    )));

    // Even if an orphaned pane somehow remains in `pane_store` without workspace tracking,
    // `ThemeChanged` must NEVER send `TerminalWrite` to it.
    let orphan_key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:orphan");
    let orphan_term = vt_engine::Terminal::new(80, 24, vt_engine::TerminalConfig::default())
        .expect("create orphan terminal");
    orphan_term.write(b"\x1b[?2031h");
    ctrl.pane_store
        .insert_terminal(orphan_key, orphan_term)
        .expect("insert orphan terminal");
    ctrl.process_runtime_event(
        RuntimeEvent::ThemeChanged(ThemeEvent::PaletteChanged(
            omarchy_theme::ThemePalette::default_light(),
        )),
        now,
    );
    let theme_cmds = ctrl.drain_pending_commands();
    assert!(
        !theme_cmds.iter().any(|c| matches!(
            c,
            HostCommand::TerminalWrite { block_id, .. }
                if block_id == "block:zombie" || block_id == "block:orphan"
        )),
        "ThemeChanged must never write Mode 2031 reports to dead/untracked blocks"
    );

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn data_stream_error_releases_opened_streams_and_lifecycle_events_cancel_host_streams() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();
    let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");

    // 1. A stream error must release `opened_streams` so the pane is never permanently wedged.
    ctrl.opened_streams.insert(key.clone());
    ctrl.process_runtime_event(
        RuntimeEvent::DataStream(DataStreamEvent {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:t3".into(),
            block_id: "block:1".into(),
            kind: DataStreamEventKind::Error {
                message: "RPC error: conflict (409): client is not attached".into(),
            },
        }),
        now,
    );
    assert!(
        !ctrl.opened_streams.contains(&key),
        "DataStreamEventKind::Error must clear opened_streams so pane can reconnect"
    );

    // 2. `BlockClosed` cancels the active block stream in `HostManager`.
    ctrl.opened_streams.insert(key.clone());
    ctrl.process_runtime_event(
        RuntimeEvent::ControlEvent {
            host_id: LOCAL_HOST_ID.into(),
            seq: 3,
            event: Box::new(ServerEvent::SessionEvent {
                session_id: "session:t3".into(),
                event: SessionEvent::BlockClosed {
                    block_id: "block:1".into(),
                },
            }),
        },
        now,
    );
    let close_cmds = ctrl.drain_pending_commands();
    assert!(close_cmds.iter().any(|c| matches!(
        c,
        HostCommand::CloseBlockStream { block_id, .. } if block_id == "block:1"
    )));

    // 3. `SessionDestroyed` cancels all active streams for that session in `HostManager`.
    ctrl.opened_streams
        .insert(BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:2"));
    ctrl.process_runtime_event(
        RuntimeEvent::ControlEvent {
            host_id: LOCAL_HOST_ID.into(),
            seq: 4,
            event: Box::new(ServerEvent::SessionDestroyed {
                session_id: "session:t3".into(),
            }),
        },
        now,
    );
    let destroy_cmds = ctrl.drain_pending_commands();
    assert!(destroy_cmds.iter().any(|c| matches!(
        c,
        HostCommand::EvictSessionStreams {
            session_id,
            disown: false,
            ..
        } if session_id == "session:t3"
    )));

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn toasts_deduplicate_schedule_wakeup_and_auto_dismiss_on_tick() {
    let mut ctrl = make_test_controller();
    let t0 = Instant::now();

    ctrl.push_toast_at(ToastSeverity::Error, "local: session.create", "boom", t0);
    assert_eq!(ctrl.ui.toasts.len(), 1);
    let first_id = ctrl.ui.toasts[0].id;

    // Identical duplicate refreshes expiry instead of stacking a second toast.
    let t1 = t0 + Duration::from_secs(3);
    ctrl.push_toast_at(ToastSeverity::Error, "local: session.create", "boom", t1);
    assert_eq!(ctrl.ui.toasts.len(), 1);
    assert_eq!(ctrl.ui.toasts[0].id, first_id);

    let expected_deadline = t1 + Duration::from_secs(8);
    assert_eq!(ctrl.next_wakeup_deadline(t1), Some(expected_deadline));

    // Before refreshed deadline -> toast stays visible.
    let _ = ctrl.tick(t0 + Duration::from_secs(9));
    assert_eq!(ctrl.ui.toasts.len(), 1);

    // At/after refreshed deadline -> toast auto-dismisses and wakeup clears to None.
    let _ = ctrl.tick(expected_deadline);
    assert!(ctrl.ui.toasts.is_empty());
    assert!(ctrl.toast_deadlines.is_empty());
    assert_eq!(ctrl.next_wakeup_deadline(expected_deadline), None);

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn live_full_omarex_window_renders_to_png() {
    let ep = RexEndpoint::discover_default();
    if let RexEndpoint::Unix(ref sock) = ep
        && !sock.exists()
    {
        eprintln!(
            "skipping live_full_omarex_window_renders_to_png: socket {} absent",
            sock.display()
        );
        return;
    }

    let png_path = std::env::var("OMAREX_LIVE_FULL_PNG").unwrap_or_else(|_| {
        std::env::temp_dir()
            .join("omarex_live_full.png")
            .to_string_lossy()
            .into_owned()
    });

    let _gpu_guard = crate::lock_headless_gpu_test();
    let report = render_headless_live_png(None, Some("T3 Code"), 1440, 900, 1.0, &png_path)
        .expect("live headless full omarex window render should succeed");

    assert!(report.host_connected, "expected live Rex connection");
    assert!(
        report.total_windows > 0,
        "expected live session windows in sidebar"
    );
    assert!(
        report.ready_panes > 0,
        "expected at least one decoded live terminal pane"
    );
    assert!(Path::new(&png_path).is_file(), "expected output PNG file");

    eprintln!(
        "OMAREX_LIVE_FULL session={:?} windows={} ready_panes={} adapter={:?} total_us={} draws={} quads={} glyphs={} png={}",
        report.active_session_label,
        report.total_windows,
        report.ready_panes,
        report.frame_stats.adapter_name,
        report.frame_stats.total_us,
        report.frame_stats.draw_calls,
        report.frame_stats.quad_count,
        report.frame_stats.glyph_count,
        png_path
    );
}

#[test]
fn sgr_mouse_tracking_allows_link_hover_and_plain_click_to_open_viewer_while_forwarding_non_link_clicks()
 {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();
    let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");

    let tmp_dir =
        std::env::temp_dir().join(format!("omarex-sgr-viewer-test-{}", std::process::id()));
    let sub_dir = tmp_dir.join("nested_dir");
    std::fs::create_dir_all(&sub_dir).expect("create temp test dir");
    let code_file = sub_dir.join("demo_target.rs");
    let mut file_body = String::new();
    for idx in 1..=25 {
        file_body.push_str(&format!("fn line_{idx}() {{}}\n"));
    }
    std::fs::write(&code_file, &file_body).expect("write demo_target.rs");

    let prefix = format!("│ {}/\r\n", sub_dir.display());
    let cont = "│ demo_target.rs:12\r\n";
    let pane_cols = (prefix.chars().count() as u16).saturating_add(16).max(80);
    ctrl.pane_store
        .create_local_test_pane(&key, pane_cols, 24)
        .unwrap();
    // Enable SGR mouse tracking (`tr3x` enables `?1000h` + `?1006h`) and emit a TUI gutter-wrapped path.
    let osc8 = format!(
        "OSC8 \x1b]8;;file://{}\x1b\\demo_link\x1b]8;;\x1b\\  plain_words\r\n",
        code_file.display()
    );
    let payload = format!("\x1b[?1000h\x1b[?1006h{prefix}{cont}{osc8}");
    ctrl.pane_store
        .feed_local_test_bytes(&key, payload.as_bytes())
        .unwrap();

    // Populate `last_ui_output` and `last_cell_metrics` so pointer hit-testing maps cleanly.
    ctrl.last_cell_metrics = Some(CellMetrics {
        font_size: 14.0,
        scale_factor: 1.0,
        cell_width: 8.0,
        cell_height: 20.0,
        baseline_y: 15.0,
        underline_y: 17.0,
        underline_thickness: 1.5,
        strikethrough_y: 10.0,
    });
    let frame_input = ctrl.build_ui_frame_input(now);
    ctrl.last_ui_output = Some(ctrl.ui.render(&frame_input, &ctrl.theme, now, 1.0));

    let pane_rect = ctrl
        .last_ui_output
        .as_ref()
        .unwrap()
        .tiled_panes
        .first()
        .unwrap()
        .terminal_rect;
    let (pad_x, pad_y) = ctrl.pane_content_padding();
    let content_x = pane_rect.x + pad_x;
    let content_y = pane_rect.y + pad_y;

    // 1. Hover row 1 (`│ demo_target.rs:12`) over `demo_target.rs` -> resolves wrapped path across 2 rows!
    let wrapped_px = content_x + 5.0 * 8.0 + 4.0;
    let wrapped_py = content_y + 1.0 * 20.0 + 10.0;
    ctrl.handle_pointer_move(wrapped_px, wrapped_py, Modifiers::default(), now);
    assert!(
        matches!(
            ctrl.hovered_terminal_target.as_ref(),
            Some((_, HoveredTerminalTarget::LocalFile { line: Some(12), .. }))
        ),
        "expected wrapped LocalFile hover with line 12 under SGR mouse tracking, got {:?}",
        ctrl.hovered_terminal_target
    );
    assert!(
        ctrl.hovered_link_spans
            .as_ref()
            .is_some_and(|(_, spans)| spans.len() >= 2),
        "expected multi-row underline spans across wrapped TUI gutter lines"
    );

    // 2. Plain left-click on the wrapped link -> opens right-hand Viewer Pane without forwarding SGR press!
    let _ = ctrl.drain_pending_commands();
    ctrl.handle_pointer_down(
        wrapped_px,
        wrapped_py,
        PointerButton::Left,
        Modifiers::default(),
        1,
        now,
    );
    ctrl.handle_pointer_up(
        wrapped_px,
        wrapped_py,
        PointerButton::Left,
        Modifiers::default(),
    );
    let cmds_after_link = ctrl.drain_pending_commands();
    assert!(
        !cmds_after_link
            .iter()
            .any(|c| matches!(c, HostCommand::TerminalWrite { .. })),
        "plain link click must not forward SGR mouse sequence to TUI"
    );
    assert!(
        ctrl.ui.viewer.open,
        "viewer pane must open on plain link click"
    );
    assert!(matches!(ctrl.ui.focus.current(), FocusTarget::Viewer));

    // Wait briefly for async text load in MediaViewerBackend and verify jump-to-line 12.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        ctrl.poll_media_events();
        if ctrl
            .ui
            .viewer
            .active_tab()
            .is_some_and(|t| matches!(t.content, ViewerContent::Text(_)))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    match ctrl.ui.viewer.active_tab().map(|t| &t.content) {
        Some(ViewerContent::Text(txt)) => {
            assert_eq!(txt.highlight_line, Some(12));
            assert_eq!(txt.line_count(), 25);
        }
        other => panic!("expected loaded TextViewerState at line 12, got {other:?}"),
    }

    // 3. Re-render chrome (terminal pane shrunk to left) and click non-link cell -> forwards SGR press!
    let frame_input = ctrl.build_ui_frame_input(now);
    ctrl.last_ui_output = Some(ctrl.ui.render(&frame_input, &ctrl.theme, now, 1.0));
    let plain_px = content_x + 20.0 * 8.0 + 4.0;
    let plain_py = content_y + 2.0 * 20.0 + 10.0;
    ctrl.handle_pointer_move(plain_px, plain_py, Modifiers::default(), now);
    assert!(ctrl.hovered_terminal_target.is_none());
    let _ = ctrl.drain_pending_commands();
    ctrl.handle_pointer_down(
        plain_px,
        plain_py,
        PointerButton::Left,
        Modifiers::default(),
        2,
        now,
    );
    let cmds_after_plain = ctrl.drain_pending_commands();
    assert!(
        cmds_after_plain.iter().any(
            |c| matches!(c, HostCommand::TerminalWrite { data, .. } if data.contains("\x1b[<"))
        ),
        "clicking non-link cell under SGR mouse tracking must still forward SGR mouse press"
    );

    // 4. Right-click on the OSC 8 hyperlink -> context menu includes `Open in Right Pane`, `Open External`, `Copy Path`.
    let osc8_px = content_x + 7.0 * 8.0 + 4.0;
    let osc8_py = content_y + 2.0 * 20.0 + 10.0;
    ctrl.handle_pointer_move(osc8_px, osc8_py, Modifiers::default(), now);
    ctrl.handle_pointer_down(
        osc8_px,
        osc8_py,
        PointerButton::Right,
        Modifiers::default(),
        3,
        now,
    );
    let labels: Vec<String> = ctrl
        .ui
        .context_menu
        .as_ref()
        .expect("context menu should open on right-click over OSC 8 link")
        .items
        .iter()
        .map(|i| i.label.clone())
        .collect();
    assert_eq!(
        labels.first().map(String::as_str),
        Some("Open in Right Pane")
    );
    assert_eq!(labels.get(1).map(String::as_str), Some("Open External"));
    assert_eq!(labels.get(2).map(String::as_str), Some("Copy Path"));

    let _ = std::fs::remove_dir_all(&tmp_dir);
    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn viewer_client_command_image_load_and_keyboard_shortcuts_work_end_to_end() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();

    let tmp_dir =
        std::env::temp_dir().join(format!("omarex-viewer-img-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp test dir");
    let png_path = tmp_dir.join("sample.png");
    let rgba = [
        255u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
    ];
    gpu_renderer::save_rgba_png(&png_path, 2, 2, &rgba).expect("write sample.png");

    // Open via Rex `client.viewer.open` command.
    ctrl.execute_client_command(
        LOCAL_HOST_ID,
        ClientCommand::ViewerOpen {
            path: png_path.to_string_lossy().into_owned(),
            line: None,
            col: None,
            remote_host: None,
        },
        now,
    );
    assert!(ctrl.ui.viewer.open);
    assert!(matches!(ctrl.ui.focus.current(), FocusTarget::Viewer));

    // Poll MediaViewerBackend until image finishes decoding and queues GPU upload.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        ctrl.poll_media_events();
        if ctrl
            .ui
            .viewer
            .active_tab()
            .is_some_and(|t| matches!(t.content, ViewerContent::Image(_)))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !ctrl.pending_viewer_uploads.is_empty(),
        "decoded image must queue a GPU RGBA8 upload"
    );

    let send_key = |ctrl: &mut OmarexController, canonical: &str, mods: Modifiers| {
        ctrl.handle_key(
            &KeyInputEvent {
                physical_key: PhysicalKey::Unidentified,
                canonical_key: canonical.to_owned(),
                text: None,
                unshifted_codepoint: 0,
                mods,
                pressed: true,
                repeat: false,
                composing: false,
            },
            now,
        );
    };

    // `+` zooms in (exiting fit mode), `0` resets to 1.0, `f` re-enables fit mode.
    send_key(&mut ctrl, "+", Modifiers::default());
    match ctrl.ui.viewer.active_tab().map(|t| &t.content) {
        Some(ViewerContent::Image(img)) => {
            assert!(!img.viewport.fit_mode);
            assert!(img.viewport.zoom > 1.0);
        }
        other => panic!("expected Image viewer tab, got {other:?}"),
    }
    send_key(&mut ctrl, "0", Modifiers::default());
    match ctrl.ui.viewer.active_tab().map(|t| &t.content) {
        Some(ViewerContent::Image(img)) => {
            assert!(!img.viewport.fit_mode);
            assert!((img.viewport.zoom - 1.0).abs() < 1e-4);
        }
        other => panic!("expected Image viewer tab, got {other:?}"),
    }
    send_key(&mut ctrl, "f", Modifiers::default());
    match ctrl.ui.viewer.active_tab().map(|t| &t.content) {
        Some(ViewerContent::Image(img)) => assert!(img.viewport.fit_mode),
        other => panic!("expected Image viewer tab, got {other:?}"),
    }

    // `QuickLook` closes open viewer when no link is hovered, then reopens it.
    ctrl.execute_client_command(LOCAL_HOST_ID, ClientCommand::QuickLook, now);
    assert!(!ctrl.ui.viewer.open);
    ctrl.execute_client_command(LOCAL_HOST_ID, ClientCommand::QuickLook, now);
    assert!(ctrl.ui.viewer.open);

    // `Ctrl+W` closes the active viewer tab and closes the pane when last tab is closed.
    send_key(
        &mut ctrl,
        "w",
        Modifiers {
            ctrl: true,
            ..Modifiers::default()
        },
    );
    assert!(ctrl.ui.viewer.tabs.is_empty());
    assert!(!ctrl.ui.viewer.open);
    assert!(matches!(ctrl.ui.focus.current(), FocusTarget::Terminal(_)));

    let _ = std::fs::remove_dir_all(&tmp_dir);
    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn text_jump_uses_viewport_height_and_remote_external_resolves_host_label() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    ctrl.set_viewport(1440, 900, 1.0);

    let tmp_dir =
        std::env::temp_dir().join(format!("omarex-jump-viewport-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp test dir");
    let md_path = tmp_dir.join("README.md");
    let mut body = String::new();
    for idx in 1..=60 {
        body.push_str(&format!("Line {idx} content\n"));
    }
    std::fs::write(&md_path, &body).expect("write README.md");

    ctrl.open_local_path_in_viewer(
        md_path.clone(),
        "README.md".to_owned(),
        Some(10),
        None,
        Some(TargetKind::Markdown),
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        ctrl.poll_media_events();
        if ctrl
            .ui
            .viewer
            .active_tab()
            .is_some_and(|t| matches!(t.content, ViewerContent::Text(_)))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    match ctrl.ui.viewer.active_tab().map(|t| &t.content) {
        Some(ViewerContent::Text(txt)) => {
            assert_eq!(txt.highlight_line, Some(10));
            assert!(
                txt.last_viewport_h >= 700.0,
                "expected real viewport height >= 700, got {}",
                txt.last_viewport_h
            );
            assert!(
                txt.scroll_y <= 1.0,
                "jumping to line 10 on a 900px viewport must keep line 1 visible, got scroll_y={}",
                txt.scroll_y
            );
        }
        other => panic!("expected TextViewerState, got {other:?}"),
    }

    // Verify remote host label -> host_id resolution for ViewerAction::OpenExternal cache lookup.
    let mut remote = HostEntry::new(
        "remote:mac-studio",
        "Mac Studio",
        RexEndpoint::parse("https://mac.example.ts.net").unwrap(),
    );
    remote.is_local = false;
    ctrl.workspace.add_host(remote);
    assert_eq!(
        ctrl.resolve_remote_viewer_host_id("Mac Studio"),
        "remote:mac-studio"
    );
    assert_eq!(
        ctrl.resolve_remote_viewer_host_id("remote:mac-studio"),
        "remote:mac-studio"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn pressing_bare_modifier_keys_does_not_scroll_viewport_to_bottom() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");
    ctrl.pane_store
        .create_local_test_pane(&key, 80, 10)
        .expect("create local pane");
    // Populate scrollback history well beyond the 10-row viewport.
    let mut lines = String::new();
    for i in 1..=80 {
        lines.push_str(&format!("history line {i:03} https://example.com/{i}\r\n"));
    }
    ctrl.pane_store
        .feed_local_test_bytes(&key, lines.as_bytes())
        .expect("feed scrollback");
    ctrl.pane_store
        .scroll_to_top(&key)
        .expect("scroll to top of history");
    let sb_top = ctrl.pane_store.scrollbar(&key).expect("scrollbar metrics");
    assert_eq!(sb_top.offset, 0, "precondition: scrolled to top");
    assert!(
        sb_top.total > sb_top.len,
        "precondition: scrollback must exceed viewport"
    );
    ctrl.drain_pending_commands();

    let now = Instant::now();
    for (phys, canonical, mods) in [
        (
            PhysicalKey::ControlLeft,
            "control",
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        ),
        (
            PhysicalKey::ControlRight,
            "control",
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        ),
        (
            PhysicalKey::ShiftLeft,
            "shift",
            Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
        ),
        (
            PhysicalKey::AltLeft,
            "alt",
            Modifiers {
                alt: true,
                ..Modifiers::NONE
            },
        ),
        (
            PhysicalKey::MetaLeft,
            "super",
            Modifiers {
                super_key: true,
                ..Modifiers::NONE
            },
        ),
        (PhysicalKey::CapsLock, "capslock", Modifiers::NONE),
    ] {
        ctrl.handle_key(
            &KeyInputEvent {
                physical_key: phys,
                canonical_key: canonical.to_owned(),
                text: None,
                unshifted_codepoint: 0,
                mods,
                pressed: true,
                repeat: false,
                composing: false,
            },
            now,
        );
        let sb_after = ctrl.pane_store.scrollbar(&key).expect("scrollbar metrics");
        assert_eq!(
            sb_after.offset, 0,
            "pressing bare modifier {phys:?} must NOT jump scrollback to bottom"
        );
    }

    // Typing an actual character DOES snap back to the live bottom row and writes to PTY.
    ctrl.handle_key(
        &KeyInputEvent {
            physical_key: PhysicalKey::KeyA,
            canonical_key: "a".to_owned(),
            text: Some("a".to_owned()),
            unshifted_codepoint: 'a' as u32,
            mods: Modifiers::NONE,
            pressed: true,
            repeat: false,
            composing: false,
        },
        now,
    );
    let sb_typed = ctrl.pane_store.scrollbar(&key).expect("scrollbar metrics");
    assert_eq!(
        sb_typed.offset,
        sb_typed.total.saturating_sub(sb_typed.len),
        "typing a real character must still snap scrollback to bottom"
    );
    assert!(ctrl.drain_pending_commands().iter().any(|c| matches!(
        c,
        HostCommand::TerminalWrite { data, .. } if data == "a"
    )));

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn resolves_t3_thread_v2_and_t3code_uris_for_desktop_and_web_fallback() {
    let uuid_link = resolve_thread_open_targets(
        "t3-thread://v2/11111111-2222-3333-4444-555555555555/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
    );
    assert_eq!(
        uuid_link.desktop_uri.as_deref(),
        Some(
            "t3code://threads/11111111-2222-3333-4444-555555555555/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
        )
    );
    assert_eq!(
        uuid_link.web_fallback_url.as_deref(),
        Some(
            "https://app.t3.codes/threads/11111111-2222-3333-4444-555555555555/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
        )
    );

    // Delegated task IDs contain colons and literal percent escapes that must round-trip.
    let delegated = resolve_thread_open_targets(
        "t3-thread://v2/env-1/thread:delegated-task:command%3Amcp%3Aabc?prompt=hi",
    );
    assert_eq!(
        delegated.desktop_uri.as_deref(),
        Some("t3code://threads/env-1/thread%3Adelegated-task%3Acommand%253Amcp%253Aabc?prompt=hi")
    );
    assert_eq!(
        delegated.web_fallback_url.as_deref(),
        Some(
            "https://app.t3.codes/threads/env-1/thread%3Adelegated-task%3Acommand%253Amcp%253Aabc?prompt=hi"
        )
    );

    // Direct `t3code://threads/...` links are preserved verbatim and gain a hosted fallback.
    let direct = resolve_thread_open_targets("t3code://threads/env-1/thread-2");
    assert_eq!(
        direct.desktop_uri.as_deref(),
        Some("t3code://threads/env-1/thread-2")
    );
    assert_eq!(
        direct.web_fallback_url.as_deref(),
        Some("https://app.t3.codes/threads/env-1/thread-2")
    );

    // Non-thread `t3code://` links stay desktop-only.
    let new_link = resolve_thread_open_targets("t3code://new?prompt=test");
    assert_eq!(
        new_link.desktop_uri.as_deref(),
        Some("t3code://new?prompt=test")
    );
    assert_eq!(new_link.web_fallback_url, None);

    // Legacy v1 or malformed `t3-thread://` URIs never fall through as raw custom schemes.
    let legacy = resolve_thread_open_targets("t3-thread://v1/thread-only");
    assert_eq!(legacy.desktop_uri, None);
    assert_eq!(legacy.web_fallback_url, None);
}

#[test]
fn copy_selection_triggers_220ms_shine_and_clears_wakeup_after_expiry() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    ctrl.set_viewport(1280, 800, 1.0);
    let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");
    ctrl.pane_store
        .create_local_test_pane(&key, 80, 24)
        .unwrap();
    ctrl.pane_store
        .feed_local_test_bytes(&key, b"hello copy shine\r\n")
        .unwrap();
    ctrl.pane_store.select_all(&key).unwrap();

    let _gpu_guard = crate::lock_headless_gpu_test();
    let mut renderer = GpuRenderer::new_headless(GpuConfig::default()).expect("headless gpu");
    let t0 = Instant::now();
    // Initial render settles first-focus border spring.
    let _ = ctrl.render_frame(&mut renderer, t0).expect("initial frame");

    // Copy selection works and triggers shine even when system clipboard is disabled.
    ctrl.dispatch_ui_action(UiAction::CopySelection { block_id: None }, t0);
    let scene_active = ctrl
        .render_frame(&mut renderer, t0 + Duration::from_millis(40))
        .expect("shine frame");
    assert!(
        scene_active
            .layers
            .values()
            .flat_map(|l| l.quads.iter())
            .any(|q| (q.params[1] - gpu_renderer::QuadInstance::MODE_SHINE).abs() < 0.01),
        "expected MODE_SHINE quad on active copy shine frame"
    );
    assert!(
        ctrl.next_wakeup_deadline(t0 + Duration::from_millis(50))
            .is_some(),
        "expected active wakeup deadline during 220ms copy shine"
    );

    // After 220ms expiry + settle, shine quads disappear and idle wakeup returns None.
    let expired = t0 + Duration::from_millis(300);
    let scene_expired = ctrl
        .render_frame(&mut renderer, expired)
        .expect("expired frame");
    assert!(
        !scene_expired
            .layers
            .values()
            .flat_map(|l| l.quads.iter())
            .any(|q| (q.params[1] - gpu_renderer::QuadInstance::MODE_SHINE).abs() < 0.01),
        "copy shine quad must clear after 220ms"
    );
    let mut t = expired;
    for _ in 0..60 {
        t += Duration::from_millis(16);
        let _ = ctrl.tick(t);
    }
    assert_eq!(
        ctrl.next_wakeup_deadline(t),
        None,
        "idle wakeup must return None (ui_anim={} inertia={} shines={} toasts={} resize={:?} snap={} media={:?} working={})",
        ctrl.ui.has_active_animations(),
        ctrl.pane_store.has_active_scroll_inertia(),
        ctrl.copy_shines.len(),
        ctrl.toast_deadlines.len(),
        ctrl.workspace.next_resize_deadline(),
        ctrl.pane_store.has_pending_snapshots(),
        ctrl.media_backend.next_wakeup_instant(),
        ctrl.window_focused && ctrl.has_visible_working_indicator(),
    );

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn sidebar_slide_gates_resize_and_settles_to_zero_idle_wakeup() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    ctrl.set_viewport(1280, 800, 1.0);
    let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");
    ctrl.pane_store
        .create_local_test_pane(&key, 80, 24)
        .unwrap();

    let _gpu_guard = crate::lock_headless_gpu_test();
    let mut renderer = GpuRenderer::new_headless(GpuConfig::default()).expect("headless gpu");
    let t0 = Instant::now();
    let _ = ctrl.render_frame(&mut renderer, t0).expect("frame 0");
    let _ = ctrl.drain_pending_commands();

    // Collapse the docked sidebar -> starts slide transition and gates PTY resize during motion.
    ctrl.ui.layout_cfg.sidebar_collapsed = true;
    ctrl.dispatch_ui_action(UiAction::ToggleSidebar, t0 + Duration::from_millis(1));
    let _ = ctrl
        .render_frame(&mut renderer, t0 + Duration::from_millis(17))
        .expect("mid-slide frame");
    assert!(
        ctrl.ui.is_layout_animating(),
        "sidebar collapse should animate layout width"
    );
    let mid_cmds = ctrl.drain_pending_commands();
    assert!(
        !mid_cmds
            .iter()
            .any(|c| matches!(c, HostCommand::TerminalResize { .. })),
        "PTY resize must be gated while sidebar layout animation is in flight"
    );
    assert!(
        ctrl.next_wakeup_deadline(t0 + Duration::from_millis(17))
            .is_some(),
        "expected wakeup deadline while sidebar spring is animating"
    );

    // Advance until settled; wakeup returns None and final PTY resize flushes.
    let mut t = t0 + Duration::from_millis(17);
    for _ in 0..90 {
        t += Duration::from_millis(16);
        let _ = ctrl.render_frame(&mut renderer, t).expect("settle frame");
        if !ctrl.ui.has_active_animations() {
            break;
        }
    }
    assert!(!ctrl.ui.is_layout_animating());
    assert_eq!(
        ctrl.next_wakeup_deadline(t),
        None,
        "expected 0% idle wakeup once sidebar spring settled (ui_anim={} inertia={} shines={} toasts={} resize={:?} snap={} media={:?} working={})",
        ctrl.ui.has_active_animations(),
        ctrl.pane_store.has_active_scroll_inertia(),
        ctrl.copy_shines.len(),
        ctrl.toast_deadlines.len(),
        ctrl.workspace.next_resize_deadline(),
        ctrl.pane_store.has_pending_snapshots(),
        ctrl.media_backend.next_wakeup_instant(),
        ctrl.window_focused && ctrl.has_visible_working_indicator(),
    );

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn program_bg_and_new_output_pill_render_on_frame_1_and_click_scrolls_to_bottom() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    ctrl.set_viewport(1280, 800, 1.0);
    let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");
    ctrl.pane_store
        .create_local_test_pane(&key, 80, 10)
        .unwrap();

    // Scroll back and emit new output -> `↓ New output` pill renders and clicking scrolls to bottom.
    let mut lines = String::new();
    for idx in 1..=30 {
        lines.push_str(&format!("line {idx}\r\n"));
    }
    ctrl.pane_store
        .feed_local_test_bytes(&key, lines.as_bytes())
        .unwrap();
    ctrl.pane_store.scroll_wheel_px(&key, 80.0, 20.0).unwrap();
    ctrl.pane_store
        .feed_local_test_bytes(&key, b"unread output line\r\n")
        .unwrap();
    assert!(ctrl.pane_store.has_unread_output(&key));

    let _gpu_guard = crate::lock_headless_gpu_test();
    let mut renderer = GpuRenderer::new_headless(GpuConfig::default()).expect("headless gpu");
    let t0 = Instant::now();
    let _ = ctrl.render_frame(&mut renderer, t0).expect("pill frame");
    let has_pill = ctrl.last_ui_output.as_ref().is_some_and(|out| {
        out.draw_list.hits.iter().any(|hb| {
            matches!(
                &hb.target,
                ui_chrome::HitTarget::PaneNewOutputPill { block_id } if block_id == "block:1"
            )
        })
    });
    assert!(has_pill, "expected floating New output pill hit target");

    ctrl.dispatch_ui_action(
        UiAction::ScrollPaneToBottom {
            block_id: "block:1".into(),
        },
        t0 + Duration::from_millis(16),
    );
    assert!(!ctrl.pane_store.is_scrolled_up(&key));
    assert!(!ctrl.pane_store.has_unread_output(&key));

    // Verify frame-1 `program_bg` capture on a fresh controller (`--headless-render` semantics).
    let mut ctrl2 = make_test_controller();
    seed_single_pane_workspace(&mut ctrl2);
    ctrl2.set_viewport(1280, 800, 1.0);
    ctrl2
        .pane_store
        .create_local_test_pane(&key, 80, 24)
        .unwrap();
    ctrl2
        .pane_store
        .feed_local_test_bytes(&key, b"\x1b[?1049h\x1b[48;2;20;60;90m\x1b[2J")
        .unwrap();
    let _ = ctrl2
        .render_frame(&mut renderer, t0 + Duration::from_millis(32))
        .expect("frame 1 program_bg");
    assert_eq!(
        ctrl2.pane_store.program_bg(&key),
        Some(vt_engine::Rgb {
            r: 20,
            g: 60,
            b: 90,
        }),
        "frame 1 render must sample alt-screen program_bg prior to chrome build"
    );

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
    if let Some(parent) = ctrl2.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn m3_viewer_search_selection_copy_shine_and_live_reload_preserve_state() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    ctrl.set_viewport(1440, 900, 1.0);
    let _gpu_guard = crate::lock_headless_gpu_test();
    let mut renderer = GpuRenderer::new_headless(GpuConfig::default()).expect("headless renderer");

    let now = Instant::now();
    let send_key = |ctrl: &mut OmarexController, canonical: &str, mods: Modifiers| {
        ctrl.handle_key(
            &KeyInputEvent {
                physical_key: PhysicalKey::Unidentified,
                canonical_key: canonical.to_owned(),
                text: None,
                unshifted_codepoint: 0,
                mods,
                pressed: true,
                repeat: false,
                composing: false,
            },
            now,
        );
    };

    let tmp_dir =
        std::env::temp_dir().join(format!("omarex-m3-bridge-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp test dir");
    let rs_path = tmp_dir.join("demo.rs");
    let mut body = String::new();
    for idx in 1..=80 {
        body.push_str(&format!("let needle_line_{idx} = {idx};\n"));
    }
    std::fs::write(&rs_path, &body).expect("write demo.rs");

    ctrl.open_local_path_in_viewer(
        rs_path.clone(),
        "demo.rs".to_owned(),
        Some(12),
        None,
        Some(TargetKind::TextCode),
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        ctrl.poll_media_events();
        if ctrl
            .ui
            .viewer
            .active_tab()
            .is_some_and(|t| matches!(t.content, ViewerContent::Text(_)))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let now = Instant::now();
    let _ = ctrl
        .render_frame(&mut renderer, now)
        .expect("render viewer");

    // 1. Ctrl+F enters FocusTarget::ViewerSearch, typing & paste updates matches, Esc returns to Viewer.
    send_key(
        &mut ctrl,
        "f",
        Modifiers {
            ctrl: true,
            ..Modifiers::default()
        },
    );
    assert_eq!(ctrl.ui.focus.current(), &FocusTarget::ViewerSearch);
    ctrl.handle_paste("needle_line_4", now);
    assert!(ctrl.ui.viewer.search.matches.len() >= 10);
    let first_idx = ctrl.ui.viewer.search.active_match;
    send_key(&mut ctrl, "enter", Modifiers::default());
    assert_ne!(ctrl.ui.viewer.search.active_match, first_idx);
    send_key(&mut ctrl, "escape", Modifiers::default());
    assert_eq!(ctrl.ui.focus.current(), &FocusTarget::Viewer);
    assert!(!ctrl.ui.viewer.search.open);

    // 2. Ctrl+Shift+C copies `path:line` toast (using highlight_line == 12 when unselected).
    send_key(
        &mut ctrl,
        "c",
        Modifiers {
            ctrl: true,
            shift: true,
            ..Modifiers::default()
        },
    );
    assert!(
        ctrl.ui
            .toasts
            .iter()
            .any(|t| t.title == "Copied path:line" && t.body.ends_with(":12"))
    );

    // 3. Ctrl+A + Ctrl+C copies selection and schedules CopyShineEffect at `layers::MODAL - 1`.
    send_key(
        &mut ctrl,
        "a",
        Modifiers {
            ctrl: true,
            ..Modifiers::default()
        },
    );
    send_key(
        &mut ctrl,
        "c",
        Modifiers {
            ctrl: true,
            ..Modifiers::default()
        },
    );
    assert!(!ctrl.copy_shines.is_empty());
    assert_eq!(
        ctrl.copy_shines.last().unwrap().layer,
        gpu_renderer::layers::MODAL - 1
    );

    // 4. `notify` reload preserves scroll_y and word_wrap and does not steal focus from terminal.
    ctrl.ui.viewer.toggle_wrap();
    ctrl.ui.viewer.scroll_by(180.0);
    let saved_scroll = match &ctrl.ui.viewer.active_tab().unwrap().content {
        ViewerContent::Text(txt) => {
            assert!(txt.word_wrap);
            txt.scroll_y
        }
        other => panic!("expected Text, got {other:?}"),
    };
    ctrl.ui.focus.restore_terminal();
    assert!(matches!(ctrl.ui.focus.current(), FocusTarget::Terminal(_)));

    let updated = body.replace("needle_line_1 =", "needle_line_1_edited =");
    std::fs::write(&rs_path, &updated).expect("modify demo.rs");
    let reload_deadline = Instant::now() + Duration::from_secs(3);
    let mut reloaded = false;
    while Instant::now() < reload_deadline {
        if ctrl.poll_media_events()
            && let Some(ViewerContent::Text(txt)) = ctrl.ui.viewer.active_tab().map(|t| &t.content)
            && txt.lines[0].contains("needle_line_1_edited")
        {
            reloaded = true;
            assert!(txt.word_wrap);
            assert!((txt.scroll_y - saved_scroll).abs() < 1.0);
            break;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    assert!(reloaded, "expected inotify live reload");
    assert!(
        matches!(ctrl.ui.focus.current(), FocusTarget::Terminal(_)),
        "live reload must never steal focus back to viewer"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn m3_multi_inline_markdown_images_splice_individually_and_audio_pdf_render() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    ctrl.set_viewport(1440, 900, 1.0);
    let _gpu_guard = crate::lock_headless_gpu_test();
    let mut renderer = GpuRenderer::new_headless(GpuConfig::default()).expect("headless renderer");

    let tmp_dir = std::env::temp_dir().join(format!("omarex-m3-multi-img-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir");
    let img_a = tmp_dir.join("a.png");
    let img_b = tmp_dir.join("b.png");
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        16,
        16,
        image::Rgba([220, 40, 40, 255]),
    ))
    .save_with_format(&img_a, image::ImageFormat::Png)
    .unwrap();
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        16,
        16,
        image::Rgba([40, 40, 220, 255]),
    ))
    .save_with_format(&img_b, image::ImageFormat::Png)
    .unwrap();

    let md_path = tmp_dir.join("multi.md");
    std::fs::write(&md_path, "# Two Images\n\n![A](a.png)\n\n![B](b.png)\n").unwrap();

    ctrl.open_local_path_in_viewer(
        md_path,
        "multi.md".to_owned(),
        None,
        None,
        Some(TargetKind::Markdown),
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        ctrl.poll_media_events();
        if ctrl
            .ui
            .viewer
            .active_tab()
            .is_some_and(|t| matches!(t.content, ViewerContent::Text(_)))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let scene = ctrl
        .render_frame(&mut renderer, Instant::now())
        .expect("render multi-image markdown");
    let ui_out = ctrl.last_ui_output.as_ref().expect("ui output");
    assert_eq!(ui_out.viewer_images.len(), 2);

    let key_a = ui_out.viewer_images[0].texture_key;
    let key_b = ui_out.viewer_images[1].texture_key;
    assert_ne!(key_a, key_b);
    let image_quads: Vec<u64> = scene
        .layers
        .values()
        .flat_map(|l| l.images.iter())
        .map(|q| q.image_id)
        .collect();
    assert!(image_quads.contains(&key_a));
    assert!(image_quads.contains(&key_b));
    assert!(
        scene.layers.values().any(|l| !l.text_runs.is_empty()),
        "expected RichText runs translated into SceneFrame"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn m3_r2_fractional_scale_richtext_alignment_reload_error_preservation_and_ctrl_tab_search() {
    let _gpu_guard = crate::lock_headless_gpu_test();
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    ctrl.set_viewport(2400, 900, 1.25);
    let mut renderer = GpuRenderer::new_headless(GpuConfig::default()).expect("headless renderer");

    let tmp_dir = std::env::temp_dir().join(format!("omarex-m3-r2-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir");
    let long_rs = tmp_dir.join("long_tok.rs");
    let mut rs_body = String::from(
        "let very_long_identifier_name_that_exceeds_forty_chars = \"01234567890123456789012345678901234567890123456789\" + 999;\n",
    );
    for i in 2..=60 {
        rs_body.push_str(&format!("let filler_item_{i} = {i};\n"));
    }
    std::fs::write(&long_rs, &rs_body).expect("write long_tok.rs");

    let tab_a = ctrl.open_local_path_in_viewer(
        long_rs.clone(),
        "long_tok.rs".to_owned(),
        None,
        None,
        Some(TargetKind::TextCode),
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        ctrl.poll_media_events();
        if ctrl
            .ui
            .viewer
            .tab_by_id(tab_a)
            .is_some_and(|t| matches!(t.content, ViewerContent::Text(_)))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let now = Instant::now();
    let scene = ctrl
        .render_frame(&mut renderer, now)
        .expect("render @1.25x");

    // 1. Exact un-snapped `mono_cell_w` and non-overlapping RichText runs at 1.25x scale.
    let expected_cell_w = renderer.atlas_mut().measure_text_advance(
        "MMMMMMMMMM",
        12.5,
        (12.5_f32 * 1.25).ceil(),
        &gpu_renderer::FontFamily::Monospace,
        gpu_renderer::FontWeight::Regular,
        gpu_renderer::FontStyle::Normal,
    ) / 10.0;
    assert!(
        (ctrl.ui.viewer.mono_cell_w - expected_cell_w).abs() < 0.01,
        "expected un-snapped mono_cell_w={expected_cell_w}, got {}",
        ctrl.ui.viewer.mono_cell_w
    );

    let line1_y = scene
        .layers
        .values()
        .flat_map(|l| l.text_runs.iter())
        .find(|r| r.text.contains("very_long_identifier"))
        .expect("line 1 identifier run")
        .y;
    let mut line1_runs: Vec<_> = scene
        .layers
        .values()
        .flat_map(|l| l.text_runs.iter())
        .filter(|r| {
            r.family == gpu_renderer::FontFamily::Monospace
                && (r.y - line1_y).abs() < 0.5
                && (r.text.contains("very_long_identifier") || r.text == "=" || r.text == "+")
        })
        .collect();
    line1_runs.sort_by(|a, b| a.x.total_cmp(&b.x));
    assert!(
        line1_runs.len() >= 3,
        "expected RichText runs for identifier, '=', and '+'"
    );
    let lh = (12.5_f32 * 1.25).ceil();
    for pair in line1_runs.windows(2) {
        let prev = pair[0];
        let next = pair[1];
        let prev_w = renderer.atlas_mut().measure_text_advance(
            &prev.text,
            prev.font_size,
            lh,
            &prev.family,
            prev.weight,
            prev.style,
        );
        assert!(
            next.x >= prev.x + prev_w - 0.05,
            "token overlap at 1.25x: {:?} (x={}, w={prev_w}) overlaps {:?} (x={})",
            prev.text,
            prev.x,
            next.text,
            next.x
        );
    }

    // 2. LoadError { is_reload: true } preserves TextViewerState (scroll_y + word_wrap) and recovers.
    ctrl.ui.viewer.toggle_wrap();
    ctrl.ui.viewer.scroll_by(140.0);
    let saved_scroll = match &ctrl.ui.viewer.tab_by_id(tab_a).unwrap().content {
        ViewerContent::Text(t) => {
            assert!(t.word_wrap);
            t.scroll_y
        }
        other => panic!("expected Text, got {other:?}"),
    };
    std::fs::remove_file(&long_rs).expect("delete long_tok.rs");
    let err_deadline = Instant::now() + Duration::from_secs(3);
    let mut saw_toast = false;
    while Instant::now() < err_deadline {
        if ctrl.poll_media_events()
            && ctrl
                .ui
                .toasts
                .iter()
                .any(|t| t.title.starts_with("Reload paused"))
        {
            saw_toast = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    assert!(saw_toast, "expected non-destructive reload toast");
    match &ctrl.ui.viewer.tab_by_id(tab_a).unwrap().content {
        ViewerContent::Text(t) => {
            assert!(t.word_wrap);
            assert!((t.scroll_y - saved_scroll).abs() < 1.0);
        }
        other => panic!("LoadError during reload must preserve TextViewerState, got {other:?}"),
    }

    let updated_rs = rs_body.replace("filler_item_2", "recreated_item_2");
    std::fs::write(&long_rs, &updated_rs).expect("recreate long_tok.rs");
    let rec_deadline = Instant::now() + Duration::from_secs(3);
    let mut recreated = false;
    while Instant::now() < rec_deadline {
        if ctrl.poll_media_events()
            && let Some(ViewerContent::Text(t)) =
                ctrl.ui.viewer.tab_by_id(tab_a).map(|tab| &tab.content)
            && t.lines.iter().any(|l| l.contains("recreated_item_2"))
        {
            recreated = true;
            assert!(t.word_wrap);
            assert!((t.scroll_y - saved_scroll).abs() < 1.0);
            break;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    assert!(recreated, "expected recreated file to reload cleanly");

    // 3. Ctrl+Tab recomputes Ctrl+F search matches for the newly active tab.
    let beta_rs = tmp_dir.join("beta.rs");
    std::fs::write(&beta_rs, "let only_in_beta_target = 1;\n").expect("write beta.rs");
    let tab_b = ctrl.open_local_path_in_viewer(
        beta_rs,
        "beta.rs".to_owned(),
        None,
        None,
        Some(TargetKind::TextCode),
    );
    let b_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < b_deadline {
        ctrl.poll_media_events();
        if ctrl
            .ui
            .viewer
            .tab_by_id(tab_b)
            .is_some_and(|t| matches!(t.content, ViewerContent::Text(_)))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    ctrl.ui.viewer.select_tab_by_id(tab_a);
    ctrl.ui.viewer.open_search();
    ctrl.ui.viewer.search.input.set_text("only_in_beta_target");
    ctrl.ui.viewer.recompute_search();
    assert_eq!(ctrl.ui.viewer.search.matches.len(), 0);
    ctrl.ui.focus_viewer();
    ctrl.handle_key(
        &KeyInputEvent {
            physical_key: PhysicalKey::Tab,
            canonical_key: "tab".to_owned(),
            text: None,
            unshifted_codepoint: 9,
            mods: Modifiers {
                ctrl: true,
                ..Modifiers::default()
            },
            pressed: true,
            repeat: false,
            composing: false,
        },
        Instant::now(),
    );
    assert_eq!(ctrl.ui.viewer.active_tab().map(|t| t.id), Some(tab_b));
    assert_eq!(ctrl.ui.viewer.search.matches.len(), 1);

    // 4. AtlasTextMeasurer gives exact proportional link hit-rect alignment (zero drift).
    let md_path = tmp_dir.join("link_align.md");
    std::fs::write(
        &md_path,
        "iiiiilllll [Click Link](https://example.com \"Tip\") end\n",
    )
    .expect("write link_align.md");
    let tab_md = ctrl.open_local_path_in_viewer(
        md_path,
        "link_align.md".to_owned(),
        None,
        None,
        Some(TargetKind::Markdown),
    );
    let md_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < md_deadline {
        ctrl.poll_media_events();
        if ctrl
            .ui
            .viewer
            .tab_by_id(tab_md)
            .is_some_and(|t| matches!(t.content, ViewerContent::Text(_)))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = ctrl
        .render_frame(&mut renderer, Instant::now())
        .expect("render md @1.25x");
    let ui_out = ctrl.last_ui_output.as_ref().expect("ui output");
    let link_hit = ui_out
        .draw_list
        .hits
        .iter()
        .find(
            |h| matches!(&h.target, HitTarget::ViewerLink { url } if url == "https://example.com"),
        )
        .expect("ViewerLink hit");
    let body_x = ui_out.viewer_metrics.text_body_rect.unwrap().x;
    let exact_prefix = renderer.atlas_mut().measure_text_advance(
        "iiiiilllll ",
        12.5,
        (12.5_f32 * 1.25).ceil(),
        &gpu_renderer::FontFamily::SansSerif,
        gpu_renderer::FontWeight::Regular,
        gpu_renderer::FontStyle::Normal,
    );
    let exact_link_w = renderer.atlas_mut().measure_text_advance(
        "Click Link",
        12.5,
        (12.5_f32 * 1.25).ceil(),
        &gpu_renderer::FontFamily::SansSerif,
        gpu_renderer::FontWeight::Regular,
        gpu_renderer::FontStyle::Normal,
    );
    assert!(
        (link_hit.rect.x - (body_x + 14.0 + exact_prefix)).abs() < 0.05,
        "expected exact measured link_x {}, got {}",
        body_x + 14.0 + exact_prefix,
        link_hit.rect.x
    );
    assert!(
        (link_hit.rect.w - exact_link_w).abs() < 0.05,
        "expected exact measured link_w {exact_link_w}, got {}",
        link_hit.rect.w
    );

    // 5. Re-opening an already-open PDF tab at a different page sets `loading_page = true`.
    if std::process::Command::new("pdfinfo")
        .arg("-v")
        .output()
        .is_ok()
        && std::process::Command::new("pdftoppm")
            .arg("-v")
            .output()
            .is_ok()
    {
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.4\n");
        let o1 = b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n";
        let o2 = b"2 0 obj\n<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>\nendobj\n";
        let o3 = b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>\nendobj\n";
        let o4 = b"4 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>\nendobj\n";
        let mut offs = Vec::new();
        for o in [&o1[..], &o2[..], &o3[..], &o4[..]] {
            offs.push(pdf.len());
            pdf.extend_from_slice(o);
        }
        let xref = pdf.len();
        pdf.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
        for off in offs {
            pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!("trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
        );

        let pdf_path = tmp_dir.join("sample.pdf");
        std::fs::write(&pdf_path, &pdf).expect("write 2-page pdf");
        let pdf_tab_id = ctrl.open_local_path_in_viewer(
            pdf_path.clone(),
            "sample.pdf".to_owned(),
            Some(1),
            None,
            Some(TargetKind::Pdf),
        );
        let p1_deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < p1_deadline {
            ctrl.poll_media_events();
            if ctrl
                .ui
                .viewer
                .tab_by_id(pdf_tab_id)
                .is_some_and(|t| matches!(t.content, ViewerContent::Pdf(_)))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        ctrl.open_local_path_in_viewer(
            pdf_path,
            "sample.pdf:2".to_owned(),
            Some(2),
            None,
            Some(TargetKind::Pdf),
        );
        match &ctrl.ui.viewer.tab_by_id(pdf_tab_id).unwrap().content {
            ViewerContent::Pdf(p) => assert!(p.loading_page, "expected loading_page = true"),
            other => panic!("expected Pdf, got {other:?}"),
        }
        let p2_deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < p2_deadline {
            ctrl.poll_media_events();
            if let Some(ViewerContent::Pdf(p)) =
                ctrl.ui.viewer.tab_by_id(pdf_tab_id).map(|t| &t.content)
                && p.current_page == 2
                && !p.loading_page
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        match &ctrl.ui.viewer.tab_by_id(pdf_tab_id).unwrap().content {
            ViewerContent::Pdf(p) => {
                assert_eq!(p.current_page, 2);
                assert!(!p.loading_page);
            }
            other => panic!("expected Pdf page 2 loaded, got {other:?}"),
        }
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn m4_f15_copy_mode_motions_visual_search_yank_shine_and_exit() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();
    let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");
    ctrl.pane_store.create_local_test_pane(&key, 40, 6).unwrap();
    for i in 0..12 {
        ctrl.pane_store
            .feed_local_test_bytes(&key, format!("row{i:02} alpha beta\r\n").as_bytes())
            .unwrap();
    }
    ctrl.set_viewport(1200, 800, 1.0);
    ctrl.last_cell_metrics = Some(CellMetrics {
        font_size: 14.0,
        scale_factor: 1.0,
        cell_width: 8.0,
        cell_height: 20.0,
        baseline_y: 15.0,
        underline_y: 17.0,
        underline_thickness: 1.5,
        strikethrough_y: 10.0,
    });
    let frame_input = ctrl.build_ui_frame_input(now);
    ctrl.last_ui_output = Some(ctrl.ui.render(&frame_input, &ctrl.theme, now, 0.0));

    // Enter Copy Mode via ClientCommand::EnterCopyMode
    ctrl.execute_client_command(LOCAL_HOST_ID, ClientCommand::EnterCopyMode, now);
    assert!(ctrl.copy_mode.is_some());
    assert_eq!(ctrl.copy_mode_badge_for(&key), Some("COPY"));

    let press = |cname: &str, text: Option<&str>, shift: bool, ctrl_mod: bool| KeyInputEvent {
        physical_key: PhysicalKey::Unidentified,
        canonical_key: cname.to_owned(),
        text: text.map(str::to_owned),
        unshifted_codepoint: 0,
        mods: Modifiers {
            shift,
            ctrl: ctrl_mod,
            alt: false,
            super_key: false,
        },
        pressed: true,
        repeat: false,
        composing: false,
    };

    // `gg` jumps to top of scrollback
    ctrl.handle_key(&press("g", Some("g"), false, false), now);
    ctrl.handle_key(&press("g", Some("g"), false, false), now);
    assert_eq!(
        ctrl.copy_mode.as_ref().unwrap().cursor,
        ScreenPoint { col: 0, row: 0 }
    );

    // `w` advances to "alpha" (col 6), `v` starts Char visual, `e` extends to end of "alpha" (col 10)
    ctrl.handle_key(&press("w", Some("w"), false, false), now);
    assert_eq!(ctrl.copy_mode.as_ref().unwrap().cursor.col, 6);
    ctrl.handle_key(&press("v", Some("v"), false, false), now);
    assert_eq!(ctrl.copy_mode_badge_for(&key), Some("VISUAL"));
    ctrl.handle_key(&press("e", Some("e"), false, false), now);
    assert_eq!(ctrl.copy_mode.as_ref().unwrap().cursor.col, 10);
    assert_eq!(
        ctrl.pane_store
            .copy_mode_selection_text(&key)
            .unwrap()
            .as_deref(),
        Some("alpha")
    );

    // `V` switches to V-LINE, `Ctrl+v` switches to V-BLOCK
    ctrl.handle_key(&press("v", Some("V"), true, false), now);
    assert_eq!(ctrl.copy_mode_badge_for(&key), Some("V-LINE"));
    ctrl.handle_key(&press("v", None, false, true), now);
    assert_eq!(ctrl.copy_mode_badge_for(&key), Some("V-BLOCK"));

    // `y` yanks, triggers Copy Shine, and exits Copy Mode
    ctrl.handle_key(&press("y", Some("y"), false, false), now);
    assert!(ctrl.copy_mode.is_none());
    assert!(!ctrl.copy_shines.is_empty(), "yank must trigger Copy Shine");

    // Re-enter Copy Mode, `/` search + `n` + `q` exit
    ctrl.execute_client_command(LOCAL_HOST_ID, ClientCommand::EnterCopyMode, now);
    ctrl.handle_key(&press("slash", Some("/"), false, false), now);
    assert_eq!(ctrl.copy_mode_badge_for(&key), Some("COPY /"));
    for ch in ['b', 'e', 't', 'a'] {
        let s = ch.to_string();
        ctrl.handle_key(&press(&s, Some(&s), false, false), now);
    }
    ctrl.handle_key(&press("enter", None, false, false), now);
    ctrl.handle_key(&press("n", Some("n"), false, false), now);
    ctrl.handle_key(&press("q", Some("q"), false, false), now);
    assert!(ctrl.copy_mode.is_none());

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn m4_f16_file_picker_quoting_and_stale_request_correlation() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();
    let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");
    ctrl.pane_store
        .create_local_test_pane(&key, 80, 24)
        .unwrap();
    ctrl.pane_store
        .feed_local_test_bytes(&key, b"\x1b[?2004h")
        .unwrap();
    let _ = ctrl.drain_pending_commands();

    ctrl.execute_client_command(LOCAL_HOST_ID, ClientCommand::InsertPath, now);
    assert_eq!(
        ctrl.ui.focus.current(),
        &FocusTarget::Modal(ModalKind::FilePicker)
    );

    // Stale request_id (0) must be ignored
    ctrl.process_runtime_event(
        crate::host_manager::RuntimeEvent::DirListed {
            key: key.clone(),
            request_id: 0,
            result: Ok(rex_proto::types::TerminalListDirResponse {
                status: rex_proto::types::ListDirStatus::Ok,
                path: "/stale".into(),
                home: "/home".into(),
                entries: vec![],
                truncated: false,
            }),
        },
        now,
    );
    assert!(matches!(
        ctrl.ui.modal.as_ref().map(|m| &m.content),
        Some(ui_chrome::ModalContent::FilePicker { current_dir, .. }) if current_dir != "/stale"
    ));

    // Accept path with space + single quote -> bracketed paste shell-escaped
    ctrl.dispatch_ui_action(
        UiAction::FilePickerAccept {
            path: "/tmp/my 'dir'/file.txt".into(),
        },
        now,
    );
    let written = ctrl
        .drain_pending_commands()
        .into_iter()
        .find_map(|c| match c {
            HostCommand::TerminalWrite { data, .. } => Some(data),
            _ => None,
        })
        .expect("InsertPath must emit TerminalWrite");
    assert_eq!(
        written,
        "\x1b[200~'/tmp/my '\\''dir'\\''/file.txt'\x1b[201~"
    );

    // GoToDirectory emits `cd ...`
    ctrl.execute_client_command(LOCAL_HOST_ID, ClientCommand::GoToDirectory, now);
    ctrl.dispatch_ui_action(
        UiAction::FilePickerAccept {
            path: "/tmp/sub dir".into(),
        },
        now,
    );
    let cd_cmd = ctrl
        .drain_pending_commands()
        .into_iter()
        .find_map(|c| match c {
            HostCommand::TerminalWrite { data, .. } => Some(data),
            _ => None,
        })
        .expect("GoToDirectory must emit cd");
    assert_eq!(cd_cmd, "cd '/tmp/sub dir'\n");

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn m4_f17_theme_switcher_live_preview_revert_commit_and_pinning() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();
    let initial_dark = ctrl.theme.is_dark;
    assert!(initial_dark);

    // Open theme picker, preview light theme without persisting, then cancel -> reverts!
    ctrl.execute_client_command(
        LOCAL_HOST_ID,
        ClientCommand::ChangeTheme { name: None },
        now,
    );
    assert_eq!(
        ctrl.ui.focus.current(),
        &FocusTarget::Modal(ModalKind::ThemePicker)
    );
    ctrl.dispatch_ui_action(
        UiAction::ThemePickerPreview {
            name: omarchy_theme::OMARCHY_DEFAULT_LIGHT_NAME.into(),
        },
        now,
    );
    assert!(!ctrl.theme.is_dark, "live preview should switch to light");
    assert_eq!(ctrl.settings.theme_name, None);

    ctrl.dispatch_ui_action(UiAction::ThemePickerCancel, now);
    assert!(ctrl.theme.is_dark, "cancel must revert palette");
    assert_eq!(ctrl.settings.theme_name, None);

    // Commit light theme -> persists theme_name and ignores live Omarchy watcher PaletteChanged
    ctrl.execute_client_command(
        LOCAL_HOST_ID,
        ClientCommand::ChangeTheme {
            name: Some(omarchy_theme::OMARCHY_DEFAULT_LIGHT_NAME.into()),
        },
        now,
    );
    assert!(!ctrl.theme.is_dark);
    assert_eq!(
        ctrl.settings.theme_name.as_deref(),
        Some(omarchy_theme::OMARCHY_DEFAULT_LIGHT_NAME)
    );
    ctrl.process_runtime_event(
        crate::host_manager::RuntimeEvent::ThemeChanged(omarchy_theme::ThemeEvent::PaletteChanged(
            omarchy_theme::ThemePalette::default_dark(),
        )),
        now,
    );
    assert!(
        !ctrl.theme.is_dark,
        "watcher PaletteChanged must not clobber pinned theme"
    );

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn m4_f18_osc52_clipboard_written_dedup_and_guards() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();
    let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");

    // Disabled clipboard suppresses write
    ctrl.clipboard_enabled = false;
    assert!(!ctrl.apply_clipboard_with_dedup(&key, "hello", now));
    ctrl.handle_clipboard_written_block_event(&key, &serde_json::json!({"text": "hello"}), now);
    assert!(ctrl.last_clipboard_write.is_none());

    // Enabled clipboard writes once and deduplicates identical write within 250ms
    ctrl.clipboard_enabled = true;
    ctrl.handle_clipboard_written_block_event(&key, &serde_json::json!({"text": "hello"}), now);
    assert!(ctrl.last_clipboard_write.is_some());
    assert!(!ctrl.apply_clipboard_with_dedup(&key, "hello", now + Duration::from_millis(50)));
    assert!(ctrl.apply_clipboard_with_dedup(&key, "hello", now + Duration::from_millis(300)));

    // Chunked clipboard_written (`id` + `bytes`, no `text`) dispatches HostCommand::ReadClipboard
    let _ = ctrl.drain_pending_commands();
    ctrl.handle_clipboard_written_block_event(
        &key,
        &serde_json::json!({"id": 9, "bytes": 65536}),
        now,
    );
    assert!(ctrl.drain_pending_commands().into_iter().any(|c| matches!(
        c,
        HostCommand::ReadClipboard {
            clipboard_id: 9,
            expected_bytes: 65536,
            ..
        }
    )));

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn m4_f19_ports_changed_and_local_vs_remote_tunnel_lifecycle() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();
    let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");

    let ports_ev = rex_proto::types::SessionEvent::BlockEvent(
        rex_proto::types::BlockEventPayload {
            block_id: "block:1".into(),
            creator_name: None,
            name: "ports_changed".into(),
            payload: serde_json::json!({
                "status": "ok",
                "ports": [
                    {"port": 5353, "address": "0.0.0.0", "protocol": "udp", "first_seen_at": "2026-11-26T20:00:00Z"},
                    {"port": 8080, "address": "0.0.0.0", "protocol": "tcp", "first_seen_at": "2026-11-26T20:00:00Z", "pid": 11},
                    {"port": 3000, "address": "::", "protocol": "tcp", "first_seen_at": "2026-11-26T20:00:00Z", "pid": 10}
                ],
                "processes": [
                    {"pid": 10, "user_id": 1000, "name": "node", "script": "vite"}
                ]
            }),
        },
    );
    ctrl.workspace
        .apply_session_event(LOCAL_HOST_ID, "session:t3", &ports_ev);
    let blk = ctrl.workspace.block(&key).unwrap();
    assert_eq!(blk.listening_ports.len(), 2, "udp must be filtered out");
    assert_eq!(blk.listening_ports[0].port, 3000);
    assert_eq!(blk.listening_ports[1].port, 8080);
    assert_eq!(
        blk.port_process_label(&blk.listening_ports[0]).as_deref(),
        Some("vite")
    );

    let ui_in = ctrl.build_ui_frame_input(now);
    assert_eq!(ui_in.pane_metas[0].listening_ports.len(), 2);

    // Local port click does NOT spawn a tunnel
    ctrl.read_only = true;
    let _ = ctrl.drain_pending_commands();
    ctrl.dispatch_ui_action(
        UiAction::OpenPortAction {
            block_id: "block:1".into(),
            port: 3000,
        },
        now,
    );
    assert!(ctrl.active_tunnels.is_empty());

    // Remote host port click spawns StartPortTunnel with normalized 0.0.0.0 -> 127.0.0.1
    let mut remote_host = HostEntry::new(
        "remote-box",
        "Remote",
        RexEndpoint::parse("https://remote.ts.net").unwrap(),
    );
    remote_host.is_local = false;
    ctrl.workspace.add_host(remote_host);
    let remote_sk = SessionKey::new("remote-box", "s-rem");
    ctrl.workspace
        .host_mut("remote-box")
        .unwrap()
        .on_session_created(rex_proto::types::SessionSummary {
            session_id: "s-rem".into(),
            label: "Rem".into(),
        });
    ctrl.workspace
        .session_mut(&remote_sk)
        .unwrap()
        .apply_view(rex_proto::types::SessionView {
            revision: 1,
            session_id: Some("s-rem".into()),
            label: "Rem".into(),
            active_window_id: Some("w-rem".into()),
            windows: vec![rex_proto::types::WindowView {
                window_id: Some("w-rem".into()),
                label: "Rem".into(),
                focused_block_id: Some("b-rem".into()),
                zoomed_block_id: None,
                tiled_zoomed_block_id: None,
                layers: vec![rex_proto::types::LayerView {
                    layer_id: Some("l-rem".into()),
                    kind: "tiled".into(),
                    state: None,
                    bounds: [0.0, 0.0, 1.0, 1.0],
                    layout: None,
                    blocks: vec![rex_proto::types::BlockView {
                        block_id: Some("b-rem".into()),
                        label: "sh".into(),
                        creator_name: None,
                        flavor: None,
                        host_id: None,
                        owner: None,
                    }],
                }],
            }],
        });
    ctrl.workspace.select_session(remote_sk);
    ctrl.workspace
        .apply_session_event("remote-box", "s-rem", &rex_proto::types::SessionEvent::BlockEvent(
            rex_proto::types::BlockEventPayload {
                block_id: "b-rem".into(),
                creator_name: None,
                name: "ports_changed".into(),
                payload: serde_json::json!({
                    "status": "ok",
                    "ports": [{"port": 5173, "address": "0.0.0.0", "protocol": "tcp", "first_seen_at": "2026-11-26T20:00:00Z"}],
                    "processes": []
                }),
            },
        ));
    ctrl.read_only = false;
    let _ = ctrl.drain_pending_commands();
    ctrl.dispatch_ui_action(
        UiAction::OpenPortAction {
            block_id: "b-rem".into(),
            port: 5173,
        },
        now,
    );
    let started = ctrl.drain_pending_commands();
    assert!(started.iter().any(|c| matches!(
        c,
        HostCommand::StartPortTunnel {
            remote_host,
            remote_port: 5173,
            ..
        } if remote_host == "127.0.0.1"
    )));

    // BlockClosed tears down tunnel
    ctrl.process_runtime_event(
        crate::host_manager::RuntimeEvent::ControlEvent {
            host_id: "remote-box".into(),
            seq: 2,
            event: Box::new(rex_proto::types::ServerEvent::SessionEvent {
                session_id: "s-rem".into(),
                event: rex_proto::types::SessionEvent::BlockClosed {
                    block_id: "b-rem".into(),
                },
            }),
        },
        now,
    );
    assert!(ctrl.active_tunnels.is_empty());
    assert!(
        ctrl.drain_pending_commands()
            .iter()
            .any(|c| matches!(c, HostCommand::StopPortTunnel { .. }))
    );

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn m4_f20_f21_diagnostics_hud_debug_bundle_and_osc7501_chimes() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();
    let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");

    // 1. FrameStats ring buffer caps at 120
    for i in 0..160u64 {
        ctrl.record_frame_stats(&gpu_renderer::FrameStats {
            cpu_prepare_us: 100,
            gpu_encode_submit_us: 200,
            total_us: 1000 + i * 10,
            draw_calls: 5,
            quad_count: 20,
            glyph_count: 100,
            image_count: 0,
            adapter_name: "MockGPU".into(),
            backend: wgpu::Backend::Vulkan,
        });
    }
    assert_eq!(ctrl.frame_times_us.len(), 120);
    assert_eq!(ctrl.frame_times_us.front().copied(), Some(1400));
    assert_eq!(ctrl.frame_times_us.back().copied(), Some(2590));

    // 2. TailscaleStatusChanged & EventDropped telemetry + OpenDiagnostics modal
    ctrl.process_runtime_event(
        crate::host_manager::RuntimeEvent::ControlEvent {
            host_id: LOCAL_HOST_ID.into(),
            seq: 1,
            event: Box::new(rex_proto::types::ServerEvent::TailscaleStatusChanged(
                rex_proto::types::TailscaleStatus {
                    state: "Running".into(),
                    error: None,
                    dns_name: Some("dev.tailnet.ts.net".into()),
                    auth_url: None,
                    account: None,
                    tailnet_name: None,
                    allow_tailnet_members: true,
                    tags: vec![],
                    https: None,
                    control_url: None,
                },
            )),
        },
        now,
    );
    ctrl.process_runtime_event(
        crate::host_manager::RuntimeEvent::ControlEvent {
            host_id: LOCAL_HOST_ID.into(),
            seq: 2,
            event: Box::new(rex_proto::types::ServerEvent::EventDropped(
                rex_proto::types::EventDroppedEvent {
                    event_type: "block_output".into(),
                    session_id: Some("session:t3".into()),
                    block_id: Some("block:1".into()),
                    creator_name: None,
                    block_event_name: None,
                    size: 4096,
                },
            )),
        },
        now,
    );

    ctrl.execute_client_command(LOCAL_HOST_ID, ClientCommand::OpenDiagnostics, now);
    assert_eq!(
        ctrl.ui.focus.current(),
        &FocusTarget::Modal(ModalKind::Diagnostics)
    );
    let bundle = ctrl.format_debug_bundle();
    assert!(bundle.contains("MockGPU"));
    assert!(bundle.contains("Running dev.tailnet.ts.net"));
    assert!(bundle.contains("dropped=1"));

    // 3. OSC 7501 chime transition & cooldown deduplication + fake pw-play stdin/reaping test
    ctrl.maybe_trigger_osc7501_chime(&key, vt_engine::ProgramStatusState::Working, false, now);
    ctrl.maybe_trigger_osc7501_chime(
        &key,
        vt_engine::ProgramStatusState::Done,
        false,
        now + Duration::from_secs(1),
    );
    assert_eq!(
        ctrl.program_chime_state.get(&key).map(|(s, _)| *s),
        Some(vt_engine::ProgramStatusState::Done)
    );

    // Fake pw-play script verifying PCM bytes piped and child reaped cleanly
    use std::os::unix::fs::PermissionsExt;
    let tmp = std::env::temp_dir().join(format!("omarex-chime-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp);
    let capture = tmp.join("captured.pcm");
    let script = tmp.join("fake-pw-play.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ncat > \"{}\"\n", capture.display()),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let pcm = crate::audio_chime::synthesize_chime_pcm(crate::audio_chime::ChimeKind::Done);
    let ok =
        crate::audio_chime::play_pcm_with_bin(script.to_str().unwrap(), &["--raw"], "0.35", &pcm);
    assert_eq!(ok, Some(true));
    assert_eq!(std::fs::read(&capture).unwrap().len(), pcm.len());

    let _ = std::fs::remove_dir_all(&tmp);
    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn m4_r2_f20_debug_bundle_and_snapshot_redact_secrets() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();

    ctrl.workspace.host_mut(LOCAL_HOST_ID).unwrap().status = HostStatus::Error(
        "connect wss://user:TOP_SECRET_PASS@rex.host:443?api_key=SECRETKEY".into(),
    );
    ctrl.process_runtime_event(
        RuntimeEvent::ControlEvent {
            host_id: LOCAL_HOST_ID.into(),
            seq: 1,
            event: Box::new(ServerEvent::TailscaleStatusChanged(
                rex_proto::types::TailscaleStatus {
                    state: "Running".into(),
                    error: Some(
                        "auth failed https://admin:SUPER_SECRET_TOKEN@api.example.com?token=XYZ987&password=PW123"
                            .into(),
                    ),
                    dns_name: Some("dev.tailnet.ts.net".into()),
                    auth_url: None,
                    account: None,
                    tailnet_name: None,
                    allow_tailnet_members: true,
                    tags: vec![],
                    https: None,
                    control_url: None,
                },
            )),
        },
        now,
    );

    let mut remote = HostEntry::new(
        "remote-1",
        "Remote Box",
        RexEndpoint::parse("https://remote.example.com").unwrap(),
    );
    remote.status = HostStatus::Reconnecting {
        reason: "Authorization: Basic dXNlcjpwYXNz".into(),
    };
    ctrl.workspace.add_host(remote);

    ctrl.keymap_diagnostics
        .push(rex_proto::types::KeymapDiagnostic {
            severity: "warn".into(),
            source: None,
            message: "invalid binding: Bearer secret_bearer_token_12345".into(),
            action: None,
            key: None,
            mode: None,
        });

    let snap = ctrl.build_diagnostics_snapshot();
    let bundle = ctrl.format_debug_bundle();
    for secret in [
        "TOP_SECRET_PASS",
        "SECRETKEY",
        "SUPER_SECRET_TOKEN",
        "XYZ987",
        "PW123",
        "dXNlcjpwYXNz",
        "secret_bearer_token_12345",
    ] {
        assert!(
            !bundle.contains(secret),
            "bundle leaked secret `{secret}`: {bundle}"
        );
    }
    assert!(bundle.contains("This Machine"));
    assert!(bundle.contains("wss://[REDACTED]@rex.host:443?api_key=[REDACTED]"));
    assert!(
        bundle.contains("https://[REDACTED]@api.example.com?token=[REDACTED]&password=[REDACTED]")
    );
    assert!(bundle.contains("Authorization: Basic [REDACTED]"));
    assert!(bundle.contains("Bearer [REDACTED]"));
    assert!(snap.keymap_warnings[0].contains("Bearer [REDACTED]"));

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn m4_r2_f16_offline_file_picker_surfaces_errors_and_preserves_prior_dir() {
    use std::os::unix::fs::PermissionsExt;

    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();
    let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");

    // Disconnect local host so FilePicker uses offline list_dir_local_fs fallback.
    ctrl.workspace.host_mut(LOCAL_HOST_ID).unwrap().status = HostStatus::Disconnected;

    let base = ctrl.settings_path.parent().unwrap().join("picker-probe");
    let sub_dir = base.join("sub_dir");
    let denied_dir = base.join("denied_dir");
    let good_file = base.join("good_file.txt");
    let missing_dir = base.join("missing_dir");
    std::fs::create_dir_all(&sub_dir).unwrap();
    std::fs::create_dir_all(&denied_dir).unwrap();
    std::fs::write(&good_file, b"hello").unwrap();
    std::fs::write(sub_dir.join("child.txt"), b"child").unwrap();
    std::fs::set_permissions(&denied_dir, std::fs::Permissions::from_mode(0o000)).unwrap();

    let base_str = base.to_string_lossy().into_owned();
    ctrl.workspace.block_mut(&key).unwrap().pwd = Some(base_str.clone());

    ctrl.execute_client_command(LOCAL_HOST_ID, ClientCommand::InsertPath, now);
    assert!(matches!(
        ctrl.ui.modal.as_ref().map(|m| &m.content),
        Some(ui_chrome::ModalContent::FilePicker {
            current_dir,
            entries,
            error: None,
            ..
        }) if current_dir == &base_str && !entries.is_empty()
    ));

    // Descending into chmod 000 directory preserves current_dir + entries and surfaces error.
    ctrl.dispatch_ui_action(
        UiAction::FilePickerDescend {
            path: denied_dir.to_string_lossy().into_owned(),
        },
        now,
    );
    assert!(matches!(
        ctrl.ui.modal.as_ref().map(|m| &m.content),
        Some(ui_chrome::ModalContent::FilePicker {
            current_dir,
            entries,
            error: Some(err),
            ..
        }) if current_dir == &base_str && !entries.is_empty() && err == "Permission denied"
    ));

    // Descending into missing directory preserves current_dir + entries and surfaces error.
    ctrl.dispatch_ui_action(
        UiAction::FilePickerDescend {
            path: missing_dir.to_string_lossy().into_owned(),
        },
        now,
    );
    assert!(matches!(
        ctrl.ui.modal.as_ref().map(|m| &m.content),
        Some(ui_chrome::ModalContent::FilePicker {
            current_dir,
            entries,
            error: Some(err),
            ..
        }) if current_dir == &base_str && !entries.is_empty() && err == "Directory does not exist"
    ));

    // Descending into regular file surfaces "Not a directory" and preserves prior state.
    ctrl.dispatch_ui_action(
        UiAction::FilePickerDescend {
            path: good_file.to_string_lossy().into_owned(),
        },
        now,
    );
    assert!(matches!(
        ctrl.ui.modal.as_ref().map(|m| &m.content),
        Some(ui_chrome::ModalContent::FilePicker {
            current_dir,
            entries,
            error: Some(err),
            ..
        }) if current_dir == &base_str && !entries.is_empty() && err == "Not a directory"
    ));

    // Valid descend clears error and updates current_dir.
    ctrl.dispatch_ui_action(
        UiAction::FilePickerDescend {
            path: sub_dir.to_string_lossy().into_owned(),
        },
        now,
    );
    assert!(matches!(
        ctrl.ui.modal.as_ref().map(|m| &m.content),
        Some(ui_chrome::ModalContent::FilePicker {
            current_dir,
            entries,
            error: None,
            ..
        }) if current_dir == &sub_dir.to_string_lossy() && entries.len() == 1
    ));

    // Opening directly on a missing pwd surfaces error immediately.
    ctrl.ui.close_modal();
    ctrl.workspace.block_mut(&key).unwrap().pwd = Some(missing_dir.to_string_lossy().into_owned());
    ctrl.execute_client_command(LOCAL_HOST_ID, ClientCommand::GoToDirectory, now);
    assert!(matches!(
        ctrl.ui.modal.as_ref().map(|m| &m.content),
        Some(ui_chrome::ModalContent::FilePicker {
            error: Some(err),
            ..
        }) if err == "Directory does not exist"
    ));

    let _ = std::fs::set_permissions(&denied_dir, std::fs::Permissions::from_mode(0o755));
    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn m4_r2_f15_copy_mode_clamps_cursor_and_visual_anchor_on_shrink() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();
    let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");
    ctrl.pane_store
        .create_local_test_pane(&key, 80, 24)
        .unwrap();
    let long_line: String = (0..78).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
    ctrl.pane_store
        .feed_local_test_bytes(&key, format!("{long_line}\r\n").as_bytes())
        .unwrap();

    ctrl.execute_client_command(LOCAL_HOST_ID, ClientCommand::EnterCopyMode, now);
    let press = |cname: &str, text: Option<&str>, shift: bool| KeyInputEvent {
        physical_key: PhysicalKey::Unidentified,
        canonical_key: cname.to_owned(),
        text: text.map(str::to_owned),
        unshifted_codepoint: 0,
        mods: Modifiers {
            shift,
            ctrl: false,
            alt: false,
            super_key: false,
        },
        pressed: true,
        repeat: false,
        composing: false,
    };

    ctrl.handle_key(&press("k", Some("k"), false), now);
    ctrl.handle_key(&press("4", Some("$"), true), now);
    assert!(ctrl.copy_mode.as_ref().unwrap().cursor.col >= 70);

    ctrl.handle_key(&press("v", Some("v"), false), now);
    assert!(
        ctrl.copy_mode
            .as_ref()
            .unwrap()
            .visual
            .is_some_and(|(_, anchor)| anchor.col >= 70)
    );

    // Shrink terminal to 15x5; cursor_override must clamp both cursor and visual anchor immediately.
    ctrl.pane_store.resize_local_test_pane(&key, 15, 5).unwrap();
    let cur = ctrl
        .copy_mode_cursor_override(&key, gpu_renderer::Color::WHITE, gpu_renderer::Color::BLACK)
        .expect("cursor override");
    assert!(
        cur.col < 15,
        "rendered cursor col must be < 15, got {}",
        cur.col
    );
    let cm = ctrl.copy_mode.as_ref().unwrap();
    assert!(
        cm.cursor.col < 15,
        "cursor col must clamp < 15, got {}",
        cm.cursor.col
    );
    let (_, anchor) = cm.visual.expect("visual mode active");
    assert!(
        anchor.col < 15,
        "visual anchor col must clamp < 15, got {}",
        anchor.col
    );

    // Subsequent key press and yank use clamped bounds cleanly.
    ctrl.handle_key(&press("h", Some("h"), false), now);
    let cm_after = ctrl.copy_mode.as_ref().unwrap();
    assert!(cm_after.cursor.col < 15);
    assert!(cm_after.visual.unwrap().1.col < 15);
    ctrl.handle_key(&press("y", Some("y"), false), now);
    assert!(ctrl.copy_mode.is_none());

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn m4_r2_f21_toggle_tab_mute_toggles_all_blocks_in_split_tab() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();
    let sk = SessionKey::new(LOCAL_HOST_ID, "session:t3");

    // Replace active window with a 2-pane split tab (block:1 + block:2).
    let split_view = SessionView {
        revision: 2,
        session_id: Some("session:t3".into()),
        label: "T3 Code".into(),
        active_window_id: Some("window:1".into()),
        windows: vec![WindowView {
            window_id: Some("window:1".into()),
            label: "Split Tab".into(),
            focused_block_id: Some("block:1".into()),
            zoomed_block_id: None,
            tiled_zoomed_block_id: None,
            layers: vec![LayerView {
                layer_id: Some("layer:tiled".into()),
                kind: "tiled".into(),
                bounds: [0.0, 0.0, 1.0, 1.0],
                state: None,
                layout: None,
                blocks: vec![
                    BlockView {
                        block_id: Some("block:1".into()),
                        label: "sh-1".into(),
                        creator_name: Some("com.superlogical.terminal".into()),
                        flavor: Some("com.superlogical.terminal.shell".into()),
                        host_id: Some("host:local".into()),
                        owner: Some("client:1".into()),
                    },
                    BlockView {
                        block_id: Some("block:2".into()),
                        label: "sh-2".into(),
                        creator_name: Some("com.superlogical.terminal".into()),
                        flavor: Some("com.superlogical.terminal.shell".into()),
                        host_id: Some("host:local".into()),
                        owner: Some("client:1".into()),
                    },
                ],
            }],
        }],
    };
    ctrl.process_runtime_event(
        RuntimeEvent::SessionViewUpdated {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:t3".into(),
            view: Box::new(split_view),
            seq: 2,
        },
        now,
    );
    assert!(
        !ctrl
            .settings
            .is_muted(LOCAL_HOST_ID, "session:t3", "block:1")
    );
    assert!(
        !ctrl
            .settings
            .is_muted(LOCAL_HOST_ID, "session:t3", "block:2")
    );

    // ToggleTabMute mutes ALL blocks in the split tab.
    ctrl.execute_client_command(LOCAL_HOST_ID, ClientCommand::ToggleTabMute, now);
    assert!(
        ctrl.settings
            .is_muted(LOCAL_HOST_ID, "session:t3", "block:1")
    );
    assert!(
        ctrl.settings
            .is_muted(LOCAL_HOST_ID, "session:t3", "block:2")
    );

    // ToggleTabMute again unmutes ALL blocks in the split tab.
    ctrl.execute_client_command(LOCAL_HOST_ID, ClientCommand::ToggleTabMute, now);
    assert!(
        !ctrl
            .settings
            .is_muted(LOCAL_HOST_ID, "session:t3", "block:1")
    );
    assert!(
        !ctrl
            .settings
            .is_muted(LOCAL_HOST_ID, "session:t3", "block:2")
    );

    // Partial mute -> ToggleTabMute normalizes to all muted.
    ctrl.settings
        .set_block_muted(LOCAL_HOST_ID, "block:1", true);
    ctrl.execute_client_command(LOCAL_HOST_ID, ClientCommand::ToggleTabMute, now);
    assert!(
        ctrl.settings
            .is_muted(LOCAL_HOST_ID, "session:t3", "block:1")
    );
    assert!(
        ctrl.settings
            .is_muted(LOCAL_HOST_ID, "session:t3", "block:2")
    );
    let _ = sk;

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn m4_r2_f19_local_ipv6_zone_url_escaping_and_cleanup_removed_session() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();
    let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");

    // 1. tcp6 + fe80::1%1 produces RFC 6874 %25-escaped URL on local open
    let ports_ev = SessionEvent::BlockEvent(rex_proto::types::BlockEventPayload {
        block_id: "block:1".into(),
        creator_name: None,
        name: "ports_changed".into(),
        payload: serde_json::json!({
            "status": "ok",
            "ports": [{"port": 3000, "address": "fe80::1%1", "protocol": "tcp6", "first_seen_at": "2026-11-26T20:00:00Z"}],
            "processes": []
        }),
    });
    ctrl.workspace
        .apply_session_event(LOCAL_HOST_ID, "session:t3", &ports_ev);
    ctrl.read_only = true;
    ctrl.dispatch_ui_action(
        UiAction::OpenPortAction {
            block_id: "block:1".into(),
            port: 3000,
        },
        now,
    );
    assert!(
        ctrl.ui
            .toasts
            .iter()
            .any(|t| t.body == "http://[fe80::1%251]:3000"),
        "expected RFC 6874 escaped URL in toast, got: {:?}",
        ctrl.ui.toasts
    );

    // 2. cleanup_removed_session prunes active_tunnels, program_chime_state, ports_queried_blocks, copy_mode
    ctrl.active_tunnels.push(ActiveTunnel {
        tunnel_id: 99,
        source_block: key.clone(),
        tunnel_session_id: key.session_id.clone(),
        tunnel_block_id: Some("tun:1".into()),
        remote_host: "127.0.0.1".into(),
        remote_port: 3000,
        local_port: Some(13000),
        error: None,
    });
    ctrl.program_chime_state
        .insert(key.clone(), (vt_engine::ProgramStatusState::Done, now));
    ctrl.ports_queried_blocks.insert(key.clone());
    ctrl.copy_mode = Some(CopyModeState {
        block_key: key.clone(),
        cursor: vt_engine::ScreenPoint { col: 0, row: 0 },
        visual: None,
        search_active: false,
        search_query: String::new(),
        pending_g: false,
    });
    let _ = ctrl.drain_pending_commands();

    ctrl.process_runtime_event(
        RuntimeEvent::ControlEvent {
            host_id: LOCAL_HOST_ID.into(),
            seq: 10,
            event: Box::new(ServerEvent::SessionDestroyed {
                session_id: "session:t3".into(),
            }),
        },
        now,
    );
    assert!(ctrl.active_tunnels.is_empty());
    assert!(!ctrl.program_chime_state.contains_key(&key));
    assert!(!ctrl.ports_queried_blocks.contains(&key));
    assert!(ctrl.copy_mode.is_none());
    assert!(
        ctrl.drain_pending_commands()
            .iter()
            .any(|c| matches!(c, HostCommand::StopPortTunnel { tunnel_id: 99, .. }))
    );

    // 3. resolve_startup_theme applies settings.theme_name and settings.material
    let settings = UserSettings {
        theme_name: Some(omarchy_theme::OMARCHY_DEFAULT_LIGHT_NAME.into()),
        material: crate::config::WindowMaterial::Translucent { alpha: 0.85 },
        ..UserSettings::default()
    };
    let resolved = resolve_startup_theme(&settings, false);
    assert!(!resolved.is_dark);
    assert_eq!(
        resolved.material,
        omarchy_theme::MaterialStyle::Translucent { alpha: 0.85 }
    );
    let forced_light = resolve_startup_theme(&settings, true);
    assert!(!forced_light.is_dark);
    assert_eq!(
        forced_light.material,
        omarchy_theme::MaterialStyle::Translucent { alpha: 0.85 }
    );

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn m4_r3_f20_redacts_url_fragments_and_raw_authorization_headers() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();

    ctrl.workspace.host_mut(LOCAL_HOST_ID).unwrap().status = HostStatus::Error(
        "Authorization: RAW_HEADER_SECRET_999 failed; Proxy-Authorization: RAW_PROXY_TOK timed out"
            .into(),
    );
    ctrl.process_runtime_event(
        RuntimeEvent::ControlEvent {
            host_id: LOCAL_HOST_ID.into(),
            seq: 1,
            event: Box::new(ServerEvent::TailscaleStatusChanged(
                rex_proto::types::TailscaleStatus {
                    state: "Running".into(),
                    error: Some(
                        "redirect https://cb.example.com/auth#access_token=FRAG_ACCESS_SECRET&token=FRAG_TOK_SECRET; next https://cb.example.com/cb#api_key=FRAG_API_KEY_SECRET"
                            .into(),
                    ),
                    dns_name: Some("dev.tailnet.ts.net".into()),
                    auth_url: None,
                    account: None,
                    tailnet_name: None,
                    allow_tailnet_members: true,
                    tags: vec![],
                    https: None,
                    control_url: None,
                },
            )),
        },
        now,
    );

    let mut remote = HostEntry::new(
        "remote-1",
        "Remote Box",
        RexEndpoint::parse("https://remote.example.com").unwrap(),
    );
    remote.status = HostStatus::Reconnecting {
        reason: "Authorization: Bearer KNOWN_BEARER_SECRET; Authorization: DPoP DPOP_TOK_SECRET ok"
            .into(),
    };
    ctrl.workspace.add_host(remote);

    let snap = ctrl.build_diagnostics_snapshot();
    let bundle = ctrl.format_debug_bundle();
    for secret in [
        "FRAG_ACCESS_SECRET",
        "FRAG_TOK_SECRET",
        "FRAG_API_KEY_SECRET",
        "RAW_HEADER_SECRET_999",
        "RAW_PROXY_TOK",
        "KNOWN_BEARER_SECRET",
        "DPOP_TOK_SECRET",
    ] {
        assert!(
            !bundle.contains(secret),
            "debug bundle leaked secret `{secret}`: {bundle}"
        );
        assert!(
            !snap
                .hosts
                .iter()
                .any(|h| h.status.contains(secret) || h.tailscale_summary.contains(secret)),
            "diagnostics snapshot leaked secret `{secret}`: {:?}",
            snap.hosts
        );
    }

    assert!(
        bundle.contains("https://cb.example.com/auth#access_token=[REDACTED]&token=[REDACTED]")
    );
    assert!(bundle.contains("https://cb.example.com/cb#api_key=[REDACTED]"));
    assert!(bundle.contains("Authorization: [REDACTED] failed"));
    assert!(bundle.contains("Proxy-Authorization: [REDACTED] timed out"));
    assert!(bundle.contains("Authorization: Bearer [REDACTED]"));
    assert!(bundle.contains("Authorization: DPoP [REDACTED] ok"));

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn m4_r3_f21_toggle_tab_mute_unmutes_active_tab_when_session_muted_preserving_siblings() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();

    let view = SessionView {
        revision: 3,
        session_id: Some("session:t3".into()),
        label: "T3 Code".into(),
        active_window_id: Some("window:1".into()),
        windows: vec![
            WindowView {
                window_id: Some("window:1".into()),
                label: "Active Tab".into(),
                focused_block_id: Some("block:1".into()),
                zoomed_block_id: None,
                tiled_zoomed_block_id: None,
                layers: vec![LayerView {
                    layer_id: Some("layer:tiled-1".into()),
                    kind: "tiled".into(),
                    bounds: [0.0, 0.0, 1.0, 1.0],
                    state: None,
                    layout: None,
                    blocks: vec![BlockView {
                        block_id: Some("block:1".into()),
                        label: "sh-1".into(),
                        creator_name: Some("com.superlogical.terminal".into()),
                        flavor: Some("com.superlogical.terminal.shell".into()),
                        host_id: Some("host:local".into()),
                        owner: Some("client:1".into()),
                    }],
                }],
            },
            WindowView {
                window_id: Some("window:2".into()),
                label: "Sibling Tab".into(),
                focused_block_id: Some("block:2".into()),
                zoomed_block_id: None,
                tiled_zoomed_block_id: None,
                layers: vec![LayerView {
                    layer_id: Some("layer:tiled-2".into()),
                    kind: "tiled".into(),
                    bounds: [0.0, 0.0, 1.0, 1.0],
                    state: None,
                    layout: None,
                    blocks: vec![
                        BlockView {
                            block_id: Some("block:2".into()),
                            label: "sh-2".into(),
                            creator_name: Some("com.superlogical.terminal".into()),
                            flavor: Some("com.superlogical.terminal.shell".into()),
                            host_id: Some("host:local".into()),
                            owner: Some("client:1".into()),
                        },
                        BlockView {
                            block_id: Some("block:3".into()),
                            label: "sh-3".into(),
                            creator_name: Some("com.superlogical.terminal".into()),
                            flavor: Some("com.superlogical.terminal.shell".into()),
                            host_id: Some("host:local".into()),
                            owner: Some("client:1".into()),
                        },
                    ],
                }],
            },
        ],
    };
    ctrl.process_runtime_event(
        RuntimeEvent::SessionViewUpdated {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:t3".into(),
            view: Box::new(view),
            seq: 3,
        },
        now,
    );

    // Mute the entire session first.
    ctrl.dispatch_ui_action(
        UiAction::ToggleSessionMute {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:t3".into(),
        },
        now,
    );
    assert!(ctrl.settings.is_session_muted(LOCAL_HOST_ID, "session:t3"));
    assert!(
        ctrl.settings
            .is_muted(LOCAL_HOST_ID, "session:t3", "block:1")
    );
    assert!(
        ctrl.settings
            .is_muted(LOCAL_HOST_ID, "session:t3", "block:2")
    );
    assert!(
        ctrl.settings
            .is_muted(LOCAL_HOST_ID, "session:t3", "block:3")
    );

    // ToggleTabMute on active window:1 unmutes ONLY window:1 while keeping sibling window:2 muted.
    ctrl.execute_client_command(LOCAL_HOST_ID, ClientCommand::ToggleTabMute, now);
    assert!(!ctrl.settings.is_session_muted(LOCAL_HOST_ID, "session:t3"));
    assert!(
        !ctrl
            .settings
            .is_muted(LOCAL_HOST_ID, "session:t3", "block:1")
    );
    assert!(
        ctrl.settings
            .is_muted(LOCAL_HOST_ID, "session:t3", "block:2")
    );
    assert!(
        ctrl.settings
            .is_muted(LOCAL_HOST_ID, "session:t3", "block:3")
    );

    let frame_input = ctrl.build_ui_frame_input(now);
    let sess = &frame_input.hosts[0].sessions[0];
    assert!(!sess.muted);
    assert!(!sess.tabs[0].muted);
    assert!(sess.tabs[1].muted);

    // Toggling active tab again re-mutes window:1 cleanly.
    ctrl.execute_client_command(LOCAL_HOST_ID, ClientCommand::ToggleTabMute, now);
    assert!(
        ctrl.settings
            .is_muted(LOCAL_HOST_ID, "session:t3", "block:1")
    );
    assert!(
        ctrl.settings
            .is_muted(LOCAL_HOST_ID, "session:t3", "block:2")
    );
    assert!(
        ctrl.settings
            .is_muted(LOCAL_HOST_ID, "session:t3", "block:3")
    );

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn m5_f22_pane_dnd_5_zones_and_out_of_bounds_cancel() {
    let mut ctrl = make_test_controller();
    let mut host = HostEntry::new(
        LOCAL_HOST_ID,
        "This Machine",
        RexEndpoint::parse("unix:///tmp/test.sock").unwrap(),
    );
    host.status = HostStatus::Connected {
        client_id: "client:1".into(),
        server_host_id: Some("host:local".into()),
        api_revision: 24,
        resumed: false,
    };
    host.on_session_created(SessionSummary {
        session_id: "session:work".into(),
        label: "Work".into(),
    });
    ctrl.workspace.add_host(host);
    ctrl.workspace
        .select_session(SessionKey::new(LOCAL_HOST_ID, "session:work"));

    let view = SessionView {
        revision: 1,
        session_id: Some("session:work".into()),
        label: "Work".into(),
        active_window_id: Some("window:1".into()),
        windows: vec![WindowView {
            window_id: Some("window:1".into()),
            label: "Split".into(),
            focused_block_id: Some("block:1".into()),
            zoomed_block_id: None,
            tiled_zoomed_block_id: None,
            layers: vec![LayerView {
                layer_id: Some("layer:tiled".into()),
                kind: "tiled".into(),
                bounds: [0.0, 0.0, 1.0, 1.0],
                state: None,
                layout: Some(LayoutNode {
                    rect: [0.0, 0.0, 1.0, 1.0],
                    block_id: None,
                    split: Some(Box::new(rex_proto::types::SplitNode {
                        split_id: Some("split:1".into()),
                        direction: "horizontal".into(),
                        ratio: 0.5,
                        before: LayoutNode {
                            rect: [0.0, 0.0, 0.5, 1.0],
                            block_id: Some("block:1".into()),
                            split: None,
                        },
                        after: LayoutNode {
                            rect: [0.5, 0.0, 0.5, 1.0],
                            block_id: Some("block:2".into()),
                            split: None,
                        },
                    })),
                }),
                blocks: vec![
                    BlockView {
                        block_id: Some("block:1".into()),
                        label: "left".into(),
                        creator_name: None,
                        flavor: Some("com.superlogical.terminal.shell".into()),
                        host_id: None,
                        owner: Some("client:1".into()),
                    },
                    BlockView {
                        block_id: Some("block:2".into()),
                        label: "right".into(),
                        creator_name: None,
                        flavor: Some("com.superlogical.terminal.shell".into()),
                        host_id: None,
                        owner: Some("client:1".into()),
                    },
                ],
            }],
        }],
    };
    let now = Instant::now();
    ctrl.process_runtime_event(
        RuntimeEvent::SessionViewUpdated {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:work".into(),
            view: Box::new(view),
            seq: 1,
        },
        now,
    );
    ctrl.set_viewport(1280, 720, 1.0);
    ctrl.ui.layout_cfg.pane_header_mode = ui_chrome::PaneHeaderMode::Always;
    ctrl.ui.snap_animations();

    let frame_input = ctrl.build_ui_frame_input(now);
    let ui_out = ctrl.ui.render(&frame_input, &ctrl.theme, now, 0.0);
    let b1_hdr = ui_out
        .draw_list
        .hits
        .iter()
        .find(|h| matches!(&h.target, ui_chrome::HitTarget::PaneHeader { block_id } if block_id == "block:1"))
        .expect("block:1 header hit")
        .rect;
    let b2_card = ui_out
        .tiled_panes
        .iter()
        .find(|p| p.id == "block:2")
        .expect("block:2 geometry")
        .card_rect;
    let b1_card = ui_out
        .tiled_panes
        .iter()
        .find(|p| p.id == "block:1")
        .expect("block:1 geometry")
        .card_rect;
    ctrl.last_ui_output = Some(ui_out);
    ctrl.drain_pending_commands();

    let start_x = b1_hdr.x + 20.0;
    let start_y = b1_hdr.y + b1_hdr.h * 0.5;
    let zones = [
        (
            ui_chrome::DropZone::CenterSwap,
            b2_card.x + b2_card.w * 0.5,
            b2_card.y + b2_card.h * 0.5,
        ),
        (
            ui_chrome::DropZone::Left,
            b2_card.x + b2_card.w * 0.1,
            b2_card.y + b2_card.h * 0.5,
        ),
        (
            ui_chrome::DropZone::Right,
            b2_card.right() - b2_card.w * 0.1,
            b2_card.y + b2_card.h * 0.5,
        ),
        (
            ui_chrome::DropZone::Top,
            b2_card.x + b2_card.w * 0.5,
            b2_card.y + b2_card.h * 0.1,
        ),
        (
            ui_chrome::DropZone::Bottom,
            b2_card.x + b2_card.w * 0.5,
            b2_card.bottom() - b2_card.h * 0.1,
        ),
    ];

    for (expected_zone, dx, dy) in zones {
        ctrl.handle_pointer_down(
            start_x,
            start_y,
            PointerButton::Left,
            Modifiers::default(),
            0,
            now,
        );
        ctrl.handle_pointer_move(dx, dy, Modifiers::default(), now);
        let overlay = ctrl
            .build_ui_frame_input(now)
            .dnd_overlay
            .expect("dnd_overlay must be populated over target zone");
        assert_eq!(overlay.zone, expected_zone);
        assert_eq!(ctrl.cursor_hint(), CursorHint::Grabbing);
        ctrl.drain_pending_commands();

        ctrl.handle_pointer_up(dx, dy, PointerButton::Left, Modifiers::default());
        let cmds = ctrl.drain_pending_commands();
        match expected_zone {
            ui_chrome::DropZone::CenterSwap => {
                assert!(cmds.iter().any(|c| matches!(
                    c,
                    HostCommand::SwapBlocks { request, .. }
                    if request.block_id == "block:1" && request.other_block_id == "block:2"
                )));
            }
            ui_chrome::DropZone::Left => {
                assert!(cmds.iter().any(|c| matches!(
                    c,
                    HostCommand::MoveBlock { block_id, anchor_block_id, direction, side, .. }
                    if block_id == "block:1" && anchor_block_id == "block:2" && direction == "horizontal" && side == "before"
                )));
            }
            ui_chrome::DropZone::Right => {
                assert!(cmds.iter().any(|c| matches!(
                    c,
                    HostCommand::MoveBlock { block_id, anchor_block_id, direction, side, .. }
                    if block_id == "block:1" && anchor_block_id == "block:2" && direction == "horizontal" && side == "after"
                )));
            }
            ui_chrome::DropZone::Top => {
                assert!(cmds.iter().any(|c| matches!(
                    c,
                    HostCommand::MoveBlock { block_id, anchor_block_id, direction, side, .. }
                    if block_id == "block:1" && anchor_block_id == "block:2" && direction == "vertical" && side == "before"
                )));
            }
            ui_chrome::DropZone::Bottom => {
                assert!(cmds.iter().any(|c| matches!(
                    c,
                    HostCommand::MoveBlock { block_id, anchor_block_id, direction, side, .. }
                    if block_id == "block:1" && anchor_block_id == "block:2" && direction == "vertical" && side == "after"
                )));
            }
        }
    }

    // Releasing out of bounds, over sidebar, or over self must cancel cleanly with zero move/swap.
    for (bad_x, bad_y) in [
        (-20.0, -20.0),
        (50.0, 200.0),
        (b1_card.x + b1_card.w * 0.5, b1_card.y + b1_card.h * 0.5),
    ] {
        ctrl.handle_pointer_down(
            start_x,
            start_y,
            PointerButton::Left,
            Modifiers::default(),
            0,
            now,
        );
        ctrl.handle_pointer_move(bad_x, bad_y, Modifiers::default(), now);
        assert!(ctrl.build_ui_frame_input(now).dnd_overlay.is_none());
        ctrl.drain_pending_commands();
        ctrl.handle_pointer_up(bad_x, bad_y, PointerButton::Left, Modifiers::default());
        let cmds = ctrl.drain_pending_commands();
        assert!(
            !cmds.iter().any(|c| matches!(
                c,
                HostCommand::MoveBlock { .. } | HostCommand::SwapBlocks { .. }
            )),
            "expected zero move/swap on invalid drop ({bad_x}, {bad_y})"
        );
    }

    // Pressing Escape during PaneMove cancels drag and clears overlay.
    ctrl.handle_pointer_down(
        start_x,
        start_y,
        PointerButton::Left,
        Modifiers::default(),
        0,
        now,
    );
    ctrl.handle_pointer_move(
        b2_card.x + b2_card.w * 0.5,
        b2_card.y + b2_card.h * 0.5,
        Modifiers::default(),
        now,
    );
    assert!(ctrl.build_ui_frame_input(now).dnd_overlay.is_some());
    ctrl.handle_key(
        &KeyInputEvent {
            physical_key: PhysicalKey::Escape,
            canonical_key: "escape".to_owned(),
            text: None,
            unshifted_codepoint: 0x1b,
            mods: Modifiers::default(),
            pressed: true,
            repeat: false,
            composing: false,
        },
        now,
    );
    assert!(ctrl.build_ui_frame_input(now).dnd_overlay.is_none());
}

#[test]
fn m5_f22_tab_drag_reorder_tabbar_and_sidebar() {
    let mut ctrl = make_test_controller();
    let mut host = HostEntry::new(
        LOCAL_HOST_ID,
        "This Machine",
        RexEndpoint::parse("unix:///tmp/test.sock").unwrap(),
    );
    host.status = HostStatus::Connected {
        client_id: "client:1".into(),
        server_host_id: Some("host:local".into()),
        api_revision: 24,
        resumed: false,
    };
    host.on_session_created(SessionSummary {
        session_id: "session:work".into(),
        label: "Work".into(),
    });
    ctrl.workspace.add_host(host);
    let sk = SessionKey::new(LOCAL_HOST_ID, "session:work");
    ctrl.workspace.select_session(sk.clone());

    let view = SessionView {
        revision: 1,
        session_id: Some("session:work".into()),
        label: "Work".into(),
        active_window_id: Some("window:1".into()),
        windows: vec![
            WindowView {
                window_id: Some("window:1".into()),
                label: "Tab 1".into(),
                focused_block_id: Some("block:1".into()),
                zoomed_block_id: None,
                tiled_zoomed_block_id: None,
                layers: Vec::new(),
            },
            WindowView {
                window_id: Some("window:2".into()),
                label: "Tab 2".into(),
                focused_block_id: Some("block:2".into()),
                zoomed_block_id: None,
                tiled_zoomed_block_id: None,
                layers: Vec::new(),
            },
            WindowView {
                window_id: Some("window:3".into()),
                label: "Tab 3".into(),
                focused_block_id: Some("block:3".into()),
                zoomed_block_id: None,
                tiled_zoomed_block_id: None,
                layers: Vec::new(),
            },
        ],
    };
    let now = Instant::now();
    ctrl.process_runtime_event(
        RuntimeEvent::SessionViewUpdated {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:work".into(),
            view: Box::new(view),
            seq: 1,
        },
        now,
    );
    ctrl.set_viewport(1280, 720, 1.0);
    ctrl.ui.focus.focus_terminal(Some("block:1".to_owned()));

    // 1. Horizontal Top TabBar drag reorder: drag window:1 past window:2 -> moves before window:3.
    ctrl.ui.layout_cfg.toolbar_position = ToolbarPosition::Top;
    ctrl.ui.snap_animations();
    let fi = ctrl.build_ui_frame_input(now);
    let out = ctrl.ui.render(&fi, &ctrl.theme, now, 0.0);
    let r1 = out
        .draw_list
        .hits
        .iter()
        .find(|h| matches!(&h.target, ui_chrome::HitTarget::TabCard { window_id, .. } if window_id == "window:1"))
        .expect("tab 1 hit")
        .rect;
    let r2 = out
        .draw_list
        .hits
        .iter()
        .find(|h| matches!(&h.target, ui_chrome::HitTarget::TabCard { window_id, .. } if window_id == "window:2"))
        .expect("tab 2 hit")
        .rect;
    ctrl.last_ui_output = Some(out);
    ctrl.drain_pending_commands();

    ctrl.handle_pointer_down(
        r1.x + 20.0,
        r1.y + r1.h * 0.5,
        PointerButton::Left,
        Modifiers::default(),
        0,
        now,
    );
    ctrl.handle_pointer_move(
        r2.right() - 10.0,
        r2.y + r2.h * 0.5,
        Modifiers::default(),
        now,
    );
    let ov = ctrl
        .ui
        .tab_reorder_overlay
        .as_ref()
        .expect("tab_reorder_overlay active");
    assert_eq!(ov.dragged_window_id, "window:1");
    assert!(ov.indicator_rect.is_some());

    ctrl.handle_pointer_up(
        r2.right() - 10.0,
        r2.y + r2.h * 0.5,
        PointerButton::Left,
        Modifiers::default(),
    );
    assert!(ctrl.ui.tab_reorder_overlay.is_none());
    let cmds = ctrl.drain_pending_commands();
    assert!(cmds.iter().any(|c| matches!(
        c,
        HostCommand::MoveWindow { window_id, before_window_id, .. }
        if window_id == "window:1" && before_window_id.as_deref() == Some("window:3")
    )));
    let order: Vec<&str> = ctrl
        .workspace
        .session(&sk)
        .unwrap()
        .view
        .as_ref()
        .unwrap()
        .windows
        .iter()
        .filter_map(|w| w.window_id.as_deref())
        .collect();
    assert_eq!(order, vec!["window:2", "window:1", "window:3"]);
    assert_eq!(
        ctrl.workspace
            .session(&sk)
            .unwrap()
            .view
            .as_ref()
            .unwrap()
            .active_window_id
            .as_deref(),
        Some("window:1")
    );
    assert_eq!(ctrl.ui.focus.last_terminal_block(), Some("block:1"));

    // 2. Vertical Sidebar drag reorder + out-of-bounds cancel check.
    ctrl.ui.layout_cfg.toolbar_position = ToolbarPosition::Left;
    ctrl.ui.snap_animations();
    let fi2 = ctrl.build_ui_frame_input(now);
    let out2 = ctrl.ui.render(&fi2, &ctrl.theme, now, 0.0);
    let sr3 = out2
        .draw_list
        .hits
        .iter()
        .find(|h| matches!(&h.target, ui_chrome::HitTarget::TabCard { window_id, .. } if window_id == "window:3"))
        .expect("sidebar tab 3 hit")
        .rect;
    let sr2 = out2
        .draw_list
        .hits
        .iter()
        .find(|h| matches!(&h.target, ui_chrome::HitTarget::TabCard { window_id, .. } if window_id == "window:2"))
        .expect("sidebar tab 2 hit")
        .rect;
    ctrl.last_ui_output = Some(out2);
    ctrl.drain_pending_commands();

    ctrl.handle_pointer_down(
        sr3.x + 20.0,
        sr3.y + sr3.h * 0.5,
        PointerButton::Left,
        Modifiers::default(),
        0,
        now,
    );
    ctrl.handle_pointer_move(sr2.x + 20.0, sr2.y + 4.0, Modifiers::default(), now);
    assert!(
        ctrl.ui
            .tab_reorder_overlay
            .as_ref()
            .and_then(|o| o.indicator_rect)
            .is_some()
    );
    ctrl.handle_pointer_up(
        sr2.x + 20.0,
        sr2.y + 4.0,
        PointerButton::Left,
        Modifiers::default(),
    );
    let cmds2 = ctrl.drain_pending_commands();
    assert!(cmds2.iter().any(|c| matches!(
        c,
        HostCommand::MoveWindow { window_id, before_window_id, .. }
        if window_id == "window:3" && before_window_id.as_deref() == Some("window:2")
    )));

    // Out-of-bounds release cancels cleanly.
    let fi3 = ctrl.build_ui_frame_input(now);
    let out3 = ctrl.ui.render(&fi3, &ctrl.theme, now, 0.0);
    let sr1 = out3
        .draw_list
        .hits
        .iter()
        .find(|h| matches!(&h.target, ui_chrome::HitTarget::TabCard { window_id, .. } if window_id == "window:1"))
        .expect("sidebar tab 1 hit")
        .rect;
    ctrl.last_ui_output = Some(out3);
    ctrl.drain_pending_commands();
    ctrl.handle_pointer_down(
        sr1.x + 20.0,
        sr1.y + sr1.h * 0.5,
        PointerButton::Left,
        Modifiers::default(),
        0,
        now,
    );
    ctrl.handle_pointer_move(900.0, 400.0, Modifiers::default(), now);
    assert!(ctrl.ui.tab_reorder_overlay.is_none());
    ctrl.handle_pointer_up(900.0, 400.0, PointerButton::Left, Modifiers::default());
    assert!(
        !ctrl
            .drain_pending_commands()
            .iter()
            .any(|c| matches!(c, HostCommand::MoveWindow { .. }))
    );
}

#[test]
fn m5_f22_float_or_dock_empty_tiled_layer_dispatches_dock_not_minimize() {
    let mut ctrl = make_test_controller();
    let mut host = HostEntry::new(
        LOCAL_HOST_ID,
        "This Machine",
        RexEndpoint::parse("unix:///tmp/test.sock").unwrap(),
    );
    host.status = HostStatus::Connected {
        client_id: "client:1".into(),
        server_host_id: Some("host:local".into()),
        api_revision: 24,
        resumed: false,
    };
    host.on_session_created(SessionSummary {
        session_id: "session:work".into(),
        label: "Work".into(),
    });
    ctrl.workspace.add_host(host);
    ctrl.workspace
        .select_session(SessionKey::new(LOCAL_HOST_ID, "session:work"));

    let view = SessionView {
        revision: 1,
        session_id: Some("session:work".into()),
        label: "Work".into(),
        active_window_id: Some("window:1".into()),
        windows: vec![
            WindowView {
                window_id: Some("window:1".into()),
                label: "TabA".into(),
                focused_block_id: Some("block:1".into()),
                zoomed_block_id: None,
                tiled_zoomed_block_id: None,
                layers: vec![
                    LayerView {
                        layer_id: Some("layer:tiled".into()),
                        kind: "tiled".into(),
                        bounds: [0.0, 0.0, 1.0, 1.0],
                        state: None,
                        layout: None,
                        blocks: Vec::new(),
                    },
                    LayerView {
                        layer_id: Some("layer:float".into()),
                        kind: "floating".into(),
                        bounds: [0.15, 0.15, 0.7, 0.7],
                        state: Some("normal".into()),
                        layout: None,
                        blocks: vec![BlockView {
                            block_id: Some("block:1".into()),
                            label: "floated".into(),
                            creator_name: None,
                            flavor: Some("com.superlogical.terminal.shell".into()),
                            host_id: None,
                            owner: Some("client:1".into()),
                        }],
                    },
                ],
            },
            WindowView {
                window_id: Some("window:2".into()),
                label: "TabB".into(),
                focused_block_id: None,
                zoomed_block_id: None,
                tiled_zoomed_block_id: None,
                layers: Vec::new(),
            },
        ],
    };
    let now = Instant::now();
    ctrl.process_runtime_event(
        RuntimeEvent::SessionViewUpdated {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:work".into(),
            view: Box::new(view),
            seq: 1,
        },
        now,
    );
    ctrl.ui.focus.focus_terminal(Some("block:1".to_owned()));
    ctrl.drain_pending_commands();

    ctrl.dispatch_ui_action(
        UiAction::PaneButton {
            block_id: "block:1".into(),
            button: PaneHeaderButton::FloatOrDock,
        },
        now,
    );
    let cmds = ctrl.drain_pending_commands();
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, HostCommand::SetLayerState { .. })),
        "FloatOrDock on empty tiled layer must NOT emit SetLayerState(\"minimized\")"
    );
    assert!(cmds.iter().any(|c| matches!(
        c,
        HostCommand::DockFloatingBlock {
            block_id,
            window_label,
            before_window_id,
            ..
        } if block_id == "block:1"
            && window_label.as_deref() == Some("TabA")
            && before_window_id.as_deref() == Some("window:2")
    )));

    // MinimizeFloating on last visible pane must also guard against 400 bad_request.
    ctrl.execute_client_command(LOCAL_HOST_ID, ClientCommand::MinimizeFloating, now);
    let min_cmds = ctrl.drain_pending_commands();
    assert!(
        !min_cmds
            .iter()
            .any(|c| matches!(c, HostCommand::SetLayerState { .. }))
    );
}

#[test]
fn m5_f23_multi_window_transfer_new_window_host_picker_and_lru_pinning() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();

    // 1. MoveTabToNewWindow dispatches TransferTabToNewWindow.
    ctrl.dispatch_ui_action(
        UiAction::MoveTabToNewWindow {
            window_id: "window:1".into(),
        },
        now,
    );
    let cmds = ctrl.drain_pending_commands();
    assert!(cmds.iter().any(|c| matches!(
        c,
        HostCommand::TransferTabToNewWindow {
            source_session_id,
            window_id,
            ..
        } if source_session_id == "session:t3" && window_id == "window:1"
    )));

    // 2. Delivering TabTransferredToNewWindow migrates PaneRuntime & reopens stream on target_session_id,
    //    and subsequent source BlockClosed does NOT destroy the transferred pane!
    let target_view = SessionView {
        revision: 1,
        session_id: Some("session:torn".into()),
        label: "quiet-marsh".into(),
        active_window_id: Some("window:1".into()),
        windows: vec![WindowView {
            window_id: Some("window:1".into()),
            label: "Build bridge.rs".into(),
            focused_block_id: Some("block:1".into()),
            zoomed_block_id: None,
            tiled_zoomed_block_id: None,
            layers: vec![LayerView {
                layer_id: Some("layer:tiled".into()),
                kind: "tiled".into(),
                bounds: [0.0, 0.0, 1.0, 1.0],
                state: None,
                layout: Some(LayoutNode {
                    rect: [0.0, 0.0, 1.0, 1.0],
                    block_id: Some("block:1".into()),
                    split: None,
                }),
                blocks: vec![BlockView {
                    block_id: Some("block:1".into()),
                    label: "t3:1baa1939/thread-1".into(),
                    creator_name: None,
                    flavor: Some("com.superlogical.terminal.shell".into()),
                    host_id: Some("host:local".into()),
                    owner: Some("client:1".into()),
                }],
            }],
        }],
    };
    ctrl.process_runtime_event(
        RuntimeEvent::TabTransferredToNewWindow {
            host_id: LOCAL_HOST_ID.into(),
            source_session_id: "session:t3".into(),
            target_session_id: "session:torn".into(),
            window_id: "window:1".into(),
            target_view: Box::new(target_view),
            source_view: None,
            seq: 10,
        },
        now,
    );
    let new_key = BlockKey::new(LOCAL_HOST_ID, "session:torn", "block:1");
    let old_key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");
    assert!(ctrl.pane_store.contains(&new_key));
    assert!(!ctrl.pane_store.contains(&old_key));
    let pending_wins = ctrl.take_pending_new_windows();
    assert!(matches!(
        pending_wins.as_slice(),
        [PendingNewWindow::Session { session, .. }] if session.session_id == "session:torn"
    ));
    let reopen_cmds = ctrl.drain_pending_commands();
    assert!(reopen_cmds.iter().any(|c| matches!(
        c,
        HostCommand::OpenBlockStream { session_id, block_id, .. }
        if session_id == "session:torn" && block_id == "block:1"
    )));

    // Source BlockClosed arriving afterwards leaves transferred pane intact.
    ctrl.process_runtime_event(
        RuntimeEvent::ControlEvent {
            host_id: LOCAL_HOST_ID.into(),
            seq: 11,
            event: Box::new(ServerEvent::SessionEvent {
                session_id: "session:t3".into(),
                event: SessionEvent::BlockClosed {
                    block_id: "block:1".into(),
                },
            }),
        },
        now,
    );
    assert!(ctrl.pane_store.contains(&new_key));

    // 3. ClientCommand::NewWindow requests open_in_new_window: true.
    ctrl.execute_client_command(LOCAL_HOST_ID, ClientCommand::NewWindow, now);
    let nw_cmds = ctrl.drain_pending_commands();
    assert!(nw_cmds.iter().any(|c| matches!(
        c,
        HostCommand::NewSession {
            open_in_new_window: true,
            ..
        }
    )));

    // 4. ClientCommand::NewSession { on_host: true } with 2 hosts opens host picker modal.
    let mut remote = HostEntry::new(
        "remote:mac",
        "MacBook",
        RexEndpoint::parse("ssh://mike@mac").unwrap(),
    );
    remote.is_local = false;
    ctrl.workspace.add_host(remote);
    ctrl.execute_client_command(
        LOCAL_HOST_ID,
        ClientCommand::NewSession { on_host: true },
        now,
    );
    assert!(ctrl.ui.modal.is_some());

    // 5. Warm-LRU pinning across open windows protects pinned session from eviction.
    let mut ws = crate::workspace::WorkspaceState::new(2);
    let s1 = SessionKey::new(LOCAL_HOST_ID, "s1");
    let s2 = SessionKey::new(LOCAL_HOST_ID, "s2");
    let s3 = SessionKey::new(LOCAL_HOST_ID, "s3");
    let s4 = SessionKey::new(LOCAL_HOST_ID, "s4");
    ws.pinned_sessions.insert(s1.clone());
    assert!(ws.select_session(s1.clone()).evicted_sessions.is_empty());
    assert!(ws.select_session(s2.clone()).evicted_sessions.is_empty());
    let plan3 = ws.select_session(s3.clone());
    assert!(
        !plan3.evicted_sessions.contains(&s1),
        "pinned session s1 must never be evicted while open in another window"
    );
    let plan4 = ws.select_session(s4.clone());
    assert!(!plan4.evicted_sessions.contains(&s1));
}

#[test]
fn m5_m4_polish_redacts_quoted_auth_and_hashbang_fragments() {
    let input = concat!(
        "Authorization: \"RAW_QUOTED_SECRET_123\"\n",
        "Authorization: 'RAW_SINGLE_QUOTED_456'\n",
        "Authorization: Bearer \"QUOTED_BEARER_SECRET\"\n",
        "Proxy-Authorization: \"PROXY_QUOTED_SECRET\"\n",
        "Bearer \"STANDALONE_QUOTED_SECRET\"\n",
        "url=https://app.example.com/#!access_token=HASHBANG_SECRET&token=HASHBANG_TOK2&ok=1\n",
    );
    let redacted = redact_diagnostics_text(input);
    assert!(!redacted.contains("RAW_QUOTED_SECRET_123"), "{redacted}");
    assert!(!redacted.contains("RAW_SINGLE_QUOTED_456"), "{redacted}");
    assert!(!redacted.contains("QUOTED_BEARER_SECRET"), "{redacted}");
    assert!(!redacted.contains("PROXY_QUOTED_SECRET"), "{redacted}");
    assert!(!redacted.contains("STANDALONE_QUOTED_SECRET"), "{redacted}");
    assert!(!redacted.contains("HASHBANG_SECRET"), "{redacted}");
    assert!(!redacted.contains("HASHBANG_TOK2"), "{redacted}");
    assert!(
        redacted.contains("Authorization: \"[REDACTED]\""),
        "{redacted}"
    );
    assert!(
        redacted.contains("Authorization: '[REDACTED]'"),
        "{redacted}"
    );
    assert!(
        redacted.contains("Authorization: Bearer \"[REDACTED]\""),
        "{redacted}"
    );
    assert!(
        redacted.contains("Proxy-Authorization: \"[REDACTED]\""),
        "{redacted}"
    );
    assert!(redacted.contains("Bearer \"[REDACTED]\""), "{redacted}");
    assert!(
        redacted.contains("#!access_token=[REDACTED]&token=[REDACTED]&ok=1"),
        "{redacted}"
    );
}

#[test]
fn m5_r2_command_launch_preserves_layout_overrides_in_create_window_context() {
    let mut ctrl = make_test_controller();
    ctrl.set_command_launch(Some(CommandLaunchSpec {
        command: vec!["btop".to_owned()],
        cwd: None,
        wait_after_command: false,
    }));
    let ctx = ctrl.create_window_context(1280, 720, 1.0, None, None);
    assert_eq!(ctx.ui.layout_cfg.toolbar_behavior, ToolbarBehavior::Hidden);
    assert_eq!(ctx.ui.layout_cfg.pane_header_mode, PaneHeaderMode::Hidden);
    assert_eq!(ctx.ui.layout_cfg.density, PaneDensity::Compact);
}

#[test]
fn m5_r2_transfer_tab_live_rex_block_closed_race_preserves_pane_and_tunnels() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();
    let old_key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");
    let new_key = BlockKey::new(LOCAL_HOST_ID, "session:torn", "block:1");

    // Seed active tunnel and copy mode on the block being torn off.
    ctrl.active_tunnels.push(ActiveTunnel {
        tunnel_id: 42,
        source_block: old_key.clone(),
        tunnel_session_id: "session:t3".to_owned(),
        tunnel_block_id: Some("block:tun1".to_owned()),
        remote_host: "127.0.0.1".to_owned(),
        remote_port: 5173,
        local_port: Some(15173),
        error: None,
    });
    ctrl.copy_mode = Some(CopyModeState {
        block_key: old_key.clone(),
        cursor: vt_engine::ScreenPoint { col: 2, row: 1 },
        visual: None,
        search_active: false,
        search_query: String::new(),
        pending_g: false,
    });
    let _ = ctrl.drain_pending_commands();

    // 1. Live Rex ordering: TabTransferStarting -> BlockClosed(src, b1) -> TabTransferredToNewWindow.
    ctrl.process_runtime_event(
        RuntimeEvent::TabTransferStarting {
            host_id: LOCAL_HOST_ID.into(),
            source_session_id: "session:t3".into(),
            target_session_id: "session:torn".into(),
            window_id: "window:1".into(),
            block_ids: vec!["block:1".into()],
        },
        now,
    );
    ctrl.process_runtime_event(
        RuntimeEvent::ControlEvent {
            host_id: LOCAL_HOST_ID.into(),
            seq: 9,
            event: Box::new(ServerEvent::SessionEvent {
                session_id: "session:t3".into(),
                event: SessionEvent::BlockClosed {
                    block_id: "block:1".into(),
                },
            }),
        },
        now,
    );

    // PaneRuntime, active_tunnels, and copy_mode must survive the early source BlockClosed!
    assert!(
        ctrl.pane_store.contains(&old_key),
        "PaneRuntime must not be deleted by BlockClosed during pending transfer"
    );
    assert_eq!(ctrl.active_tunnels.len(), 1);
    assert!(ctrl.copy_mode.is_some());
    assert!(
        !ctrl
            .drain_pending_commands()
            .iter()
            .any(|c| matches!(c, HostCommand::StopPortTunnel { .. })),
        "active tunnel must not be torn down on source BlockClosed during pending transfer"
    );

    let target_view = SessionView {
        revision: 1,
        session_id: Some("session:torn".into()),
        label: "quiet-marsh".into(),
        active_window_id: Some("window:1".into()),
        windows: vec![WindowView {
            window_id: Some("window:1".into()),
            label: "Torn Tab".into(),
            focused_block_id: Some("block:1".into()),
            zoomed_block_id: None,
            tiled_zoomed_block_id: None,
            layers: vec![LayerView {
                layer_id: Some("layer:tiled".into()),
                kind: "tiled".into(),
                bounds: [0.0, 0.0, 1.0, 1.0],
                state: None,
                layout: Some(LayoutNode {
                    rect: [0.0, 0.0, 1.0, 1.0],
                    block_id: Some("block:1".into()),
                    split: None,
                }),
                blocks: vec![BlockView {
                    block_id: Some("block:1".into()),
                    label: "zsh".into(),
                    creator_name: None,
                    flavor: Some("com.superlogical.terminal.shell".into()),
                    host_id: Some("host:local".into()),
                    owner: Some("client:1".into()),
                }],
            }],
        }],
    };
    ctrl.process_runtime_event(
        RuntimeEvent::TabTransferredToNewWindow {
            host_id: LOCAL_HOST_ID.into(),
            source_session_id: "session:t3".into(),
            target_session_id: "session:torn".into(),
            window_id: "window:1".into(),
            target_view: Box::new(target_view),
            source_view: None,
            seq: 10,
        },
        now,
    );

    assert!(ctrl.pane_store.contains(&new_key));
    assert!(!ctrl.pane_store.contains(&old_key));
    assert_eq!(ctrl.active_tunnels.len(), 1);
    assert_eq!(ctrl.active_tunnels[0].source_block, new_key);
    assert_eq!(ctrl.active_tunnels[0].tunnel_session_id, "session:t3");
    assert_eq!(
        ctrl.copy_mode.as_ref().map(|c| &c.block_key),
        Some(&new_key)
    );
    assert!(ctrl.drain_pending_commands().iter().any(|c| matches!(
        c,
        HostCommand::OpenBlockStream { session_id, block_id, .. }
        if session_id == "session:torn" && block_id == "block:1"
    )));

    // 2. Also verify TabTransferFailed cleans up if BlockClosed already fired.
    let fail_key = BlockKey::new(LOCAL_HOST_ID, "session:torn", "block:1");
    ctrl.process_runtime_event(
        RuntimeEvent::TabTransferStarting {
            host_id: LOCAL_HOST_ID.into(),
            source_session_id: "session:torn".into(),
            target_session_id: "session:torn2".into(),
            window_id: "window:1".into(),
            block_ids: vec!["block:1".into()],
        },
        now,
    );
    ctrl.process_runtime_event(
        RuntimeEvent::ControlEvent {
            host_id: LOCAL_HOST_ID.into(),
            seq: 11,
            event: Box::new(ServerEvent::SessionEvent {
                session_id: "session:torn".into(),
                event: SessionEvent::BlockClosed {
                    block_id: "block:1".into(),
                },
            }),
        },
        now,
    );
    assert!(ctrl.pane_store.contains(&fail_key));
    ctrl.process_runtime_event(
        RuntimeEvent::TabTransferFailed {
            host_id: LOCAL_HOST_ID.into(),
            source_session_id: "session:torn".into(),
            window_id: "window:1".into(),
            block_ids: vec!["block:1".into()],
        },
        now,
    );
    assert!(!ctrl.pane_store.contains(&fail_key));
    assert!(ctrl.active_tunnels.is_empty());
    assert!(ctrl.copy_mode.is_none());
    assert!(ctrl.drain_pending_commands().iter().any(|c| matches!(
        c,
        HostCommand::StopPortTunnel { session_id, tunnel_id, .. }
        if session_id == "session:t3" && *tunnel_id == 42
    )));
}

#[test]
fn m5_r2_multi_window_runtime_event_routing() {
    let mut ctrl = make_test_controller();
    let now = Instant::now();

    let mut host = HostEntry::new(
        LOCAL_HOST_ID,
        "This Machine",
        RexEndpoint::parse("unix:///tmp/test.sock").unwrap(),
    );
    host.status = HostStatus::Connected {
        client_id: "client:1".into(),
        server_host_id: Some("host:local".into()),
        api_revision: 24,
        resumed: false,
    };
    host.on_session_created(SessionSummary {
        session_id: "session:a".into(),
        label: "Alpha".into(),
    });
    host.on_session_created(SessionSummary {
        session_id: "session:b".into(),
        label: "Beta".into(),
    });
    host.on_session_created(SessionSummary {
        session_id: "session:ephem".into(),
        label: "omarex-run-btop".into(),
    });
    ctrl.workspace.add_host(host);

    let mut remote = HostEntry::new(
        "remote:box",
        "Remote Box",
        RexEndpoint::parse("ssh://mike@box").unwrap(),
    );
    remote.is_local = false;
    remote.status = HostStatus::Connected {
        client_id: "client:2".into(),
        server_host_id: Some("host:box".into()),
        api_revision: 24,
        resumed: false,
    };
    remote.on_session_created(SessionSummary {
        session_id: "session:ra".into(),
        label: "RemoteAlpha".into(),
    });
    ctrl.workspace.add_host(remote);

    let make_view = |sid: &str, label: &str, wid: &str, bid: &str| SessionView {
        revision: 1,
        session_id: Some(sid.to_owned()),
        label: label.to_owned(),
        active_window_id: Some(wid.to_owned()),
        windows: vec![WindowView {
            window_id: Some(wid.to_owned()),
            label: "Tab1".to_owned(),
            focused_block_id: Some(bid.to_owned()),
            zoomed_block_id: None,
            tiled_zoomed_block_id: None,
            layers: vec![LayerView {
                layer_id: Some("layer:tiled".into()),
                kind: "tiled".into(),
                bounds: [0.0, 0.0, 1.0, 1.0],
                state: None,
                layout: Some(LayoutNode {
                    rect: [0.0, 0.0, 1.0, 1.0],
                    block_id: Some(bid.to_owned()),
                    split: None,
                }),
                blocks: vec![BlockView {
                    block_id: Some(bid.to_owned()),
                    label: "zsh".into(),
                    creator_name: None,
                    flavor: Some("com.superlogical.terminal.shell".into()),
                    host_id: Some("host:local".into()),
                    owner: Some("client:1".into()),
                }],
            }],
        }],
    };

    ctrl.workspace
        .session_mut(&SessionKey::new("remote:box", "session:ra"))
        .unwrap()
        .apply_view(make_view(
            "session:ra",
            "RemoteAlpha",
            "win:ra1",
            "block:ra1",
        ));
    ctrl.workspace
        .session_mut(&SessionKey::new(LOCAL_HOST_ID, "session:a"))
        .unwrap()
        .apply_view(make_view("session:a", "Alpha", "win:a1", "block:a1"));
    ctrl.workspace
        .session_mut(&SessionKey::new(LOCAL_HOST_ID, "session:b"))
        .unwrap()
        .apply_view(make_view("session:b", "Beta", "win:b1", "block:b1"));

    let mut ctx_a = ctrl.create_window_context(
        800,
        600,
        1.0,
        Some(SessionKey::new("remote:box", "session:ra")),
        Some("block:ra1".to_owned()),
    );
    let mut ctx_b = ctrl.create_window_context(
        800,
        600,
        1.0,
        Some(SessionKey::new(LOCAL_HOST_ID, "session:b")),
        Some("block:b1".to_owned()),
    );
    let ctx_a_id = ctx_a.id;

    // 1. DirListed routes to ctx_a even while ctx_b is focused (preferred_idx = Some(1)).
    ctrl.with_window_context(&mut ctx_a, |c| {
        c.execute_client_command("remote:box", ClientCommand::InsertPath, now);
    });
    let list_req_id = ctrl
        .drain_pending_commands()
        .into_iter()
        .find_map(|c| match c {
            HostCommand::ListDir { request_id, .. } => Some(request_id),
            _ => None,
        })
        .expect("ListDir command");
    {
        let mut contexts = [&mut ctx_a, &mut ctx_b];
        ctrl.process_runtime_event_multi(
            RuntimeEvent::DirListed {
                key: BlockKey::new("remote:box", "session:ra", "block:ra1"),
                request_id: list_req_id,
                result: Ok(rex_proto::types::TerminalListDirResponse {
                    status: rex_proto::types::ListDirStatus::Ok,
                    path: "/tmp".to_owned(),
                    home: "/home/user".to_owned(),
                    entries: vec![rex_proto::types::DirEntry {
                        name: "demo.rs".to_owned(),
                        file: true,
                        symlink: false,
                    }],
                    truncated: false,
                }),
            },
            &mut contexts,
            Some(1),
            now,
        );
    }
    match ctx_a.ui.modal.as_ref().map(|m| &m.content) {
        Some(ui_chrome::ModalContent::FilePicker {
            entries, loading, ..
        }) => {
            assert!(!loading);
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].name, "demo.rs");
        }
        other => panic!("expected ctx_a FilePicker entries, got {other:?}"),
    }
    assert!(ctx_b.ui.modal.is_none());
    ctx_a.ui.close_modal();

    // Switch ctx_a to local session:a for remaining checks.
    ctrl.with_window_context(&mut ctx_a, |c| {
        c.activate_session(SessionKey::new(LOCAL_HOST_ID, "session:a"), &HashMap::new());
    });
    let _ = ctrl.drain_pending_commands();

    // 2. DownloadFinished routes to ctx_a's pending_viewer_downloads without hijacking ctx_b.
    ctrl.with_window_context(&mut ctx_a, |c| {
        let key = BlockKey::new(LOCAL_HOST_ID, "session:a", "block:a1");
        let _ = c.open_remote_path_in_viewer(
            &key,
            "/tmp/nonexistent_remote_test.rs",
            ui_chrome::TargetKind::TextCode,
            None,
            None,
        );
    });
    let dl_id = ctrl
        .drain_pending_commands()
        .into_iter()
        .find_map(|c| match c {
            HostCommand::DownloadFile { transfer_id, .. } => Some(transfer_id),
            _ => None,
        })
        .expect("DownloadFile command");
    assert!(ctx_a.has_pending_viewer_download(&dl_id));
    {
        let mut contexts = [&mut ctx_a, &mut ctx_b];
        ctrl.process_runtime_event_multi(
            RuntimeEvent::DownloadFinished {
                host_id: LOCAL_HOST_ID.into(),
                session_id: "session:a".into(),
                block_id: "block:a1".into(),
                transfer_id: dl_id.clone(),
                result: Err("simulated error".to_owned()),
            },
            &mut contexts,
            Some(1),
            now,
        );
    }
    assert!(!ctx_a.has_pending_viewer_download(&dl_id));
    assert!(ctx_a.ui.viewer.open);
    assert!(!ctx_b.ui.viewer.open);

    // 3. WindowCreated and SessionCreatedAndAttached route to requesting ctx_a, never hijacking ctx_b.
    {
        let mut contexts = [&mut ctx_a, &mut ctx_b];
        ctrl.process_runtime_event_multi(
            RuntimeEvent::WindowCreated {
                host_id: LOCAL_HOST_ID.into(),
                session_id: "session:a".into(),
                window_id: "win:a2".into(),
                block_ids: vec!["block:a2".into()],
                view: None,
                seq: 5,
                origin_window_id: Some(ctx_a_id),
            },
            &mut contexts,
            Some(1),
            now,
        );
    }
    assert_eq!(ctx_a.ui.focus.last_terminal_block(), Some("block:a2"));
    assert_eq!(
        ctx_b.active_session(),
        Some(&SessionKey::new(LOCAL_HOST_ID, "session:b"))
    );
    assert_eq!(ctx_b.ui.focus.last_terminal_block(), Some("block:b1"));

    {
        let mut contexts = [&mut ctx_a, &mut ctx_b];
        ctrl.process_runtime_event_multi(
            RuntimeEvent::SessionCreatedAndAttached {
                host_id: LOCAL_HOST_ID.into(),
                session_id: "session:c".into(),
                view: Box::new(make_view("session:c", "Gamma", "win:c1", "block:c1")),
                seq: 6,
                open_in_new_window: false,
                origin_window_id: Some(ctx_a_id),
            },
            &mut contexts,
            Some(1),
            now,
        );
    }
    assert_eq!(
        ctx_a.active_session(),
        Some(&SessionKey::new(LOCAL_HOST_ID, "session:c"))
    );
    assert_eq!(
        ctx_b.active_session(),
        Some(&SessionKey::new(LOCAL_HOST_ID, "session:b"))
    );

    // 4. SessionViewUpdated updates shared view & syncs ctx_a even when ctx_b is focused.
    let updated_c = SessionView {
        revision: 2,
        session_id: Some("session:c".into()),
        label: "Gamma".into(),
        active_window_id: Some("win:c2".into()),
        windows: vec![WindowView {
            window_id: Some("win:c2".into()),
            label: "Tab2".into(),
            focused_block_id: Some("block:c2".into()),
            zoomed_block_id: None,
            tiled_zoomed_block_id: None,
            layers: vec![LayerView {
                layer_id: Some("layer:tiled".into()),
                kind: "tiled".into(),
                bounds: [0.0, 0.0, 1.0, 1.0],
                state: None,
                layout: None,
                blocks: vec![BlockView {
                    block_id: Some("block:c2".into()),
                    label: "zsh".into(),
                    creator_name: None,
                    flavor: Some("com.superlogical.terminal.shell".into()),
                    host_id: Some("host:local".into()),
                    owner: Some("client:1".into()),
                }],
            }],
        }],
    };
    {
        let mut contexts = [&mut ctx_a, &mut ctx_b];
        ctrl.process_runtime_event_multi(
            RuntimeEvent::SessionViewUpdated {
                host_id: LOCAL_HOST_ID.into(),
                session_id: "session:c".into(),
                view: Box::new(updated_c),
                seq: 7,
            },
            &mut contexts,
            Some(1),
            now,
        );
    }
    assert_eq!(ctx_a.ui.focus.last_terminal_block(), Some("block:c2"));

    // 5. ThemeChanged broadcasts rounding & needs_redraw across ALL windows.
    ctx_a.ui.layout_cfg.hyprland_rounding = Some(0.0);
    ctx_b.ui.layout_cfg.hyprland_rounding = Some(0.0);
    {
        let mut contexts = [&mut ctx_a, &mut ctx_b];
        ctrl.process_runtime_event_multi(
            RuntimeEvent::ThemeChanged(omarchy_theme::ThemeEvent::RoundingChanged(14.0)),
            &mut contexts,
            Some(1),
            now,
        );
    }
    assert_eq!(ctx_a.ui.layout_cfg.hyprland_rounding, Some(14.0));
    assert_eq!(ctx_b.ui.layout_cfg.hyprland_rounding, Some(14.0));
    assert!(ctx_a.needs_redraw());
    assert!(ctx_b.needs_redraw());

    // 5b. PromptRequested opens on preferred ctx_a, then PromptEnded dismisses ctx_a even when ctx_b is focused.
    {
        let mut contexts = [&mut ctx_a, &mut ctx_b];
        ctrl.process_runtime_event_multi(
            RuntimeEvent::PromptRequested {
                host_id: LOCAL_HOST_ID.into(),
                seq: 70,
                request: Box::new(ClientPromptRequestedEvent {
                    call_id: "call-mw-1".into(),
                    action: None,
                    prompt: PromptSpec {
                        kind: PromptKind::Input,
                        title: "Token?".into(),
                        placeholder: None,
                        value: None,
                        confirm: None,
                        multiple: false,
                        allow_other: false,
                        items: vec![],
                    },
                    timeout_ms: 5000,
                }),
            },
            &mut contexts,
            Some(0),
            now,
        );
    }
    assert!(ctx_a.ui.modal.is_some());
    assert!(ctx_b.ui.modal.is_none());
    {
        let mut contexts = [&mut ctx_a, &mut ctx_b];
        ctrl.process_runtime_event_multi(
            RuntimeEvent::PromptEnded {
                host_id: LOCAL_HOST_ID.into(),
                seq: 71,
                event: ClientPromptEndedEvent {
                    call_id: "call-mw-1".into(),
                    reason: "cancelled".into(),
                },
            },
            &mut contexts,
            Some(1),
            now,
        );
    }
    assert!(ctx_a.ui.modal.is_none());

    // 6. Per-window ephemeral_session close_requested does NOT set global should_exit or close ctx_b.
    {
        let mut contexts = [&mut ctx_a, &mut ctx_b];
        ctrl.process_runtime_event_multi(
            RuntimeEvent::CommandSessionReady {
                host_id: LOCAL_HOST_ID.into(),
                session_id: "session:ephem".into(),
                view: Box::new(make_view(
                    "session:ephem",
                    "omarex-run-btop",
                    "win:e1",
                    "block:e1",
                )),
                origin_window_id: Some(ctx_a_id),
            },
            &mut contexts,
            Some(1),
            now,
        );
    }
    assert_eq!(
        ctx_a.ephemeral_session(),
        Some(&SessionKey::new(LOCAL_HOST_ID, "session:ephem"))
    );
    {
        let mut contexts = [&mut ctx_a, &mut ctx_b];
        ctrl.process_runtime_event_multi(
            RuntimeEvent::ControlEvent {
                host_id: LOCAL_HOST_ID.into(),
                seq: 8,
                event: Box::new(ServerEvent::SessionEvent {
                    session_id: "session:ephem".into(),
                    event: SessionEvent::BlockClosed {
                        block_id: "block:e1".into(),
                    },
                }),
            },
            &mut contexts,
            Some(1),
            now,
        );
    }
    assert!(ctx_a.is_close_requested());
    assert!(!ctx_b.is_close_requested());
    assert!(!ctrl.should_exit());
}

#[test]
fn m5_r2_minimize_all_floating_guards_last_visible_pane() {
    let mut ctrl = make_test_controller();
    let mut host = HostEntry::new(
        LOCAL_HOST_ID,
        "This Machine",
        RexEndpoint::parse("unix:///tmp/test.sock").unwrap(),
    );
    host.status = HostStatus::Connected {
        client_id: "client:1".into(),
        server_host_id: Some("host:local".into()),
        api_revision: 24,
        resumed: false,
    };
    host.on_session_created(SessionSummary {
        session_id: "session:work".into(),
        label: "Work".into(),
    });
    ctrl.workspace.add_host(host);
    ctrl.workspace
        .select_session(SessionKey::new(LOCAL_HOST_ID, "session:work"));

    let now = Instant::now();
    let single_float_view = SessionView {
        revision: 1,
        session_id: Some("session:work".into()),
        label: "Work".into(),
        active_window_id: Some("window:1".into()),
        windows: vec![WindowView {
            window_id: Some("window:1".into()),
            label: "Tab1".into(),
            focused_block_id: Some("block:f1".into()),
            zoomed_block_id: None,
            tiled_zoomed_block_id: None,
            layers: vec![
                LayerView {
                    layer_id: Some("layer:tiled".into()),
                    kind: "tiled".into(),
                    bounds: [0.0, 0.0, 1.0, 1.0],
                    state: None,
                    layout: None,
                    blocks: Vec::new(),
                },
                LayerView {
                    layer_id: Some("layer:f1".into()),
                    kind: "floating".into(),
                    bounds: [0.1, 0.1, 0.5, 0.5],
                    state: Some("normal".into()),
                    layout: None,
                    blocks: vec![BlockView {
                        block_id: Some("block:f1".into()),
                        label: "f1".into(),
                        creator_name: None,
                        flavor: Some("com.superlogical.terminal.shell".into()),
                        host_id: None,
                        owner: Some("client:1".into()),
                    }],
                },
            ],
        }],
    };
    ctrl.process_runtime_event(
        RuntimeEvent::SessionViewUpdated {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:work".into(),
            view: Box::new(single_float_view),
            seq: 1,
        },
        now,
    );
    ctrl.ui.focus.focus_terminal(Some("block:f1".to_owned()));
    ctrl.drain_pending_commands();

    ctrl.execute_client_command(LOCAL_HOST_ID, ClientCommand::MinimizeAllFloating, now);
    let cmds1 = ctrl.drain_pending_commands();
    assert!(
        !cmds1
            .iter()
            .any(|c| matches!(c, HostCommand::SetLayerState { .. })),
        "MinimizeAllFloating must guard the last visible floating pane when tiled layer is empty"
    );

    // With 2 floating layers and empty tiled layer, minimize the unfocused one and keep focused visible.
    let two_float_view = SessionView {
        revision: 2,
        session_id: Some("session:work".into()),
        label: "Work".into(),
        active_window_id: Some("window:1".into()),
        windows: vec![WindowView {
            window_id: Some("window:1".into()),
            label: "Tab1".into(),
            focused_block_id: Some("block:f1".into()),
            zoomed_block_id: None,
            tiled_zoomed_block_id: None,
            layers: vec![
                LayerView {
                    layer_id: Some("layer:tiled".into()),
                    kind: "tiled".into(),
                    bounds: [0.0, 0.0, 1.0, 1.0],
                    state: None,
                    layout: None,
                    blocks: Vec::new(),
                },
                LayerView {
                    layer_id: Some("layer:f1".into()),
                    kind: "floating".into(),
                    bounds: [0.1, 0.1, 0.5, 0.5],
                    state: Some("normal".into()),
                    layout: None,
                    blocks: vec![BlockView {
                        block_id: Some("block:f1".into()),
                        label: "f1".into(),
                        creator_name: None,
                        flavor: Some("com.superlogical.terminal.shell".into()),
                        host_id: None,
                        owner: Some("client:1".into()),
                    }],
                },
                LayerView {
                    layer_id: Some("layer:f2".into()),
                    kind: "floating".into(),
                    bounds: [0.2, 0.2, 0.5, 0.5],
                    state: Some("normal".into()),
                    layout: None,
                    blocks: vec![BlockView {
                        block_id: Some("block:f2".into()),
                        label: "f2".into(),
                        creator_name: None,
                        flavor: Some("com.superlogical.terminal.shell".into()),
                        host_id: None,
                        owner: Some("client:1".into()),
                    }],
                },
            ],
        }],
    };
    ctrl.process_runtime_event(
        RuntimeEvent::SessionViewUpdated {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:work".into(),
            view: Box::new(two_float_view),
            seq: 2,
        },
        now,
    );
    ctrl.drain_pending_commands();

    ctrl.execute_client_command(LOCAL_HOST_ID, ClientCommand::MinimizeAllFloating, now);
    let cmds2 = ctrl.drain_pending_commands();
    assert_eq!(cmds2.len(), 1);
    assert!(matches!(
        &cmds2[0],
        HostCommand::SetLayerState { request, .. }
        if request.layer_id == "layer:f2" && request.state == "minimized"
    ));
}

#[test]
fn scenario_overlay_preserves_live_session_and_respects_existing_state() {
    use super::render::{
        activate_dnd_split_overlay, apply_scenario_to_live_session, resolve_scenario_startup_theme,
    };

    let base_opts = HeadlessRenderOptions::default();

    // 1. CopyMode preserves live session ("session:t3") and existing layout_cfg (e.g. hidden sidebar).
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    ctrl.ui.layout_cfg.toolbar_behavior = ui_chrome::ToolbarBehavior::Hidden;
    ctrl.ui.layout_cfg.pane_header_mode = ui_chrome::PaneHeaderMode::Hidden;
    apply_scenario_to_live_session(&mut ctrl, HeadlessScenario::CopyMode, &base_opts);
    assert_eq!(
        ctrl.workspace.active_session,
        Some(SessionKey::new(LOCAL_HOST_ID, "session:t3"))
    );
    assert_eq!(
        ctrl.ui.layout_cfg.toolbar_behavior,
        ui_chrome::ToolbarBehavior::Hidden
    );
    let cm = ctrl.copy_mode.as_ref().expect("copy mode active");
    assert_eq!(
        cm.block_key,
        BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1")
    );
    assert!(cm.visual.is_some());

    // 2. SyntaxViewer skips synthetic tab when viewer_paths is already provided.
    let opts_with_viewer = HeadlessRenderOptions {
        viewer_paths: vec!["sample.rs".to_owned()],
        ..HeadlessRenderOptions::default()
    };
    apply_scenario_to_live_session(&mut ctrl, HeadlessScenario::SyntaxViewer, &opts_with_viewer);
    assert!(ctrl.ui.viewer.tabs.is_empty());

    apply_scenario_to_live_session(&mut ctrl, HeadlessScenario::SyntaxViewer, &base_opts);
    assert_eq!(ctrl.ui.viewer.tabs.len(), 1);

    // 3. PortsPill injects sample ports when absent, but preserves existing live ports.
    let mut ctrl_ports = make_test_controller();
    seed_single_pane_workspace(&mut ctrl_ports);
    let key_t3 = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");
    apply_scenario_to_live_session(&mut ctrl_ports, HeadlessScenario::PortsPill, &base_opts);
    assert_eq!(
        ctrl_ports
            .workspace
            .block(&key_t3)
            .expect("block")
            .listening_ports
            .len(),
        2
    );

    let mut ctrl_live_ports = make_test_controller();
    seed_single_pane_workspace(&mut ctrl_live_ports);
    ctrl_live_ports
        .workspace
        .block_mut(&key_t3)
        .expect("block")
        .apply_ports_response(rex_proto::types::TerminalPortsResponse {
            status: rex_proto::types::PortScanStatus::Ok,
            ports: vec![rex_proto::types::PortEntry {
                port: 9999,
                address: "127.0.0.1".into(),
                protocol: "tcp".into(),
                first_seen_at: "2026-11-27T00:00:00Z".into(),
                pid: Some(1234),
            }],
            processes: Vec::new(),
            truncated: Some(false),
            id: None,
        });
    apply_scenario_to_live_session(
        &mut ctrl_live_ports,
        HeadlessScenario::PortsPill,
        &base_opts,
    );
    let live_ports = &ctrl_live_ports
        .workspace
        .block(&key_t3)
        .expect("block")
        .listening_ports;
    assert_eq!(live_ports.len(), 1);
    assert_eq!(live_ports[0].port, 9999);

    // 4. FloatingPr injects floating layer when absent, and leaves existing floating layers untouched.
    let mut ctrl_float = make_test_controller();
    seed_single_pane_workspace(&mut ctrl_float);
    apply_scenario_to_live_session(&mut ctrl_float, HeadlessScenario::FloatingPr, &base_opts);
    let sk_t3 = SessionKey::new(LOCAL_HOST_ID, "session:t3");
    let layers_after = &ctrl_float
        .workspace
        .session(&sk_t3)
        .unwrap()
        .view
        .as_ref()
        .unwrap()
        .windows[0]
        .layers;
    assert!(
        layers_after
            .iter()
            .any(|l| l.kind == "floating" && l.layer_id.as_deref() == Some("layer:float-pr"))
    );

    // Existing floating layer (even with empty tiled layer) must be preserved.
    let mut ctrl_existing_float = make_test_controller();
    seed_single_pane_workspace(&mut ctrl_existing_float);
    let custom_float_view = SessionView {
        revision: 2,
        session_id: Some("session:t3".into()),
        label: "T3 Code".into(),
        active_window_id: Some("window:1".into()),
        windows: vec![WindowView {
            window_id: Some("window:1".into()),
            label: "Float Only".into(),
            focused_block_id: Some("block:1".into()),
            zoomed_block_id: None,
            tiled_zoomed_block_id: None,
            layers: vec![
                LayerView {
                    layer_id: Some("layer:tiled".into()),
                    kind: "tiled".into(),
                    bounds: [0.0, 0.0, 1.0, 1.0],
                    state: None,
                    layout: None,
                    blocks: Vec::new(),
                },
                LayerView {
                    layer_id: Some("layer:user-float".into()),
                    kind: "floating".into(),
                    bounds: [0.1, 0.1, 0.5, 0.5],
                    state: Some("normal".into()),
                    layout: None,
                    blocks: vec![BlockView {
                        block_id: Some("block:1".into()),
                        label: "user float".into(),
                        creator_name: None,
                        flavor: Some("com.superlogical.terminal.shell".into()),
                        host_id: Some("host:local".into()),
                        owner: Some("client:1".into()),
                    }],
                },
            ],
        }],
    };
    ctrl_existing_float.process_runtime_event(
        RuntimeEvent::SessionViewUpdated {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:t3".into(),
            view: Box::new(custom_float_view),
            seq: 2,
        },
        Instant::now(),
    );
    apply_scenario_to_live_session(
        &mut ctrl_existing_float,
        HeadlessScenario::FloatingPr,
        &base_opts,
    );
    let preserved_layers = &ctrl_existing_float
        .workspace
        .session(&sk_t3)
        .unwrap()
        .view
        .as_ref()
        .unwrap()
        .windows[0]
        .layers;
    assert_eq!(preserved_layers.len(), 2);
    assert_eq!(
        preserved_layers[1].layer_id.as_deref(),
        Some("layer:user-float")
    );

    // 5. activate_dnd_split_overlay uses live session ("session:t3") instead of hardcoded "session:work".
    let _gpu_guard = crate::lock_headless_gpu_test();
    let mut renderer = GpuRenderer::new_headless(GpuConfig::default()).expect("headless Vulkan");
    ctrl.set_viewport(1280, 720, 1.0);
    let _ = ctrl
        .render_frame(&mut renderer, Instant::now())
        .expect("frame");
    activate_dnd_split_overlay(&mut ctrl);
    assert!(matches!(
        &ctrl.active_drag,
        Some(ActiveDrag::PaneMove { session_id, block_id, .. })
            if session_id == "session:t3" && block_id == "block:1"
    ));

    // 6. Scenario startup theme resolution honors SidebarDark / SidebarLight.
    let settings = UserSettings::default();
    let dark_theme =
        resolve_scenario_startup_theme(&settings, Some(HeadlessScenario::SidebarDark), false);
    let light_theme =
        resolve_scenario_startup_theme(&settings, Some(HeadlessScenario::SidebarLight), false);
    assert_eq!(dark_theme.palette.mode, omarchy_theme::AppearanceMode::Dark);
    assert_eq!(
        light_theme.palette.mode,
        omarchy_theme::AppearanceMode::Light
    );
}

#[test]
fn gap01_read_only_controller_still_queries_active_window_ports() {
    let _gpu_guard = crate::lock_headless_gpu_test();
    let mut renderer =
        GpuRenderer::new_headless(GpuConfig::default()).expect("headless Vulkan renderer");
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    ctrl.read_only = true;
    ctrl.set_viewport(1280, 720, 1.0);

    // 1. Direct query_active_window_ports dispatches HostCommand::ListPorts even when read_only=true.
    let keys = ctrl.query_active_window_ports();
    assert_eq!(keys.len(), 1);
    let cmds = ctrl.drain_pending_commands();
    assert!(
        cmds.iter()
            .any(|c| matches!(c, HostCommand::ListPorts { key } if key == &keys[0])),
        "expected read-only ListPorts command, got {cmds:?}"
    );

    // 2. ensure_visible_pane_streams (invoked during render_frame) also queries unqueried blocks when read_only=true.
    ctrl.ports_queried_blocks.clear();
    let _ = ctrl
        .render_frame(&mut renderer, Instant::now())
        .expect("render frame");
    let render_cmds = ctrl.drain_pending_commands();
    assert!(
        render_cmds
            .iter()
            .any(|c| matches!(c, HostCommand::ListPorts { key } if key == &keys[0])),
        "expected ensure_visible_pane_streams to emit ListPorts when read_only=true"
    );

    // 3. PortsListed updates workspace listening_ports cleanly.
    ctrl.process_runtime_event(
        RuntimeEvent::PortsListed {
            key: keys[0].clone(),
            result: Ok(rex_proto::types::TerminalPortsResponse {
                status: rex_proto::types::PortScanStatus::Ok,
                ports: vec![rex_proto::types::PortEntry {
                    port: 51685,
                    protocol: "tcp".into(),
                    address: "127.0.0.1".into(),
                    first_seen_at: "2026-11-27T00:00:00Z".into(),
                    pid: Some(1234),
                }],
                processes: Vec::new(),
                truncated: Some(false),
                id: None,
            }),
        },
        Instant::now(),
    );
    let blk = ctrl.workspace.block(&keys[0]).expect("block");
    assert_eq!(blk.listening_ports.len(), 1);
    assert_eq!(blk.listening_ports[0].port, 51685);

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

fn build_two_page_pdf_bytes_for_test() -> Vec<u8> {
    let mut pdf = Vec::new();
    pdf.extend_from_slice(b"%PDF-1.4\n");
    let mut offsets = Vec::new();
    let obj1 = b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n";
    let obj2 = b"2 0 obj\n<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>\nendobj\n";
    let obj3 =
        b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R >>\nendobj\n";
    let stream1 = b"0 0 1 rg 0 0 200 200 re f\n";
    let obj4 = format!(
        "4 0 obj\n<< /Length {} >>\nstream\n{}endstream\nendobj\n",
        stream1.len(),
        std::str::from_utf8(stream1).unwrap()
    );
    let obj5 =
        b"5 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 6 0 R >>\nendobj\n";
    let stream2 = b"1 0 0 rg 0 0 200 200 re f\n";
    let obj6 = format!(
        "6 0 obj\n<< /Length {} >>\nstream\n{}endstream\nendobj\n",
        stream2.len(),
        std::str::from_utf8(stream2).unwrap()
    );
    let objects: [&[u8]; 6] = [obj1, obj2, obj3, obj4.as_bytes(), obj5, obj6.as_bytes()];
    for o in objects {
        offsets.push(pdf.len());
        pdf.extend_from_slice(o);
    }
    let xref_pos = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    for off in offsets {
        pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

#[test]
fn gap02_pdf_loaded_honors_pending_tab_jump_lines_for_remote_and_loading_local() {
    if std::process::Command::new("pdfinfo")
        .arg("-v")
        .output()
        .is_err()
        || std::process::Command::new("pdftoppm")
            .arg("-v")
            .output()
            .is_err()
    {
        return;
    }

    let tmp_dir = std::env::temp_dir().join(format!("omarex-gap02-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let pdf_path = tmp_dir.join("two_page.pdf");
    std::fs::write(&pdf_path, build_two_page_pdf_bytes_for_test()).unwrap();

    // 1. Repeated open_local_path_in_viewer while ViewerContent::Loading queues page 2 jump.
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let tab1 = ctrl.open_local_path_in_viewer(
        pdf_path.clone(),
        "two_page.pdf".to_owned(),
        Some(1),
        None,
        Some(TargetKind::Pdf),
    );
    let tab1_again = ctrl.open_local_path_in_viewer(
        pdf_path.clone(),
        "two_page.pdf".to_owned(),
        Some(2),
        None,
        Some(TargetKind::Pdf),
    );
    assert_eq!(tab1, tab1_again);
    assert_eq!(ctrl.pending_tab_jump_lines.get(&tab1), Some(&(2, None)));

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        ctrl.poll_media_events();
        if let Some(tab) = ctrl.ui.viewer.tab_by_id(tab1)
            && let ViewerContent::Pdf(pdf) = &tab.content
            && !pdf.loading_page
            && pdf.current_page == 2
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    match &ctrl.ui.viewer.tab_by_id(tab1).unwrap().content {
        ViewerContent::Pdf(pdf) => {
            assert_eq!(pdf.page_count, 2);
            assert_eq!(pdf.current_page, 2);
            assert!(!pdf.loading_page);
        }
        other => panic!("expected Pdf on page 2, got {other:?}"),
    }

    // 2. Remote PDF path (`open_remote_path_in_viewer` + download completion + PdfLoaded) jumps to page 2.
    let key = BlockKey::new(LOCAL_HOST_ID, "session:t3", "block:1");
    let remote_tab = ctrl.open_remote_path_in_viewer(
        &key,
        "/remote/two_page.pdf",
        TargetKind::Pdf,
        Some(2),
        None,
    );
    let dl_tid = ctrl
        .drain_pending_commands()
        .into_iter()
        .find_map(|c| match c {
            HostCommand::DownloadFile { transfer_id, .. } => Some(transfer_id),
            _ => None,
        })
        .expect("DownloadFile command");
    let pending = ctrl.pending_viewer_downloads.remove(&dl_tid).unwrap();
    ctrl.apply_viewer_download_result(
        LOCAL_HOST_ID.to_owned(),
        pending,
        Ok(rex_proto::DownloadResult::Downloaded {
            remote_path: "/remote/two_page.pdf".to_owned(),
            name: "two_page.pdf".to_owned(),
            local_path: pdf_path.clone(),
            bytes_downloaded: std::fs::metadata(&pdf_path).unwrap().len(),
            modified_at: String::new(),
            version: None,
        }),
    );

    let deadline2 = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline2 {
        ctrl.poll_media_events();
        if let Some(tab) = ctrl.ui.viewer.tab_by_id(remote_tab)
            && let ViewerContent::Pdf(pdf) = &tab.content
            && !pdf.loading_page
            && pdf.current_page == 2
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    match &ctrl.ui.viewer.tab_by_id(remote_tab).unwrap().content {
        ViewerContent::Pdf(pdf) => {
            assert_eq!(pdf.page_count, 2);
            assert_eq!(pdf.current_page, 2);
            assert!(!pdf.loading_page);
        }
        other => panic!("expected remote Pdf on page 2, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn gap03_rex_control_apc_theme_handles_multibyte_utf8_background_without_panic() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let apc_payload = b"\x1b_rex1;theme {\"background\":\"\xe2\x82\xac\xe2\x82\xac\"}\x1b\\";
    ctrl.process_runtime_event(
        RuntimeEvent::DataStream(DataStreamEvent {
            host_id: LOCAL_HOST_ID.into(),
            session_id: "session:t3".into(),
            block_id: "block:1".into(),
            kind: DataStreamEventKind::LiveVtChunk {
                bytes: bytes::Bytes::from_static(apc_payload),
                bytes_received: apc_payload.len() as u64,
            },
        }),
        Instant::now(),
    );
    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn gap05_multi_window_sync_settings_font_and_adaptive_rounding() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();

    let mut ctx_a = ctrl.create_window_context(
        1024,
        768,
        1.0,
        Some(SessionKey::new(LOCAL_HOST_ID, "session:t3")),
        Some("block:1".to_owned()),
    );
    let mut ctx_b = ctrl.create_window_context(
        1024,
        768,
        1.0,
        Some(SessionKey::new(LOCAL_HOST_ID, "session:t3")),
        Some("block:1".to_owned()),
    );

    // Open Settings modal on ctx_b and clear initial flags.
    ctrl.with_window_context(&mut ctx_b, |c| {
        c.dispatch_ui_action(UiAction::OpenModal(ModalKind::Settings), now);
        c.needs_redraw = false;
        c.force_immediate_resize = false;
    });
    assert!(!ctx_b.needs_redraw());
    assert!(!ctx_b.force_immediate_resize);

    // 1. apply_setting_change on ctx_a propagates layout/pane_options/modal snapshot/resize/redraw to ctx_b.
    ctrl.with_window_context(&mut ctx_a, |c| {
        c.dispatch_ui_action(
            UiAction::UpdateSetting(SettingChange::ToolbarPosition(ToolbarPosition::Right)),
            now,
        );
        c.dispatch_ui_action(
            UiAction::UpdateSetting(SettingChange::ToggleAdaptiveRounding),
            now,
        );
    });
    ctrl.sync_window_context(&mut ctx_b);
    assert_eq!(ctx_b.ui.layout_cfg.toolbar_position, ToolbarPosition::Right);
    assert!(ctx_b.needs_redraw());
    assert!(ctx_b.force_immediate_resize);
    match ctx_b.ui.modal.as_ref().map(|m| &m.content) {
        Some(ui_chrome::ModalContent::Settings { snapshot, .. }) => {
            assert_eq!(snapshot.toolbar_position, ToolbarPosition::Right);
            assert!(!snapshot.adaptive_rounding);
        }
        other => panic!("expected refreshed Settings modal on ctx_b, got {other:?}"),
    }

    // 2. FontIncrease / FontDecrease / FontReset set force_immediate_resize + needs_redraw + refresh Settings + sync sibling.
    let base_pt = ctrl.settings.font_size_pt;
    ctrl.with_window_context(&mut ctx_b, |c| {
        c.needs_redraw = false;
        c.force_immediate_resize = false;
    });
    ctrl.with_window_context(&mut ctx_a, |c| {
        c.needs_redraw = false;
        c.force_immediate_resize = false;
        c.execute_client_command(LOCAL_HOST_ID, ClientCommand::FontIncrease, now);
        assert!(c.force_immediate_resize);
        assert!(c.needs_redraw);
        assert!(c.settings.font_size_pt > base_pt);
    });
    ctrl.sync_window_context(&mut ctx_b);
    assert!(ctx_b.force_immediate_resize);
    assert!(ctx_b.needs_redraw());
    match ctx_b.ui.modal.as_ref().map(|m| &m.content) {
        Some(ui_chrome::ModalContent::Settings { snapshot, .. }) => {
            assert!((snapshot.font_size_pt - ctrl.settings.font_size_pt).abs() < 0.01);
        }
        other => panic!("expected updated font size on ctx_b Settings modal, got {other:?}"),
    }

    ctrl.with_window_context(&mut ctx_b, |c| {
        c.needs_redraw = false;
        c.force_immediate_resize = false;
    });
    ctrl.with_window_context(&mut ctx_a, |c| {
        c.execute_client_command(LOCAL_HOST_ID, ClientCommand::FontDecrease, now);
        c.execute_client_command(LOCAL_HOST_ID, ClientCommand::FontReset, now);
    });
    ctrl.sync_window_context(&mut ctx_b);
    assert!((ctrl.settings.font_size_pt - UserSettings::default().font_size_pt).abs() < 0.01);
    assert!(ctx_b.force_immediate_resize);
    assert!(ctx_b.needs_redraw());

    // 3. apply_theme_palette respects adaptive_rounding == false and syncs sibling windows.
    ctrl.theme.rounding = 12.0;
    assert!(!ctrl.settings.adaptive_rounding);
    ctrl.with_window_context(&mut ctx_b, |c| {
        c.needs_redraw = false;
    });
    ctrl.with_window_context(&mut ctx_a, |c| {
        c.apply_theme_palette(omarchy_theme::ThemePalette::default_light());
        assert_eq!(c.ui.layout_cfg.hyprland_rounding, None);
    });
    ctrl.sync_window_context(&mut ctx_b);
    assert_eq!(ctx_b.ui.layout_cfg.hyprland_rounding, None);
    assert!(ctx_b.needs_redraw());

    // When adaptive_rounding is re-enabled, theme palette applies rounding to both windows.
    ctrl.with_window_context(&mut ctx_a, |c| {
        c.dispatch_ui_action(
            UiAction::UpdateSetting(SettingChange::ToggleAdaptiveRounding),
            now,
        );
        c.apply_theme_palette(omarchy_theme::ThemePalette::default_dark());
        assert_eq!(c.ui.layout_cfg.hyprland_rounding, Some(c.theme.rounding));
    });
    ctrl.sync_window_context(&mut ctx_b);
    assert_eq!(
        ctx_b.ui.layout_cfg.hyprland_rounding,
        Some(ctrl.theme.rounding)
    );

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn theme_changed_preserves_stale_sibling_settings_sync() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();

    let mut ctx_a = ctrl.create_window_context(
        1024,
        768,
        1.0,
        Some(SessionKey::new(LOCAL_HOST_ID, "session:t3")),
        Some("block:1".to_owned()),
    );
    let mut ctx_b = ctrl.create_window_context(
        1024,
        768,
        1.0,
        Some(SessionKey::new(LOCAL_HOST_ID, "session:t3")),
        Some("block:1".to_owned()),
    );

    ctrl.with_window_context(&mut ctx_b, |c| {
        c.dispatch_ui_action(UiAction::OpenModal(ModalKind::Settings), now);
    });
    let initial_highlight = ctx_b.ui.pane_options.highlight_active_border;

    // Simulate a non-resize shared settings mutation in Window A (`mark_shared_settings_changed(false)`).
    ctrl.with_window_context(&mut ctx_a, |c| {
        let next_highlight = !c.settings.highlight_active_border;
        c.settings.highlight_active_border = next_highlight;
        c.ui.pane_options.highlight_active_border = next_highlight;
        c.mark_shared_settings_changed(false);
    });

    // Deliver RuntimeEvent::ThemeChanged before sync_window_context runs on Window B.
    ctrl.process_runtime_event_multi(
        RuntimeEvent::ThemeChanged(omarchy_theme::ThemeEvent::PaletteChanged(
            omarchy_theme::ThemePalette::default_light(),
        )),
        &mut [&mut ctx_a, &mut ctx_b],
        Some(0),
        now,
    );

    // Window B must still sync the pending non-resize setting & refresh its Settings modal snapshot.
    ctrl.sync_window_context(&mut ctx_b);
    assert_eq!(
        ctx_b.ui.pane_options.highlight_active_border,
        !initial_highlight
    );
    match ctx_b.ui.modal.as_ref().map(|m| &m.content) {
        Some(ui_chrome::ModalContent::Settings { snapshot, .. }) => {
            assert_eq!(snapshot.highlight_active_border, !initial_highlight);
        }
        other => panic!("expected refreshed Settings modal on ctx_b, got {other:?}"),
    }

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

#[test]
fn gap06_seed_scenario_state_skips_synthetic_tab_when_explicit_viewer_paths_given() {
    let mut ctrl_with_explicit = make_test_controller();
    super::render::seed_scenario_state(
        &mut ctrl_with_explicit,
        HeadlessScenario::SyntaxViewer,
        true,
    );
    assert!(
        ctrl_with_explicit.ui.viewer.tabs.is_empty(),
        "expected no synthetic viewer tab when explicit viewer paths are provided"
    );

    let mut ctrl_md_explicit = make_test_controller();
    super::render::seed_scenario_state(
        &mut ctrl_md_explicit,
        HeadlessScenario::MarkdownViewer,
        true,
    );
    assert!(ctrl_md_explicit.ui.viewer.tabs.is_empty());

    let mut ctrl_default = make_test_controller();
    super::render::seed_scenario_state(&mut ctrl_default, HeadlessScenario::SyntaxViewer, false);
    assert_eq!(ctrl_default.ui.viewer.tabs.len(), 1);
    assert_eq!(ctrl_default.ui.viewer.tabs[0].path, "src/multi_window.rs");

    for c in [ctrl_with_explicit, ctrl_md_explicit, ctrl_default] {
        if let Some(parent) = c.settings_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}

#[test]
fn gap07_sidebar_dark_preserves_custom_dark_theme_and_overrides_light() {
    let custom_dark_toml = r##"
mode = "dark"
accent = "#ff5500"
selection = "#3a0000"
muted = "#aa6666"
background = "#3a0000"
dark_background = "#2d0000"
darker_background = "#1a0000"
lighter_background = "#4d0000"
foreground = "#ffffff"
dark_foreground = "#cccccc"
light_foreground = "#ffffff"
bright_foreground = "#ffffff"
red = "#ff3333"
yellow = "#ffcc00"
green = "#33cc33"
cyan = "#33cccc"
blue = "#3366ff"
magenta = "#cc33ff"
bright_red = "#ff6666"
bright_yellow = "#ffdd55"
bright_green = "#66dd66"
bright_cyan = "#66dddd"
bright_blue = "#6699ff"
bright_magenta = "#dd66ff"
"##;
    let tmp_dir = std::env::temp_dir().join(format!("omarex-gap07-theme-{}", std::process::id()));
    let theme_dir = tmp_dir.join("custom-red");
    std::fs::create_dir_all(&theme_dir).unwrap();
    std::fs::write(theme_dir.join("colors.toml"), custom_dark_toml).unwrap();

    let pal = omarchy_theme::load_theme_by_name_in("custom-red", Some(&tmp_dir), None, None)
        .expect("load custom dark theme");
    assert!(pal.mode.is_dark());

    // Verify resolve_scenario_startup_theme_in preserves custom dark theme when is_dark == true,
    // and falls back to dark when base_theme.is_dark == false.
    let mut settings = UserSettings {
        theme_name: Some("custom-red".to_owned()),
        ..Default::default()
    };
    let dark_res = super::render::resolve_scenario_startup_theme_in(
        &settings,
        Some(HeadlessScenario::SidebarDark),
        false,
        Some(&tmp_dir),
        None,
        None,
    );
    assert!(dark_res.is_dark);
    assert_eq!(
        dark_res.palette.terminal.background,
        pal.terminal.background
    );

    settings.theme_name = Some("omarchy-default-light".to_owned());
    let forced_dark = super::render::resolve_scenario_startup_theme_in(
        &settings,
        Some(HeadlessScenario::SidebarDark),
        false,
        Some(&tmp_dir),
        None,
        None,
    );
    assert!(
        forced_dark.is_dark,
        "SidebarDark must override light base theme with dark fallback"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[test]
fn gap08_parse_rfc3339_unix_secs_handles_multibyte_utf8_timezone_without_panic() {
    assert_eq!(
        super::events::parse_rfc3339_unix_secs("2026-01-01T00:00:00+€€"),
        None
    );
    assert_eq!(
        super::events::parse_rfc3339_unix_secs("€€€€-€-€T00:00:00Z"),
        None
    );
    assert_eq!(
        super::events::parse_rfc3339_unix_secs("2026-01-01T00:00:00+xx:yy"),
        None
    );
    assert_eq!(
        super::events::parse_rfc3339_unix_secs("2026-01-01T00:00:00Z"),
        Some(1_767_225_600)
    );
    assert_eq!(
        super::events::parse_rfc3339_unix_secs("2026-01-01T02:00:00+02:00"),
        Some(1_767_225_600)
    );
}

#[test]
fn sidebar_resize_drag_commit_syncs_sibling_window_contexts() {
    let mut ctrl = make_test_controller();
    seed_single_pane_workspace(&mut ctrl);
    let now = Instant::now();

    let mut ctx_a = ctrl.create_window_context(
        1024,
        768,
        1.0,
        Some(SessionKey::new(LOCAL_HOST_ID, "session:t3")),
        Some("block:1".to_owned()),
    );
    let mut ctx_b = ctrl.create_window_context(
        1024,
        768,
        1.0,
        Some(SessionKey::new(LOCAL_HOST_ID, "session:t3")),
        Some("block:1".to_owned()),
    );

    ctrl.with_window_context(&mut ctx_b, |c| {
        c.force_immediate_resize = false;
        c.needs_redraw = false;
    });

    ctrl.with_window_context(&mut ctx_a, |c| {
        c.dispatch_ui_action(UiAction::BeginSidebarResize, now);
        c.handle_pointer_move(320.0, 200.0, Modifiers::NONE, now);
        c.handle_pointer_up(320.0, 200.0, PointerButton::Left, Modifiers::NONE);
        assert!((c.settings.sidebar_width_px - 320.0).abs() < 0.01);
        assert!((c.ui.layout_cfg.sidebar_width - 320.0).abs() < 0.01);
    });

    ctrl.sync_window_context(&mut ctx_b);
    assert!(
        (ctx_b.ui.layout_cfg.sidebar_width - 320.0).abs() < 0.01,
        "expected sibling window sidebar width to sync to 320.0, got {}",
        ctx_b.ui.layout_cfg.sidebar_width
    );
    assert!(ctx_b.force_immediate_resize);
    assert!(ctx_b.needs_redraw());

    if let Some(parent) = ctrl.settings_path.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}
