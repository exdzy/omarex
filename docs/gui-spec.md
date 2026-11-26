# `omarex` — Native Linux GUI & UX Specification

> **Purpose:** Implementation-ready visual, interaction, and UX specification for **`omarex`**, a fast native Linux (Wayland/Hyprland) GUI client for **Superlogical's Rex** and **macnason's `tr3x`**, built to match Rex's macOS/Windows polish while integrating natively into **Omarchy**.
>
> **Primary sources analyzed:**
> - Video walkthrough of Rex Beta 0.1.0 (`youtu.be/8GtOKXm1meU`, 1080p frame-by-frame inspection of top tabs, left/right sidebars, split pane headers, OSC 7501 badges, Switch Session modal, Command Palette, dark/light theme live preview).
> - `macnason/tr3x` showcase media (`docs/assets/hero.png` and `docs/assets/tr3x.mp4`, 36s frame-by-frame inspection of sidebar priority sorting, multi-host grouping, top tabs, New Thread chips, inline HTML previews, `/` slash command menu, and `/pr` `sup` floating drawer).
> - Superlogical Rex documentation (`custom-ui.md`, `actions.md`, `keyboard-shortcuts.md`, `key-modes.md`, `key-sequences.md`, `config.md`, `features/session-restore.md`, `build/program-status.md`, `reference/mac-actions.md`, `reference/mac-settings.md`, `reference/events.md`).
> - `macnason/tr3x` docs & source (`README.md`, `docs/usage.md`, `docs/how-it-works.md`, `~/.config/rex/tr3x.lua`, `actions.rs`, `pr.rs`, `icons.rs`, `status.rs`, `tabs.rs`, `t3client/src/inbox.rs`).
> - Live Omarchy host inspection (`~/.local/state/omarchy/current/theme/colors.toml`, `shell.toml`, `ghostty.conf`, `t3code.json`, `hyprland.lua`, `~/.config/ghostty/config`, live `hyprctl binds -j`, and live `rex-server` session `T3 Code`).

---

## 1. Design Philosophy & Visual Language

Rex is a **native terminal workspace and multiplexer** — browser-grade workspace chrome wrapping ultra-fast `libghostty`-powered terminal panes, backed by a persistent daemon (`rex-server`). On Linux/Omarchy, `omarex` MUST preserve Rex's distinctive craft while feeling 100% indigenous to Hyprland/Wayland:

1. **Dark-first, ultra-crisp spatial hierarchy (with automatic light mode parity):**
   - **Window Frame / Sidebar Canvas** (`surface.canvas`): darkest neutral plane (`dark_background` / `darker_background`).
   - **Terminal Pane Surface** (`surface.pane`): subtly elevated card inset from the frame (`background`), separated by crisp 1px hairlines and soft corner rounding (`8px` in Comfortable density, `0px` when Hyprland `rounding = 0` or Compact edge-to-edge mode).
   - **Interactive Chrome / Active Pills** (`surface.elevated`): subtle fill + 1px border (`lighter_background` / translucent white alpha overlays) for active tabs, sidebar rows, command palette, and floating layers.
   - **Semantic Status Color Discipline:** Interface chrome stays monochrome and calm; color is reserved for **active provider icons**, **active focus accent**, and **OSC 7501 status indicators** (blocked/needs-you, working, error, done) so the eye immediately finds what needs attention.
