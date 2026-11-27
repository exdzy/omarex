//! Dual client-action dispatcher (`client.action.client_actions` and pushed
//! `client_perform_requested`) plus multi-server `client_call_requested` proxy routing.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::BridgeError;
use crate::prompts::ClientReplyPayload;

/// Maximum server-enforced timeout for answering `client_call_requested` (`protocol-spec.md §6.4`).
pub const CLIENT_CALL_PROXY_TIMEOUT: Duration = Duration::from_secs(10);

/// One queued action returned inline inside a `client.action` RPC response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientActionItem {
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
}

/// Wire response payload for `client.action` (Rex API rev 24).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ClientActionResponse {
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(
        default,
        deserialize_with = "rex_proto::types::deserialize_null_default"
    )]
    pub client_actions: Vec<ClientActionItem>,
    /// Request ID of the preceding ordered request (e.g. `"key-41"`), if any.
    #[serde(default)]
    pub ordered_after: Option<String>,
}

/// Server-verified requester principal (used for authorization; never trust `requester.info`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClientPrincipal {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub transport: String,
    #[serde(default)]
    pub display: String,
    #[serde(default)]
    pub remote_addr: Option<String>,
    #[serde(default)]
    pub uid: Option<u32>,
    #[serde(default)]
    pub gid: Option<u32>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub sid: Option<String>,
    /// Tailscale user ID owning the device (`kind == "tailscale"`).
    #[serde(default)]
    pub user_id: Option<String>,
    /// Tailscale login name for display (`kind == "tailscale"`).
    #[serde(default)]
    pub login_name: Option<String>,
    /// Tailscale device name (`kind == "tailscale"`).
    #[serde(default)]
    pub node: Option<String>,
    /// Tailscale device tags; non-empty indicates a machine/CI principal rather than a person.
    #[serde(
        default,
        deserialize_with = "rex_proto::types::deserialize_null_default"
    )]
    pub tags: Vec<String>,
    /// Route taken by the client (`"tailscale"`, `"tailnet"`, `"login-server"`, etc.).
    #[serde(
        default,
        deserialize_with = "rex_proto::types::deserialize_null_default"
    )]
    pub path: Vec<String>,
}

impl ClientPrincipal {
    /// Returns `true` when this Tailscale principal carries device tags (machine/CI runner,
    /// not an interactive user).
    pub fn is_tagged_machine(&self) -> bool {
        !self.tags.is_empty()
    }
}

/// Self-reported client info (display only — never used for authorization).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClientInfo {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

/// Server-populated requester descriptor in `client_perform_requested`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClientRequester {
    pub client_id: String,
    #[serde(default)]
    pub info: Option<ClientInfo>,
    #[serde(default)]
    pub principal: ClientPrincipal,
}

/// Wire `client_perform_requested` event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientPerformRequest {
    pub call_id: String,
    pub action: String,
    #[serde(default)]
    pub args: Option<serde_json::Value>,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub block_id: Option<String>,
    pub requester: ClientRequester,
}

/// Wire `client_call_requested` event payload (multi-server Lua proxy).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientCallRequest {
    pub call_id: String,
    pub server: String,
    pub method: String,
    #[serde(default)]
    pub args: Option<serde_json::Value>,
}

/// Split direction for `pane.split.*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Left,
    Right,
    Up,
    Down,
    Auto,
}

/// Spatial direction for `pane.focus.*` and `pane.move.*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneDirection {
    Left,
    Right,
    Up,
    Down,
}

/// Normalized native client command executed by `omarex`'s workspace/UI layer.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientCommand {
    SelectSession {
        session_id: Option<String>,
        window_id: Option<String>,
        block_id: Option<String>,
    },
    Paste {
        session_id: Option<String>,
        block_id: Option<String>,
    },
    CopySelection,
    TogglePalette,
    OpenSessionSwitcher,
    OpenGoTo,
    OpenActivity,
    OpenFind,
    FindNext,
    FindPrevious,
    OpenSettings,
    OpenConnectIos,
    OpenServerSettings,
    OpenDiagnostics,
    ToggleSidebar,
    ToggleTabStyle,
    FocusSidebarFilter,
    RevealToolbar,
    RevealPaneHeader,
    DismissNotice,
    RefreshSessions,
    NavigateBack,
    NavigateForward,
    NewSession {
        on_host: bool,
    },
    NewWindow,
    NewTab,
    CloseCurrent,
    CloseTab,
    ClosePane,
    UndoClose,
    NextTab,
    PrevTab,
    GotoTab {
        /// 1-based tab index (`-1` means last tab per Rex spec).
        index: i32,
    },
    MoveTab {
        forward: bool,
    },
    MarkTabUnread,
    ToggleTabMute,
    SplitPane(SplitDirection),
    FocusPane(PaneDirection),
    FocusNextPane,
    FocusPrevPane,
    MovePane(PaneDirection),
    ZoomPane,
    FloatPane,
    MinimizeFloating,
    MinimizeAllFloating,
    BalancePanes,
    EnterCopyMode,
    QuickLook,
    ViewerOpen {
        path: String,
        line: Option<u32>,
        col: Option<u32>,
        remote_host: Option<String>,
    },
    GoToDirectory,
    InsertPath,
    PromptJump {
        next: bool,
    },
    FontIncrease,
    FontDecrease,
    FontReset,
    ChangeTheme {
        name: Option<String>,
    },
    ReloadConfig,
    FitToDevice,
    EnterKeyMode {
        mode: String,
    },
    ExitKeyMode,
    ExitAllKeyModes,
    /// Server-side Lua or block action (invoke via `client.action`).
    ServerAction {
        action: String,
        args: Option<serde_json::Value>,
    },
}

