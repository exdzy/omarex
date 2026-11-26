//! Unified centered modals (`460–620px`) driven by `FocusTarget::Modal` + `PaletteMatcher` +
//! `TextInput`, including Command Palette, Switch Session, Go to Anything, Activity Picker,
//! live Settings panel, Rename / Add Host, Keyboard Shortcuts Help, native `rex.ui` `input` &
//! `pick` overlay (`PromptSession` with timeout bar & multi-select), Which-Key chord/mode popup,
//! inline terminal Find bar, and 6-anchor Toast notifications.

use std::time::Instant;

use omarchy_theme::{MaterialStyle, Theme, UiTokens};
use tr3x_bridge::keymap::WhichKeyHint;
use tr3x_bridge::prompts::{ClientReplyPayload, PromptKind, PromptSession};

use crate::draw::{
    FontKind, FontWeight, HitTarget, ModalKind, SettingChange, ShadowSpec, TextAlign, UiAction,
    UiDrawList, UiInteraction, cap_radius, z_layer,
};
use crate::icons::IconId;
use crate::layout::{
    ChromeLayoutConfig, PaneDensity, PaneHeaderMode, RectF, ToolbarBehavior, ToolbarPosition,
};
use crate::palette::{PaletteItem, PaletteMatcher, ScoredMatch, highlight_spans};
use crate::text_input::TextInput;

/// Normalized key events understood by modals and overlay inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalKey {
    Up,
    Down,
    PageUp,
    PageDown,
    Enter,
    CtrlEnter,
    Space,
    Escape,
    Backspace,
    Delete,
    WordBackspace,
    Left { extend: bool },
    Right { extend: bool },
    Home { extend: bool },
    End { extend: bool },
    SelectAll,
    Char(char),
    Paste(String),
}

/// Result of handling a keyboard event inside [`ModalState`].
#[derive(Debug, Clone, PartialEq)]
pub enum ModalKeyOutcome {
    Consumed,
    Action(UiAction),
    ReplyPrompt(ClientReplyPayload),
    Closed,
}

/// Interactive settings snapshot displayed inside the Settings modal (`Ctrl+Shift+,`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SettingsViewSnapshot {
    pub toolbar_position: ToolbarPosition,
    pub toolbar_behavior: ToolbarBehavior,
    pub density: PaneDensity,
    pub pane_header_mode: PaneHeaderMode,
    pub material: MaterialStyle,
    pub highlight_active_border: bool,
    pub fade_inactive_panes: bool,
    pub adaptive_rounding: bool,
}

impl SettingsViewSnapshot {
    #[must_use]
    pub fn from_config(
        cfg: &ChromeLayoutConfig,
        material: MaterialStyle,
        highlight_active_border: bool,
        fade_inactive_panes: bool,
    ) -> Self {
        Self {
            toolbar_position: cfg.toolbar_position,
            toolbar_behavior: cfg.toolbar_behavior,
            density: cfg.density,
            pane_header_mode: cfg.pane_header_mode,
            material,
            highlight_active_border,
            fade_inactive_panes,
            adaptive_rounding: cfg.hyprland_rounding.is_some(),
        }
    }
}

/// Active modal content backing `FocusTarget::Modal(ModalKind)`.
#[derive(Debug, Clone)]
pub enum ModalContent {
    List {
        kind: ModalKind,
        title: String,
        shortcut_badge: Option<String>,
        input: TextInput,
        items: Vec<PaletteItem>,
        matches: Vec<ScoredMatch>,
        selected_row: usize,
        scroll_row: usize,
    },
    RexPrompt {
        session: PromptSession,
        input: TextInput,
        scroll_row: usize,
    },
    Settings {
        snapshot: SettingsViewSnapshot,
        selected_row: usize,
    },
    Rename {
        target_id: String,
        title: String,
        input: TextInput,
    },
    AddHost {
        input: TextInput,
    },
    ShortcutsHelp {
        scroll_row: usize,
    },
}

/// Unified modal controller handling filtering, selection, keyboard navigation, and rendering.
pub struct ModalState {
    pub content: ModalContent,
    matcher: PaletteMatcher,
}

impl ModalState {
    /// Opens a fuzzy-searchable list modal (Command Palette, Switch Session, Go to Anything, Activity Picker).
    #[must_use]
    pub fn open_list(
        kind: ModalKind,
        title: impl Into<String>,
        placeholder: impl Into<String>,
        shortcut_badge: Option<String>,
        items: Vec<PaletteItem>,
    ) -> Self {
        let mut matcher = PaletteMatcher::new();
        let matches = matcher.filter("", &items);
        Self {
            content: ModalContent::List {
                kind,
                title: title.into(),
                shortcut_badge,
                input: TextInput::new(placeholder),
                items,
                matches,
                selected_row: 0,
                scroll_row: 0,
            },
            matcher,
        }
    }

    /// Opens a native `rex.ui` `input` or `pick` overlay backed by `tr3x_bridge::prompts::PromptSession`.
    #[must_use]
    pub fn open_rex_prompt(session: PromptSession) -> Self {
        let mut input = TextInput::with_value(&session.placeholder, &session.input_text);
        if session.kind == PromptKind::Input && !session.input_text.is_empty() {
            input.select_all();
        }
        Self {
            content: ModalContent::RexPrompt {
                session,
                input,
                scroll_row: 0,
            },
            matcher: PaletteMatcher::new(),
        }
    }

    /// Opens the live Settings panel (`Ctrl+Shift+,`).
    #[must_use]
    pub fn open_settings(snapshot: SettingsViewSnapshot) -> Self {
        Self {
            content: ModalContent::Settings {
                snapshot,
                selected_row: 0,
            },
            matcher: PaletteMatcher::new(),
        }
    }

    /// Opens the inline Rename modal pre-filled with `current_label`.
    #[must_use]
    pub fn open_rename(
        target_id: impl Into<String>,
        title: impl Into<String>,
        current_label: impl Into<String>,
    ) -> Self {
        let mut input = TextInput::with_value("New label…", current_label);
        input.select_all();
        Self {
            content: ModalContent::Rename {
                target_id: target_id.into(),
                title: title.into(),
                input,
            },
            matcher: PaletteMatcher::new(),
        }
    }

    /// Opens the Add Remote Host modal (`Name = https://...`, `https://`, `ssh://`, or `unix://`).
    #[must_use]
    pub fn open_add_host() -> Self {
        Self {
            content: ModalContent::AddHost {
                input: TextInput::new("Name = https://box.tailnet.ts.net or ssh://user@host"),
            },
            matcher: PaletteMatcher::new(),
        }
    }

    /// Opens the Omarchy conflict-free Keyboard Shortcuts Help modal (`?`).
    #[must_use]
    pub fn open_shortcuts_help() -> Self {
        Self {
            content: ModalContent::ShortcutsHelp { scroll_row: 0 },
            matcher: PaletteMatcher::new(),
        }
    }

    #[must_use]
    pub fn kind(&self) -> ModalKind {
        match &self.content {
            ModalContent::List { kind, .. } => *kind,
            ModalContent::RexPrompt { .. } => ModalKind::RexPrompt,
            ModalContent::Settings { .. } => ModalKind::Settings,
            ModalContent::Rename { .. } => ModalKind::Rename,
            ModalContent::AddHost { .. } => ModalKind::AddHost,
            ModalContent::ShortcutsHelp { .. } => ModalKind::ShortcutsHelp,
        }
    }

