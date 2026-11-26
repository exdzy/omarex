# Omarex Forensic UX, Responsiveness & Rex-Parity Audit

> **Scope:** Read-only forensic code & live-environment audit of `omarex` (native Rust + Vulkan `wgpu` + `libghostty-vt` GUI client for Superlogical's Rex and `tr3x` on Omarchy Hyprland).
> **Split for parallel implementation:**
> - **Part 1:** Rex Architecture & UX Re-grounding (shared design truth)
> - **Part 2 (Section A):** `crates/ui-chrome/` fixes for `@chrome-ux-polisher`
> - **Part 3 (Section B):** `crates/omarex/` & `crates/gpu-renderer/` fixes for `@viewport-runtime-fixer`

---

## Executive Summary — Root Causes of All Live-Use & Screenshot Issues

Live inspection of the running `omarex` instance on Omarchy Hyprland (`hyprctl clients -j`, `hyprctl plugins list`, `rex ls`, `rex client ls`, `rex api call -s 60b77a session.view`, `rex api describe ...`), the live screenshot, `~/.config/rex/tr3x.lua`, and all Rust crates confirmed **five primary root causes** plus a catalog of dead/incomplete UI interactions:

| # | User Symptom / Screenshot Defect | Confirmed Forensic Root Cause(s) | Owner |
|---|---|---|---|
| **1** | **Viewports not responsive on Hyprland split open/close; text stuck wrapped at ~80 cols with garbled continuation lines (`nsion/,`, `g, Offline-Queue,`) and black void on right half; sidebar stays fixed width** | **(a)** `omarex` never claims `OwnMode::Take` on resize, tab switch, or window focus (`workspace.rs:390-438`, `bridge.rs:437-443, 692-713, 915-946, 2235-2260, 4234-4248`); live server had a remote macOS client (`f3e716`) connected and owning 4/26 `T3 Code` blocks, so `com.superlogical.terminal.resize` queues on the server without taking effect. **(b)** `bridge.rs:695-704` optimistically calls `term.resize()` locally *before* `RexApc::Resize` arrives and without marking `pane.dirty = true`, desyncing in-flight 80-col PTY output against a wide grid. **(c)** `pane_store.rs:794-805` never clears/syncs `blk.last_sent_resize` on `RexControlApc::Resize` (nor on `HostError`), so `workspace.rs:722` permanently suppresses resizing back to a previous dimension after a split/unsplit or remote resize! **(d)** `workspace.rs:198-200` never clears `entry.owner` back to `None`. **(e)** `app.rs:400-412` (`ScaleFactorChanged`) and `gpu-renderer/src/lib.rs:513-536` (`render_to_surface`) do not guard surface-vs-frame size mismatches or repaint on `suboptimal`. **(f)** `ui-chrome/src/layout.rs:248` clamps sidebar only to `vw * 0.5` with no proportional cap or narrow-viewport auto-collapse/restore. | Both (`A` + `B`) |
| **2** | **Cannot collapse `T3 Code` (or any session) in the sidebar** | **(a)** `UiAction::ToggleSessionCollapse` and `HitTarget::SessionChevron` did not exist (`draw.rs:605-713`, `lib.rs:364-370`, `sidebar.rs:1051-1058`). Clicking `v T3 Code` only emitted `SelectSession`. **(b)** `bridge.rs:2616-2627` (`CollapseOtherSessions`) is the *only* writer to `self.collapsed_sessions`, skips `keep_session_id` (so `T3 Code` can never collapse itself), uses `active_session.host_id` instead of the clicked session's `host_id`, and **nothing in `bridge.rs` ever calls `self.collapsed_sessions.remove(...)`** (collapsed sessions stay stuck forever!). **(c)** `state.hovered_session_id` is never updated in `bridge.rs:3710-3721`, making per-session hover highlight, inline `+`, and mute bell permanently invisible. | Both (`A` + `B`) |
| **3** | **Pressing `+` opens a new `T3 Code` thread instead of a Ghostty-backed Rex shell terminal** | **(a)** Live `tr3x` installs a server Lua `rex.on("window_created", ...)` hook on `T3 Code` that intercepts any new shell window inside `T3 Code` and replaces it with `tr3x new` (`"New Thread"`). **(b)** `bridge.rs:1987-1994` (`pick_initial_session_id`) hardcodes preferring `T3 Code` over normal sessions, `settings.last_session_id`, and `server_selected` — even when launched via `Super+Return` (`--working-directory`). **(c)** Global top `+` (`TabNew` -> `bridge.rs:2275-2291`) calls `NewWindow` inside `active_session` (`T3 Code`), getting hijacked by `tr3x`. **(d)** Bottom `+ New Session` (`bridge.rs:2211-2233`, `host_manager.rs:1124-1128`, `bridge.rs:1676-1680`) sends `initial_windows: None` (empty 0-window session!), discards `SessionCreateResponse.session_id`, never calls `session.attach` (required by Rex API before `session.own_block`), and never switches/focuses the new session. | Both (`A` + `B`) |
| **4** | **Unwanted `Omarex   - □ ×` titlebar on Hyprland** | `Cargo.toml:28` already omits `wayland-csd-adwaita` and `app.rs:302` calls `.with_decorations(false)`. Live inspection (`hyprctl plugins list`, `hyprctl clients -j`, `/usr/share/omarchy/default/hypr/workspace-modes.lua:458-511`) confirmed the `30px` bar with `- □ ×` is drawn by Omarchy's `hyprbars` Hyprland plugin because the live window was tagged `omarchy-floating-workspace` on a floating workspace. Harden `app.rs` (`set_decorations(false)` post-create) AND document the Omarchy Hyprland `hyprbars:no_bar` rule for `omarex`. | `B` + Packaging/Docs |
| **5** | **Sidebar bottom card clipped in half above footer bar; low-discoverability footer icons; broken/dead chrome interactions (Settings clicks dead, no hover feedback, broken Rename/CloseSession/FloatOrDock, etc.)** | `sidebar.rs:524-590, 660-730` draws no opaque `surface_sidebar` background on the footer bar and no scroll bottom-padding/fade (cutting `Monitor-Grafik stark ver...` straight in half above the 1px hairline, visually crowding the bottom-left `Search` icon right beneath the clipped `[T3]` badge). `modal.rs:1479-1486` places `HitTarget::SettingsToggle` above `ModalRow` while `lib.rs:329` maps `SettingsToggle => UiAction::None`, making every Settings toggle unclickable. Plus 14 additional dead/incomplete UX interactions audited below. | Both (`A` + `B`) |

---

## Part 1 — Re-grounding on Rex's Real Architecture & UX Principle

### 1.1 What Real Rex.app Is (and What Omarex Forgot)

From Superlogical's Rex documentation (`superlogical.com/rex/docs`), `docs/gui-spec.md`, and `~/.config/rex/tr3x.lua`:

- **Rex is a general-purpose, Ghostty-powered, multi-host, multi-session terminal multiplexer first.**
- **Hierarchy:** `Host` (Local / SSH / Tailscale) -> `Session` (named workspace, e.g. `Terminal`, `work`, `omarex`, `T3 Code`) -> `Window` (Tab) -> `Layer` (Tiled split tree + 0..N Floating drawers/scratchpads) -> `Block` (live `libghostty-vt` terminal PTY or plugin block).
- **`T3 Code` is NOT the app — it is just ONE optional, collapsible session** managed by `tr3x` (`~/.config/rex/tr3x.lua`).
- **Why `tr3x` hijacks new tabs inside `T3 Code`:** Forensic inspection of `~/.local/bin/tr3x` confirmed it registers a server-side Lua hook `rex.on("window_created", ...)` scoped to the `T3 Code` session: whenever a default shell block (`flavor == "com.superlogical.terminal.shell"`, `label == "shell"`) is created inside `T3 Code`, `tr3x` immediately splits in `tr3x new` (`"New Thread"`), closes the shell block, and renames the window `"New Thread"`.
- **Consequence:** Any global "New Terminal / `+`" action that blindly sends `session.new_window` to `active_session` while `T3 Code` is selected will ALWAYS be hijacked into a T3 thread.

### 1.2 Exact Rex-Native Semantics for Every `+`, Session, Tab & Launch Entry Point

Every entry point must obey this clean, unambiguous mental model:

| Entry Point / Shortcut | Intended Rex Behavior | Target Session / Action |
|---|---|---|
| **Sidebar Top-Right `+`** (`TabNew`), **Horizontal Tab-Bar `+`**, **`Ctrl+Shift+T`** (when not explicitly inside a non-T3 session), **Empty-State Primary CTA** | **Open a real Ghostty-backed Rex shell terminal.** Never hijacked into a T3 thread. | Emits `UiAction::NewTerminal`. If `active_session` is already a normal (non-T3) session, creates a new shell tab (`session.new_window` with explicit `build_shell_layout_spec` including cwd + theme) in that session, attaches/focuses, and claims ownership (`OwnMode::Take`). If `active_session` is `T3 Code` (or none), switches to the most recent / first existing non-T3 session on that host and opens a new shell tab there — or if **no** non-T3 session exists on the host, calls `session.create` with `label: Some("Terminal")` and `initial_windows: Some([shell_window])`, **attaches (`session.attach`)**, activates it, opens its block stream, and takes ownership! |
| **Per-Session Inline `+` on a Normal Session** | Open a new Ghostty shell tab in *that specific* normal session. | Emits `UiAction::NewTab { session_id: Some(sid) }` -> `session.new_window` with shell layout + switch/focus + `OwnMode::Take`. |
| **Per-Session Inline `+` on `T3 Code`** (explicitly scoped to `T3 Code` row, tooltip `"New T3 Thread (Ctrl+Alt+N)"`) or **`Ctrl+Alt+N`** | Explicitly open a new T3 thread inside `T3 Code`. | Emits `UiAction::RunCommand { action_id: "t3_new".into() }` (or `HitTarget::SessionNewT3Thread` -> `t3_new`). Also keep `Ctrl+Alt+O` (New T3 Project), `Ctrl+Alt+I` (T3 Inbox), `Ctrl+Alt+A` (T3 Answer). |
| **Sidebar Bottom `+` (`+ Session`)** / **`Ctrl+Shift+N`** (`NewSession`) | Create a brand-new normal Rex terminal session with 1 live Ghostty shell window immediately ready to type in. | Emits `UiAction::NewSession { host_id: None }`. **Always** passes `initial_windows: Some(vec![InitialWindowSpec { window_label: None, layout: self.build_shell_layout_spec(None, false) }])` (even when `default_working_dir` is `None`!). On `session.create` response, **immediately calls `session.attach(&created.session_id)`**, activates the new session, opens the initial block's `/api/data` stream, focuses the terminal, and claims ownership (`OwnMode::Take`). |
| **`Super+Return` / `xdg-terminal-exec` (`omarex --working-directory=<dir>`)** | Open a real shell terminal in `<dir>` — NOT hijack focus into `T3 Code`! | When `--working-directory` is passed (and `--session` is NOT explicitly `"T3 Code"`), `pick_initial_session_id` must NEVER prefer `T3 Code`; instead open a shell window in a normal session (or create `"Terminal"` with `cwd = <dir>`) with `initial_windows` set to that directory. |
| **Default Startup (`omarex` with no `--session` / `--working-directory`)** | Respect user continuity, never force `T3 Code`. | `pick_initial_session_id` priority order: (1) explicit `--session` CLI filter, (2) `settings.last_session_id` if still present on host, (3) `server_selected` if present, (4) first non-T3 session if any, (5) `host.session_order.first()`. |
| **Session Collapse (`v` / `>` chevron, clicking active session header, right-click "Collapse Session" / "Collapse Other Sessions")** | Any session — **including `T3 Code`** — can be collapsed and re-expanded freely. | Emits `UiAction::ToggleSessionCollapse { host_id, session_id }`. Collapsed sessions show a compact tab-count pill (e.g. `26`) + highest-priority status dot (amber/red/green) so collapsing `T3 Code` keeps your sidebar clean while still showing attention state. Persist `collapsed_sessions` in `settings.json`! |

### 1.3 Coordinated Cross-Crate API Contract (`ui-chrome` <-> `omarex`)

To keep `@chrome-ux-polisher` (`crates/ui-chrome/`) and `@viewport-runtime-fixer` (`crates/omarex/`) 100% aligned (matching board post `#59`):

1. **New `UiAction` variants (`crates/ui-chrome/src/draw.rs`):**
   - `ToggleSessionCollapse { host_id: String, session_id: String }`
   - `NewTerminal`
   - *(Recommended addition)* `OpenRename { target: RenameTarget }` (or pass `target_id` + `target_kind` into `ModalKind::Rename` / `RenameTarget` so right-click Rename on Host/Session/Tab/Pane renames the actual clicked target, not always `active_session`!).
   - *(Recommended addition)* `PaneSplit { block_id: String, direction: SplitAxis }` / `PaneDockOrFloat` clarity — existing `PaneButton { block_id, button }` already carries `PaneHeaderButton::FloatOrDock`; `bridge.rs` just needs to inspect whether `block_id` is currently in a floating layer and dispatch `DockBlock` vs `FloatBlock`!
2. **New `HitTarget` variants (`crates/ui-chrome/src/draw.rs`):**
   - `SessionChevron { host_id: String, session_id: String }`
   - `SessionNewT3Thread { host_id: String, session_id: String }`
   - `SettingsButton` (in sidebar footer & top tabbar -> `OpenModal(ModalKind::Settings)`)
   - `EmptyStateNewTerminal` (`-> UiAction::NewTerminal`)
   - `SidebarExpandButton` (or reuse `SidebarToggle` in pane chrome when sidebar is hidden/auto-collapsed)
   - Helper: `HitTarget::is_pointer_clickable(&self) -> bool` (replaces manual list drift in `bridge.rs:539-575`).
3. **Additive `UiTree` hover/press methods (`crates/ui-chrome/src/lib.rs`):**
   - `ui.update_pointer_hover(&draw_list, px, py) -> bool` (updates `hovered_target`, `sidebar.hovered_tab_window_id`, `sidebar.hovered_session_id`, `context_menu.hovered_index`, `modal.hovered_index`, `hovered_divider_index`, `hovered_pane_id`, tooltip text; returns `true` only when hover state actually changed so idle mouse motion doesn't force redundant frames).
   - `ui.set_pointer_pressed(&draw_list, px, py) -> bool`
   - `ui.clear_pointer_pressed() -> bool`
4. **Responsive layout field (`crates/ui-chrome/src/layout.rs`):**
   - `ChromeRects.sidebar_auto_collapsed: bool` — set when viewport width drops below auto-collapse threshold (`720px`) while `layout_cfg.sidebar_collapsed == false`, so widening the Hyprland tile immediately restores the sidebar without forgetting manual user collapse!

---

## Part 2 — Section A: `crates/ui-chrome/` Fixes (for `@chrome-ux-polisher`)

### P0 — Critical Functional & Responsiveness Fixes

#### A1. Responsive Sidebar Width Clamping & Auto-Collapse/Restore
- **Files:** `crates/ui-chrome/src/layout.rs:163-168, 224-275`, `crates/ui-chrome/src/pane_chrome.rs:180-360`
- **Bug:**
  - `layout.rs:248` clamps sidebar width to `clamp(MIN_SIDEBAR_WIDTH, (vw * 0.5).max(MIN_SIDEBAR_WIDTH))`. On a 640px–960px Hyprland split tile, a 248px sidebar consumes 26%–40% of horizontal width, crushing the terminal viewport.
  - `layout.rs:167` sets `MAX_SIDEBAR_WIDTH = 380.0` while `crates/omarex/src/config.rs:147` clamps to `420.0`.
  - When the Hyprland window narrows below ~720px (e.g., user opens a side terminal or third split), the sidebar never yields space.
- **Fix:**
  1. Add `pub const SIDEBAR_AUTO_COLLAPSE_VW: f32 = 720.0;` and `pub const SIDEBAR_MAX_VIEWPORT_FRACTION: f32 = 0.30;` (keep `MIN_SIDEBAR_WIDTH = 180.0`, `MAX_SIDEBAR_WIDTH = 380.0`).
  2. In `compute_chrome_layout`:
     - If `mode == ToolbarLayoutMode::Sidebar && !sidebar_collapsed && vw < SIDEBAR_AUTO_COLLAPSE_VW`, set `sidebar_auto_collapsed = true` and omit `sidebar_rect` (without mutating `sidebar_collapsed` in user config). When `vw >= SIDEBAR_AUTO_COLLAPSE_VW` again, `sidebar_auto_collapsed` becomes `false` and the sidebar seamlessly reappears.
     - Otherwise clamp effective sidebar width to `configured.clamp(MIN_SIDEBAR_WIDTH, (vw * SIDEBAR_MAX_VIEWPORT_FRACTION).clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH))`.
  3. When sidebar is collapsed (either manually OR `sidebar_auto_collapsed`), render a compact `[◧]` (`IconId::Sidebar`, `HitTarget::SidebarToggle`) button at the left edge of the focused pane header (or top-left overlay pill if pane headers are hidden) with tooltip `"Show Sidebar (Ctrl+Shift+B)"` so the user is never stranded without sidebar access.

#### A2. Session Collapse (`T3 Code` & All Sessions) + Collapsed Count/Attention Badge
- **Files:** `crates/ui-chrome/src/draw.rs:605-713`, `crates/ui-chrome/src/lib.rs:230-270, 355-390`, `crates/ui-chrome/src/sidebar.rs:936-1059`
- **Bug:**
  - `sidebar.rs:1051-1058` only emits `HitTarget::SessionRow` which `lib.rs:364-370` maps exclusively to `UiAction::SelectSession`. There is no chevron hit target or collapse action.
  - Right-click session context menu (`lib.rs:254-262`, `drawer.rs:116-140`) only offers `"Collapse Other Sessions"` (not `"Collapse Session"` / `"Expand Session"` for the clicked session itself!).
  - When a session is collapsed (`session.collapsed == true`), `render_session_header` shows only `> T3 Code` with no indicator of how many tabs are inside or whether any tab needs attention.
- **Fix:**
  1. Add `UiAction::ToggleSessionCollapse { host_id, session_id }` and `HitTarget::SessionChevron { host_id, session_id }`.
  2. In `render_session_header` (`sidebar.rs:936-1059`):
     - Register `HitTarget::SessionChevron` over the chevron button (`24×24px` hit box at `z + 2`) so clicking the `v`/`>` chevron ALWAYS toggles collapse of that session (including `T3 Code`, even when active!).
     - Also allow clicking an *already-active* `SessionRow` to toggle collapse (or clicking the chevron anytime), while clicking an *inactive* `SessionRow` selects & expands it.
     - When `session.collapsed == true`, render a compact pill on the right showing tab count (e.g. `26`) plus the highest-priority aggregate status dot (`NeedsYou` amber > `Error` red > `Working` pulsing > `Done` green) so collapsed `T3 Code` stays glanceable.
     - In `drawer.rs:116-140` (`session_context_menu`), pass `is_collapsed: bool` and add `"Collapse Session"` / `"Expand Session"` as a first-class item emitting `UiAction::ToggleSessionCollapse { host_id, session_id }`, right above `"Collapse Other Sessions"`.
     - Ensure `SessionContextMenu` passes the clicked session's `(host_id, session_id)` accurately (don't drop `host_id`).

#### A3. Rex-Native `+` Button Separation (Ghostty Terminal vs T3 Thread vs New Session)
- **Files:** `crates/ui-chrome/src/sidebar.rs:593-658, 660-730, 990-1059`, `crates/ui-chrome/src/tabbar.rs:125-165`, `crates/ui-chrome/src/pane_chrome.rs:881-975`, `crates/ui-chrome/src/lib.rs:330-420`
- **Bug:**
  - Top-right `+` (`sidebar.rs:623`), horizontal tabbar `+` (`tabbar.rs:139`), and empty-state CTA (`pane_chrome.rs:965`) emit `NewTab { session_id: None }`, which opens inside `T3 Code` and gets hijacked by `tr3x`.
  - Bottom footer `+` (`sidebar.rs:724`) uses an identical bare `+` icon as top `+` with zero visual distinction or tooltip.
  - Per-session inline `+` on `T3 Code` (`sidebar.rs:1016`) sends `NewTab`, which relies on `tr3x`'s hijack rather than explicit `t3_new`.
- **Fix:**
  1. Map `HitTarget::TabNew` (top-right sidebar `+` and horizontal tab bar `+`) and `HitTarget::EmptyStateNewTerminal` to **`UiAction::NewTerminal`** (tooltip: `"New Terminal (Ctrl+Shift+T)"`).
  2. For per-session inline `+` (`sidebar.rs:1016-1028`):
     - On normal sessions: emit `HitTarget::SessionNewTab` -> `UiAction::NewTab { session_id: Some(sid) }` (tooltip: `"New Tab in <session>"`).
     - On `T3 Code` (`is_t3_session`): emit `HitTarget::SessionNewT3Thread` -> `UiAction::RunCommand { action_id: "t3_new".into() }` (tooltip: `"New T3 Thread (Ctrl+Alt+N)"`).
  3. In the sidebar footer (`sidebar.rs:660-730`), make the bottom button unmistakably distinct from top `+`: render a pill or icon+label button `+ Session` (`HitTarget::NewSessionButton`, tooltip `"New Terminal Session (Ctrl+Shift+N)"`), keep `Search` on the left, and add a `Settings` gear icon (`IconId::Gear`, `HitTarget::SettingsButton`, tooltip `"Settings (Ctrl+,)"`) alongside `Hosts` (`IconId::Globe`)!
  4. In `pane_chrome.rs:881-975` (`render_empty_state`):
     - Differentiate states: `Connecting`, `Offline` (with Retry hint), `No Sessions` (`"+ New Terminal"` primary CTA -> `EmptyStateNewTerminal`), and `Empty T3 Session` (`"+ New T3 Thread"` -> `t3_new` + secondary `"+ New Terminal"` -> `EmptyStateNewTerminal`).

#### A4. Fix Dead Settings Modal Clicks (`HitTarget::SettingsToggle` Swallow Bug)
- **Files:** `crates/ui-chrome/src/modal.rs:1470-1505`, `crates/ui-chrome/src/lib.rs:329, 390-415`
- **Bug:** `modal.rs:1479-1486` pushes `HitTarget::SettingsToggle { key }` at `z_layer::MODAL + 1` directly on top of `HitTarget::ModalRow` (`z_layer::MODAL`). Then `lib.rs:329` matches `HitTarget::SettingsToggle { .. } => UiAction::None`! Clicking any setting toggle pill does literally nothing.
- **Fix:** Map `HitTarget::SettingsToggle { key }` in `lib.rs` to actually cycle/toggle that setting and emit `UiAction::UpdateSetting { key, value }` (matching the Enter-key handler in `modal.rs`), and handle `HitTarget::ModalRow` in Settings mode identically so clicking anywhere on a settings row toggles it immediately!

#### A5. Fix Sidebar Footer Backdrop, Card Clipping & Bottom-Left Filter Icon Discoverability
- **Files:** `crates/ui-chrome/src/sidebar.rs:524-591, 660-730`
- **Bug:**
  - `render_footer_bar` (`sidebar.rs:660-730`) draws ONLY a 1px hairline at `footer_rect.y`, with NO opaque `surface_sidebar` background rect.
  - `scroll_rect` ends flush at `footer_y` with no bottom padding/clearance inside content calculation (`compute_scroll_metrics`), so the bottom visible card (`Monitor-Grafik stark ver...` in screenshot) gets hard-sliced in half right above the hairline, visually swallowing the adjacent bottom-left `Search` icon.
- **Fix:**
  1. Paint an opaque `tokens.surface_sidebar` fill rect for `footer_rect` *before* its 1px `tokens.border_hairline` top separator.
  2. Add `8px` bottom padding inside sidebar scroll content height and a subtle bottom edge fade or crisp separation so cards don't visually collide with footer buttons.
  3. Give the footer buttons (`Search`, `+ Session`, `Hosts`, `Settings`) consistent `26×26px` rounded hover/active pills, crisp `tokens.fg_secondary` -> `tokens.fg_primary` on hover, and hover tooltips so every footer control is immediately discoverable.

---

### P1 — High-Impact UX Polish & Missing Interactions (`crates/ui-chrome/`)

#### A6. Full Chrome Hover, Pressed States & Tooltips
- **Files:** `crates/ui-chrome/src/lib.rs`, `crates/ui-chrome/src/sidebar.rs`, `crates/ui-chrome/src/tabbar.rs`, `crates/ui-chrome/src/pane_chrome.rs`, `crates/ui-chrome/src/drawer.rs`, `crates/ui-chrome/src/modal.rs`
- **Bug:** Currently ONLY `sidebar.hovered_tab_window_id` renders a hover highlight (`sidebar.rs:1158`). All icon buttons (`SidebarToggle`, `ActivityToggle`, `TabNew`, `SidebarFilterToggle`, `NewSessionButton`, `HostsButton`, `SettingsButton`), session headers (`SessionRow` — hiding inline `+` and mute bell!), host headers, tab close `×`, pane header buttons (`SplitRight`, `SplitDown`, `FloatOrDock`, `Zoom`, `Close`), dividers, scrollbar thumbs, context menu rows, and modal rows have ZERO hover or pressed visual feedback, and ZERO tooltips.
- **Fix:**
  1. Store `hovered_target: Option<HitTarget>` and `pressed_target: Option<HitTarget>` on `UiTree`, updated via `update_pointer_hover` / `set_pointer_pressed` / `clear_pointer_pressed`.
  2. Automatically sync `sidebar.hovered_session_id`, `sidebar.hovered_tab_window_id`, `context_menu.hovered_index`, `modal.hovered_index`, and `hovered_divider_index` from `hovered_target`!
  3. Render subtle `surface_row_hover` rounded pills (`btn_r`) on every hovered icon button, slightly brighter/accented tint on hover, and subtle press-down tint when `pressed_target == Some(target)`.
  4. Render hover highlight on `SplitDivider` / `SidebarResizeHandle` (1px -> 2px accent hairline on hover/drag so users discover resizable panes & sidebar!).
  5. Render a compact single-line floating tooltip pill (`z_layer::TOOLTIP`, clamped inside viewport) for icon-only buttons on hover (e.g., `"Toggle Sidebar (Ctrl+Shift+B)"`, `"New Terminal (Ctrl+Shift+T)"`, `"Activity Mode (Ctrl+Shift+A)"`, `"Filter Tabs"`, `"Hosts"`, `"Settings (Ctrl+,)"`, `"Split Right"`, `"Split Down"`, `"Float / Dock Pane"`, `"Zoom Pane"`, `"Close Pane"`).

#### A7. Context Menu Accuracy & Keyboard Navigation
- **Files:** `crates/ui-chrome/src/lib.rs:220-275`, `crates/ui-chrome/src/drawer.rs:95-295`
- **Bug:**
  - `lib.rs:238, 248, 257` hardcodes `is_floating = false`, `is_zoomed = false`, `muted = false` when constructing pane/tab/session context menus!
  - `drawer.rs:128, 170, 267` all emit bare `OpenModal(ModalKind::Rename)` without identifying *which* host/session/tab/pane was right-clicked.
  - `ContextMenuState` supports `hovered_index`, and needs `move_selection(delta)` + `activate_selected()` so Up/Down/Enter keyboard navigation works when a context menu is open.
- **Fix:**
  1. Accept real `is_floating`, `is_zoomed`, `muted`, `is_collapsed` flags from `UiFrameInput` / live pane/tab/session state when opening context menus (so `"Unzoom Pane"`, `"Dock Pane"`, `"Unmute"`, `"Expand Session"` display accurately).
  2. Add `move_selection(delta: isize)` and `selected_action() -> Option<UiAction>` on `ContextMenuState`.
  3. Carry rename target context (`RenameTarget::Session { host_id, session_id }`, `RenameTarget::Window { host_id, session_id, window_id }`, `RenameTarget::Block { host_id, session_id, block_id }`, `RenameTarget::Host { host_id }`) when opening `ModalKind::Rename`.

#### A8. Scrollbar Interactive Position + Drag Support
- **Files:** `crates/ui-chrome/src/lib.rs:410-414`, `crates/ui-chrome/src/pane_chrome.rs:640-720`
- **Bug:** `lib.rs:410-414` maps clicking anywhere on `HitTarget::PaneScrollbar` to hardcoded `UiAction::ScrollPaneTo { fraction: 0.5 }`!
- **Fix:** Compute exact normalized click fraction `((py - track.y) / track.h).clamp(0.0, 1.0)` from the scrollbar track rect (stored on `PaneRenderGeometry` or passed into `click_hit`), and thicken/brighten the scrollbar thumb when `hovered` or dragging.

#### A9. Horizontal Tab Bar Overflow Scrolling & Active Tab Visibility
- **Files:** `crates/ui-chrome/src/tabbar.rs:170-260`
- **Bug:** When the user switches to Horizontal Tab Bar mode (`Ctrl+Shift+S`) with 10+ tabs (like `T3 Code`'s 26 threads), tabs beyond `strip_rect.right()` are clipped off-screen with no horizontal scroll offset or overflow fade/controls.
- **Fix:** Add `tabbar_scroll_x: f32` state to `UiTree`, auto-scroll to keep the active tab in view on tab switch, accept wheel delta over the tab strip, and render subtle left/right overflow chevrons or edge fade indicators.

#### A10. Pane Header Polish & Always-Accessible Controls
- **Files:** `crates/ui-chrome/src/pane_chrome.rs:280-490`
- **Bug:**
  - Pane header action icons (`SplitRight`, `SplitDown`, `Float`, `Zoom`, `Close`) lack hover pill backgrounds, tooltips, and distinct `Dock` icon when floating (`is_floating` should show `IconId::Dock` with tooltip `"Dock Pane"`).
  - Close button (`×`) has no subtle red danger tint on hover (`tokens.status_error`).
  - Active vs inactive pane differentiation in multi-split layouts is weak when `fade_inactive_panes` is off.
- **Fix:** Add hover/pressed pills for every pane header icon (with subtle `status_error` hover tint on `Close`), swap `Float` -> `Dock` icon when floating, highlight `Zoom` button with `accent_primary` when `is_zoomed`, and ensure `PaneZoomBadge` (`"ZOOMED · Click to restore"`) has hover state.

#### A11. Modal Polish (Scroll Indicators, Find Bar Prev Icon, Shortcuts Help Sync)
- **Files:** `crates/ui-chrome/src/modal.rs:1590-1988`
- **Bug:**
  - `modal.rs:1969` uses `IconId::ChevronRight` for `FindBarPrev` and `IconId::ChevronDown` for `FindBarNext` (confusing directional pairing).
  - `modal.rs:1600-1617` (`SHORTCUT_HELP_ROWS`) omits `Ctrl+Shift+N` (New Session), `Ctrl+,` (Settings), `Ctrl+Alt+B` / session collapse, and `Ctrl+Shift+T` (New Terminal).
  - Command palette, session switcher, activity picker, and shortcuts help lack a scrollbar thumb when results overflow visible rows, and don't show hover highlight distinct from keyboard selection.
- **Fix:** Add subtle scrollbar thumb + mouse-hover selection highlight in scrollable modals, update `SHORTCUT_HELP_ROWS` to match live keybindings, and use clearer prev/next symbols (`↑` / `↓` text glyphs or rotated/matched icons) in `FindBar`.

---

## Part 3 — Section B: `crates/omarex/` & `crates/gpu-renderer/` Fixes (for `@viewport-runtime-fixer`)

### P0 — Critical Viewport Responsiveness, Resize & Rex Session Fixes

#### B1. Fix Terminal Resize Stall, Ownership Claiming, Local VT Desync & `last_sent_resize` Lockout
- **Files:**
  - `crates/omarex/src/workspace.rs:195-205, 360-440, 712-760`
  - `crates/omarex/src/pane_store.rs:790-830`
  - `crates/omarex/src/bridge.rs:436-444, 691-714, 915-946, 1317-1336, 1485-1505, 1790-1805, 2235-2260, 4230-4250`
- **Forensic Root Cause Chain (all 5 links must be fixed together):**
  1. **Ownership never forcibly taken (`OwnMode::Take`) on resize, focus, or tab switch:**
     - Live server verified 4/26 `T3 Code` blocks owned by a remote macOS client (`client:...f3e716`). Per `rex api describe com.superlogical.terminal.resize`, the server *only* applies a resize immediately if the caller owns the block (or unowned); otherwise it silently holds it until caller becomes owner.
     - `bridge.rs:437-443` (`set_viewport`), `bridge.rs:692-713` (`tick` resize drain), `bridge.rs:915-946` (`maybe_queue_pane_resize`), `bridge.rs:2235-2260` (`SelectTab`), and `bridge.rs:4234-4248` (`handle_window_focus(true)`) NEVER call `maybe_claim_ownership(..., OwnershipTrigger::ExplicitInteractOrFit)` (`OwnMode::Take`)!
     - **Fix:** When the window is focused (`self.window_focused == true`) and any visible pane resizes (`drain_ready_resizes` or `set_viewport`), OR when the user explicitly selects a tab (`SelectTab`) or clicks/focuses the window (`handle_window_focus(true)`), claim `OwnMode::Take` on the active window's visible block(s) **before** sending `HostCommand::TerminalResize`! (Respect `self.read_only` so read-only mirror mode stays passive.)
  2. **Do NOT optimistically call `term.resize()` locally before `RexApc::Resize` (and NEVER bypass `pane.dirty`):**
     - `bridge.rs:695-704` calls `term.resize(args.columns, args.rows, ...)` locally BEFORE `rex-server` resizes the PTY and emits `RexApc::Resize` in the byte stream. That causes pre-resize 80-col PTY output to wrap/corrupt against a 195-col local grid (exact cause of screenshot's `nsion/,`, `g, Offline-Queue,`, `rain/ beim Uninstall`), AND fails to set `pane.dirty = true`!
     - **Fix:** Remove the premature `term.resize(...)` in `bridge.rs:695-704`! Let the server's inline `RexControlApc::Resize` marker inside `/api/data` drive `term.apply_rex_apc(&vt_apc)` at the exact byte boundary (which already sets `pane.dirty = true` at `pane_store.rs:823`). *Only* apply a local fallback resize if the pane is in standalone/non-Rex mode or if no `RexApc::Resize` arrives within a short fallback timeout while `omarex` is confirmed owner — and ALWAYS go through a `PaneStore::resize_pane` method that sets `pane.dirty = true` and invalidates `pane.pane_cache`.
  3. **Stale `blk.last_sent_resize` permanently blocks resizing back after a split/unsplit or remote resize:**
     - `pane_store.rs:794-805` handles `RexControlApc::Resize` by updating `blk.owner`, **but NEVER updates/clears `blk.last_sent_resize`**. If `omarex` sent `195×50`, then another client (or a split cycle) caused `RexApc::Resize(80, 40)`, `blk.last_sent_resize` is still `Some(195×50)` — so `workspace.rs:722` (`if blk.last_sent_resize.as_ref() == Some(&args) && blk.pending_resize.is_none() { return; }`) **permanently drops** the resize back to `195×50`!
     - Similarly, `bridge.rs:1317-1336` (`RuntimeEvent::HostError` for `com.superlogical.terminal.resize`) never clears `blk.last_sent_resize`, so a rejected resize is never retried.
     - **Fix:**
       - On `RexControlApc::Resize { cols, rows, .. }` (`pane_store.rs:794-805`): clear `blk.last_sent_resize = None` if `(cols, rows)` differs from `blk.last_sent_resize`, OR store the server-confirmed `(cols, rows)` and compare against *actual* `pane_store.size(key)`!
       - Better yet, in `workspace.rs:714-755`: clear `blk.last_sent_resize` whenever `pane_store.size(key) != Some((args.columns, args.rows))` after a retry window (e.g., 250ms) OR whenever `owner` changes (`BlockOwnerChanged` / `RexApc::Resize`) OR on `HostError` for `com.superlogical.terminal.resize`!
       - Also in `bridge.rs:437-443` (`set_viewport`), when viewport dimensions change, immediately recompute layout / trigger a redraw AND shorten resize debounce during discrete Hyprland tiling jumps (e.g., `16ms` instead of `24ms`, or send immediately on first resize edge and debounce subsequent rapid drag frames).
  4. **Stale `entry.owner` retention in `SessionEntry::apply_view` (`workspace.rs:198-200`):**
     - `if blk.owner.is_some() { entry.owner = blk.owner.clone(); }` never clears `entry.owner` when the server reports `owner: None` (disowned).
     - **Fix:** Unconditionally assign `entry.owner = blk.owner.clone();`.
  5. **Ensure `session.attach` before `session.own_block` (`host_manager.rs:882-945`):**
     - Per `rex api describe session.own_block`: *"The calling client must be attached to the session, with `session.attach` or `session.bootstrap`. A connection that only called `session.attach_all` fails with `bad_request`."*
     - Verify `HostCommand::SelectSession` / `NewSession` always runs `client.session_attach(&session_id)` before `OwnBlock` / `TerminalResize` is issued!

#### B2. Fix Wayland/Hyprland Resize, `ScaleFactorChanged`, & `wgpu` Surface/Frame Synchronization
- **Files:**
  - `crates/omarex/src/app.rs:250-281, 384-416`
  - `crates/gpu-renderer/src/lib.rs:513-536`
  - `crates/gpu-renderer/src/context.rs:295-361`
- **Bugs:**
  1. `app.rs:400-412` (`ScaleFactorChanged { scale_factor, mut inner_size_writer }`): reads potentially stale `win.inner_size()` instead of synchronizing with the new physical size / `inner_size_writer` and immediately updating `renderer.resize(...)` + `controller.set_viewport(...)`.
  2. `app.rs:250-280` (`render_now`): never checks whether `window.inner_size()` drifted from `renderer.context().surface_size()` (can happen on Hyprland tiling transitions between queued events).
  3. `gpu-renderer/src/lib.rs:513-536` (`render_to_surface`):
     - Does NOT check `surface_tex.texture.width() != frame.physical_width || surface_tex.texture.height() != frame.physical_height`. If a swapchain image arrives with stale dimensions after a Hyprland resize, `render_into_view` renders a `frame.physical_width × height` scene into a mismatched swapchain texture!
     - On `suboptimal == true` (`lib.rs:532`), calls `reconfigure_surface()` and returns `Ok(Some(stats))`, so `app.rs:269` does NOT schedule a follow-up redraw with the fresh swapchain!
- **Fix:**
  1. In `app.rs:250` (`render_now`), compare `window.inner_size()` & `window.scale_factor()` against `renderer.context().surface_size()` / `controller.scale_factor()` before calling `render_frame`; if mismatched, call `renderer.resize(w, h, scale)` and `controller.set_viewport(w, h, scale)` synchronously before building the frame!
  2. In `gpu-renderer/src/lib.rs:513-536`: if `surface_tex.texture.width() != frame.physical_width || surface_tex.texture.height() != frame.physical_height`, drop `surface_tex`, reconfigure the surface to `(frame.physical_width, frame.physical_height)`, and return `Ok(None)` so `app.rs:270-273` immediately repaints at exact dimensions. Also when `suboptimal`, reconfigure and request a follow-up redraw (return `Ok(None)` or flag suboptimal so `app.rs` calls `window.request_redraw()`).
  3. On `WindowEvent::Resized`, immediately invalidate `self.last_ui_output` / mark `needs_redraw` and force a synchronous redraw request; ensure `next_wakeup_deadline` wakes cleanly to flush debounced `TerminalResize` even if no further Wayland events arrive!

#### B3. Fix `ToggleSessionCollapse`, `CollapseOtherSessions`, & Persistence
- **Files:** `crates/omarex/src/bridge.rs:2196-2209, 2616-2627`, `crates/omarex/src/config.rs:85-135`
- **Bugs:**
  - No handler for `UiAction::ToggleSessionCollapse { host_id, session_id }` — nothing in `bridge.rs` ever removes an entry from `self.collapsed_sessions`!
  - `CollapseOtherSessions { keep_session_id }` (`bridge.rs:2616-2627`) uses `self.workspace.active_session.host_id` instead of the target session's `host_id` (wrong on multi-host!) and never allows collapsing `keep_session_id` itself.
  - `collapsed_sessions` is lost on restart (not persisted in `config.rs`).
- **Fix:**
  1. Handle `UiAction::ToggleSessionCollapse { host_id, session_id }`: toggle `(host_id, session_id)` in `self.collapsed_sessions` (remove if present, insert if absent), persist to `self.settings.collapsed_sessions`, and mark `needs_redraw = true`.
  2. Fix `CollapseOtherSessions`: resolve the right `host_id` (ideally passed in the action or looked up across hosts), collapse all other sessions on that host, and ensure `keep_session_id` is expanded (`remove`).
  3. Persist `collapsed_sessions: Vec<String>` (e.g. `"{host_id}/{session_id}"`) in `AppSettings` (`config.rs`) and hydrate on startup.

#### B4. Fix Rex-Native `+` (`NewTerminal`), `+ New Session`, `pick_initial_session_id`, & `Super+Return`
- **Files:** `crates/omarex/src/bridge.rs:1676-1680, 1973-2002, 2211-2233, 2275-2291`, `crates/omarex/src/host_manager.rs:1124-1128`
- **Bugs:**
  1. `pick_initial_session_id` (`bridge.rs:1987-1994`) unconditionally picks `T3 Code` first — overriding `settings.last_session_id`, `server_selected`, AND `--working-directory` (`Super+Return`).
  2. `UiAction::NewTab` (`bridge.rs:2275-2291`) only passes `layout` when `default_working_dir.is_some()` and sends `NewWindow` to `T3 Code` when active (getting hijacked by `tr3x`).
  3. `UiAction::NewSession` (`bridge.rs:2211-2233`) passes `initial_windows: None` when `default_working_dir.is_none()`, creating an empty 0-window session!
  4. `HostCommand::NewSession` (`host_manager.rs:1124-1128`) discards `SessionCreateResponse.session_id` and NEVER calls `session.attach`!
  5. `ServerEvent::SessionCreated` (`bridge.rs:1676-1680`) never activates or focuses the newly created session!
- **Fix:**
  1. **Handle `UiAction::NewTerminal`:**
     - Determine target host (active host or `LOCAL_HOST_ID`).
     - If `active_session` is on that host and `!is_t3_session(&sess.label)`: send `HostCommand::NewWindow` with `layout: Some(self.build_shell_layout_spec(None, false))`, `focus: Some(true)` (and expand the session if collapsed!).
     - Else if any non-T3 session exists on that host (prefer most recently used non-T3 session or first non-T3 session): switch to it (`activate_session`), expand it if collapsed, and send `HostCommand::NewWindow` with `layout: Some(self.build_shell_layout_spec(None, false))`, `focus: Some(true)`.
     - Else (only `T3 Code` or 0 sessions exist): create a new session labeled `Some("Terminal".into())` with `initial_windows: Some(vec![InitialWindowSpec { window_label: None, layout: self.build_shell_layout_spec(None, false) }])` via the attach-and-activate flow below!
  2. **Fix `HostCommand::NewSession` (`host_manager.rs:1124-1128`):**
     - Just like `CreateCommandSession` (`host_manager.rs:1129-1165`), capture `created.session_id`, immediately call `client.session_attach(&created.session_id).await`, and emit a `RuntimeEvent::SessionCreatedAndAttached { host_id, session_id, view }` (or `SessionViewUpdated` + mark pending auto-activate).
     - In `bridge.rs`, when that locally requested session is ready: `activate_session`, open its block stream, focus its terminal block, and claim `OwnMode::Take`!
     - Always pass `initial_windows: Some(vec![InitialWindowSpec { window_label: None, layout: self.build_shell_layout_spec(None, false) }])` in `UiAction::NewSession` (`bridge.rs:2220`) so a new session always opens with a live, themed Ghostty shell immediately!
  3. **Fix `UiAction::NewTab { session_id }` (`bridge.rs:2275-2291`):**
     - Always pass `layout: Some(self.build_shell_layout_spec(None, false))` so every new shell tab inherits Omarchy theme colors + cwd (not just when `--working-directory` was passed).
     - Expand the target session if it was collapsed and switch to it if inactive.
  4. **Fix `pick_initial_session_id` (`bridge.rs:1973-2001`) & `Super+Return` (`--working-directory`):**
     - Priority order:
       1. `self.preferred_session_filter` (`--session`)
       2. If `self.default_working_dir.is_some()` (launched via `Super+Return` / `xdg-terminal-exec`): prefer a non-T3 session (or create `"Terminal"` with that cwd via `NewTerminal` instead of dumping the user into `T3 Code`!)
       3. `self.settings.last_session_id` if present on host
       4. `server_selected` if present
       5. First non-T3 session on host, then fallback to `host.session_order.first()`.
  5. **Pass `build_shell_layout_spec` on splits too (`bridge.rs:2659, 2674, 3211`):** Replace bare `LayoutCreateSpec::shell()` with `self.build_shell_layout_spec(None, false)` so split panes also inherit theme + working directory.

#### B5. Unwanted `Omarex - □ ×` Titlebar on Omarchy Hyprland
- **Files:** `crates/omarex/src/app.rs:297-325` (+ Hyprland config note)
- **Forensic Diagnosis:**
  - `app.rs:302` already sets `.with_decorations(false)` and `Cargo.toml:28` omits `wayland-csd-adwaita`.
  - The visible `30px` `Omarex   - □ ×` bar in the screenshot is drawn by Omarchy's `hyprbars` Hyprland plugin (`/usr/lib/omarchy-hyprland-titlebars/titlebars.so`, loaded by `/usr/share/omarchy/default/hypr/workspace-modes.lua:458-511`) whenever a window is tagged `omarchy-floating-workspace` (which the live `omarex` window had!).
- **Fix:**
  1. In `app.rs:319`, defensively call `window.set_decorations(false);` right after `create_window` to guarantee Wayland `xdg-decoration` never negotiates SSD on compositors that ignore initial attributes.
  2. Do **not** silently overwrite user files in `~/.config/hypr/` from inside the binary; instead document (and ship in Omarchy packaging / install script) the one-line Hyprland rule so `hyprbars` doesn't add a duplicate titlebar above `omarex`'s native chrome when floated on floating workspaces:
     ```lua
     -- In ~/.config/hypr/hyprland.lua (or omarex Hyprland drop-in):
     hl.window_rule({ match = { class = "^(omarex)$" }, ["hyprbars:no_bar"] = true })
     ```

---

### P1 — High-Impact Runtime & Interaction Wiring Fixes (`crates/omarex/`)

#### B6. Wire Pointer Hover, Pressed States & `cursor_hint()`
- **Files:** `crates/omarex/src/bridge.rs:501-580, 1155-1180, 3680-3770`
- **Bug:**
  - `bridge.rs:3710-3721` only updates `self.ui.sidebar.hovered_tab_window_id`; NEVER updates `hovered_session_id`, `context_menu.hovered_index`, `modal.hovered_index`, `hovered_divider_index`, `pane_metas[i].hovered` (`bridge.rs:1174` hardcoded `false`), or `scrollbar.hovered` (`bridge.rs:1161` hardcoded `false`).
  - `bridge.rs:538-575` manually lists clickable `HitTarget` variants and will miss new variants (`SessionChevron`, `SessionNewT3Thread`, `SettingsButton`, `EmptyStateNewTerminal`).
- **Fix:**
  1. Call `self.ui.update_pointer_hover(&out.draw_list, px, py)` in `handle_pointer_move` (setting `self.needs_redraw = true` when it returns `true`), and `set_pointer_pressed` / `clear_pointer_pressed` in `handle_pointer_down` / `handle_pointer_up`.
  2. Set `pane_meta.hovered` and `scrollbar.hovered` from live pointer position / `hovered_target` in `build_ui_frame_input` (`bridge.rs:1161, 1174`).
  3. Update `cursor_hint()` to delegate pointer hand detection to `hit.is_pointer_clickable()`.

#### B7. Persist All Settings Toggles & Apply Live
- **Files:** `crates/omarex/src/config.rs:85-160`, `crates/omarex/src/bridge.rs:340-390, 2737-2768`
- **Bug:**
  - `config.rs:85-113` does NOT persist `pane_header_mode`, `highlight_active_border`, `fade_inactive_panes`, or `adaptive_rounding` (changes reset on restart).
  - `config.rs:147` clamps `sidebar_width_px` to `180..=420` instead of matching `ui_chrome::MAX_SIDEBAR_WIDTH` (`380.0`).
- **Fix:** Add those 4 fields (with `#[serde(default = "...")]`) to `AppSettings`, load them into `self.ui.pane_options` / `self.theme` on startup, write them back + `save_settings_best_effort()` inside `UiAction::UpdateSetting`, and align `sidebar_width_px` clamp to `180..=380`.

#### B8. Fix Broken `CloseSession`, `FloatOrDock`, `Rename`, Modal Wheel Scroll & Context Menu Keys
- **Files:** `crates/omarex/src/bridge.rs:2425-2455, 2595-2695, 2885-2915, 3465-3490, 4008-4080`
- **Bugs & Exact Fixes:**
  1. **`CloseSession` (`bridge.rs:2599-2614`):** Sends `HostCommand::ClientAction { action: "session.destroy" }` instead of `HostCommand::DestroySession { host_id, session_id }`! Switch to `HostCommand::DestroySession` and clean up `collapsed_sessions` / pick next session cleanly.
  2. **`FloatOrDock` (`bridge.rs:2679-2688`):** Always sends `HostCommand::FloatBlock` even when `block_id` is already inside a floating layer! Inspect the active window's layers: if `block_id` is in a floating layer (`!kind.eq_ignore_ascii_case("tiled")`), send `HostCommand::DockBlock`; otherwise send `HostCommand::FloatBlock`.
  3. **Rename Modal (`bridge.rs:2432-2445, 2895-2910`):** `OpenModal(ModalKind::Rename)` always prefills and renames `active_session` even when invoked from a tab, pane, or inactive session context menu! Store the explicit `RenameTarget` (`Session`, `Window`, `Block`, `Host`) so `SubmitRename` dispatches `RenameSession` / `RenameWindow` / `RenameBlock` / host-label update for the actual clicked target (and works with real Rex `session:<uuid>` / `window:<uuid>` / `block:<uuid>` IDs).
  4. **Modal Wheel Scroll (`bridge.rs:4008-4076`):** `handle_pointer_wheel` never checks `self.ui.active_modal.is_some()` — scrolling with a modal open scrolls the terminal behind it! When a modal is open, route wheel events to modal list scrolling (and return early). Similarly, when over the horizontal tab bar, route wheel events to `tabbar_scroll_x`.
  5. **Context Menu Keyboard Navigation (`bridge.rs:3472-3476`):** Currently only handles `Escape`; add `ArrowUp` / `ArrowDown` / `Enter` / `Space` handling when `self.ui.context_menu.is_some()`.
  6. **Context Menu Live State:** Pass actual `is_floating`, `is_zoomed`, `muted`, `is_collapsed` flags when opening context menus (`bridge.rs:3920-3980`).
  7. **Scrollbar Drag (`bridge.rs:3780-3910`):** Add `ActiveDrag::PaneScrollbar { key, track_rect }` so dragging the terminal scrollbar thumb smoothly scrolls scrollback in real time.

---

### P2 — Completeness & Rex Action Parity (`crates/omarex/src/bridge.rs:3200-3305`)

#### B9. Wire Remaining No-Op `ClientCommand` / `UiAction` Arms
- **File:** `crates/omarex/src/bridge.rs:2597, 3294`
- Currently silently ignored (`_ => {}`):
  - `ToggleTabMute` -> wire to `self.settings.toggle_mute_block` + save
  - `MarkTabUnread` -> toggle in `self.unread_windows`
  - `BalancePanes` -> dispatch `session.balance` / `client.action("pane.balance")`
  - `MoveTab` / `MovePane` -> dispatch `session.move_window` / `session.move_block`
  - `MinimizeAllFloating` -> minimize all visible floating layers in active window
  - `RevealPaneHeader` -> temporarily reveal pane headers if set to `Hover` / `Never`
  - `NewWindow` -> alias to `NewTerminal` (or spawn new OS window)
  - `ChangeTheme` -> open Theme picker / reload Omarchy theme
  - `GoToDirectory` -> open directory picker / prompt
- Also verify `Ctrl+Shift+N` maps to `NewSession` and `Ctrl+Shift+T` maps to `NewTerminal` (not hijacked `NewTab`), and `Ctrl+,` maps to `OpenModal(ModalKind::Settings)`.

---

## Verification Checklist After Fixes Land

1. **Responsive Viewport & Resize (Hyprland Live Test):**
   - Open `omarex` on `T3 Code` (even with Mac mini connected as a second client), then open a side terminal in Hyprland (`Super+Return` / half-screen split), wait 1s, then close the side terminal.
   - **Expected:** Sidebar smoothly narrows (and auto-collapses below `720px`, restoring when widened back); `omarex` takes ownership (`OwnMode::Take`), sends `com.superlogical.terminal.resize`, receives `RexApc::Resize`, and `tr3x` / shell reflows cleanly to full 1920px width with zero 80-col wrapping or black void.
2. **Session Collapse (`T3 Code`):**
   - Click the `v` chevron next to `T3 Code` (or right-click `T3 Code` -> `"Collapse Session"`): `T3 Code` collapses immediately, showing `> T3 Code [26]` + status dot. Click `>` again: expands cleanly. Right-click another session -> `"Collapse Other Sessions"`: collapses `T3 Code` and still allows re-expanding it. State persists across restart.
3. **Rex-Native `+` Behavior:**
   - While viewing `T3 Code`, click top-right `+` (or press `Ctrl+Shift+T`): opens a **real Ghostty shell terminal** in a normal session (creating `"Terminal"` if none exists), NOT a new T3 thread!
   - Click bottom `+ Session` (or `Ctrl+Shift+N`): creates a new normal session with 1 live shell tab, attaches, switches to it, and focuses the prompt ready to type immediately.
   - Hover `T3 Code` and click its inline `+` (or press `Ctrl+Alt+N`): explicitly opens a new T3 thread.
   - Launch `Super+Return` (`omarex --working-directory=/tmp`): opens a real Ghostty shell in `/tmp`, not `T3 Code`.
4. **UI Polish & Interactions:**
   - Open Settings (`Ctrl+,` or footer gear button) and click every toggle row: each toggle visibly updates live and persists across restarts.
   - Hover every sidebar, footer, tab, pane header, and divider control: crisp hover pills, pointer/resize cursors, and tooltips appear; sidebar bottom cards do not collide with the footer bar.
   - Right-click any tab/pane/session -> Rename renames that specific target; Close Session destroys the session; Float/Dock toggles both directions; wheel scroll inside modals scrolls the modal (not the terminal behind it).