/// Authorizes a `client_perform_requested` call using `requester.principal` ONLY
/// (never trusts self-reported `requester.info`).
pub fn authorize_principal(
    principal: &ClientPrincipal,
    local_uid: Option<u32>,
) -> Result<(), &'static str> {
    match principal.kind.as_str() {
        "peer" => {
            if let (Some(expected), Some(actual)) = (local_uid, principal.uid)
                && actual != expected
            {
                return Err("peer UID mismatch");
            }
            Ok(())
        }
        "tailscale" => {
            if principal.is_tagged_machine() {
                return Err("tagged Tailscale machine principals may not drive remote actions");
            }
            Ok(())
        }
        "windows_peer" => Ok(()),
        "anonymous" | "" => Err("anonymous clients may not drive remote actions"),
        _ => Err("unrecognized requester principal kind"),
    }
}

/// Parses a client action name + JSON args into a strongly-typed [`ClientCommand`].
pub fn parse_client_command(
    action: &str,
    args: Option<&serde_json::Value>,
    fallback_session_id: Option<&str>,
    fallback_block_id: Option<&str>,
) -> Result<ClientCommand, BridgeError> {
    let get_str = |key: &str| -> Option<String> {
        args.and_then(|v| v.get(key))
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned)
    };

    let cmd = match action {
        "session.select" => ClientCommand::SelectSession {
            session_id: get_str("session_id")
                .or_else(|| fallback_session_id.map(ToOwned::to_owned)),
            window_id: get_str("window_id"),
            block_id: get_str("block_id").or_else(|| fallback_block_id.map(ToOwned::to_owned)),
        },
        "pane.paste" => ClientCommand::Paste {
            session_id: get_str("session_id")
                .or_else(|| fallback_session_id.map(ToOwned::to_owned)),
            block_id: get_str("block_id").or_else(|| fallback_block_id.map(ToOwned::to_owned)),
        },
        "pane.copy" => ClientCommand::CopySelection,
        "client.palette.toggle" | "client.palette.open" | "palette.open" => {
            ClientCommand::TogglePalette
        }
        "session.switch" | "session.switcher" => ClientCommand::OpenSessionSwitcher,
        "pane.go_to" => ClientCommand::OpenGoTo,
        "client.sidebar.activity.toggle" | "activity.open" => ClientCommand::OpenActivity,
        "client.find.open" | "find.open" => ClientCommand::OpenFind,
        "client.find.next" => ClientCommand::FindNext,
        "client.find.previous" => ClientCommand::FindPrevious,
        "client.settings" => ClientCommand::OpenSettings,
        "client.connect_ios" | "client.ios.connect" | "client.pair.ios" => {
            ClientCommand::OpenConnectIos
        }
        "client.server_settings" | "client.server.settings" => ClientCommand::OpenServerSettings,
        "client.diagnostics" => ClientCommand::OpenDiagnostics,
        "client.sidebar.toggle" => ClientCommand::ToggleSidebar,
        "client.tab_style.toggle" => ClientCommand::ToggleTabStyle,
        "client.sidebar.filter.focus" => ClientCommand::FocusSidebarFilter,
        "client.toolbar.reveal" => ClientCommand::RevealToolbar,
        "client.pane_header.reveal" => ClientCommand::RevealPaneHeader,
        "client.notice.dismiss" => ClientCommand::DismissNotice,
        "session.refresh" => ClientCommand::RefreshSessions,
        "client.navigate.back" => ClientCommand::NavigateBack,
        "client.navigate.forward" => ClientCommand::NavigateForward,
        "session.new" => ClientCommand::NewSession { on_host: false },
        "session.new.on_host" => ClientCommand::NewSession { on_host: true },
        "client.window.new" => ClientCommand::NewWindow,
        "client.tab.new" => ClientCommand::NewTab,
        "client.close" => ClientCommand::CloseCurrent,
        "client.tab.close" => ClientCommand::CloseTab,
        "pane.close" => ClientCommand::ClosePane,
        "session.undo_close" => ClientCommand::UndoClose,
        "client.tab.next" => ClientCommand::NextTab,
        "client.tab.previous" => ClientCommand::PrevTab,
        "client.tab.goto" => {
            let index = args
                .and_then(|v| v.get("index").or_else(|| v.get("tab")))
                .and_then(|v| v.as_i64())
                .unwrap_or(1) as i32;
            ClientCommand::GotoTab { index }
        }
        "client.tab.move.forward" => ClientCommand::MoveTab { forward: true },
        "client.tab.move.backward" => ClientCommand::MoveTab { forward: false },
        "client.tab.unread" => ClientCommand::MarkTabUnread,
        "client.tab.mute" => ClientCommand::ToggleTabMute,
        "pane.split" => {
            let dir = match get_str("direction").as_deref() {
                Some("left") => SplitDirection::Left,
                Some("up") => SplitDirection::Up,
                Some("down") | Some("vertical") => SplitDirection::Down,
                Some("auto") => SplitDirection::Auto,
                _ => SplitDirection::Right,
            };
            ClientCommand::SplitPane(dir)
        }
        "pane.split.right" => ClientCommand::SplitPane(SplitDirection::Right),
        "pane.split.left" => ClientCommand::SplitPane(SplitDirection::Left),
        "pane.split.down" => ClientCommand::SplitPane(SplitDirection::Down),
        "pane.split.up" => ClientCommand::SplitPane(SplitDirection::Up),
        "pane.split.auto" => ClientCommand::SplitPane(SplitDirection::Auto),
        "pane.focus" => {
            let dir = match get_str("direction").as_deref() {
                Some("left") => PaneDirection::Left,
                Some("up") => PaneDirection::Up,
                Some("down") => PaneDirection::Down,
                _ => PaneDirection::Right,
            };
            ClientCommand::FocusPane(dir)
        }
        "pane.focus.left" => ClientCommand::FocusPane(PaneDirection::Left),
        "pane.focus.right" => ClientCommand::FocusPane(PaneDirection::Right),
        "pane.focus.up" => ClientCommand::FocusPane(PaneDirection::Up),
        "pane.focus.down" => ClientCommand::FocusPane(PaneDirection::Down),
        "pane.focus_next" => ClientCommand::FocusNextPane,
        "pane.focus_previous" => ClientCommand::FocusPrevPane,
        "pane.move.left" => ClientCommand::MovePane(PaneDirection::Left),
        "pane.move.right" => ClientCommand::MovePane(PaneDirection::Right),
        "pane.move.up" => ClientCommand::MovePane(PaneDirection::Up),
        "pane.move.down" => ClientCommand::MovePane(PaneDirection::Down),
        "pane.zoom" => ClientCommand::ZoomPane,
        "pane.float" => ClientCommand::FloatPane,
        "pane.minimize_floating" => ClientCommand::MinimizeFloating,
        "pane.minimize_all_floating" => ClientCommand::MinimizeAllFloating,
        "pane.balance" => ClientCommand::BalancePanes,
        "pane.copy_mode.enter" => ClientCommand::EnterCopyMode,
        "client.quick_look" => ClientCommand::QuickLook,
        "client.viewer.open" => {
            let path = get_str("path")
                .or_else(|| get_str("file"))
                .or_else(|| get_str("url"))
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    BridgeError::UnknownAction("client.viewer.open missing path".to_owned())
                })?;
            let line = args
                .and_then(|v| v.get("line"))
                .and_then(|v| v.as_u64())
                .and_then(|n| u32::try_from(n).ok())
                .filter(|&n| n > 0);
            let col = args
                .and_then(|v| v.get("col").or_else(|| v.get("column")))
                .and_then(|v| v.as_u64())
                .and_then(|n| u32::try_from(n).ok())
                .filter(|&n| n > 0);
            let remote_host = get_str("remote_host").or_else(|| get_str("host"));
            ClientCommand::ViewerOpen {
                path,
                line,
                col,
                remote_host,
            }
        }
        "pane.go_to_directory" => ClientCommand::GoToDirectory,
        "pane.insert_path" => ClientCommand::InsertPath,
        "client.prompt.next" => ClientCommand::PromptJump { next: true },
        "client.prompt.previous" => ClientCommand::PromptJump { next: false },
        "client.font.increase" => ClientCommand::FontIncrease,
        "client.font.decrease" => ClientCommand::FontDecrease,
        "client.font.reset" => ClientCommand::FontReset,
        "client.theme.change" => ClientCommand::ChangeTheme {
            name: get_str("theme").or_else(|| get_str("name")),
        },
        "client.config.reload" => ClientCommand::ReloadConfig,
        "fit_to_device" | "t3_fit" => ClientCommand::FitToDevice,
        "client.mode.enter" => {
            let mode = get_str("mode").or_else(|| get_str("name")).ok_or_else(|| {
                BridgeError::UnknownAction("client.mode.enter missing mode".to_owned())
            })?;
            ClientCommand::EnterKeyMode { mode }
        }
        "client.mode.exit" => ClientCommand::ExitKeyMode,
        "client.mode.exit_all" => ClientCommand::ExitAllKeyModes,
        other => {
            return Err(BridgeError::UnknownAction(other.to_owned()));
        }
    };

    Ok(cmd)
}