    /// Processes a keyboard event while the modal holds `FocusTarget::Modal`.
    pub fn handle_key(&mut self, key: ModalKey) -> ModalKeyOutcome {
        match &mut self.content {
            ModalContent::List {
                input,
                items,
                matches,
                selected_row,
                scroll_row,
                ..
            } => match key {
                ModalKey::Escape => ModalKeyOutcome::Closed,
                ModalKey::Up => {
                    if !matches.is_empty() {
                        *selected_row = (*selected_row + matches.len() - 1) % matches.len();
                        ensure_row_visible(*selected_row, scroll_row, 8);
                    }
                    ModalKeyOutcome::Consumed
                }
                ModalKey::Down => {
                    if !matches.is_empty() {
                        *selected_row = (*selected_row + 1) % matches.len();
                        ensure_row_visible(*selected_row, scroll_row, 8);
                    }
                    ModalKeyOutcome::Consumed
                }
                ModalKey::PageUp => {
                    *selected_row = selected_row.saturating_sub(6);
                    ensure_row_visible(*selected_row, scroll_row, 8);
                    ModalKeyOutcome::Consumed
                }
                ModalKey::PageDown => {
                    if !matches.is_empty() {
                        *selected_row = (*selected_row + 6).min(matches.len() - 1);
                        ensure_row_visible(*selected_row, scroll_row, 8);
                    }
                    ModalKeyOutcome::Consumed
                }
                ModalKey::Enter | ModalKey::CtrlEnter => {
                    if let Some(m) = matches.get(*selected_row) {
                        let id = items[m.index].id.clone();
                        ModalKeyOutcome::Action(UiAction::RunCommand { action_id: id })
                    } else {
                        ModalKeyOutcome::Closed
                    }
                }
                other => {
                    if apply_text_key(input, other) {
                        *matches = self.matcher.filter(input.text(), items);
                        *selected_row = 0;
                        *scroll_row = 0;
                    }
                    ModalKeyOutcome::Consumed
                }
            },
            ModalContent::RexPrompt {
                session,
                input,
                scroll_row,
            } => match key {
                ModalKey::Escape => ModalKeyOutcome::ReplyPrompt(session.cancel()),
                ModalKey::Up if session.kind == PromptKind::Pick => {
                    session.move_focus(-1);
                    ensure_row_visible(session.focused_row, scroll_row, 8);
                    ModalKeyOutcome::Consumed
                }
                ModalKey::Down if session.kind == PromptKind::Pick => {
                    session.move_focus(1);
                    ensure_row_visible(session.focused_row, scroll_row, 8);
                    ModalKeyOutcome::Consumed
                }
                ModalKey::PageUp if session.kind == PromptKind::Pick => {
                    session.move_focus(-6);
                    ensure_row_visible(session.focused_row, scroll_row, 8);
                    ModalKeyOutcome::Consumed
                }
                ModalKey::PageDown if session.kind == PromptKind::Pick => {
                    session.move_focus(6);
                    ensure_row_visible(session.focused_row, scroll_row, 8);
                    ModalKeyOutcome::Consumed
                }
                ModalKey::Space if session.multiple => {
                    session.toggle_focused_selection();
                    ModalKeyOutcome::Consumed
                }
                ModalKey::Enter => {
                    if let Some(reply) = session.submit_enter() {
                        ModalKeyOutcome::ReplyPrompt(reply)
                    } else {
                        ModalKeyOutcome::Consumed
                    }
                }
                ModalKey::CtrlEnter => {
                    if let Some(reply) = session.submit_confirm() {
                        ModalKeyOutcome::ReplyPrompt(reply)
                    } else {
                        ModalKeyOutcome::Consumed
                    }
                }
                other => {
                    if apply_text_key(input, other) {
                        session.set_input_text(input.text());
                        *scroll_row = 0;
                    }
                    ModalKeyOutcome::Consumed
                }
            },
            ModalContent::Settings {
                snapshot,
                selected_row,
            } => {
                const ROW_COUNT: usize = 8;
                match key {
                    ModalKey::Escape => ModalKeyOutcome::Closed,
                    ModalKey::Up => {
                        *selected_row = (*selected_row + ROW_COUNT - 1) % ROW_COUNT;
                        ModalKeyOutcome::Consumed
                    }
                    ModalKey::Down => {
                        *selected_row = (*selected_row + 1) % ROW_COUNT;
                        ModalKeyOutcome::Consumed
                    }
                    ModalKey::Enter | ModalKey::Space => {
                        if let Some(change) = cycle_setting_row(*snapshot, *selected_row) {
                            apply_setting_to_snapshot(snapshot, &change);
                            ModalKeyOutcome::Action(UiAction::UpdateSetting(change))
                        } else {
                            ModalKeyOutcome::Consumed
                        }
                    }
                    _ => ModalKeyOutcome::Consumed,
                }
            }
            ModalContent::Rename {
                target_id, input, ..
            } => match key {
                ModalKey::Escape => ModalKeyOutcome::Closed,
                ModalKey::Enter | ModalKey::CtrlEnter => {
                    let trimmed = input.text().trim();
                    if trimmed.is_empty() {
                        ModalKeyOutcome::Closed
                    } else {
                        ModalKeyOutcome::Action(UiAction::SubmitRename {
                            target_id: target_id.clone(),
                            new_label: trimmed.to_owned(),
                        })
                    }
                }
                other => {
                    apply_text_key(input, other);
                    ModalKeyOutcome::Consumed
                }
            },
            ModalContent::AddHost { input } => match key {
                ModalKey::Escape => ModalKeyOutcome::Closed,
                ModalKey::Enter | ModalKey::CtrlEnter => {
                    let endpoint = input.text().trim();
                    if endpoint.is_empty() {
                        ModalKeyOutcome::Closed
                    } else {
                        ModalKeyOutcome::Action(UiAction::SubmitAddHost {
                            endpoint: endpoint.to_owned(),
                        })
                    }
                }
                other => {
                    apply_text_key(input, other);
                    ModalKeyOutcome::Consumed
                }
            },
            ModalContent::ShortcutsHelp { scroll_row } => match key {
                ModalKey::Escape | ModalKey::Enter => ModalKeyOutcome::Closed,
                ModalKey::Up => {
                    *scroll_row = scroll_row.saturating_sub(1);
                    ModalKeyOutcome::Consumed
                }
                ModalKey::Down => {
                    *scroll_row = (*scroll_row + 1).min(SHORTCUT_HELP_ROWS.len().saturating_sub(1));
                    ModalKeyOutcome::Consumed
                }
                _ => ModalKeyOutcome::Consumed,
            },
        }
    }

    /// Activates a clicked row index inside the modal.
    pub fn activate_row(&mut self, row_index: usize) -> ModalKeyOutcome {
        match &mut self.content {
            ModalContent::List {
                items,
                matches,
                selected_row,
                ..
            } => {
                if let Some(m) = matches.get(row_index) {
                    *selected_row = row_index;
                    ModalKeyOutcome::Action(UiAction::RunCommand {
                        action_id: items[m.index].id.clone(),
                    })
                } else {
                    ModalKeyOutcome::Consumed
                }
            }
            ModalContent::RexPrompt { session, .. } => {
                session.focused_row = row_index;
                if let Some(reply) = session.submit_enter() {
                    ModalKeyOutcome::ReplyPrompt(reply)
                } else {
                    ModalKeyOutcome::Consumed
                }
            }
            ModalContent::Settings {
                snapshot,
                selected_row,
            } => {
                *selected_row = row_index;
                if let Some(change) = cycle_setting_row(*snapshot, row_index) {
                    apply_setting_to_snapshot(snapshot, &change);
                    ModalKeyOutcome::Action(UiAction::UpdateSetting(change))
                } else {
                    ModalKeyOutcome::Consumed
                }
            }
            _ => ModalKeyOutcome::Consumed,
        }
    }

    /// Toggles a multi-select `rex.ui.pick` checkbox row without submitting the modal.
    pub fn toggle_checkbox_row(&mut self, row_index: usize) -> ModalKeyOutcome {
        if let ModalContent::RexPrompt { session, .. } = &mut self.content
            && session.multiple
        {
            session.focused_row = row_index;
            session.toggle_focused_selection();
            return ModalKeyOutcome::Consumed;
        }
        self.activate_row(row_index)
    }

    /// Activates a Settings row by its stable key (called when clicking `HitTarget::SettingsToggle`).
    pub fn activate_setting_key(&mut self, key: &str) -> ModalKeyOutcome {
        let row_index = match key {
            "toolbar_position" => 0,
            "toolbar_behavior" => 1,
            "pane_density" => 2,
            "pane_headers" => 3,
            "material" => 4,
            "active_border" => 5,
            "fade_inactive" => 6,
            "adaptive_rounding" => 7,
            _ => return ModalKeyOutcome::Consumed,
        };
        self.activate_row(row_index)
    }

    /// Scrolls a scrollable modal list by `delta_rows` (`> 0` scrolls down, `< 0` scrolls up).
    pub fn scroll_by(&mut self, delta_rows: isize) -> bool {
        match &mut self.content {
            ModalContent::List {
                matches,
                scroll_row,
                ..
            } => {
                let max_scroll = matches.len().saturating_sub(8);
                let next =
                    (*scroll_row as isize + delta_rows).clamp(0, max_scroll as isize) as usize;
                let changed = next != *scroll_row;
                *scroll_row = next;
                changed
            }
            ModalContent::RexPrompt {
                session,
                scroll_row,
                ..
            } => {
                let max_scroll = session.visible_row_count().saturating_sub(8);
                let next =
                    (*scroll_row as isize + delta_rows).clamp(0, max_scroll as isize) as usize;
                let changed = next != *scroll_row;
                *scroll_row = next;
                changed
            }
            ModalContent::ShortcutsHelp { scroll_row } => {
                let max_scroll = SHORTCUT_HELP_ROWS.len().saturating_sub(10);
                let next =
                    (*scroll_row as isize + delta_rows).clamp(0, max_scroll as isize) as usize;
                let changed = next != *scroll_row;
                *scroll_row = next;
                changed
            }
            _ => false,
        }
    }
}

fn apply_setting_to_snapshot(snapshot: &mut SettingsViewSnapshot, change: &SettingChange) {
    match *change {
        SettingChange::ToolbarPosition(pos) => snapshot.toolbar_position = pos,
        SettingChange::ToolbarBehavior(beh) => snapshot.toolbar_behavior = beh,
        SettingChange::PaneDensity(den) => snapshot.density = den,
        SettingChange::PaneHeaderMode(mode) => snapshot.pane_header_mode = mode,
        SettingChange::ToggleTranslucentMaterial => {
            snapshot.material = match snapshot.material {
                MaterialStyle::Classic => MaterialStyle::Translucent { alpha: 0.90 },
                MaterialStyle::Translucent { .. } => MaterialStyle::Classic,
            };
        }
        SettingChange::ToggleActivePaneBorder => {
            snapshot.highlight_active_border = !snapshot.highlight_active_border;
        }
        SettingChange::ToggleFadeInactivePanes => {
            snapshot.fade_inactive_panes = !snapshot.fade_inactive_panes;
        }
        SettingChange::ToggleAdaptiveRounding => {
            snapshot.adaptive_rounding = !snapshot.adaptive_rounding;
        }
    }
}