2. **Zero-jank, compositor-native feel on Omarchy:**
   - Frameless Wayland top-level window (no redundant GTK titlebar, no macOS traffic lights, no double window border fighting Hyprland's `border_size`).
   - All chrome animations obey `animation_style` (`Snappy`, `Natural`, `Off`) and stay within a 120–300 Hz frame budget with zero layout thrash.
   - Live hot-reload of Omarchy theme colors (`colors.toml`) without restarting or disconnecting from `rex-server`.

---

## 2. Exact Visual Anatomy of the Rex GUI

### 2.1 Top-Level Workspace Layouts

Rex supports **four toolbar/tab layouts** (`interface.toolbar.layout`), switchable live via `client.tab_style.toggle` or Settings:

| Layout | Description | Chrome Placement |
| :--- | :--- | :--- |
| **`left` (Recommended default for `tr3x`)** | Vertical sidebar on the left + pane workspace on the right | Top of sidebar: Sidebar Toggle (left), Activity Eye + `+` New Tab (right). Footer of sidebar: Tab Filter (left), New Session + Hosts/Tailscale (right). |
| **`right`** | Vertical sidebar mirrored on the right | Top of right sidebar: Activity Eye + Command Palette + `+` New Tab. Pane workspace sits on the left. |
| **`top`** | Classic browser-style horizontal tab bar above panes | Single `38px` top bar: Session Switcher Pill (left) → Scrollable Horizontal Tab Strip (center) → Activity Eye + Command Palette + `+` New Tab (right). |
| **`bottom`** | Horizontal tab bar docked below panes | Identical to `top`, anchored to the bottom edge. |

**Toolbar Visibility (`interface.toolbar.auto_hide_toolbar`):**
- `always` (default): Toolbar/sidebar remains persistently visible.
- `on_hover`: Hides the horizontal tab bar (or the sidebar's top utility row) until the pointer enters a `6px` hot-edge trigger zone at the top (or side) of the window; reveals smoothly as an overlay.
- `hidden`: Completely hidden until toggled via `client.toolbar.reveal` (or `client.sidebar.toggle`).

**Optional Back/Forward Navigation (`interface.window.show_back_and_forward_buttons`):**
- Two compact `24×24px` chevron buttons (`‹` `›`) stepping through focus history across panes, tabs, and sessions (`client.navigate.back` / `client.navigate.forward`).
- Placed directly before the Session Picker in horizontal tab layouts, or beside the Activity Eye in sidebar layouts.

---

### 2.2 Window Chrome & Header Utility Bar

Because Hyprland manages window closing/tiling/borders externally, **`omarex` omits macOS traffic lights and OS titlebars** by default (`csd_window_controls = false`, opt-in toggle available for floating/non-tiling WMs).

#### A. Horizontal Mode Top Bar (`height: 38px`, `padding: 0 10px`)
```text
┌──────────────────────────────────────────────────────────────────────────────────────────┐
│ [‹ ›] [◫ T3 Code ▾] │ [🟠● Hotkey chip… •••]  [⚙ Release notes…]  [✦ Fix flaky CI…] │ [👁●] [⌘] [+] │
└──────────────────────────────────────────────────────────────────────────────────────────┘
```
- **Session Switcher Pill** (left): `height: 26px`, `padding: 0 8px`, `radius: 6px`, subtle hover fill. Displays stacked-layers icon (`14px`) + current session label (`12.5px` medium) + subtle status dot if another session has blocked/error activity. Clicking opens **Switch Session** (`session.switch`).
- **Tab Strip** (flex-grow middle):
  - `gap: 4px`, smooth horizontal wheel-scroll when overflowing (no clunky scrollbar).
  - **Active Tab Pill:** `height: 28px`, `radius: 7px`, `background: surface.tab_active`, `border: 1px solid border.subtle`, `padding: 0 10px`. Displays provider icon (`16×16px` rounded-square `4px` radius), optional overlaid status dot, medium-weight primary text, trailing working indicator (`•••`) or close button (`×` on hover).
  - **Inactive Tab:** transparent fill (`surface.tab_hover` on hover), subtle `1px` vertical hairline separator (`14px` tall, `alpha 0.12`) between adjacent inactive tabs, muted text (`fg.secondary`), monochrome icon (unless `tab_icons.color == "always"`).
  - **`tabs_fill_available_space`:** When enabled and `tab_count >= 2`, tabs expand equally (`flex: 1`, `min-width: 110px`, `max-width: 240px`); when `tab_count == 1`, tab background hides for a cleaner single-tab bar.
  - **Interactions:** Left-click selects tab; middle-click closes tab; drag horizontally reorders (`session.move_window`); drag outside window tears off (`client.tab.move_to_new_window`); right-click opens tab context menu (Rename, Mark Unread, Mute, Duplicate/Split, Close, Close Others).
- **Right Utility Cluster** (`gap: 4px`):
  - **Activity Eye (`28×28px`, `radius: 6px`):** Eye glyph (`15px`). Shows a **`6px` status dot badge** at top-right whenever any unfocused tab/session is `blocked`, `error`, or unread `done` (accent/blue for unread/needs-you, amber for permission/question, red for error). Clicking toggles Activity mode (`client.sidebar.activity.toggle` in sidebar, or opens the Activity Popover in top-tab mode).
  - **Command Palette (`28×28px`, `radius: 6px`):** Command/terminal-menu glyph (`14px`). Clicking opens Command Palette (`client.palette.toggle`).
  - **New Tab (`+`, `28×28px`, `radius: 6px`):** Plus glyph (`15px`). Clicking creates a new window (`client.tab.new`).

#### B. Sidebar Mode Chrome (`default width: 248px`, resizable `180px..380px`)
Directly matching `hero.png` and `tr3x.mp4`:
```text
┌──────────────────────────────┬─────────────────────────────────────────────────────────┐
│ [◧]               [👁●] [+]  │  ┌───────────────────────────────────────────────────┐  │
│                              │  │  Pane Header (optional/hover/always)              │  │
│ 🖥 This Mac                  │  │───────────────────────────────────────────────────│  │
│ T3 Code                      │  │                                                   │  │
│ ┌──────────────────────────┐ │  │  Terminal Grid (libghostty-vt + GPU atlas)        │  │
│ │ [🟠●] Hotkey chips in…   │ │  │                                                   │  │
│ │       pulse · Needs you now│ │  │                                                   │  │
│ └──────────────────────────┘ │  │                                                   │  │
│   [⚙] Release notes…   •••   │  │                                                   │  │
│       site · Working   12m   │  │                                                   │  │
│   [✦] Fix flaky CI…    •••   │  │                                                   │  │
│       pulse · Working  18m   │  │                                                   │  │
│                              │  │                                                   │  │
│ ▭ Mini                       │  │                                                   │  │
│ T3 Code                      │  │                                                   │  │
│   [▣] Dark mode for…         │  │                                                   │  │
│       notes · Done     25m   │  │                                                   │  │
│                              │  │                                                   │  │
│ [≡]               [❏+] [🌐]  │  └───────────────────────────────────────────────────┘  │
└──────────────────────────────┴─────────────────────────────────────────────────────────┘
```
1. **Sidebar Top Row (`height: 36px`, `padding: 0 10px`):**
   - Left: Sidebar Collapse Toggle (`◧`, `client.sidebar.toggle`) + optional Back/Forward chevrons.
   - Right: Activity Eye (`👁` with top-right unread/attention dot) + `+` New Tab button.
2. **Inline Filter Bar (conditional):** Appears smoothly directly under the top row when **Focus Tab Filter** (`client.sidebar.filter.focus`) is triggered or the bottom filter button (`≡`) is clicked. Single `28px` rounded search field (`Filter tabs, sessions, hosts…`, `Esc` clears & closes).
3. **Scrollable Host → Session → Tab Tree (`padding: 4px 8px`):**
   - **Host Header Row (`height: 28px`, `margin-top: 8px`):**
     - Device icon (`14px` monochrome: desktop monitor for local Linux host, mini/server glyph for remote SSH/Tailscale hosts) + Host label (`This Machine` or host name, `12.5px` medium, `fg.primary`).
     - Connection indicator on right if disconnected/reconnecting (amber spinner or muted `offline` pill).
     - Click collapses/expands host; right-click opens host menu (New Session on Host, Forward Port, Disconnect/Reconnect).
   - **Session Header Row (`height: 24px`, `padding: 0 6px`):**
     - Session name (`T3 Code`, `Onboarding`, etc.) in `11px` semibold `fg.muted`.
     - Subtle collapse chevron (`▾`/`▸`) revealed on hover; right-aligned `+` button on hover to add a tab directly to that session.
     - When collapsed (`session.collapse`), displays a compact pill count of hidden tabs + highest-priority OSC 7501 status dot if any child tab is blocked/working/error.
   - **Rich 2-Line Tab Row (`height: 46px` default / `40px` compact / `52px` large via `sidebar_text_size`, `padding: 6px 8px`, `radius: 8px`, `gap: 10px`):**
     - **Left Icon Tile (`26×26px`, `radius: 6px`):**
       - Displays the focused pane's provider/program icon (or an overlapping mini-stack of up to `tab_icons.count` `0..3` pane icons for multi-pane tabs).
       - **Color rule (`tab_icons.color`):** Default `active_tabs` renders the active tab's icon in full brand color (e.g. Claude `#D96B43` orange tile with white starburst) and inactive tabs in muted monochrome (`surface.icon_inactive` `#262626` tile with `fg.secondary` glyph). Also supports `always`, `active_panes`, and `never`.
       - **Status Dot Overlay:** `7px` filled circle with `1.5px` cutout ring matching row background, anchored to the **top-right corner** (`-2px, -2px`) of the `26×26px` icon tile whenever status is `blocked` (amber/blue), `error` (red), `done` unread (accent/blue), or manually marked unread (`client.tab.unread`).
     - **Center 2-Line Text Stack (flex-grow, `min-width: 0`):**
       - **Line 1 (Title):** `12.5px` medium (`fg.primary` when active/unread, `fg.secondary` when idle), single-line ellipsis truncation. For `tr3x` subagents, preserves `↳ title` prefix.
       - **Line 2 (Metadata / Subtitle):** `11px` regular `fg.muted`, single-line ellipsis truncation. Shows:
         - OSC 7501 `msg` or synthesized `<project/cwd> · <Status>` (e.g., `pulse · Needs you`, `site · Working`, `notes · Done`, `~/Work/omarex`).
         - When a tab is `blocked` or `error` and `animate_blocked_tabs = true`, gently marquees a long `msg` on Line 2 (and bounces the icon tile once on transition).
     - **Right Status & Age Column (fixed right-aligned, `gap: 2px`):**
       - **Line 1 right:**
         - Animated 3-dot working pulse (`•••`) when `working` (indeterminate), OR a crisp `12×12px` circular **progress pie** when `working`/`blocked` reports `progress=0..100`.
         - Optional subtle shortcut hint (`Alt+1`..`Alt+9`) when `show_tab_shortcut_hints` is enabled or while holding the tab-jump modifier.
         - On hover: swaps cleanly to a subtle `×` close button (`16×16px` hit target) so closing/settling a tab is always one click.
       - **Line 2 right:** Relative timestamp (`now`, `2m`, `12m`, `1h`, `2d`) in `10.5px` tabular numerals (`fg.muted`).
   - **Active Row Styling:** Rounded rect (`radius: 8px`) with `surface.row_active` fill (`rgba(fg, 0.08)` or subtle theme-tinted elevation) + optional `1px solid border.subtle`.
   - **Hover Row Styling:** `surface.row_hover` (`rgba(fg, 0.04)`).
4. **Sidebar Footer Bar (`height: 36px`, `padding: 0 10px`, `border-top: 1px solid border.hairline`):**
   - Left: **Tab Filter Toggle (`≡` inside circle, `26×26px`)** — focuses/toggles the sidebar filter input.
   - Right (`gap: 6px`):
     - **New Session (`❏+`, `26×26px`)** — triggers `session.new` (right-click or `Alt`-click triggers `session.new.on_host`).
     - **Remote Hosts / Tailscale (`🌐`, `26×26px`)** — opens Remote Hosts popover/settings showing local + remote Rex hosts, latency, port forwards, and Tailscale status.
5. **Sidebar Resizing & Width Preservation:**
   - `4px` invisible drag handle along the sidebar divider (cursor changes to `ew-resize`, subtle `2px` accent line highlights on hover/drag).
   - Double-clicking the divider resets to `248px`.
   - Respects `preserve_width_when_resizing_sidebar` (always true under Hyprland tiling; reflows terminal panes smoothly via `smooth_terminal_resizing`).

---

### 2.3 Activity View (Sidebar Mode & Popover)

Triggered by clicking the **Eye icon** or pressing `client.sidebar.activity.toggle`:
- Filters the sidebar (or opens a `340px` anchored popover in horizontal tab layouts) to show **only tabs/panes with active OSC 7501 records** (`blocked`, `error`, `done`, `working`, and optionally `idle` when `include_idle_processes_in_activity = true`).
- **Two grouping modes** (toggleable via header segmented pill or actions):
  1. **By Priority (`client.sidebar.activity.by_priority`, default):**
     - Section 1: **Needs You** (`blocked`: `permission`, `question`, `auth`)
     - Section 2: **Failed** (`error`)
     - Section 3: **Done** (unseen `done` records)
     - Section 4: **Working** (`working` with spinner or progress pie)
     - Section 5 (optional): **Idle** (`idle`)
  2. **By Session (`client.sidebar.activity.by_session`):** Groups active items under their Host → Session headers.
- Clicking any row jumps directly to that exact host, session, tab, and pane (`session.select` + `session.focus_block`).

---

### 2.4 OSC 7501 Program Status Visual System

Rex's signature UX feature is native visualization of the **Program Status Protocol (OSC 7501)**. `omarex` MUST render every record state consistently across **four surfaces**: (1) Tab icon/row, (2) Pane header, (3) Session Switcher & Go to Anything, and (4) Desktop notifications / window urgency.

| OSC 7501 State | `kind` | Color Token (Omarchy-mapped) | Icon / Indicator | Pane Header Chip | Clears Automatically On |
| :--- | :--- | :--- | :--- | :--- | :--- |
| `blocked` | `permission` | `warning` / amber (or `accent` if high-contrast) | Shield / filled dot + bounce | `● blocked · permission` (or decoded `msg`) | Replaced, cleared, process exit, OSC 133 `A` prompt |
| `blocked` | `question` | `accent` / blue-amber | Question bubble / filled dot | `● blocked · question` (or decoded `msg`) | Same as above |
| `blocked` | `auth` | `warning` / orange | Key / login glyph + dot | `● blocked · auth` (or decoded `msg`) | Same as above |
| `error` | — | `danger` / `red` | Cross circle / red dot | `✕ error` (or decoded `msg`) | User focuses pane & presses a key, or replaced/cleared |
| `done` | — | `accent` (unseen dot) / `success` (`✓`) | Accent dot when unseen; `✓` in subtitle | `✓ done` (briefly until acknowledged) | User focuses pane & interacts (or marks read) |
| `working` | — | `blue` / `accent` | Animated `•••` pulse (or `12px` circular progress pie if `progress=0..100`) | `⠿ working` (braille spinner + optional `40%` + `msg`) | Replaced, cleared, process exit, OSC 133 `A` prompt |
| `idle` | — | `muted` | None on tab (subtle hollow ring in Activity if enabled) | Hidden | Replaced or cleared |
| `clear` | — | — | Removes record & all child `id/...` records | Hidden | Immediate |

**Additional OSC 7501 & Notification Rules:**
- **Multi-record aggregation (root + child `id` paths):** A pane can hold up to 256 records (e.g., root `working` while child `eu-west` is `blocked`). Tab/session rollup priority is strictly: **`blocked` > `error` > `done` (unseen) > `working` > `idle`**.
- **OSC 9;4 fallback:** Map ConEmu progress (`OSC 9;4`) to the root record ONLY until the first OSC 7501 report arrives (until full terminal reset `RIS`).
- **Security sanitization (MUST follow spec):** Reject/discard reports with control chars in `title`/`msg`; strip Unicode BiDi overrides (`U+202A–U+202E`, `U+2066–U+2069`) before rendering in GUI chrome; never parse markup; always attribute notifications with the source session/tab name.
- **Per-state notification matrix:** Honor independent toggles for `tab_indicators`, `system_notifications` (via `org.freedesktop.Notifications` D-Bus when unfocused, click focuses tab), `sounds` (optional low-latency PipeWire/PulseAudio chime with `sound_volume`), and rate-limiting (max 1 desktop notification per block per 3s, coalescing bursts).
- **Per-tab Mute (`client.tab.mute`) & Mark as Unread (`client.tab.unread`):** Muted tabs display a tiny crossed-bell glyph and suppress sounds/desktop notifications; Mark as Unread manually sets the unseen dot until next visit.

---

### 2.5 Provider & Program Icons

Rex maps OSC 7501 `app` (and foreground process fallback) to crisp vector icons drawn inside the `26×26px` sidebar tile (and `16×16px` horizontal tab tile / `14×14px` pane header):

| `app` / Process | Brand Accent (Active Color Mode) | Glyph Description |
| :--- | :--- | :--- |
| `claude`, `claude-code` | `#D96B43` (Anthropic Terracotta) | 12-ray starburst |
| `codex` | `#10A37F` or `#FFFFFF` on `#202123` | OpenAI hexagonal knot |
| `gemini` | `#4E8DF5` / `#8B7CF6` | 4-point sparkle diamond |
| `cursor` | `#FFFFFF` on `#1E1E1E` | Isometric cube cursor |
| `opencode` | `#F5F5F5` on `#262626` | Framed square monogram |
| `pi` | `#E28203` / monochrome | Geometric `π` mark |
| `grok` | `#FFFFFF` on `#181818` | Slash-ring mark |
| `fx`, `muse`, `antigravity` | Provider brand tint | Corresponding vector glyph |
| `t3code` | `#E28203` / Omarchy accent | T3 badge glyph |
| `zsh`, `bash`, `fish`, `nu`, `sh` (default shell) | Follows `shell_icon_theme`: `default` (green `>_` on dark tile), `use_theme` (theme bg/fg), or `use_theme_accent` | Rounded terminal `>_` glyph |
| `nvim`, `vim`, `hx`, `git`, `cargo`, `ssh`, `docker`, `python`, `node` | Recognizable subtle monochrome/tinted dev glyphs | Clean 1.5px stroke icons |

> **Reuse of `tr3x` SVG assets:** `macnason/tr3x` ships official MIT-licensed provider SVGs under `tr3x/assets/logos/` (`claudeAgent.svg`, `codex.svg`, `cursor.svg`, `fx.svg`, `grok.svg`, `muse.svg`, `opencode.svg`, `pi.svg`, `acpRegistry.svg`). `omarex` should bundle crisp vector paths for all of these plus `gemini` and standard shell/tool icons.

---

### 2.6 Split Panes, Pane Headers, Zoom & Density

#### A. Pane Container & Density (`interface.panes.density`)
Each Rex **window** contains a base binary split tree (`layers[0]`) plus zero or more floating layers (`layers[1..]`).

| Property | `comfortable` (Rex default) | `compact` |
| :--- | :--- | :--- |
| Outer workspace margin | `6px` (`0px` on sidebar edge) | `0px` |
| Gap between split panes | `6px` | `1px` hairline divider |
| Pane card corner radius | `8px` (or `0px` if user selects sharp/Hyprland `rounding=0` mirror) | `0px` |
| Pane card border | `1px solid border.pane` | `0px` (shared `1px` divider) |
| Terminal text padding (`padding_x` / `padding_y`) | `12px` / `10px` (configurable; matches Omarchy Ghostty `14px` feel) | `6px` / `4px` |

#### B. Pane Header (`height: 30px`, `padding: 0 10px`)
Directly matching `yt_00_05_12.png`, `t_023.png`, and `t_039.png`:
```text
┌───────────────────────────────────────────────────────────────────────────┐
│ >_ claude /tmp   ● blocked · permission                   [◫] [⬓] [❏] [⤢] [×] │
├───────────────────────────────────────────────────────────────────────────┤
│ terminal grid...                                                          │
```
- **Visibility (`interface.panes.headers`):**
  - `always`: Always rendered inside the top of each pane card.
  - `on_hover` (great default for single-pane `tr3x` tabs!): Hidden cleanly when single-pane; slides/fades in as a floating overlay strip when hovering the top `24px` of a pane OR when invoking `client.pane_header.reveal`. (When a tab has `>= 2` split panes, optional smart mode shows subtle headers to identify splits.)
  - `hidden`: Never shown unless revealed via `client.pane_header.reveal`.
- **Left side (`gap: 6px`):**
  - Program/provider icon (`13×13px`).
  - Pane label or foreground program name (`12px` medium `fg.primary`, e.g., `zsh`, `claude`, `Monitor`). Double-click inline-renames (`pane.rename`).
  - Working directory basename/short path (`12px` regular `fg.muted`, e.g., `/tmp`, `~/Work/omarex`).
  - Live OSC 7501 status pill (`11.5px`): e.g. `● blocked · permission` (amber), `⠿ working` (blue/accent), `✕ error` (red).
  - If pane is **Zoomed**, displays a compact `ZOOMED` accent pill (`height: 18px`, `radius: 4px`) right beside the title.
- **Right side (5 action icon buttons, `20×20px` each, `gap: 4px`, shown on focused/hovered pane):**
  1. **Split Right** (`pane.split.right`) — vertical split box icon
  2. **Split Down** (`pane.split.down`) — horizontal split box icon
  3. **Float / Dock Pane** (`pane.float`) — overlapping squares icon
  4. **Zoom / Unzoom Pane** (`pane.zoom`) — diagonal expand/contract arrows (`⤢`)
  5. **Close Pane** (`pane.close`) — `×` icon
- **Program Color Fill (`padding.fill_with_program_colors` + `padding.include_pane_header`):**
  - When a full-screen TUI (`nvim`, `btop`, `tr3x`, `sup`) paints an explicit background color on edge cells, `omarex` extends that background seamlessly across the pane's `padding_x`/`padding_y` margins — and optionally into the pane header — eliminating jarring dark frames around themed TUIs.

#### C. Split Dividers & Resizing
- **Hit target:** `6px` wide/tall invisible hit zone centered on the visual gap/divider (`ew-resize` / `ns-resize` cursor).
- **Hover feedback:** Subtle `2px` pill line in `accent` at `0.5` alpha fades in while dragging.
- **Double-click divider:** Triggers `pane.balance` (equalizes sibling splits).
- **Smooth Terminal Resizing (`terminal.resizing.smooth`):** Immediately reflows local `libghostty-vt` grid on the exact frame the drag/layout changes (optimistic local resize) while sending the authoritative resize RPC to `rex-server`, preventing visual jitter or blank flashes.

#### D. Zoom (`pane.zoom`)
- Expands the focused split pane to 100% of the tab's workspace rect without altering the underlying split tree on the server.
- Other sibling panes remain alive in the background; switching focus (`pane.focus.*`) or pressing `pane.zoom` again cleanly restores the split tree.
- Clearly indicated by both a `ZOOMED` badge in the pane header (or top-right corner pill if pane headers are hidden) and a subtle indicator on the active tab pill.

---

### 2.7 Floating Panes, Layers & Minimized Capsules

Rex supports **floating layers** (`session.new_layer` / `pane.float` / `pane.new_floating`), which are essential for BOTH normal scratchpad workflows AND **`tr3x` (`/pr` drawer, `tr3x pair`, `tr3x update`, fallback `Find Thread` / `Rename` / `Delete`)**:

- **Normalized Coordinate System:** Server layer `bounds = [x, y, w, h]` in `[0.0, 1.0]` relative to the tab's workspace viewport rect.
  - Full-height right drawer (e.g. `tr3x` `/pr` wide mode `[0.45, 0.0, 0.55, 1.0]`): docks flush to right/top/bottom with `1px` crisp left border, subtle elevation shadow (`-8px 0 32px rgba(0,0,0,0.45)`), and optional light backdrop dim (`rgba(0,0,0,0.22)`) over the underlying conversation pane.
  - Centered floating overlay (e.g. `tr3x` Pair `[0.15, 0.2, 0.7, 0.5]`, `tr3x` `/pr` narrow `[0.03, 0.03, 0.94, 0.94]`, or user `pane.new_floating`): `10px` corner radius, `1px` accent-tinted active border, soft drop shadow (`0 16px 48px rgba(0,0,0,0.55)`).
- **Interactive Manipulation:**
  - Drag the floating pane's header bar to reposition; drag any of the 4 edges or 4 corners to resize (`8px` grab zone).
  - Clicking outside a floating pane optionally focuses the underlying tiled pane (without closing the floating layer; `Esc` / `q` inside `sup` or `pane.close` closes it).
  - Supports docking back into the split tree via `pane.float` toggle.
- **Minimized Floating Capsules (`pane.minimize_floating` / `pane.minimize_all_floating`):**
  - Minimizing a floating pane shrinks it into a compact **pill capsule** (`height: 28px`, `padding: 0 10px`, `radius: 14px`, elevated card with `1px` border + shadow) showing `[program icon] [label] [status dot]` stacked cleanly in the configured corner of the tab (`floating_windows.minimized_corner`: `bottom_right` default, or `bottom_left` / `top_right` / `top_left`, draggable between corners).
  - Clicking a capsule restores the floating pane immediately; hovering shows a quick `×` close button.

---

### 2.8 Focus States, Borders, Scrollbars, Selection & Empty States

#### A. Active Pane Focus & Inactive Dimming
Directly mirroring Rex's `Interface > Active Pane` settings:
- **Active Pane Edge Highlight (`active_panes.active_border`):**
  - Only drawn when `visible_pane_count >= 2` (or always if user enables single-pane border).
  - `1.5px` crisp outline following the pane's corner radius.
  - Color (`active_border_color`): defaults to Omarchy `accent` (`#e28203` in current theme), configurable to any terminal ANSI color or `foreground`.
  - Opacity (`active_border_opacity`): default `0.75` (`0.0..1.0`).
  - Animation (`active_border_animation`): `fade` (120ms crossfade), `move` (150ms spring glide rect between panes), or `off`.
- **Inactive Pane Dimming (`active_panes.fade`):**
  - Modes: `none`, `surface` (slightly darker card background), `content` (terminal glyph opacity `0.78`), `both` (default when `>= 2` panes).
- **Inactive Window Dimming (`active_panes.inactive_window`):**
  - When Wayland surface loses keyboard focus, optionally dims active border to `border.inactive` and lowers content contrast slightly (note: check Hyprland's `dim_inactive` to avoid double-dimming; default `false` on Omarchy unless opted in).
- **Focus Follows Mouse (`splits_focus.focus_follows_mouse`):** Optional sloppy focus between split panes.

#### B. Scrollbars & Smooth Scrolling
- **Overlay Scrollbar:** Hidden when idle; appears along the right edge of a pane (`width: 6px`, expanding to `8px` on hover, `radius: 3px`, `padding-right: 3px`) during scrollback activity or mouse proximity, fading out after `800ms` of inactivity.
- **Thumb color:** `rgba(fg, 0.25)` idle, `rgba(fg, 0.48)` on hover/drag.
- **Smooth Pixel Scrolling (`terminal.scrolling.smooth`):** Sub-line pixel scroll offset from Wayland `wl_pointer` high-resolution wheel/touchpad events, snapping cleanly to cell rows on copy mode or snap-to-bottom on new output/keypress.
- **Jump-to-Bottom Pill:** When scrolled up in history and new output arrives, display a compact floating pill (`↓ New output`, `height: 24px`, `radius: 12px`) anchored bottom-right (`12px` inset); clicking jumps smoothly to the live bottom.

#### C. Selection, Copy Shine & Copy Mode
- **Rounded Selection Corners (`terminal.selection.corners`):** `3px` corner radius on contiguous selection polygons, using Omarchy `selection` background (`#2a2a2a`) and contrast-safe foreground.
- **Copy Shine Animation (`terminal.selection.play_animation_on_copy`):** A crisp `220ms` diagonal specular sheen sweeping left-to-right across the copied text region to confirm copy visually.
- **Keyboard Copy Mode (`pane.copy_mode.enter`):**
  - Full Vim-style scrollback navigation (`h/j/k/l`, `w/b/e`, `0/^/$`, `H/M/L`, `Ctrl+U/D/B/F`, `g/G`, `{/}`, `/` & `?` search + `n/N`, `f/F/t/T` + `;/,`, numeric counts `0–9`, `v`/`V`/`Ctrl+V` char/line/block selection, `o` swap ends, `y`/`Enter` yank & exit, `D` yank to EOL, `Esc`/`q` exit).
  - Visual indicator: Compact **`COPY MODE`** (or `VISUAL` / `VISUAL LINE` / `VISUAL BLOCK`) pill in the top-right corner of the pane with current `[line/total]` position and search prompt at the bottom-left when `/` or `?` is active.

#### D. Empty, Loading & Error States
Every non-happy state has a clean, minimal, actionable design (no blank black screens, no raw stack traces):
1. **Connecting to `rex-server`:** Subtle centered monochrome Rex mark + `Connecting to rex-server…` with minimal indeterminate pulse (`200ms` delay before showing so fast local Unix-socket connects never flash).
2. **Server Unavailable / Disconnected:** Centered card with status icon, exact socket/URL attempted (e.g. `/run/user/1000/rex/server.sock`), one-click **`Reconnect`** button, and hint (`systemctl --user start rex-server` for local). If a previous connection drops mid-session, keep last terminal frames visible at `0.75` opacity and show a sleek top reconnect banner (`Connection lost · Reconnecting in 2s… [Retry Now]`) so scrollback remains readable!
3. **Zero Sessions:** Clean centered empty state with primary action **`New Session`** (`Ctrl+Shift+N`) and secondary **`Add Remote Host…`**.
4. **Empty `T3 Code` Session:** Handled automatically by `tr3x`, which opens a **New Thread** tab displaying project/model/worktree chips and keybinding hints (or "Start T3 Code" message if T3 isn't running).
5. **Offline Remote Host / Machine:** Keeps tabs visible in sidebar (matching `tr3x`'s `(offline)` behavior), renders a subtle muted `offline` pill next to the host header, and dims unreachable host icons without destroying layout state.

---

## 3. Command Palette, Modals, Pickers & Custom UI

### 3.1 Command Palette (`client.palette.toggle`)

Directly matching `yt_palette_dark.png` and `yt_theme_light_palette.png`:
- **Position & Geometry:** Horizontally centered, top edge at `18%` of window height, `width: clamp(460px, 52vw, 620px)`, `max-height: min(460px, 65vh)`.
- **Backdrop:** Subtle dimming scrim (`rgba(0,0,0,0.35)` dark / `rgba(0,0,0,0.18)` light); clicking outside dismisses.
- **Container Surface:** `surface.modal` (elevated card), `radius: 12px`, `border: 1px solid border.modal`, deep soft shadow (`0 20px 56px rgba(0,0,0,0.55)`).
- **Search Input Header (`height: 46px`, `padding: 0 14px`, `border-bottom: 1px solid border.hairline`):**
  - Leading magnifying glass icon (`15px`, `fg.muted`) — or context icon/breadcrumb title when in a sub-picker (e.g. `Change Theme`, `Find Thread`, `Rename Thread`).
  - Text input (`14px` regular, accent caret, placeholder `Type a command…`).
  - Trailing keycap chip (`height: 22px`, `padding: 0 6px`, `radius: 5px`, `border: 1px solid border.subtle`, `11px` monospace `fg.muted`) showing the invoking shortcut or `Esc`.
- **Results List (`padding: 6px`, virtualized for up to 2,000 items at 120fps):**
  - **Category Headers** (`10.5px` uppercase/semibold `fg.muted`, `padding: 8px 10px 4px`): e.g. `T3 Code`, `Session`, `Tab`, `Pane`, `View`, `Actions` (custom Lua actions), `Settings`.
  - **Action Row (`height: 36px` single-line / `48px` with subtitle, `padding: 0 10px`, `radius: 7px`):**
    - Optional leading icon (`15px`).
    - Main title (`13px` medium, matched fuzzy characters highlighted in `accent` or bold) + optional quieter subtitle (`11.5px` `fg.muted`).
    - Right-aligned `detail` text (e.g., `dark`/`light`, `3 windows`, git status `??`) OR shortcut keycap pills (e.g., `Ctrl` `Shift` `D`).
  - **Selected Row:** Rounded fill (`surface.row_selected`) + crisp `1px solid` subtle accent/border highlight (as seen in `t_022.png` and `yt_theme_light_palette.png`).
- **Fuzzy Search & Ranking:** Matches across `title`, `category`, `name` (e.g. `pane.split`), and `keywords` (both built-in and Lua `rex.action.keywords`), boosting recently used actions (MRU) and exact prefix matches.
- **Live Theme Preview (`client.theme.change`):** Navigating rows (`↑`/`↓`) in the Theme Picker immediately previews that theme on the workspace behind the modal; `Enter` commits, `Esc` reverts to the previous theme.

### 3.2 Quick Switchers: Switch Session (`session.switch`) & Go to Anything (`pane.go_to`)

- **Switch Session (`t_022.png`, `t_032.png`):**
  - Uses the exact Command Palette shell (`placeholder: "Switch session…"`).
  - Lists every session across connected hosts with:
    - Leading `8px` OSC 7501 status dot (amber for blocked/needs-you, red for error, blue/accent for working, muted gray for idle).
    - Session label (`13.5px` medium).
    - Right-aligned window count in tabular monospace (`3 windows`, `24 windows`) + host badge when multiple hosts are connected.
- **Go to Anything (`pane.go_to`):**
  - Unified fuzzy switcher across **Activity (blocked/error/done/working programs)**, **Sessions**, **Tabs (Windows)**, and **Panes (Blocks)** — including terminal OSC 2 titles (`tr3x` sets OSC 2 to the thread title specifically so Go to Anything finds threads!).
  - Pressing `Enter` jumps directly to the matching host → session → window → block.

### 3.3 Custom UI Prompts (`rex.ui.input` & `rex.ui.pick` — Control API rev 17+)

`tr3x` relies heavily on Rex's native prompt protocol (`client_prompt_requested` → `client.reply` / `client_prompt_ended`) for **Find Thread…**, **Open Project…**, **Answer…**, **Answer Next…**, **Snooze Thread…**, **Rename Thread…**, and **Delete Thread…**. `omarex` MUST implement 100% first-class support (advertising prompt capability during client registration) inside the Command Palette modal shell:

1. **Text Input Mode (`rex.ui.input`):**
   - Header displays `title` as a subtle top breadcrumb/badge above the input bar (e.g. `Rename Thread`).
   - Input pre-filled with `value` (all selected or cursor at end for immediate editing) and shows `placeholder` when empty.
   - Single primary confirmation row directly beneath the divider: displays `confirm` label (e.g. `Rename`, `Answer`) or default `Use "<typed text>"` with a `↵` Return keycap badge.
   - `Enter` submits exact typed text (including empty string or surrounding whitespace, per spec!). `Esc` or clicking backdrop sends `cancelled`.
2. **Single-Select Pick Mode (`rex.ui.pick`):**
   - Header shows `title` + filter input pre-filled with `value` and `placeholder` (e.g. `Filter threads`, `Run git push?`).
   - Renders up to **2,000 items** in exact server order (filtered live client-side by substring/fuzzy match across `title`, `subtitle`, `detail`, `id`).
   - Each row renders:
     - **Icon** (`15px` vector glyph mapped from all **16 official Rex icon names**: `document`, `folder`, `branch`, `terminal`, `code`, `server`, `database`, `package`, `globe`, `link`, `person`, `tag`, `clock`, `star`, `gear`, `plus`; unknown names omitted gracefully).
     - **`title`** (`13px` medium, defaults to `id`).
     - **`subtitle`** (`11.5px` `fg.muted`, second line when present).
     - **`detail`** (`11.5px` monospace/muted, far right of row — e.g. `9am`, `Open`, `Settled`, `??`).
   - **`allow_other = true`:** Whenever the user has typed non-empty text (or as a dedicated bottom/top submit row `confirm` / `Use "<text>"`), selecting that row submits `{ "text": "<typed>" }` instead of an item `id` (critical for `Snooze Thread…` custom durations like `45m` or `2d`, and free-text answers to agent questions!).
3. **Multi-Select Pick Mode (`rex.ui.pick` with `multiple = true`):**
   - Each row gains a leading `16×16px` rounded checkbox (`selected: true` pre-ticked).
   - `Space` or clicking toggles tick state; `Enter` toggles focused row (or confirms if on submit footer).
   - Sticky footer bar shows: `<N> selected` + primary button `[confirm label] (Ctrl+Enter)` — pressing **`Ctrl+Enter`** submits the array of ticked `id`s in original `items` order (or `[]` if none ticked).
4. **Multi-Client Lifecycle & Concurrency:**
   - Prompts NEVER block terminal input/rendering on other windows or background event updates; keyboard focus cleanly shifts to the modal and returns to the previously focused pane when closed.
   - If another connected Rex client (e.g. iPhone/Mac) answers or cancels first, `client_prompt_ended` immediately dismisses the modal cleanly with no error toast.
   - If `omarex` is already showing a prompt when another arrives, reply with `busy` per spec.

### 3.4 Key Modes, Key Sequence Hints & Find Overlay

- **Active Key Mode & Sequence Hints Pill (`terminal.keyboard.sequence_hints`):**
  - Anchored bottom-right of the active workspace (`14px` inset, elevated rounded card `radius: 8px`, `1px` border, shadow).
  - **In a Key Mode** (e.g. `resize` entered via `client.mode.enter`): shows a colored mode badge (`MODE: RESIZE` + `exclusive` indicator if exclusive) alongside a compact 2-column grid of bound keys in that mode (`←/→/↑/↓ Resize`, `= Balance`, `Esc Exit`).
  - **Mid Key Sequence** (e.g. after pressing `Ctrl+B` in `ctrl+b>c`): displays the prefix trail (`Ctrl+B ›`) and all valid follow-up keys + action titles ("which-key" style). Pressing `Esc` cancels immediately.
- **In-Pane Find Bar (`client.find.open`):**
  - Compact floating bar anchored top-right inside the focused pane (`width: 320px`, `height: 34px`, `radius: 8px`, elevated surface + shadow).
  - Contains: search input, live match counter (`3 / 18` or `0 / 0`), toggle buttons for Case Sensitive (`Aa`) and Regex (`.*`), Previous/Next chevrons (`↑` `↓`), and Close (`×` / `Esc`).
  - Highlights all visible matches in muted accent fill and the active match in bright accent fill with high-contrast text; draws match tick-marks on the pane's overlay scrollbar track.

### 3.5 In-App Notifications / Toast Banners

- Configurable anchor position (`notifications.overview.position`): 6-slot grid (`top_left`, `top_center`, `top_right` [default], `bottom_left`, `bottom_center`, `bottom_right`), `14px` inset from workspace edges.
- Card design: `max-width: 340px`, `padding: 10px 12px`, `radius: 10px`, elevated surface with `1px` border colored by severity (`accent` info, `warning` amber, `danger` red, `success` green) + subtle drop shadow.
- Shows icon, title (with source session/tab attribution), 1–2 line message, optional click action (e.g. jump to blocked tab or view config error), and dismiss `×` (`client.notice.dismiss`). Auto-dismisses info toasts after `4s`; persists errors/config warnings until dismissed or fixed.

### 3.6 Settings Window & Utility Modals

- **Settings (`client.settings`):** Native in-app modal/sheet or dedicated window with a clean left category sidebar matching Rex's organization:
  1. **General** (Client Label, Remote Control toggle, Startup Behavior, Linux Keymap Translation)
  2. **Interface** (Appearance/Omarchy Theme Sync vs Built-in Palettes, Classic vs Translucent Material, Toolbar Layout & Behavior, Tab Icons & Fill, Pane Density/Headers/Padding/Active Highlight/Fade, Minimized Floating Corner, Animations)
  3. **Terminal** (Typography & Nerd Font/Ligatures/Line Height, Contrast & Palette, Cursor Style/Blink, Smooth Scroll & Resize, Selection & Copy Shine, OSC 8 Links, Scrollback)
  4. **Notifications** (Toast Position, OSC 7501 Per-State Matrix for Tab/Desktop/Sound, OSC 9/777 Terminal Notifications)
  5. **Remote Hosts** (Local Unix Socket, SSH/Tailscale Hosts, Port Forwarding, Connection Status)
  6. **Integrations & `tr3x`** (Live status of `tr3x sync`, T3 Code session shortcuts, Hide Pane Headers in `tr3x` Session toggle)
  7. **Diagnostics** (`client.diagnostics` — Server version/API rev, active socket/host, `libghostty-vt` version, GPU adapter/FPS telemetry, `init.lua` config warnings, Copy Debug Info).
- **Insert Path Browser (`pane.insert_path`):** Command-palette-style file/folder browser starting at the focused pane's CWD, listing MRU paths first (`path_browser.recent_paths`), inserting shell-escaped path(s) into the terminal on `Enter`.

---

## 4. Full `tr3x` Experience Specification

On this Omarchy machine, `tr3x 0.1.8` runs continuously via `~/.config/rex/tr3x.lua`, syncing live T3 Code threads into the **`T3 Code`** session (currently 24 live windows). `tr3x` renders its rich TUI inside standard Rex terminal blocks, using advanced terminal & Rex API protocols. **Every single `tr3x` capability below MUST work seamlessly out of the box in `omarex`:**

### 4.1 T3 Code Session Priority Order & Sidebar Fidelity

`tr3x sync` automatically creates, labels, and reorders windows inside `T3 Code` (or per-machine `T3 Code · <host>`) following T3 Code's exact sidebar logic (`t3client/src/inbox.rs`):

1. **Tier 1 — Attention (`Place::Attention`, top of tab list):**
   - Threads whose status is `Approval`, `Input` (question), `Failed`, `Limited` (usage limit reached), `Woke` (snooze woke up unseen), or `Done` (completed turn unseen).
   - Standalone subagents waiting on an approval/question (even if their parent is settled/snoozed/archived) appear here as `↳ title · for Parent`.
2. **Tier 2 — Open (`Place::Open`, middle):**
   - Pinned threads (`pinOrderKey` / newest created first — stay put even while working), followed by active inbox threads sorted newest-returned first.
   - Working subagents appear directly nested beneath their parent thread as `↳ title`.
3. **Tier 3 — Working (`Place::Working`, bottom):**
   - Non-pinned threads actively running (`Working` / `Goal` / `Waiting` on background work) with nothing needing user input, sorted newest-sent first.
4. **Parked / Hidden:**
   - Snoozed, Settled, Archived, and tab-budget-parked idle threads have no window; they are instantly recoverable via **Find Thread…** (`t3_find`) or **Reopen Last Settled** (`t3_reopen_settled`).

**What `omarex` MUST render faithfully for `tr3x` tabs:**
- Strictly follow `rex-server`'s window ordering on every `session_view_changed` event (never apply competing alphabetical/MRU sorting to the tab list!).
- Parse each `tr3x` pane's OSC 7501 reports (`app=claude|codex|gemini|cursor|opencode|pi|grok|fx|antigravity|t3code`, `state`, `kind`, `title`, `msg`) to drive:
  - Provider brand icon in the `26×26px` sidebar tile (colored when active, monochrome when inactive).
  - Status dot badge on the tile corner (`blocked` → blue/amber attention dot, `error` → red dot, unseen `done` → accent dot) and `•••` pulse when `working`.
  - Line 2 subtitle showing `msg` / status (`pulse · Needs you`, `site · Working`, `notes · Done`, `tr3x · Limited · resets 3pm`).
- **One-click settle:** Closing a `T3 Code` tab (via `×` button, middle-click, or `client.tab.close`) sends `window.close` to `rex-server` without a blocking confirmation modal (matching `tr3x`'s "one click to settle, not two" design; `tr3x` watches `window_closed` and settles the thread, or merely hides it if running/blocked).
- **Smart `tr3x` pane chrome option:** `tr3x`'s own docs note that Rex's pane header buttons (`split`, `float`, `zoom`, `close`) add visual clutter above single-pane `tr3x` chat tabs. Setting `pane_headers = "on_hover"` by default (or an optional "Clean single-pane mode for T3 Code" setting) gives the exact crisp look of `hero.png` while preserving hover access to every pane control!

### 4.2 Conversation View, Inline Reply vs. Pane Reply, & Terminal Protocol Requirements

For `tr3x chat`, `tr3x compose`, and `tr3x new` to look and behave identically to Rex on macOS, `omarex`'s terminal surface (`libghostty-vt` + input/graphics pipeline) MUST implement:

| Feature | Exact Protocol / Behavior Required by `tr3x` |
| :--- | :--- |
| **Inline Reply (`/reply inline`)** | Proper **DECSTBM** scroll-region handling so conversation lines scroll off the top into native `omarex` scrollback while the Reply box stays pinned at the bottom and grows smoothly (1–8 lines, up to 35% height). |
| **Pane Reply (`/reply pane`)** | Respect programmatic `session.resize_pane` events emitted by `tr3x compose` so the bottom Reply split dynamically grows/shrinks to fit multi-line drafts without fighting user layout. |
| **Light/Dark Theme Sync (Mode 2031)** | Support **DEC Private Mode 2031** (color scheme preference notifications): immediately reply with `CSI ? 997 ; 1 n` (dark) or `CSI ? 997 ; 2 n` (light) when enabled/queried AND push updates live whenever the Omarchy theme switches between dark and light! This makes `tr3x`'s HTML snapshots, provider logos, and Reply background fill match `omarex` automatically. |
| **OSC 11 Background Query** | Answer `OSC 11 ; ? ST` with the active pane background RGB (`rgb:RRRR/GGGG/BBBB`) so `tr3x` (and other TUIs) can compute the exact elevated Reply input fill color dynamically — **improving even on Rex macOS**, which didn't answer OSC 11! |
| **Kitty Keyboard Protocol (`CSI > 1 u`)** | Implement level-1 disambiguate mode cleanly: `Shift+Enter` MUST encode as `CSI 13 ; 2 u` (inserts newline in Reply), `Alt+Enter` / `Ctrl+J` for newline, `Ctrl+Enter` / `Mod+Enter` for steer-vs-queue toggle, `Esc` distinct from `Alt` chords, and standard word/line editing keys. |
| **Selective Mouse Reporting** | Honor `tr3x`'s dynamic enabling/disabling of SGR mouse tracking (`?1000h/?1006h`): when `tr3x` turns mouse reporting ON (for clickable **Approve / Always / Decline** buttons, question options, model chips, and `/` slash menu), left-clicks route to the PTY; when `tr3x` turns it OFF (including for 1.5s after scroll wheel!), mouse drags perform native text selection and wheel scrolls native scrollback! Also support `Shift+Drag` override to always force native selection. |
| **Kitty Graphics Protocol (Images & HTML Previews)** | Full GPU texture rendering of Kitty Graphics (`APC G ... ST`) supporting: `a=T` (transmit+display), `f=100` (PNG), `t=f` (local file path — used by `tr3x` for inline project favicons `c=2,r=1`, provider logos, attached screenshots, and headless-Chromium HTML preview snapshots!), `t=d` (direct base64), `C=1` (do not move cursor), `q=2` (quiet), image IDs `i=<id>`, and delete commands `a=d,d=A` (all) and `a=d,d=I,i=<id>` (tagged icon painter IDs `>= 0x7000_0000` so icons clear without wiping conversation HTML/image previews!). Images MUST scroll smoothly with scrollback rows. |
| **Image & File Paste / Drop (Mode 5522 + Upload)** | Support **Kitty Paste Events (Mode 5522 / OSC 5522)** so pressing paste (`Ctrl+Shift+V` / `Shift+Insert` / `Super+V`) with an image on the Wayland clipboard (`image/png`, `image/jpeg`, `image/webp`) delivers MIME types + image bytes directly to `tr3x` Reply (showing as an image attachment chip!). Also handle Wayland drag-and-drop (`wl_data_device` `text/uri-list`): paste shell-escaped local paths locally, or upload over `com.superlogical.terminal.upload_begin` + `rex.data.bytes.v1` for remote hosts. |
| **OSC 8 Hyperlinks** | Detect and underline OSC 8 hyperlinks on hover (with subtle floating URL preview tooltip at bottom-left of pane). Clicking (or `Ctrl+Click`) opens `<origin>/<env>/<thread>` or `https://...` via `xdg-open`, and asks/honors `terminal.links.custom_schemes` for custom schemes (`t3code://`, `ssh://`, `vscode://`). |

### 4.3 Approval, Question & Secret Flows

`omarex` supports BOTH of `tr3x`'s interaction modes seamlessly:
1. **In-Pane Direct Interaction:**
   - **Approvals:** Clickable `[ Approve ]`, `[ Always ]`, `[ Decline ]` buttons rendered above Reply (or `Tab` + `Enter`, or `a`/`s`/`d` + `Enter`).
   - **Questions:** Clickable option buttons, number hotkeys (`1..9`), or free-text Reply input.
   - **Secrets:** Masked bullet (`••••`) input directly in the pane (never shown in plaintext palette).
2. **Cross-Tab Palette Triage (`t3_answer` & `t3_answer_next`):**
   - Pressing **`Ctrl+Alt+A`** (or running **Answer…** / **Answer Next…** in Command Palette) invokes `tr3x`'s Lua action, which drives `omarex`'s native `rex.ui.pick` / `rex.ui.input` modal:
     - Shows thread title as modal heading and `Run <command>?` / `Edit <file>?` / question text as placeholder.
     - Displays `Approve` (with `terminal`/`document` icon + command subtitle), `Approve Always`, `Decline`, `Go to Thread` (`link` icon), and (in `Answer Next…`) `Skip` (`clock` icon).
     - Multi-question prompts chain seamlessly inside the same modal; multi-select questions use `multiple = true` checkboxes; secret requests automatically jump to the thread's tab (`session.select`).

### 4.4 Pull Request Drawer (`/pr` & `/pr create`)

- Triggered via `/pr` or `/pr create` in Reply, or **Show Pull Request** / **Create Pull Request** (`t3_pr` / `t3_pr_create`) in Command Palette.
- `tr3x` computes the thread window's column width and spawns a floating Rex layer labeled `"Pull Request"` via `rex.session.new_layer`:
  - **Wide viewport (`>= 140` columns):** `bounds = [0.45, 0.0, 0.55, 1.0]` — sleek full-height right-side drawer covering 55% width while keeping the left 45% conversation visible.
  - **Narrow viewport (`< 140` columns):** `bounds = [0.03, 0.03, 0.94, 0.94]` — near-full-screen floating overlay card.
- Runs **`sup`** (for full GitHub PR review, diffs, checks, comments, `A` approve, `M` comment, `m` merge, `q` close) or falls back to `gh pr view` with one-keypress `sup` installation.
- `omarex` renders this layer with crisp elevation styling (left border + drop shadow + clean focus ring), routes focus immediately to the drawer, and returns focus cleanly to the conversation pane when `sup` exits or auto-closes on PR merge.

### 4.5 Find Thread, Open Project, Fit to This Device & Complete `tr3x` Action Surface

All 35 actions registered by `~/.config/rex/tr3x.lua` automatically appear in `omarex`'s Command Palette under the **`T3 Code`** category:
- **Navigation & Creation:** `Go to Thread That Needs You` (`t3_inbox`), `New Thread` (`t3_new`), `Open Project…` (`t3_project` — 2-step `rex.ui.pick`: project → worktree), `Find Thread…` (`t3_find` — `rex.ui.pick` across all Open/Snoozed/Settled/Archived threads on all machines), `Reopen Last Settled` (`t3_reopen_settled`), `Go to Thread This Subagent Works For` (`t3_parent`), `Go to Subagent` (`t3_child`), `Open Thread in T3 Code` (`t3_open`).
- **Triage & Lifecycle:** `Answer…` (`t3_answer`), `Answer Next…` (`t3_answer_next`), `Approve` (`t3_approve`), `Approve Always` (`t3_approve_always`), `Decline` (`t3_decline`), `Settle Thread` (`t3_settle`), `Snooze Thread…` (`t3_snooze`, `allow_other = true`), `Snooze 1h/3h/Tomorrow`, `Pin/Unpin Thread`, `Rename Thread…` (`rex.ui.input`), `Regenerate Thread Title`, `Fork Thread`, `Archive Thread`, `Delete Thread…` (`rex.ui.pick` confirmation), `Stop Thread`, `Force Stop Thread`, `Change Model…`.
- **Pull Requests, Multi-Device & Maintenance:** `Show Pull Request`, `Create Pull Request`, `Fit to This Device` (`t3_fit`), `Pair Another Machine…` (`t3_pair`, floating `[0.15, 0.2, 0.7, 0.5]`), `Update tr3x, T3 and Rex…` (`t3_update`, floating `[0.15, 0.2, 0.7, 0.5]`), `Sessions: One per Machine` / `One for All Machines`.
- **`Fit to This Device` & Ownership Polish:**
  - When `t3_fit` calls `session.own_block{ mode = "take" }`, `omarex` immediately claims ownership of all blocks in the session and sends its exact Wayland pixel/cell dimensions + current dark/light scheme (Mode 2031) so every tab cleanly re-wraps to this monitor.
  - Additionally, `omarex` automatically claims block ownership when switching tabs AND (configurable `auto_own_on_focus = true`) when regaining window focus after viewing the session on a phone/Mac — eliminating the stale-column-width paper cut documented in `tr3x`!

---

## 5. Linux / Omarchy Keybinding Specification (Zero Hyprland Conflicts)

### 5.1 Audited Omarchy Hyprland Conflict Map

Auditing live `hyprctl binds -j` on this Omarchy machine revealed that Hyprland globally intercepts almost all **`Super+*`**, **`Super+Shift+*`**, and **`Ctrl+Super+*`** chords — including **every single key `tr3x.lua` binds with `ctrl+cmd+*`** (`Ctrl+Super+T` = Theme menu, `Ctrl+Super+I` = Toggle idle lock, `Ctrl+Super+N` = Network, `Ctrl+Super+O` = Toggle menu, `Ctrl+Super+A` = Audio)!

Conversely:
- **`Ctrl+Shift+*`** has **ZERO** Hyprland conflicts.
- **`Ctrl+Alt+*`** has only 4 Hyprland bindings (`Delete`, `Tab`, `Shift+Tab`, `Space`), leaving all letters, brackets, digits, and arrows **100% conflict-free**!
- **`Alt+1..9`** and **`Alt+Arrows`** have **ZERO** Hyprland conflicts.
- **Omarchy Universal Clipboard:** Omarchy translates `Super+C` / `Super+V` into **`Ctrl+Insert`** / **`Shift+Insert`** for terminals (per `~/.config/ghostty/config`). `omarex` MUST bind both `Ctrl+Insert`/`Shift+Insert` AND `Ctrl+Shift+C`/`Ctrl+Shift+V`!

### 5.2 Complete Rex & `tr3x` Default Linux/Omarchy Keymap

> **Modifier convention:** macOS `Cmd` → **`Ctrl+Shift`** (primary app/workspace chrome); macOS `Alt+Cmd` / `Ctrl+Cmd` → **`Ctrl+Alt`** (pane navigation & `tr3x` global chords). Standard terminal control keys (`Ctrl+C`, `Ctrl+D`, `Ctrl+Z`, `Ctrl+R`, `Ctrl+L`, `Ctrl+A/E/K/U/W`) always pass straight through to the PTY untouched!

#### A. Workspace, Palette, Navigation & View
| Action | Rex Action ID | macOS Rex Default | **`omarex` Omarchy Default** |
| :--- | :--- | :--- | :--- |
| Command Palette… | `client.palette.toggle` | `Shift+Cmd+P` | **`Ctrl+Shift+P`** |
| Go to Anything… | `pane.go_to` | `Cmd+P` | **`Ctrl+Alt+P`** |
| Change Session… | `session.switch` | `Cmd+K` | **`Ctrl+Shift+K`** |
| New Session | `session.new` | `Cmd+N` | **`Ctrl+Shift+N`** |
| New Session on Host… | `session.new.on_host` | `Alt+Cmd+N` | **`Ctrl+Alt+Shift+N`** |
| New Window | `client.window.new` | `Shift+Cmd+N` | **`Ctrl+Alt+Shift+W`** (or via Palette) |
| Settings… | `client.settings` | `Cmd+,` | **`Ctrl+Shift+,`** |
| Rex Diagnostics | `client.diagnostics` | `Shift+Cmd+,` | **`Ctrl+Alt+,`** |
| Toggle Sidebar | `client.sidebar.toggle` | `Cmd+/` | **`Ctrl+Shift+B`** and **`Ctrl+Alt+/`** |
| Toggle Toolbar Layout | `client.tab_style.toggle` | `Shift+Cmd+S` | **`Ctrl+Shift+S`** |
| Toggle Activity Filter | `client.sidebar.activity.toggle` | `Cmd+.` | **`Ctrl+Shift+.`** |
| Focus Tab Filter | `client.sidebar.filter.focus` | `Alt+Cmd+F` | **`Ctrl+Alt+F`** |
| Reveal Toolbar | `client.toolbar.reveal` | `Alt+Cmd+T` | **`Ctrl+Alt+Shift+T`** |
| Reveal Pane Header | `client.pane_header.reveal` | `Alt+Shift+Cmd+T` | **`Ctrl+Alt+H`** |
| Dismiss Notice | `client.notice.dismiss` | `Shift+Cmd+.` | **`Ctrl+Alt+.`** |
| Refresh Sessions | `session.refresh` | `Cmd+R` | **`Ctrl+Shift+R`** |
| Navigate Back / Forward | `client.navigate.back` / `.forward` | `Alt+Cmd+[` / `]` | **`Alt+Left`** / **`Alt+Right`** (when not in PTY) or **`Ctrl+Alt+Shift+[`** / **`]`** |

#### B. Tabs (Windows)
| Action | Rex Action ID | macOS Rex Default | **`omarex` Omarchy Default** |
| :--- | :--- | :--- | :--- |
| New Tab | `client.tab.new` | `Cmd+T` | **`Ctrl+Shift+T`** |
| Close Pane (or Tab/Window) | `client.close` | `Cmd+W` | **`Ctrl+Shift+W`** |
| Close Tab | `client.tab.close` | `Alt+Cmd+W` | **`Ctrl+Alt+W`** |
| Undo Close | `session.undo_close` | `Shift+Cmd+T` | **`Ctrl+Alt+Shift+Z`** |
| Next / Previous Tab | `client.tab.next` / `.previous` | `Ctrl+Tab` / `Ctrl+Shift+Tab`, `Shift+Cmd+]` / `[` | **`Ctrl+Tab`** / **`Ctrl+Shift+Tab`**, **`Ctrl+PageDown`** / **`Ctrl+PageUp`** |
| Go to Tab 1..9 | `client.tab.goto` | `Cmd+1..9` | **`Alt+1..9`** |
| Move Tab Backward / Forward | `client.tab.move.backward` / `.forward` | `Ctrl+Shift+PageUp` / `PageDown` | **`Ctrl+Shift+PageUp`** / **`Ctrl+Shift+PageDown`** |
| Mark Tab as Unread | `client.tab.unread` | `Alt+Cmd+U` | **`Ctrl+Alt+U`** |

#### C. Panes, Splits, Floating & Zoom
| Action | Rex Action ID | macOS Rex Default | **`omarex` Omarchy Default** |
| :--- | :--- | :--- | :--- |
| Split Pane Right | `pane.split.right` | `Cmd+D`, `Shift+Cmd+Right` | **`Ctrl+Shift+D`** (or **`Ctrl+Shift+Right`**) |
| Split Pane Down | `pane.split.down` | `Shift+Cmd+D`, `Shift+Cmd+Down` | **`Ctrl+Shift+E`** (or **`Ctrl+Shift+Down`**) |
| Split Pane Left / Up | `pane.split.left` / `.up` | `Shift+Cmd+Left` / `Up` | **`Ctrl+Shift+Left`** / **`Ctrl+Shift+Up`** |
| Split Pane Best Fit | `pane.split.auto` | `Cmd+Enter` | **`Ctrl+Shift+Enter`** |
| Focus Pane Left/Right/Up/Down | `pane.focus.*` | `Alt+Cmd+Arrows` | **`Ctrl+Alt+Arrows`** (and **`Ctrl+Alt+H/J/K/L`** optional) |
| Focus Next / Prev Pane | `pane.focus_next` / `_previous` | `Cmd+]` / `Cmd+[` | **`Ctrl+Shift+]`** / **`Ctrl+Shift+[`** |
| Move / Swap Pane | `pane.move.*` | `Alt+Shift+Cmd+Arrows` | **`Ctrl+Alt+Shift+Arrows`** |
| Zoom Pane | `pane.zoom` | `Shift+Cmd+Enter` | **`Ctrl+Shift+Z`** (and **`Ctrl+Alt+Z`**) |
| Float / Dock Pane | `pane.float` | `Alt+Cmd+Enter` | **`Ctrl+Alt+Enter`** |
| Minimize Floating Pane | `pane.minimize_floating` | `Alt+Cmd+M` | **`Ctrl+Alt+M`** |
| Minimize All Floating | `pane.minimize_all_floating` | `Alt+Shift+Cmd+Enter` | **`Ctrl+Alt+Shift+M`** |
| Resize Pane | `pane.resize` | (via `resize` mode) | **`Ctrl+Shift+Alt+Arrows`** (matches Omarchy Ghostty) + Lua `resize` mode |
| Go to Directory… | `pane.go_to_directory` | `Shift+Cmd+G` | **`Ctrl+Alt+G`** |
| Insert Path… | `pane.insert_path` | `Ctrl+Cmd+I` | **`Ctrl+Alt+Shift+I`** |

#### D. Terminal, Clipboard, Find, Copy Mode & Typography
| Action | Rex Action ID | macOS Rex Default | **`omarex` Omarchy Default** |
| :--- | :--- | :--- | :--- |
| Copy | `pane.copy` | `Cmd+C` | **`Ctrl+Shift+C`** and **`Ctrl+Insert`** (Omarchy `Super+C`) |
| Paste | `pane.paste` | `Cmd+V` | **`Ctrl+Shift+V`** and **`Shift+Insert`** (Omarchy `Super+V`) |
| Enter Copy Mode | `pane.copy_mode.enter` | `Shift+Cmd+C` | **`Ctrl+Shift+X`** |
| Find… / Next / Previous | `client.find.open` / `.next` / `.previous` | `Cmd+F` / `Cmd+G` / `Shift+Cmd+G` | **`Ctrl+Shift+F`** / **`F3`** (or `Enter` in Find) / **`Shift+F3`** |
| Jump Prev / Next Prompt | `client.prompt.previous` / `.next` | `Cmd+Up` / `Cmd+Down` | **`Ctrl+Shift+Home`** / **`Ctrl+Shift+End`** (or **`Alt+Shift+Up/Down`**) |
| Increase / Decrease / Reset Font | `client.font.increase` / `.decrease` / `.reset` | `Cmd+=` / `Cmd+-` / `Cmd+0` | **`Ctrl+=`** (and **`Ctrl+Shift+=`**) / **`Ctrl+-`** / **`Ctrl+0`** |
| Quick Look / Preview Link/File | `client.quick_look` | `Cmd+Y` | **`Ctrl+Shift+Y`** |
| Newline in `tr3x` Reply | PTY CSI-u | `Shift+Enter` | **`Shift+Enter`** (`CSI 13 ; 2 u`) / `Alt+Enter` / `Ctrl+J` |
| Steer vs Queue in `tr3x` Reply | PTY CSI-u | `Cmd+Enter` | **`Ctrl+Enter`** (`CSI 13 ; 5 u`, or configurable) |

#### E. `tr3x` Global Shortcuts & Smart `ctrl+cmd` Translation
`~/.config/rex/tr3x.lua` binds 7 global shortcuts using `ctrl+cmd+<key>`. Because Hyprland steals `Ctrl+Super+*` on Omarchy, `omarex` includes an opt-in-by-default setting **`keymap.remap_ctrl_cmd_to_ctrl_alt = true`** on Linux: any server-supplied Lua binding whose chord is `ctrl+cmd+<key>` is automatically also bound to **`Ctrl+Alt+<key>`** (unless that `Ctrl+Alt` key is explicitly bound in `init.lua`)!

| `tr3x` Action | Lua Name | `tr3x.lua` Binding | **`omarex` Omarchy Shortcut (Auto-Mapped)** |
| :--- | :--- | :--- | :--- |
| Go to Thread That Needs You | `t3_inbox` | `ctrl+cmd+i` | **`Ctrl+Alt+I`** |
| New Thread | `t3_new` | `ctrl+cmd+n` | **`Ctrl+Alt+N`** |
| Open Project… | `t3_project` | `ctrl+cmd+o` | **`Ctrl+Alt+O`** |
| Answer… (in palette) | `t3_answer` | `ctrl+cmd+a` | **`Ctrl+Alt+A`** |
| Fit to This Device | `t3_fit` | `ctrl+cmd+t` | **`Ctrl+Alt+T`** |
| Go to Parent Thread | `t3_parent` | `ctrl+cmd+[` | **`Ctrl+Alt+[`** |
| Go to Subagent | `t3_child` | `ctrl+cmd+]` | **`Ctrl+Alt+]`** |

**Key Resolution Priority (exact Rex spec):**
1. Built-in `omarex` Linux defaults (lowest)
2. Server `init.lua` / `tr3x.lua` bindings (`rex.bind` / `rex.unbind`, key sequences `a>b`, key modes `mode/key`, with optional `ctrl+cmd` → `ctrl+alt` alias)
3. User overrides edited inside `omarex` (`client.shortcuts.edit`) (highest)

---

## 6. Omarchy Desktop Integration

### 6.1 Live Theme Integration (`colors.toml` + `shell.toml`)

`omarex` defaults to **`theme_source = "omarchy"`** (while also allowing built-in Rex dark/light themes or Ghostty/iTerm2 theme imports).

- **Watched Path:** `~/.local/state/omarchy/current/theme/colors.toml` (watching parent directory `~/.local/state/omarchy/current/theme` via `inotify` because `omarchy-theme-set` atomically replaces the symlink/files!).
- **Immediate Live Reload:** On file change, re-parse TOML in `< 1ms`, update GPU uniform color table, redraw chrome + terminal surfaces on the very next frame, and emit **Mode 2031** (`CSI ? 997 ; 1 n` for `mode = "dark"`, `2 n` for `mode = "light"`) to all owned terminal blocks so `tr3x` immediately flips logos & HTML previews to match!

#### Exact Token Mapping from Omarchy `colors.toml`

Example values shown from the live active Omarchy theme on this host (`mode = "dark"`):

| `omarex` UI Token | Omarchy `colors.toml` Source | Live Example Value | Usage |
| :--- | :--- | :--- | :--- |
| `appearance.mode` | `mode` (`"dark"` \| `"light"`) | `"dark"` | Controls alpha overlays, Mode 2031, icon contrast |
| `surface.frame` / `surface.sidebar` | `dark_background` (fallback: darken `background` 4%) | `#0e0e0e` | Outer workspace frame, sidebar, top tab bar |
| `surface.pane` / `terminal.bg` | `background` | `#121212` | Terminal grid & inset pane card surface |
| `surface.pane_header` | `background` (or `dark_background` at 50% blend) | `#121212` | Seamless top strip inside pane card |
| `surface.modal` | `background` lifted 4% (dark) / `light_foreground` (light) | `#181818` | Command Palette, Switch Session, Popovers, Toasts |
| `surface.tab_active` / `row_active` | `lighter_background` (or `rgba(fg, 0.08)` over frame) | `#2a2a2a` / `#1e1e1e` | Active tab pill, active sidebar row |
| `surface.row_hover` | `rgba(foreground, 0.05)` | `rgba(255,255,255,0.05)` | Hovered tabs, sidebar rows, palette rows |
| `surface.row_selected` | `selection` (or `accent` at 16% tint) | `#2a2a2a` | Keyboard-selected row in Palette / Pickers |
| `fg.primary` / `terminal.fg` | `foreground` | `#ffffff` | Primary text, active tab title, terminal text |
| `fg.secondary` | `dark_foreground` | `#bfbfbf` | Inactive tab titles, pane header icons |
| `fg.muted` | `muted` | `#9e9e9e` | Subtitles (`pulse · Working`), timestamps, cwd, placeholders |
| `accent.primary` | `accent` | `#e28203` | Active pane border, focus rings, search highlights, carets |
| `border.hairline` | `rgba(foreground, 0.08)` (dark) / `rgba(0,0,0,0.10)` (light) | `rgba(255,255,255,0.08)` | Sidebar divider, pane card outline, modal dividers |
| `border.subtle` | `rgba(foreground, 0.14)` | `rgba(255,255,255,0.14)` | Active tab pill outline, keycap chips, modal border |
| `status.blocked` | `yellow` / `orange` (high contrast vs frame) | `#f15647` / `#bca674` | OSC 7501 `blocked` (`permission` / `question` / `auth`) |
| `status.working` | `bright_blue` / `blue` / `accent` | `#ff980f` / `#e28203` | OSC 7501 `working` (`•••` dots, progress pie, spinner) |
| `status.error` | `red` / `bright_red` | `#ef3827` | OSC 7501 `error`, failed actions, low-quota red (`<=10%`) |
| `status.done` | `green` / `accent` | `#b79d62` / `#e28203` | OSC 7501 `done`, unseen badge dot, checkmarks |
| `terminal.selection_bg` | `selection` | `#2a2a2a` | Terminal text selection fill |
| `terminal.selection_fg` | `selection_foreground` | `#121212` | Terminal text selection foreground (contrast-checked!) |
| `terminal.cursor` | `bright_foreground` (fallback `foreground`) | `#ffffff` | Terminal block/bar/underline cursor |
| `terminal.ansi[0..15]` | `background`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `foreground`, `muted`, `bright_red`, `bright_green`, `bright_yellow`, `bright_blue`, `bright_magenta`, `bright_cyan`, `bright_foreground` | Matches `ghostty.conf` 1:1 | Exact parity with Omarchy's Ghostty palette |

> **Light Theme Parity:** When `mode = "light"`, all white-alpha hover/border tokens invert automatically to dark-ink alphas (`rgba(0,0,0,0.05)` hover, `rgba(0,0,0,0.10)` hairline, `rgba(0,0,0,0.16)` subtle border), drop shadows soften (`0 12px 36px rgba(0,0,0,0.14)`), and `surface.frame` shifts slightly cooler/darker than `surface.pane` — matching Rex's *Alpine Milk* light palette seen in `yt_theme_light_palette.png`.

### 6.2 Hyprland & Wayland Native Integration

1. **Hyprland Corner Rounding & Borders:**
   - Outer window rounding is delegated cleanly to Hyprland (`decoration:rounding`, currently `0` on this host; `8` when enabled in `looknfeel.lua`). `omarex` does **not** draw redundant outer window borders or client-side drop shadows around the main OS window.
   - Internal pane card corner radius defaults to `8px` in `comfortable` mode, with an option **`adaptive_rounding = "hyprland"`** that reads `decoration:rounding` on launch/IPC so users who prefer sharp Omarchy corners (`rounding = 0`) get crisp `0px` internal pane corners too (or `8px` rounded cards matching Rex macOS).
2. **Material Style (`Classic` vs `Translucent/Glass`):**
   - `classic` (default on Omarchy): Opaque `surface.frame` and `surface.pane` for maximum contrast, zero compositor blur cost, and VRR/direct-scanout cleanliness.
   - `translucent`: Honors background alpha (`0.88–0.94`) via Wayland ARGB surface transparency so Omarchy users who opt in via `o.transparent_window("omarex")` get native Hyprland Kawase blur seamlessly behind the sidebar and panes!
3. **Typography & Scaling:**
   - **Terminal Grid Font:** Reads `~/.config/ghostty/config` or Omarchy `omarchy-font-current` as initial default → **`JetBrainsMono Nerd Font`, Regular, 11pt** (with ligatures, Nerd Font fallback, emoji fallback, subpixel/grayscale AA, and fractional Wayland HiDPI scaling via `wp_fractional_scale_v1`).
   - **UI Chrome Font:** System sans-serif via `fontconfig` (`sans-serif` → `Liberation Sans` / `Inter` / `Suisse Intl`-class geometric grotesque), honoring Omarchy `shell.toml` `[font] base-size` (`12px` root scale: `10.5px` caption/age, `11px` metadata/section, `12.5px` tab/row title, `13.5px` palette input, `15px` headings) with crisp tabular numerals (`tnum`) for counts and timestamps.
4. **Desktop Entry, Icon & Notifications:**
   - Installs `/usr/share/applications/omarex.desktop` (`Name=Omarex`, `StartupWMClass=omarex`, `Categories=System;TerminalEmulator;Development;`, crisp vector SVG app icon).
   - Native Wayland D-Bus notifications (`org.freedesktop.Notifications`) with `default` action handler that raises/focuses the `omarex` window and selects the exact notifying session/tab/pane.
   - Native Wayland clipboard (`wl_data_device` / `zwp_primary_selection_v1`) supporting UTF-8 text AND `image/png` / `image/webp` / `image/jpeg` for `tr3x` screenshot paste, plus `text/uri-list` drag-and-drop.

---

## 7. Verification & Acceptance Checklist for GUI Implementation

Before `omarex` is considered visually complete, verify every item against live `rex-server` + `tr3x` on Omarchy:

- [ ] **Sidebar & Tab Strip Parity:** All 4 toolbar layouts (`left`, `right`, `top`, `bottom`) render cleanly with live `T3 Code` (24+ tabs), multi-host grouping, 2-line sidebar rows, brand-colored active provider icons + monochrome inactive icons, overlaid status dots, `•••` working animation, and right-aligned timestamps.
- [ ] **OSC 7501 End-to-End:** Emitting `working` (indeterminate & `progress=40`), `blocked` (`permission`, `question`, `auth`), `done`, `error`, and `clear` updates tab badges, pane headers, Switch Session dots, and Activity view within one frame.
- [ ] **Split Panes, Zoom & Floating Layers:** Multi-pane splits, draggable dividers, active edge highlight, inactive fade, `pane.zoom` with badge, `pane.float`, minimized floating capsules, and `tr3x` `/pr` right-side floating drawer (`[0.45, 0, 0.55, 1]`) all work smoothly at 60–120+ FPS.
- [ ] **Command Palette & Custom UI (rev 17):** `Ctrl+Shift+P`, `Ctrl+Shift+K`, `Ctrl+Alt+P`, and `tr3x` `rex.ui.input` / `rex.ui.pick` prompts (**Find Thread…**, **Open Project…**, **Answer…**, **Snooze Thread…** with `allow_other`, multi-select with `Ctrl+Enter`) render crisply and dismiss cleanly on multi-client race.
- [ ] **Full `tr3x` Interaction:** Inline Reply DECSTBM scrolling, `/reply pane` dynamic split resizing, clicking Approve/Always/Decline & `/` menu without breaking text selection/scrollback, `Shift+Enter` newline (`CSI 13 ; 2 u`), Kitty Graphics HTML previews & project icons (with tagged `d=I` clearing), Wayland image paste (Mode 5522), OSC 8 link clicking, and `Ctrl+Alt+T` (`Fit to This Device`) + Mode 2031 dark/light sync.
- [ ] **Omarchy Native Polish:** Zero Hyprland keybinding conflicts; `Super+C`/`Super+V` (`Ctrl+Insert`/`Shift+Insert`) clipboard works; running `omarchy-theme-set` live-updates all chrome + terminal + `tr3x` colors with zero restart; crisp `JetBrainsMono Nerd Font` + HiDPI rendering.