/// Converts a `client.action` response's inline `client_actions` array into executable commands.
pub fn dispatch_queued_actions(resp: &ClientActionResponse) -> Vec<ClientCommand> {
    let mut cmds = Vec::with_capacity(resp.client_actions.len());
    for item in &resp.client_actions {
        match parse_client_command(&item.action, item.args.as_ref(), None, None) {
            Ok(cmd) => cmds.push(cmd),
            Err(err) => {
                tracing::warn!(
                    action = %item.action,
                    error = %err,
                    "ignoring unsupported queued client action"
                );
            }
        }
    }
    cmds
}

/// Authorizes and dispatches an incoming server-pushed `client_perform_requested` event,
/// returning `(Option<ClientCommand>, ClientReplyPayload)`.
pub fn handle_perform_request(
    req: &ClientPerformRequest,
    local_uid: Option<u32>,
) -> (Option<ClientCommand>, ClientReplyPayload) {
    if let Err(reason) = authorize_principal(&req.requester.principal, local_uid) {
        return (
            None,
            ClientReplyPayload::refused(req.call_id.clone(), reason),
        );
    }

    match parse_client_command(
        &req.action,
        req.args.as_ref(),
        req.session_id.as_deref(),
        req.block_id.as_deref(),
    ) {
        Ok(cmd) => (
            Some(cmd),
            ClientReplyPayload::ok_value(req.call_id.clone(), serde_json::json!({})),
        ),
        Err(_) => (
            None,
            ClientReplyPayload::not_supported(
                req.call_id.clone(),
                format!("unsupported client action `{}`", req.action),
            ),
        ),
    }
}