fn ensure_row_visible(row: usize, scroll_row: &mut usize, visible_rows: usize) {
    if row < *scroll_row {
        *scroll_row = row;
    } else if row >= *scroll_row + visible_rows {
        *scroll_row = row + 1 - visible_rows;
    }
}

fn apply_text_key(input: &mut TextInput, key: ModalKey) -> bool {
    match key {
        ModalKey::Space => {
            input.insert_char(' ');
            true
        }
        ModalKey::Char(c) => {
            input.insert_char(c);
            true
        }
        ModalKey::Paste(s) => {
            input.paste(&s);
            true
        }
        ModalKey::Backspace => input.backspace(),
        ModalKey::Delete => input.delete_forward(),
        ModalKey::WordBackspace => input.delete_word_backward(),
        ModalKey::Left { extend } => {
            input.move_left(extend);
            false
        }
        ModalKey::Right { extend } => {
            input.move_right(extend);
            false
        }
        ModalKey::Home { extend } => {
            input.move_home(extend);
            false
        }
        ModalKey::End { extend } => {
            input.move_end(extend);
            false
        }
        ModalKey::SelectAll => {
            input.select_all();
            false
        }
        _ => false,
    }
}

fn cycle_setting_row(s: SettingsViewSnapshot, row: usize) -> Option<SettingChange> {
    match row {
        0 => Some(SettingChange::ToolbarPosition(match s.toolbar_position {
            ToolbarPosition::Left => ToolbarPosition::Right,
            ToolbarPosition::Right => ToolbarPosition::Top,
            ToolbarPosition::Top => ToolbarPosition::Bottom,
            ToolbarPosition::Bottom => ToolbarPosition::Left,
        })),
        1 => Some(SettingChange::ToolbarBehavior(match s.toolbar_behavior {
            ToolbarBehavior::Always => ToolbarBehavior::OnHover,
            ToolbarBehavior::OnHover => ToolbarBehavior::Hidden,
            ToolbarBehavior::Hidden => ToolbarBehavior::Always,
        })),
        2 => Some(SettingChange::PaneDensity(match s.density {
            PaneDensity::Comfortable => PaneDensity::Compact,
            PaneDensity::Compact => PaneDensity::Comfortable,
        })),
        3 => Some(SettingChange::PaneHeaderMode(match s.pane_header_mode {
            PaneHeaderMode::OnHover => PaneHeaderMode::Always,
            PaneHeaderMode::Always => PaneHeaderMode::Hidden,
            PaneHeaderMode::Hidden => PaneHeaderMode::OnHover,
        })),
        4 => Some(SettingChange::ToggleTranslucentMaterial),
        5 => Some(SettingChange::ToggleActivePaneBorder),
        6 => Some(SettingChange::ToggleFadeInactivePanes),
        7 => Some(SettingChange::ToggleAdaptiveRounding),
        _ => None,
    }
}

/// Renders the centered modal (`460–620px`) over a dimming backdrop scrim.
pub fn render_modal(
    draw_list: &mut UiDrawList,
    viewport: RectF,
    modal: &ModalState,
    interaction: UiInteraction<'_>,
    theme: &Theme,
    now: Instant,
) {
    if viewport.is_empty() {
        return;
    }
    let tokens = theme.effective_ui();

    // Backdrop scrim
    draw_list.push_rect(viewport, tokens.surface_scrim, None, 0.0, 0.0, None);
    draw_list.push_hit(viewport, HitTarget::ModalBackdrop, z_layer::MODAL_BACKDROP);

    let modal_w = (viewport.w * 0.52)
        .clamp(460.0, 620.0)
        .min((viewport.w - 32.0).max(200.0));
    let max_h = (viewport.h * 0.65).clamp(160.0, 460.0);
    let top_y =
        (viewport.y + viewport.h * 0.16).min((viewport.bottom() - 180.0).max(viewport.y + 16.0));
    let left_x = viewport.x + (viewport.w - modal_w) * 0.5;
    let card_r = cap_radius(12.0, theme.rounding);

    let shadow = ShadowSpec {
        offset_x: 0.0,
        offset_y: 20.0,
        blur_radius: 56.0,
        spread: 0.0,
        color: tokens.shadow_color,
    };

    match &modal.content {
        ModalContent::List {
            title,
            shortcut_badge,
            input,
            items,
            matches,
            selected_row,
            scroll_row,
            ..
        } => {
            let visible_count = matches.len().min(8);
            let list_h = if matches.is_empty() {
                54.0
            } else {
                visible_count as f32 * 44.0 + 12.0
            };
            let card_h = (46.0 + list_h).min(max_h);
            let card = RectF::new(
                left_x.round(),
                top_y.round(),
                modal_w.round(),
                card_h.round(),
            );

            draw_list.push_rect(
                card,
                tokens.surface_modal,
                Some(tokens.border_modal),
                1.0,
                card_r,
                Some(shadow),
            );
            render_modal_search_header(
                draw_list,
                RectF::new(card.x, card.y, card.w, 46.0),
                input,
                shortcut_badge.as_deref().unwrap_or(title.as_str()),
                &tokens,
                theme.rounding,
            );

            let list_rect = RectF::new(card.x, card.y + 46.0, card.w, (card.h - 46.0).max(0.0));
            draw_list.push_clip(list_rect);

            if matches.is_empty() {
                draw_list.push_text(
                    RectF::new(
                        list_rect.x + 16.0,
                        list_rect.y + 16.0,
                        (list_rect.w - 32.0).max(0.0),
                        20.0,
                    ),
                    "No matching items",
                    tokens.fg_muted,
                    FontKind::Sans,
                    12.5,
                    FontWeight::Regular,
                    TextAlign::Center,
                    true,
                );
            } else {
                let mut y = list_rect.y + 6.0;
                let row_r = cap_radius(7.0, theme.rounding);
                for (vis_idx, m) in matches.iter().enumerate().skip(*scroll_row).take(8) {
                    let item = &items[m.index];
                    let row_rect =
                        RectF::new(list_rect.x + 6.0, y, (list_rect.w - 12.0).max(0.0), 42.0);
                    let row_target = HitTarget::ModalRow { index: vis_idx };
                    let is_sel = vis_idx == *selected_row;
                    let is_hov = interaction.is_hovered(&row_target);
                    if is_sel {
                        draw_list.push_rect(
                            row_rect,
                            tokens.surface_row_selected,
                            Some(tokens.border_subtle),
                            1.0,
                            row_r,
                            None,
                        );
                    } else if is_hov {
                        draw_list.push_rect(
                            row_rect,
                            tokens.surface_row_hover,
                            Some(tokens.border_hairline),
                            1.0,
                            row_r,
                            None,
                        );
                    }

                    let mut tx = row_rect.x + 10.0;
                    if let Some(icon) = item.icon {
                        draw_list.push_icon(
                            RectF::new(tx, row_rect.y + 13.0, 15.0, 15.0),
                            icon,
                            if is_sel {
                                tokens.accent_primary
                            } else {
                                tokens.fg_secondary
                            },
                        );
                        tx += 24.0;
                    }

                    let right_text = item.shortcut.as_deref().or(item.detail.as_deref());
                    let right_w = right_text
                        .map_or(0.0, |t| (t.chars().count() as f32 * 6.8 + 16.0).min(130.0));
                    let text_w = (row_rect.right() - tx - right_w - 10.0).max(0.0);

                    let title_y = if item.subtitle.is_some() {
                        row_rect.y + 5.0
                    } else {
                        row_rect.y + 12.0
                    };
                    let hi_ranges: Vec<_> = highlight_spans(&item.title, &m.title_indices)
                        .into_iter()
                        .filter_map(|(r, hi)| hi.then_some(r))
                        .collect();
                    draw_list.push_highlighted_text(
                        RectF::new(tx, title_y, text_w, 17.0),
                        &item.title,
                        if is_sel {
                            tokens.fg_primary
                        } else {
                            tokens.fg_secondary
                        },
                        FontKind::Sans,
                        13.0,
                        FontWeight::Medium,
                        TextAlign::Left,
                        true,
                        hi_ranges,
                        tokens.accent_primary,
                    );

                    if let Some(sub) = &item.subtitle {
                        let sub_hi: Vec<_> = highlight_spans(sub, &m.subtitle_indices)
                            .into_iter()
                            .filter_map(|(r, hi)| hi.then_some(r))
                            .collect();
                        draw_list.push_highlighted_text(
                            RectF::new(tx, row_rect.y + 22.0, text_w, 15.0),
                            sub,
                            tokens.fg_muted,
                            FontKind::Sans,
                            11.5,
                            FontWeight::Regular,
                            TextAlign::Left,
                            true,
                            sub_hi,
                            tokens.accent_primary,
                        );
                    }

                    if let Some(badge) = right_text {
                        let badge_rect = RectF::new(
                            (row_rect.right() - right_w - 4.0).max(tx),
                            row_rect.y + 11.0,
                            right_w,
                            20.0,
                        );
                        if item.shortcut.is_some() {
                            draw_list.push_rect(
                                badge_rect,
                                tokens.surface_pane,
                                Some(tokens.border_hairline),
                                1.0,
                                cap_radius(5.0, theme.rounding),
                                None,
                            );
                        }
                        draw_list.push_text(
                            badge_rect,
                            badge,
                            tokens.fg_muted,
                            FontKind::Mono,
                            10.5,
                            FontWeight::Regular,
                            if item.shortcut.is_some() {
                                TextAlign::Center
                            } else {
                                TextAlign::Right
                            },
                            true,
                        );
                    }

                    draw_list.push_hit(
                        row_rect,
                        HitTarget::ModalRow { index: vis_idx },
                        z_layer::MODAL + 1,
                    );
                    y += 44.0;
                }
                render_modal_scrollbar(
                    draw_list,
                    list_rect,
                    *scroll_row,
                    8,
                    matches.len(),
                    &tokens,
                );
            }
            draw_list.pop_clip();
        }
        ModalContent::RexPrompt {
            session,
            input,
            scroll_row,
        } => {
            render_rex_prompt_modal(
                draw_list,
                left_x,
                top_y,
                modal_w,
                max_h,
                card_r,
                shadow,
                session,
                input,
                *scroll_row,
                interaction,
                &tokens,
                theme.rounding,
                now,
            );
        }
        ModalContent::Settings {
            snapshot,
            selected_row,
        } => {
            render_settings_modal(
                draw_list,
                left_x,
                top_y,
                modal_w,
                card_r,
                shadow,
                *snapshot,
                *selected_row,
                interaction,
                &tokens,
                theme.rounding,
            );
        }
        ModalContent::Rename { title, input, .. } => {
            render_single_input_modal(
                draw_list,
                left_x,
                top_y,
                modal_w.min(480.0),
                card_r,
                shadow,
                title,
                input,
                "Rename",
                interaction,
                &tokens,
                theme.rounding,
            );
        }
        ModalContent::AddHost { input } => {
            render_single_input_modal(
                draw_list,
                left_x,
                top_y,
                modal_w.min(520.0),
                card_r,
                shadow,
                "Add Remote Rex Host",
                input,
                "Connect",
                interaction,
                &tokens,
                theme.rounding,
            );
        }
        ModalContent::ShortcutsHelp { scroll_row } => {
            render_shortcuts_help_modal(
                draw_list,
                left_x,
                top_y,
                modal_w,
                max_h,
                card_r,
                shadow,
                *scroll_row,
                &tokens,
                theme.rounding,
            );
        }
    }
}

fn render_modal_scrollbar(
    draw_list: &mut UiDrawList,
    list_rect: RectF,
    scroll_row: usize,
    visible_rows: usize,
    total_rows: usize,
    tokens: &UiTokens,
) {
    if total_rows <= visible_rows || list_rect.h < 24.0 {
        return;
    }
    let track_h = (list_rect.h - 12.0).max(16.0);
    let thumb_h = ((visible_rows as f32 / total_rows as f32) * track_h).clamp(20.0, track_h);
    let max_scroll = total_rows.saturating_sub(visible_rows).max(1) as f32;
    let frac = (scroll_row as f32 / max_scroll).clamp(0.0, 1.0);
    let thumb_y = list_rect.y + 6.0 + frac * (track_h - thumb_h);
    draw_list.push_rect(
        RectF::new(
            (list_rect.right() - 5.0).max(list_rect.x),
            thumb_y,
            3.0,
            thumb_h,
        ),
        tokens.fg_muted.with_alpha_f32(0.38),
        None,
        0.0,
        1.5,
        None,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_rex_prompt_modal(
    draw_list: &mut UiDrawList,
    left_x: f32,
    top_y: f32,
    modal_w: f32,
    max_h: f32,
    card_r: f32,
    shadow: ShadowSpec,
    session: &PromptSession,
    input: &TextInput,
    scroll_row: usize,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
    now: Instant,
) {
    let header_h = 74.0; // timeout bar (2px) + title breadcrumb (26px) + search/input bar (46px)
    let footer_h = if session.multiple { 42.0 } else { 0.0 };
    let body_h = match session.kind {
        PromptKind::Input => 48.0,
        PromptKind::Pick => {
            let rows = session.visible_row_count().min(8);
            if rows == 0 {
                52.0
            } else {
                rows as f32 * 44.0 + 12.0
            }
        }
    };
    let card_h = (header_h + body_h + footer_h).min(max_h);
    let card = RectF::new(
        left_x.round(),
        top_y.round(),
        modal_w.round(),
        card_h.round(),
    );

    draw_list.push_rect(
        card,
        tokens.surface_modal,
        Some(tokens.border_modal),
        1.0,
        card_r,
        Some(shadow),
    );

    // Timeout countdown progress bar along top edge of `rex.ui` modal
    let remaining_frac = if session.timeout_ms > 0 {
        (session.remaining_ms(now) as f32 / session.timeout_ms as f32).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let bar_color = if remaining_frac <= 0.15 {
        tokens.status_error
    } else if remaining_frac <= 0.30 {
        tokens.status_blocked
    } else {
        tokens.accent_primary
    };
    draw_list.push_progress(
        RectF::new(card.x + 1.0, card.y + 1.0, (card.w - 2.0).max(0.0), 2.5),
        remaining_frac,
        bar_color,
        tokens.border_hairline,
        0.0,
    );

    // Title breadcrumb row (`26px`)
    draw_list.push_text(
        RectF::new(card.x + 14.0, card.y + 8.0, (card.w - 72.0).max(0.0), 18.0),
        &session.title,
        tokens.accent_primary,
        FontKind::Sans,
        11.5,
        FontWeight::SemiBold,
        TextAlign::Left,
        true,
    );
    draw_list.push_text(
        RectF::new(card.right() - 56.0, card.y + 8.0, 42.0, 18.0),
        "Esc",
        tokens.fg_muted,
        FontKind::Mono,
        10.5,
        FontWeight::Regular,
        TextAlign::Right,
        false,
    );

    // Search / text input row (`46px`)
    render_modal_search_header(
        draw_list,
        RectF::new(card.x, card.y + 28.0, card.w, 46.0),
        input,
        if session.kind == PromptKind::Input {
            "↵"
        } else {
            &session.confirm_label
        },
        tokens,
        theme_rounding,
    );

    let body_rect = RectF::new(
        card.x,
        card.y + header_h,
        card.w,
        (card.h - header_h - footer_h).max(0.0),
    );
    draw_list.push_clip(body_rect);
    let row_r = cap_radius(7.0, theme_rounding);

    match session.kind {
        PromptKind::Input => {
            let submit_rect = RectF::new(
                body_rect.x + 6.0,
                body_rect.y + 6.0,
                (body_rect.w - 12.0).max(0.0),
                36.0,
            );
            draw_list.push_rect(
                submit_rect,
                tokens.surface_row_selected,
                Some(tokens.border_subtle),
                1.0,
                row_r,
                None,
            );
            let label = if session.confirm_label == "OK" && !session.input_text.is_empty() {
                format!("Use “{}”", session.input_text)
            } else {
                session.confirm_label.clone()
            };
            draw_list.push_text(
                RectF::new(
                    submit_rect.x + 12.0,
                    submit_rect.y + 9.0,
                    (submit_rect.w - 50.0).max(0.0),
                    18.0,
                ),
                label,
                tokens.fg_primary,
                FontKind::Sans,
                13.0,
                FontWeight::Medium,
                TextAlign::Left,
                true,
            );
            draw_list.push_hit(
                submit_rect,
                HitTarget::ModalConfirmButton,
                z_layer::MODAL + 1,
            );
        }
        PromptKind::Pick => {
            let rows = session.filtered_rows();
            let mut y = body_rect.y + 6.0;
            let mut rendered = 0usize;

            for (vis_idx, row) in rows.iter().enumerate().skip(scroll_row).take(8) {
                let item = &session.items[row.original_index];
                let row_rect =
                    RectF::new(body_rect.x + 6.0, y, (body_rect.w - 12.0).max(0.0), 42.0);
                let is_sel = vis_idx == session.focused_row;
                let is_hov = interaction.is_hovered(&HitTarget::ModalRow { index: vis_idx })
                    || interaction.is_hovered(&HitTarget::ModalCheckbox { index: vis_idx });
                if is_sel {
                    draw_list.push_rect(
                        row_rect,
                        tokens.surface_row_selected,
                        Some(tokens.border_subtle),
                        1.0,
                        row_r,
                        None,
                    );
                } else if is_hov {
                    draw_list.push_rect(
                        row_rect,
                        tokens.surface_row_hover,
                        Some(tokens.border_hairline),
                        1.0,
                        row_r,
                        None,
                    );
                }

                let mut cx = row_rect.x + 10.0;
                if session.multiple {
                    let cb = RectF::new(cx, row_rect.y + 13.0, 16.0, 16.0);
                    let cb_fill = if row.checked {
                        tokens.accent_primary
                    } else {
                        tokens.surface_pane
                    };
                    draw_list.push_rect(
                        cb,
                        cb_fill,
                        Some(if row.checked {
                            tokens.accent_primary
                        } else {
                            tokens.border_subtle
                        }),
                        1.0,
                        cap_radius(4.0, theme_rounding),
                        None,
                    );
                    if row.checked {
                        draw_list.push_icon(
                            cb.inset_uniform(2.0),
                            IconId::Check,
                            tokens.accent_foreground,
                        );
                    }
                    draw_list.push_hit(
                        cb,
                        HitTarget::ModalCheckbox { index: vis_idx },
                        z_layer::MODAL + 2,
                    );
                    cx += 24.0;
                }

                if let Some(icon) = item.icon.as_deref().and_then(IconId::from_rex_ui_name) {
                    draw_list.push_icon(
                        RectF::new(cx, row_rect.y + 13.0, 15.0, 15.0),
                        icon,
                        if is_sel {
                            tokens.accent_primary
                        } else {
                            tokens.fg_secondary
                        },
                    );
                    cx += 24.0;
                }

                let detail_w = item
                    .detail
                    .as_deref()
                    .map_or(0.0, |d| (d.chars().count() as f32 * 6.8 + 10.0).min(110.0));
                let text_w = (row_rect.right() - cx - detail_w - 8.0).max(0.0);
                let title_y = if item.subtitle.is_some() {
                    row_rect.y + 5.0
                } else {
                    row_rect.y + 12.0
                };
                draw_list.push_text(
                    RectF::new(cx, title_y, text_w, 17.0),
                    item.display_title(),
                    if is_sel {
                        tokens.fg_primary
                    } else {
                        tokens.fg_secondary
                    },
                    FontKind::Sans,
                    13.0,
                    FontWeight::Medium,
                    TextAlign::Left,
                    true,
                );
                if let Some(sub) = &item.subtitle {
                    draw_list.push_text(
                        RectF::new(cx, row_rect.y + 22.0, text_w, 15.0),
                        sub,
                        tokens.fg_muted,
                        FontKind::Sans,
                        11.5,
                        FontWeight::Regular,
                        TextAlign::Left,
                        true,
                    );
                }
                if let Some(det) = &item.detail {
                    draw_list.push_text(
                        RectF::new(
                            (row_rect.right() - detail_w - 6.0).max(cx),
                            row_rect.y + 13.0,
                            detail_w,
                            16.0,
                        ),
                        det,
                        tokens.fg_muted,
                        FontKind::Mono,
                        11.0,
                        FontWeight::Regular,
                        TextAlign::Right,
                        true,
                    );
                }
                draw_list.push_hit(
                    row_rect,
                    HitTarget::ModalRow { index: vis_idx },
                    z_layer::MODAL + 1,
                );
                y += 44.0;
                rendered += 1;
            }

            // Dedicated `allow_other` row for custom typed text (e.g., `Snooze Thread…` `"45m"`).
            if session.has_other_row() && rendered < 8 {
                let other_idx = rows.len();
                let is_sel = session.focused_row == other_idx;
                let row_rect =
                    RectF::new(body_rect.x + 6.0, y, (body_rect.w - 12.0).max(0.0), 42.0);
                if is_sel {
                    draw_list.push_rect(
                        row_rect,
                        tokens.surface_row_selected,
                        Some(tokens.border_subtle),
                        1.0,
                        row_r,
                        None,
                    );
                } else if interaction.is_hovered(&HitTarget::ModalOtherRow) {
                    draw_list.push_rect(
                        row_rect,
                        tokens.surface_row_hover,
                        Some(tokens.border_hairline),
                        1.0,
                        row_r,
                        None,
                    );
                }
                draw_list.push_icon(
                    RectF::new(row_rect.x + 10.0, row_rect.y + 13.0, 15.0, 15.0),
                    IconId::Plus,
                    tokens.accent_primary,
                );
                draw_list.push_text(
                    RectF::new(
                        row_rect.x + 34.0,
                        row_rect.y + 12.0,
                        (row_rect.w - 44.0).max(0.0),
                        17.0,
                    ),
                    format!("{} “{}”", session.confirm_label, session.input_text),
                    tokens.fg_primary,
                    FontKind::Sans,
                    12.5,
                    FontWeight::Medium,
                    TextAlign::Left,
                    true,
                );
                draw_list.push_hit(row_rect, HitTarget::ModalOtherRow, z_layer::MODAL + 1);
            }
            render_modal_scrollbar(
                draw_list,
                body_rect,
                scroll_row,
                8,
                session.visible_row_count(),
                tokens,
            );
        }
    }
    draw_list.pop_clip();

    // Multi-select footer with ticked count + `Confirm (Ctrl+Enter)` primary pill
    if session.multiple {
        let footer = RectF::new(
            card.x,
            (card.bottom() - footer_h).max(card.y),
            card.w,
            footer_h,
        );
        draw_list.push_rect(
            RectF::new(footer.x, footer.y, footer.w, 1.0),
            tokens.border_hairline,
            None,
            0.0,
            0.0,
            None,
        );
        draw_list.push_text(
            RectF::new(footer.x + 14.0, footer.y + 12.0, 160.0, 18.0),
            format!("{} selected · Space toggles", session.checked_ids.len()),
            tokens.fg_muted,
            FontKind::Sans,
            11.5,
            FontWeight::Regular,
            TextAlign::Left,
            true,
        );
        let confirm_text = format!("{} (Ctrl+Enter)", session.confirm_label);
        let btn_w = ((confirm_text.chars().count() as f32) * 6.8 + 24.0).clamp(120.0, 210.0);
        let btn = RectF::new(
            (footer.right() - btn_w - 10.0).max(footer.x),
            footer.y + 7.0,
            btn_w,
            28.0,
        );
        draw_list.push_rect(
            btn,
            tokens.accent_primary,
            None,
            0.0,
            cap_radius(6.0, theme_rounding),
            None,
        );
        draw_list.push_text(
            btn,
            confirm_text,
            tokens.accent_foreground,
            FontKind::Sans,
            11.5,
            FontWeight::SemiBold,
            TextAlign::Center,
            false,
        );
        draw_list.push_hit(btn, HitTarget::ModalConfirmButton, z_layer::MODAL + 2);
    }
}

fn render_modal_search_header(
    draw_list: &mut UiDrawList,
    header: RectF,
    input: &TextInput,
    badge: &str,
    tokens: &UiTokens,
    theme_rounding: f32,
) {
    draw_list.push_icon(
        RectF::new(header.x + 14.0, header.y + 15.0, 15.0, 15.0),
        IconId::Search,
        tokens.fg_muted,
    );
    let badge_w = ((badge.chars().count() as f32) * 6.6 + 14.0).clamp(32.0, 112.0);
    let badge_rect = RectF::new(
        (header.right() - badge_w - 12.0).max(header.x),
        header.y + 12.0,
        badge_w,
        22.0,
    );
    draw_list.push_rect(
        badge_rect,
        tokens.surface_pane,
        Some(tokens.border_subtle),
        1.0,
        cap_radius(5.0, theme_rounding),
        None,
    );
    draw_list.push_text(
        badge_rect,
        badge,
        tokens.fg_muted,
        FontKind::Mono,
        10.5,
        FontWeight::Regular,
        TextAlign::Center,
        true,
    );

    let text_rect = RectF::new(
        header.x + 38.0,
        header.y + 13.0,
        (badge_rect.x - header.x - 46.0).max(0.0),
        20.0,
    );
    let (display, color) = if input.is_empty() {
        (input.placeholder().to_owned(), tokens.fg_muted)
    } else {
        (input.display_text().0, tokens.fg_primary)
    };
    draw_list.push_text(
        text_rect,
        display,
        color,
        FontKind::Sans,
        14.0,
        FontWeight::Regular,
        TextAlign::Left,
        true,
    );
    let caret_x = text_rect.x
        + (input.display_text().0.chars().count() as f32 * 7.6).min((text_rect.w - 2.0).max(0.0));
    draw_list.push_rect(
        RectF::new(caret_x, text_rect.y + 2.0, 1.5, 16.0),
        tokens.accent_primary,
        None,
        0.0,
        0.75,
        None,
    );

    draw_list.push_rect(
        RectF::new(
            header.x,
            (header.bottom() - 1.0).max(header.y),
            header.w,
            1.0,
        ),
        tokens.border_hairline,
        None,
        0.0,
        0.0,
        None,
    );
    draw_list.push_hit(header, HitTarget::ModalInput, z_layer::MODAL + 1);
}

#[allow(clippy::too_many_arguments)]
fn render_settings_modal(
    draw_list: &mut UiDrawList,
    left_x: f32,
    top_y: f32,
    modal_w: f32,
    card_r: f32,
    shadow: ShadowSpec,
    s: SettingsViewSnapshot,
    selected_row: usize,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
) {
    let rows: [(&str, &str, String); 8] = [
        (
            "toolbar_position",
            "Toolbar Layout",
            format!("{:?}", s.toolbar_position),
        ),
        (
            "toolbar_behavior",
            "Toolbar Behavior",
            format!("{:?}", s.toolbar_behavior),
        ),
        ("pane_density", "Pane Density", format!("{:?}", s.density)),
        (
            "pane_headers",
            "Pane Headers",
            format!("{:?}", s.pane_header_mode),
        ),
        (
            "material",
            "Surface Material",
            match s.material {
                MaterialStyle::Classic => "Classic (Opaque)".to_owned(),
                MaterialStyle::Translucent { alpha } => {
                    format!("Translucent ({:.0}%)", alpha * 100.0)
                }
            },
        ),
        (
            "active_border",
            "Highlight Active Pane Border",
            if s.highlight_active_border {
                "On"
            } else {
                "Off"
            }
            .to_owned(),
        ),
        (
            "fade_inactive",
            "Fade Inactive Panes",
            if s.fade_inactive_panes { "On" } else { "Off" }.to_owned(),
        ),
        (
            "adaptive_rounding",
            "Follow Hyprland Rounding",
            if s.adaptive_rounding { "On" } else { "Off" }.to_owned(),
        ),
    ];

    let card_h = 46.0 + rows.len() as f32 * 38.0 + 14.0;
    let card = RectF::new(
        left_x.round(),
        top_y.round(),
        modal_w.round(),
        card_h.round(),
    );
    draw_list.push_rect(
        card,
        tokens.surface_modal,
        Some(tokens.border_modal),
        1.0,
        card_r,
        Some(shadow),
    );

    draw_list.push_icon(
        RectF::new(card.x + 14.0, card.y + 15.0, 15.0, 15.0),
        IconId::Gear,
        tokens.accent_primary,
    );
    draw_list.push_text(
        RectF::new(
            card.x + 36.0,
            card.y + 13.0,
            (card.w - 110.0).max(0.0),
            20.0,
        ),
        "Settings",
        tokens.fg_primary,
        FontKind::Sans,
        14.0,
        FontWeight::SemiBold,
        TextAlign::Left,
        true,
    );
    draw_list.push_text(
        RectF::new(card.right() - 96.0, card.y + 15.0, 82.0, 16.0),
        "Enter cycles",
        tokens.fg_muted,
        FontKind::Mono,
        10.5,
        FontWeight::Regular,
        TextAlign::Right,
        false,
    );
    draw_list.push_rect(
        RectF::new(card.x, card.y + 45.0, card.w, 1.0),
        tokens.border_hairline,
        None,
        0.0,
        0.0,
        None,
    );

    let mut y = card.y + 52.0;
    let row_r = cap_radius(7.0, theme_rounding);
    for (idx, (key, label, value)) in rows.into_iter().enumerate() {
        let row_rect = RectF::new(card.x + 8.0, y, (card.w - 16.0).max(0.0), 34.0);
        let toggle_target = HitTarget::SettingsToggle {
            key: key.to_owned(),
        };
        let is_hovered = interaction.is_hovered(&toggle_target)
            || interaction.is_hovered(&HitTarget::ModalRow { index: idx });
        let is_pressed = interaction.is_pressed(&toggle_target);
        if idx == selected_row || is_pressed {
            draw_list.push_rect(
                row_rect,
                tokens.surface_row_selected,
                Some(tokens.border_subtle),
                1.0,
                row_r,
                None,
            );
        } else if is_hovered {
            draw_list.push_rect(
                row_rect,
                tokens.surface_row_hover,
                Some(tokens.border_hairline),
                1.0,
                row_r,
                None,
            );
        }
        draw_list.push_text(
            RectF::new(
                row_rect.x + 10.0,
                row_rect.y + 8.0,
                (row_rect.w - 150.0).max(0.0),
                18.0,
            ),
            label,
            tokens.fg_primary,
            FontKind::Sans,
            12.5,
            FontWeight::Medium,
            TextAlign::Left,
            true,
        );
        let pill = RectF::new(
            (row_rect.right() - 134.0).max(row_rect.x),
            row_rect.y + 6.0,
            124.0,
            22.0,
        );
        draw_list.push_rect(
            pill,
            tokens.surface_pane,
            Some(if is_hovered || idx == selected_row {
                tokens.accent_primary.with_alpha_f32(0.50)
            } else {
                tokens.border_hairline
            }),
            1.0,
            cap_radius(5.0, theme_rounding),
            None,
        );
        draw_list.push_text(
            pill,
            value,
            tokens.accent_primary,
            FontKind::Mono,
            11.0,
            FontWeight::Medium,
            TextAlign::Center,
            true,
        );
        draw_list.push_hit(row_rect, toggle_target, z_layer::MODAL + 1);
        draw_list.push_hit(row_rect, HitTarget::ModalRow { index: idx }, z_layer::MODAL);
        y += 38.0;
    }
}

#[allow(clippy::too_many_arguments)]
fn render_single_input_modal(
    draw_list: &mut UiDrawList,
    left_x: f32,
    top_y: f32,
    modal_w: f32,
    card_r: f32,
    shadow: ShadowSpec,
    title: &str,
    input: &TextInput,
    confirm_label: &str,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
) {
    let card = RectF::new(left_x.round(), top_y.round(), modal_w.round(), 134.0);
    draw_list.push_rect(
        card,
        tokens.surface_modal,
        Some(tokens.border_modal),
        1.0,
        card_r,
        Some(shadow),
    );

    draw_list.push_text(
        RectF::new(card.x + 14.0, card.y + 12.0, (card.w - 28.0).max(0.0), 18.0),
        title,
        tokens.fg_primary,
        FontKind::Sans,
        13.5,
        FontWeight::SemiBold,
        TextAlign::Left,
        true,
    );

    let field = RectF::new(card.x + 14.0, card.y + 40.0, (card.w - 28.0).max(0.0), 34.0);
    draw_list.push_rect(
        field,
        tokens.surface_pane,
        Some(tokens.border_active),
        1.0,
        cap_radius(7.0, theme_rounding),
        None,
    );
    let (display, color) = if input.is_empty() {
        (input.placeholder().to_owned(), tokens.fg_muted)
    } else {
        (input.display_text().0, tokens.fg_primary)
    };
    draw_list.push_text(
        RectF::new(
            field.x + 10.0,
            field.y + 8.0,
            (field.w - 20.0).max(0.0),
            18.0,
        ),
        display,
        color,
        FontKind::Mono,
        12.5,
        FontWeight::Regular,
        TextAlign::Left,
        true,
    );
    draw_list.push_hit(field, HitTarget::ModalInput, z_layer::MODAL + 1);

    let ok_btn = RectF::new(
        (card.right() - 102.0).max(card.x),
        card.y + 88.0,
        88.0,
        30.0,
    );
    let cancel_btn = RectF::new((ok_btn.x - 86.0).max(card.x), card.y + 88.0, 78.0, 30.0);
    let btn_r = cap_radius(6.0, theme_rounding);

    let cancel_fill = if interaction.is_pressed(&HitTarget::ModalCancelButton) {
        tokens.surface_row_selected
    } else if interaction.is_hovered(&HitTarget::ModalCancelButton) {
        tokens.surface_row_active
    } else {
        tokens.surface_row_hover
    };
    draw_list.push_rect(
        cancel_btn,
        cancel_fill,
        Some(if interaction.is_hovered(&HitTarget::ModalCancelButton) {
            tokens.border_subtle
        } else {
            tokens.border_hairline
        }),
        1.0,
        btn_r,
        None,
    );
    draw_list.push_text(
        cancel_btn,
        "Cancel",
        if interaction.is_hovered(&HitTarget::ModalCancelButton) {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
        FontKind::Sans,
        12.0,
        FontWeight::Medium,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(cancel_btn, HitTarget::ModalCancelButton, z_layer::MODAL + 1);

    let ok_fill = if interaction.is_pressed(&HitTarget::ModalConfirmButton) {
        tokens.accent_primary.with_alpha_f32(0.80)
    } else if interaction.is_hovered(&HitTarget::ModalConfirmButton) {
        tokens.accent_primary.with_alpha_f32(0.92)
    } else {
        tokens.accent_primary
    };
    draw_list.push_rect(ok_btn, ok_fill, None, 0.0, btn_r, None);
    draw_list.push_text(
        ok_btn,
        confirm_label,
        tokens.accent_foreground,
        FontKind::Sans,
        12.0,
        FontWeight::SemiBold,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(ok_btn, HitTarget::ModalConfirmButton, z_layer::MODAL + 1);
}

const SHORTCUT_HELP_ROWS: &[(&str, &str)] = &[
    ("Command Palette", "Ctrl+Shift+P"),
    ("Switch Session", "Ctrl+Shift+K"),
    ("Go to Anything", "Ctrl+Alt+P"),
    ("Activity Filter", "Ctrl+Shift+."),
    ("Toggle Sidebar", "Ctrl+Shift+B"),
    ("Toggle Toolbar Layout", "Ctrl+Shift+S"),
    ("New Terminal", "Ctrl+Shift+T"),
    ("New Terminal Session", "Ctrl+Shift+N"),
    ("Close Pane / Tab", "Ctrl+Shift+W / Ctrl+Alt+W"),
    ("Jump to Tab 1..9", "Alt+1 .. Alt+9"),
    ("Split Right / Down", "Ctrl+Shift+D / E"),
    ("Zoom / Float Pane", "Ctrl+Shift+Z / Ctrl+Alt+Enter"),
    ("Copy / Paste (Omarchy)", "Ctrl+Shift+C/V · Ctrl/Shift+Ins"),
    ("Find in Terminal", "Ctrl+Shift+F"),
    ("Settings", "Ctrl+Shift+,"),
    ("T3: Needs You Inbox", "Ctrl+Alt+I"),
    ("T3: New Thread / Project", "Ctrl+Alt+N / O"),
    ("T3: Answer… / Fit Device", "Ctrl+Alt+A / T"),
    ("T3: Parent / Subagent", "Ctrl+Alt+[ / ]"),
];

#[allow(clippy::too_many_arguments)]
fn render_shortcuts_help_modal(
    draw_list: &mut UiDrawList,
    left_x: f32,
    top_y: f32,
    modal_w: f32,
    max_h: f32,
    card_r: f32,
    shadow: ShadowSpec,
    scroll_row: usize,
    tokens: &UiTokens,
    theme_rounding: f32,
) {
    let card_h = (46.0_f32 + 10.0 * 30.0 + 16.0).min(max_h);
    let card = RectF::new(
        left_x.round(),
        top_y.round(),
        modal_w.round(),
        card_h.round(),
    );
    draw_list.push_rect(
        card,
        tokens.surface_modal,
        Some(tokens.border_modal),
        1.0,
        card_r,
        Some(shadow),
    );

    draw_list.push_text(
        RectF::new(card.x + 14.0, card.y + 13.0, (card.w - 80.0).max(0.0), 20.0),
        "Keyboard Shortcuts (Omarchy Conflict-Free)",
        tokens.fg_primary,
        FontKind::Sans,
        13.5,
        FontWeight::SemiBold,
        TextAlign::Left,
        true,
    );
    draw_list.push_text(
        RectF::new(card.right() - 56.0, card.y + 14.0, 42.0, 18.0),
        "Esc",
        tokens.fg_muted,
        FontKind::Mono,
        10.5,
        FontWeight::Regular,
        TextAlign::Right,
        false,
    );
    draw_list.push_rect(
        RectF::new(card.x, card.y + 45.0, card.w, 1.0),
        tokens.border_hairline,
        None,
        0.0,
        0.0,
        None,
    );

    let list = RectF::new(card.x, card.y + 46.0, card.w, (card.h - 46.0).max(0.0));
    draw_list.push_clip(list);
    let mut y = list.y + 8.0;
    for (label, chord) in SHORTCUT_HELP_ROWS.iter().skip(scroll_row).take(10) {
        let row = RectF::new(list.x + 14.0, y, (list.w - 28.0).max(0.0), 26.0);
        draw_list.push_text(
            RectF::new(row.x, row.y + 4.0, (row.w - 170.0).max(0.0), 18.0),
            *label,
            tokens.fg_secondary,
            FontKind::Sans,
            12.0,
            FontWeight::Regular,
            TextAlign::Left,
            true,
        );
        let pill = RectF::new((row.right() - 166.0).max(row.x), row.y + 3.0, 166.0, 20.0);
        draw_list.push_rect(
            pill,
            tokens.surface_pane,
            Some(tokens.border_hairline),
            1.0,
            cap_radius(5.0, theme_rounding),
            None,
        );
        draw_list.push_text(
            pill,
            *chord,
            tokens.accent_primary,
            FontKind::Mono,
            10.5,
            FontWeight::Medium,
            TextAlign::Center,
            true,
        );
        y += 30.0;
    }
    render_modal_scrollbar(
        draw_list,
        list,
        scroll_row,
        10,
        SHORTCUT_HELP_ROWS.len(),
        tokens,
    );
    draw_list.pop_clip();
}

/// Active key-mode or multi-key chord prefix displayed in the bottom-right which-key popup.
#[derive(Debug, Clone)]
pub struct WhichKeyState {
    pub header: String,
    pub exclusive: bool,
    pub hints: Vec<WhichKeyHint>,
}

/// Renders the bottom-right which-key chord/mode hint bar (`terminal.keyboard.sequence_hints`).
pub fn render_which_key(
    draw_list: &mut UiDrawList,
    workspace: RectF,
    state: &WhichKeyState,
    theme: &Theme,
) {
    if workspace.is_empty() {
        return;
    }
    let tokens = theme.effective_ui();
    let visible_hints = state.hints.len().min(8);
    let cols = if visible_hints > 4 { 2 } else { 1 };
    let rows = visible_hints.div_ceil(cols).max(1);
    let w = (if cols == 2 { 390.0_f32 } else { 230.0_f32 }).min((workspace.w - 28.0).max(120.0));
    let h = 34.0 + rows as f32 * 24.0 + 8.0;
    let card = RectF::new(
        (workspace.right() - w - 14.0).max(workspace.x + 8.0),
        (workspace.bottom() - h - 14.0).max(workspace.y + 8.0),
        w,
        h,
    );

    draw_list.push_rect(
        card,
        tokens.surface_modal,
        Some(tokens.border_active),
        1.0,
        cap_radius(8.0, theme.rounding),
        Some(ShadowSpec {
            offset_x: 0.0,
            offset_y: 8.0,
            blur_radius: 24.0,
            spread: 0.0,
            color: tokens.shadow_color,
        }),
    );

    let title = if state.exclusive {
        format!("{} · exclusive · Esc exits", state.header)
    } else {
        format!("{} · Esc cancels", state.header)
    };
    draw_list.push_text(
        RectF::new(card.x + 12.0, card.y + 8.0, (card.w - 24.0).max(0.0), 18.0),
        title,
        tokens.accent_primary,
        FontKind::Mono,
        11.0,
        FontWeight::SemiBold,
        TextAlign::Left,
        true,
    );

    let col_w = ((card.w - 24.0) / cols as f32).max(40.0);
    for (idx, hint) in state.hints.iter().take(8).enumerate() {
        let col = idx % cols;
        let row = idx / cols;
        let hx = card.x + 12.0 + col as f32 * col_w;
        let hy = card.y + 32.0 + row as f32 * 24.0;
        let label = match (&hint.action, hint.is_prefix) {
            (Some(act), true) => format!("{act} …"),
            (Some(act), false) => act.clone(),
            (None, true) => "+prefix".to_owned(),
            (None, false) => String::new(),
        };
        draw_list.push_text(
            RectF::new(hx, hy, 56.0, 18.0),
            &hint.display_key,
            tokens.fg_primary,
            FontKind::Mono,
            11.0,
            FontWeight::Bold,
            TextAlign::Left,
            true,
        );
        draw_list.push_text(
            RectF::new(hx + 60.0, hy, (col_w - 64.0).max(0.0), 18.0),
            label,
            tokens.fg_muted,
            FontKind::Sans,
            11.0,
            FontWeight::Regular,
            TextAlign::Left,
            true,
        );
    }
}

/// Inline terminal Find bar state (`Ctrl+Shift+F`).
#[derive(Debug, Clone)]
pub struct FindBarState {
    pub open: bool,
    pub input: TextInput,
    pub case_sensitive: bool,
    pub regex: bool,
    pub current_match: usize,
    pub total_matches: usize,
}

impl Default for FindBarState {
    fn default() -> Self {
        Self {
            open: false,
            input: TextInput::new("Find in terminal…"),
            case_sensitive: false,
            regex: false,
            current_match: 0,
            total_matches: 0,
        }
    }
}

/// Renders the inline terminal Find bar (`336×34px`) anchored top-right inside the focused pane.
pub fn render_find_bar(
    draw_list: &mut UiDrawList,
    pane_rect: RectF,
    state: &FindBarState,
    interaction: UiInteraction<'_>,
    theme: &Theme,
) {
    if !state.open || pane_rect.is_empty() {
        return;
    }
    let tokens = theme.effective_ui();
    let w = 336.0_f32.min((pane_rect.w - 20.0).max(160.0));
    let bar = RectF::new(
        (pane_rect.right() - w - 10.0).max(pane_rect.x + 6.0),
        pane_rect.y + 10.0,
        w,
        34.0,
    );
    draw_list.push_rect(
        bar,
        tokens.surface_modal,
        Some(tokens.border_active),
        1.0,
        cap_radius(8.0, theme.rounding),
        Some(ShadowSpec {
            offset_x: 0.0,
            offset_y: 8.0,
            blur_radius: 20.0,
            spread: 0.0,
            color: tokens.shadow_color,
        }),
    );

    let close_btn = RectF::new(bar.right() - 26.0, bar.y + 6.0, 22.0, 22.0);
    let next_btn = RectF::new(close_btn.x - 22.0, bar.y + 6.0, 20.0, 22.0);
    let prev_btn = RectF::new(next_btn.x - 22.0, bar.y + 6.0, 20.0, 22.0);
    let regex_btn = RectF::new(prev_btn.x - 26.0, bar.y + 6.0, 24.0, 22.0);
    let case_btn = RectF::new(regex_btn.x - 26.0, bar.y + 6.0, 24.0, 22.0);
    let count_rect = RectF::new(case_btn.x - 46.0, bar.y + 8.0, 42.0, 18.0);
    let input_rect = RectF::new(
        bar.x + 8.0,
        bar.y + 7.0,
        (count_rect.x - bar.x - 12.0).max(20.0),
        20.0,
    );

    let (display, color) = if state.input.is_empty() {
        (state.input.placeholder().to_owned(), tokens.fg_muted)
    } else {
        (state.input.text().to_owned(), tokens.fg_primary)
    };
    draw_list.push_text(
        input_rect,
        display,
        color,
        FontKind::Mono,
        12.0,
        FontWeight::Regular,
        TextAlign::Left,
        true,
    );
    let caret_x = input_rect.x
        + (state.input.text().chars().count() as f32 * 7.2).min((input_rect.w - 2.0).max(0.0));
    draw_list.push_rect(
        RectF::new(caret_x, input_rect.y + 3.0, 1.5, 14.0),
        tokens.accent_primary,
        None,
        0.0,
        0.75,
        None,
    );
    draw_list.push_hit(input_rect, HitTarget::FindBarInput, z_layer::FIND_BAR + 1);

    let count_label = if state.total_matches == 0 {
        "0/0".to_owned()
    } else {
        format!(
            "{}/{}",
            state.current_match.min(state.total_matches),
            state.total_matches
        )
    };
    draw_list.push_text(
        count_rect,
        count_label,
        tokens.fg_muted,
        FontKind::Mono,
        10.5,
        FontWeight::Regular,
        TextAlign::Right,
        false,
    );

    let toggle_r = cap_radius(4.0, theme.rounding);
    if state.case_sensitive {
        draw_list.push_rect(case_btn, tokens.accent_primary, None, 0.0, toggle_r, None);
    } else if let Some((fill, border)) =
        interaction.button_surface(&HitTarget::FindBarCaseToggle, false, &tokens)
    {
        draw_list.push_rect(case_btn, fill, border, 1.0, toggle_r, None);
    }
    draw_list.push_text(
        case_btn,
        "Aa",
        if state.case_sensitive {
            tokens.accent_foreground
        } else if interaction.is_hovered(&HitTarget::FindBarCaseToggle) {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
        FontKind::Mono,
        10.5,
        FontWeight::SemiBold,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(
        case_btn,
        HitTarget::FindBarCaseToggle,
        z_layer::FIND_BAR + 1,
    );

    if state.regex {
        draw_list.push_rect(regex_btn, tokens.accent_primary, None, 0.0, toggle_r, None);
    } else if let Some((fill, border)) =
        interaction.button_surface(&HitTarget::FindBarRegexToggle, false, &tokens)
    {
        draw_list.push_rect(regex_btn, fill, border, 1.0, toggle_r, None);
    }
    draw_list.push_text(
        regex_btn,
        ".*",
        if state.regex {
            tokens.accent_foreground
        } else if interaction.is_hovered(&HitTarget::FindBarRegexToggle) {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
        FontKind::Mono,
        10.5,
        FontWeight::SemiBold,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(
        regex_btn,
        HitTarget::FindBarRegexToggle,
        z_layer::FIND_BAR + 1,
    );

    if let Some((fill, border)) =
        interaction.button_surface(&HitTarget::FindBarPrev, false, &tokens)
    {
        draw_list.push_rect(prev_btn, fill, border, 1.0, toggle_r, None);
    }
    draw_list.push_text(
        prev_btn,
        "↑",
        if interaction.is_hovered(&HitTarget::FindBarPrev) {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
        FontKind::Sans,
        12.0,
        FontWeight::SemiBold,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(prev_btn, HitTarget::FindBarPrev, z_layer::FIND_BAR + 1);

    if let Some((fill, border)) =
        interaction.button_surface(&HitTarget::FindBarNext, false, &tokens)
    {
        draw_list.push_rect(next_btn, fill, border, 1.0, toggle_r, None);
    }
    draw_list.push_text(
        next_btn,
        "↓",
        if interaction.is_hovered(&HitTarget::FindBarNext) {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
        FontKind::Sans,
        12.0,
        FontWeight::SemiBold,
        TextAlign::Center,
        false,
    );
    draw_list.push_hit(next_btn, HitTarget::FindBarNext, z_layer::FIND_BAR + 1);

    let (close_bg, close_tint) =
        interaction.danger_button_style(&HitTarget::FindBarClose, tokens.fg_secondary, &tokens);
    if let Some((fill, border)) = close_bg {
        draw_list.push_rect(close_btn, fill, border, 1.0, toggle_r, None);
    }
    draw_list.push_icon(close_btn.inset_uniform(4.0), IconId::Close, close_tint);
    draw_list.push_hit(close_btn, HitTarget::FindBarClose, z_layer::FIND_BAR + 1);
}

/// 6-position anchor grid for in-app floating notifications (`notifications.overview.position`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToastAnchor {
    TopLeft,
    TopCenter,
    #[default]
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastSeverity {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastItem {
    pub id: u64,
    pub severity: ToastSeverity,
    pub title: String,
    pub body: String,
}

/// Renders stacked in-app toast banners at any of the 6 Rex anchor positions.
pub fn render_toasts(
    draw_list: &mut UiDrawList,
    workspace: RectF,
    anchor: ToastAnchor,
    toasts: &[ToastItem],
    interaction: UiInteraction<'_>,
    theme: &Theme,
) {
    if toasts.is_empty() || workspace.is_empty() {
        return;
    }
    let tokens = theme.effective_ui();
    const TOAST_W: f32 = 330.0;
    const TOAST_H: f32 = 58.0;
    const INSET: f32 = 14.0;
    const GAP: f32 = 8.0;

    let w = TOAST_W.min((workspace.w - INSET * 2.0).max(140.0));
    let is_bottom = matches!(
        anchor,
        ToastAnchor::BottomLeft | ToastAnchor::BottomCenter | ToastAnchor::BottomRight
    );

    for (idx, toast) in toasts.iter().take(4).enumerate() {
        let x = match anchor {
            ToastAnchor::TopLeft | ToastAnchor::BottomLeft => workspace.x + INSET,
            ToastAnchor::TopCenter | ToastAnchor::BottomCenter => {
                workspace.x + (workspace.w - w) * 0.5
            }
            ToastAnchor::TopRight | ToastAnchor::BottomRight => {
                (workspace.right() - INSET - w).max(workspace.x)
            }
        };
        let stack = idx as f32 * (TOAST_H + GAP);
        let y = if is_bottom {
            (workspace.bottom() - INSET - TOAST_H - stack).max(workspace.y)
        } else {
            workspace.y + INSET + stack
        };
        let card = RectF::new(x.round(), y.round(), w.round(), TOAST_H);

        let (accent, icon) = match toast.severity {
            ToastSeverity::Info => (tokens.accent_primary, IconId::Activity),
            ToastSeverity::Success => (tokens.status_done, IconId::Check),
            ToastSeverity::Warning => (tokens.status_blocked, IconId::Warning),
            ToastSeverity::Error => (tokens.status_error, IconId::Error),
        };

        draw_list.push_rect(
            card,
            tokens.surface_modal,
            Some(accent),
            1.0,
            cap_radius(10.0, theme.rounding),
            Some(ShadowSpec {
                offset_x: 0.0,
                offset_y: 8.0,
                blur_radius: 24.0,
                spread: 0.0,
                color: tokens.shadow_color,
            }),
        );
        draw_list.push_icon(
            RectF::new(card.x + 12.0, card.y + 12.0, 15.0, 15.0),
            icon,
            accent,
        );
        draw_list.push_text(
            RectF::new(card.x + 34.0, card.y + 10.0, (card.w - 64.0).max(0.0), 17.0),
            &toast.title,
            tokens.fg_primary,
            FontKind::Sans,
            12.5,
            FontWeight::SemiBold,
            TextAlign::Left,
            true,
        );
        draw_list.push_text(
            RectF::new(card.x + 34.0, card.y + 29.0, (card.w - 48.0).max(0.0), 18.0),
            &toast.body,
            tokens.fg_muted,
            FontKind::Sans,
            11.5,
            FontWeight::Regular,
            TextAlign::Left,
            true,
        );

        let dismiss = RectF::new(card.right() - 26.0, card.y + 8.0, 18.0, 18.0);
        let dismiss_target = HitTarget::ToastDismiss { id: toast.id };
        let (dismiss_bg, dismiss_tint) =
            interaction.danger_button_style(&dismiss_target, tokens.fg_muted, &tokens);
        if let Some((fill, border)) = dismiss_bg {
            draw_list.push_rect(
                dismiss,
                fill,
                border,
                1.0,
                cap_radius(4.0, theme.rounding),
                None,
            );
        }
        draw_list.push_icon(dismiss.inset_uniform(3.0), IconId::Close, dismiss_tint);
        draw_list.push_hit(dismiss, dismiss_target, z_layer::TOAST + 2);
        draw_list.push_hit(
            card,
            HitTarget::ToastBody { id: toast.id },
            z_layer::TOAST + 1,
        );
    }
}