/// Validated multi-server RPC forwarding plan for `client_call_requested`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProxyCallPlan {
    pub call_id: String,
    pub target_server: String,
    pub method: String,
    pub args: serde_json::Value,
    pub timeout: Duration,
}

impl ProxyCallPlan {
    /// Builds the unordered `client.reply` payload when the remote server succeeds.
    pub fn reply_ok(&self, result: serde_json::Value) -> ClientReplyPayload {
        ClientReplyPayload::ok_value(self.call_id.clone(), result)
    }

    /// Builds the unordered `client.reply` payload when the remote server fails or times out.
    pub fn reply_error(&self, code: &str, message: impl Into<String>) -> ClientReplyPayload {
        ClientReplyPayload::error(self.call_id.clone(), code, message)
    }
}

/// Validates a `client_call_requested` event against the set of connected remote server IDs/labels.
pub fn plan_remote_call(
    req: ClientCallRequest,
    connected_servers: &[&str],
) -> Result<ProxyCallPlan, ClientReplyPayload> {
    if req.server.trim().is_empty() || req.method.trim().is_empty() {
        return Err(ClientReplyPayload::error(
            req.call_id,
            "bad_request",
            "missing server or method",
        ));
    }

    if !connected_servers.contains(&req.server.as_str()) {
        return Err(ClientReplyPayload::error(
            req.call_id,
            "not_found",
            format!("server `{}` is not connected", req.server),
        ));
    }

    Ok(ProxyCallPlan {
        call_id: req.call_id,
        target_server: req.server,
        method: req.method,
        args: req.args.unwrap_or_else(|| serde_json::json!({})),
        timeout: CLIENT_CALL_PROXY_TIMEOUT,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_client_viewer_open_command() {
        let args = serde_json::json!({
            "path": "/home/user/projects/omarex/README.md",
            "line": 42,
            "col": 7,
            "remote_host": "mac-studio"
        });
        let cmd = parse_client_command("client.viewer.open", Some(&args), None, None)
            .expect("client.viewer.open should parse");
        assert_eq!(
            cmd,
            ClientCommand::ViewerOpen {
                path: "/home/user/projects/omarex/README.md".to_owned(),
                line: Some(42),
                col: Some(7),
                remote_host: Some("mac-studio".to_owned()),
            }
        );

        assert!(
            parse_client_command(
                "client.viewer.open",
                Some(&serde_json::json!({})),
                None,
                None
            )
            .is_err(),
            "client.viewer.open without path must error"
        );
    }
}
