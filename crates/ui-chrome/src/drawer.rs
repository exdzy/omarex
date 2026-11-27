//! Context menus (pane/tab/session right-click), file-path/image hover preview cards,
//! and specialized `tr3x` `/pr` floating drawer chrome styling.

use omarchy_theme::{Theme, UiTokens};

use crate::draw::{
    FontKind, FontWeight, HitTarget, PaneHeaderButton, ShadowSpec, TextAlign, UiAction, UiDrawList,
    UiInteraction, cap_radius, z_layer,
};
use crate::icons::IconId;
use crate::layout::{FloatingPaneGeometry, RectF};

/// Single entry inside a right-click context menu.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextMenuItem {
    pub label: String,
    pub icon: Option<IconId>,
    pub shortcut: Option<String>,
    pub danger: bool,
    pub separator_before: bool,
    pub action: UiAction,
}

/// Active right-click context menu state.
#[derive(Debug, Clone)]
pub struct ContextMenuState {
    pub anchor_x: f32,
    pub anchor_y: f32,
    pub items: Vec<ContextMenuItem>,
    pub hovered_index: Option<usize>,
}

impl ContextMenuState {
    /// Builds the standard Rex pane right-click context menu.
    #[must_use]
    pub fn for_pane(
        anchor_x: f32,
        anchor_y: f32,
        block_id: impl Into<String>,
        is_floating: bool,
        is_zoomed: bool,
    ) -> Self {
        let id = block_id.into();
        let items = vec![
            ContextMenuItem {
                label: "Copy".to_owned(),
                icon: Some(IconId::Document),
                shortcut: Some("Ctrl+Shift+C".to_owned()),
                danger: false,
                separator_before: false,
                action: UiAction::CopySelection {
                    block_id: Some(id.clone()),
                },
            },
            ContextMenuItem {
                label: "Paste".to_owned(),
                icon: Some(IconId::Document),
                shortcut: Some("Ctrl+Shift+V".to_owned()),
                danger: false,
                separator_before: false,
                action: UiAction::PasteClipboard {
                    block_id: Some(id.clone()),
                },
            },
            ContextMenuItem {
                label: "Split Right".to_owned(),
                icon: Some(IconId::SplitRight),
                shortcut: Some("Ctrl+Shift+D".to_owned()),
                danger: false,
                separator_before: true,
                action: UiAction::PaneButton {
                    block_id: id.clone(),
                    button: PaneHeaderButton::SplitRight,
                },
            },
            ContextMenuItem {
                label: "Split Down".to_owned(),
                icon: Some(IconId::SplitDown),
                shortcut: Some("Ctrl+Shift+E".to_owned()),
                danger: false,
                separator_before: false,
                action: UiAction::PaneButton {
                    block_id: id.clone(),
                    button: PaneHeaderButton::SplitDown,
                },
            },
            ContextMenuItem {
                label: if is_floating {
                    "Dock Pane".to_owned()
                } else {
                    "Float Pane".to_owned()
                },
                icon: Some(if is_floating {
                    IconId::Dock
                } else {
                    IconId::Float
                }),
                shortcut: Some("Ctrl+Alt+Enter".to_owned()),
                danger: false,
                separator_before: true,
                action: UiAction::PaneButton {
                    block_id: id.clone(),
                    button: PaneHeaderButton::FloatOrDock,
                },
            },
            ContextMenuItem {
                label: if is_zoomed {
                    "Restore Zoom".to_owned()
                } else {
                    "Zoom Pane".to_owned()
                },
                icon: Some(IconId::Zoom),
                shortcut: Some("Ctrl+Shift+Z".to_owned()),
                danger: false,
                separator_before: false,
                action: UiAction::PaneButton {
                    block_id: id.clone(),
                    button: PaneHeaderButton::Zoom,
                },
            },
            ContextMenuItem {
                label: "Rename Pane…".to_owned(),
                icon: Some(IconId::Tag),
                shortcut: None,
                danger: false,
                separator_before: true,
                action: UiAction::OpenModal(crate::draw::ModalKind::Rename),
            },
            ContextMenuItem {
                label: "Close Pane".to_owned(),
                icon: Some(IconId::Close),
                shortcut: Some("Ctrl+Shift+W".to_owned()),
                danger: true,
                separator_before: true,
                action: UiAction::PaneButton {
                    block_id: id,
                    button: PaneHeaderButton::Close,
                },
            },
        ];
        Self {
            anchor_x,
            anchor_y,
            items,
            hovered_index: None,
        }
    }

    /// Builds the standard Rex/tr3x tab right-click context menu.
    #[must_use]
    pub fn for_tab(
        anchor_x: f32,
        anchor_y: f32,
        host_id: impl Into<String>,
        session_id: impl Into<String>,
        window_id: impl Into<String>,
        muted: bool,
    ) -> Self {
        let host = host_id.into();
        let session = session_id.into();
        let win = window_id.into();
        let items = vec![
            ContextMenuItem {
                label: "Rename Tab…".to_owned(),
                icon: Some(IconId::Tag),
                shortcut: None,
                danger: false,
                separator_before: false,
                action: UiAction::OpenModal(crate::draw::ModalKind::Rename),
            },
            ContextMenuItem {
                label: "Mark as Unread".to_owned(),
                icon: Some(IconId::Activity),
                shortcut: Some("Ctrl+Alt+U".to_owned()),
                danger: false,
                separator_before: false,
                action: UiAction::MarkTabUnread {
                    window_id: win.clone(),
                },
            },
            ContextMenuItem {
                label: if muted {
                    "Unmute Session".to_owned()
                } else {
                    "Mute Session".to_owned()
                },
                icon: Some(if muted { IconId::Bell } else { IconId::BellOff }),
                shortcut: None,
                danger: false,
                separator_before: false,
                action: UiAction::ToggleSessionMute {
                    host_id: host.clone(),
                    session_id: session.clone(),
                },
            },
            ContextMenuItem {
                label: "Move Tab to New Window".to_owned(),
                icon: Some(IconId::Float),
                shortcut: None,
                danger: false,
                separator_before: true,
                action: UiAction::MoveTabToNewWindow {
                    window_id: win.clone(),
                },
            },
            ContextMenuItem {
                label: "Close Other Tabs".to_owned(),
                icon: Some(IconId::Close),
                shortcut: None,
                danger: false,
                separator_before: true,
                action: UiAction::CloseOtherTabs {
                    session_id: session.clone(),
                    keep_window_id: win.clone(),
                },
            },
            ContextMenuItem {
                label: "Close Tab".to_owned(),
                icon: Some(IconId::Close),
                shortcut: Some("Ctrl+Alt+W".to_owned()),
                danger: true,
                separator_before: false,
                action: UiAction::CloseTab {
                    host_id: host,
                    session_id: session,
                    window_id: win,
                },
            },
        ];
        Self {
            anchor_x,
            anchor_y,
            items,
            hovered_index: None,
        }
    }

    /// Builds the session right-click context menu with collapse & T3 state awareness.
    #[must_use]
    pub fn for_session_with_state(
        anchor_x: f32,
        anchor_y: f32,
        host_id: impl Into<String>,
        session_id: impl Into<String>,
        muted: bool,
        collapsed: bool,
        is_t3: bool,
    ) -> Self {
        let host = host_id.into();
        let session = session_id.into();
        let items = vec![
            if is_t3 {
                ContextMenuItem {
                    label: "New T3 Thread".to_owned(),
                    icon: Some(IconId::T3),
                    shortcut: Some("Ctrl+Alt+N".to_owned()),
                    danger: false,
                    separator_before: false,
                    action: UiAction::NewT3Thread {
                        host_id: host.clone(),
                        session_id: session.clone(),
                    },
                }
            } else {
                ContextMenuItem {
                    label: "New Terminal Tab".to_owned(),
                    icon: Some(IconId::Plus),
                    shortcut: Some("Ctrl+Shift+T".to_owned()),
                    danger: false,
                    separator_before: false,
                    action: UiAction::NewTab {
                        host_id: Some(host.clone()),
                        session_id: Some(session.clone()),
                    },
                }
            },
            ContextMenuItem {
                label: if collapsed {
                    "Expand Session".to_owned()
                } else {
                    "Collapse Session".to_owned()
                },
                icon: Some(if collapsed {
                    IconId::ChevronDown
                } else {
                    IconId::ChevronRight
                }),
                shortcut: None,
                danger: false,
                separator_before: false,
                action: UiAction::ToggleSessionCollapse {
                    host_id: host.clone(),
                    session_id: session.clone(),
                },
            },
            ContextMenuItem {
                label: "Rename Session…".to_owned(),
                icon: Some(IconId::Tag),
                shortcut: None,
                danger: false,
                separator_before: false,
                action: UiAction::OpenModal(crate::draw::ModalKind::Rename),
            },
            ContextMenuItem {
                label: if muted {
                    "Unmute Session".to_owned()
                } else {
                    "Mute Session".to_owned()
                },
                icon: Some(if muted { IconId::Bell } else { IconId::BellOff }),
                shortcut: None,
                danger: false,
                separator_before: true,
                action: UiAction::ToggleSessionMute {
                    host_id: host,
                    session_id: session.clone(),
                },
            },
            ContextMenuItem {
                label: "Collapse Other Sessions".to_owned(),
                icon: Some(IconId::ChevronRight),
                shortcut: None,
                danger: false,
                separator_before: false,
                action: UiAction::CollapseOtherSessions {
                    keep_session_id: session.clone(),
                },
            },
            ContextMenuItem {
                label: "Close Session".to_owned(),
                icon: Some(IconId::Close),
                shortcut: None,
                danger: true,
                separator_before: true,
                action: UiAction::CloseSession {
                    session_id: session,
                },
            },
        ];
        Self {
            anchor_x,
            anchor_y,
            items,
            hovered_index: None,
        }
    }

    /// Builds the host-header right-click context menu matching macOS Rex
    /// (`New Session on Host`, `Disconnect`/`Connect`, `Edit Host…`, `Collapse`/`Expand`, `Remove Host`).
    #[must_use]
    pub fn for_host(
        anchor_x: f32,
        anchor_y: f32,
        host_id: impl Into<String>,
        is_local: bool,
        collapsed: bool,
        is_connected: bool,
    ) -> Self {
        let host = host_id.into();
        let mut items = vec![ContextMenuItem {
            label: "New Session on Host".to_owned(),
            icon: Some(IconId::Plus),
            shortcut: None,
            danger: false,
            separator_before: false,
            action: UiAction::NewSession {
                host_id: Some(host.clone()),
            },
        }];
        if !is_local {
            if is_connected {
                items.push(ContextMenuItem {
                    label: "Disconnect".to_owned(),
                    icon: Some(IconId::Server),
                    shortcut: None,
                    danger: false,
                    separator_before: true,
                    action: UiAction::DisconnectHost {
                        host_id: host.clone(),
                    },
                });
            } else {
                items.push(ContextMenuItem {
                    label: "Connect".to_owned(),
                    icon: Some(IconId::Server),
                    shortcut: None,
                    danger: false,
                    separator_before: true,
                    action: UiAction::ReconnectHost {
                        host_id: host.clone(),
                    },
                });
            }
            items.push(ContextMenuItem {
                label: "Edit Host…".to_owned(),
                icon: Some(IconId::Tag),
                shortcut: None,
                danger: false,
                separator_before: false,
                action: UiAction::StartRenameHost {
                    host_id: host.clone(),
                },
            });
        }
        items.push(ContextMenuItem {
            label: if collapsed {
                "Expand Host".to_owned()
            } else {
                "Collapse Host".to_owned()
            },
            icon: Some(if collapsed {
                IconId::ChevronDown
            } else {
                IconId::ChevronRight
            }),
            shortcut: None,
            danger: false,
            separator_before: is_local,
            action: UiAction::ToggleHostCollapse {
                host_id: host.clone(),
            },
        });
        if !is_local {
            items.push(ContextMenuItem {
                label: "Remove Host".to_owned(),
                icon: Some(IconId::Close),
                shortcut: None,
                danger: true,
                separator_before: true,
                action: UiAction::RemoveHost { host_id: host },
            });
        }
        Self {
            anchor_x,
            anchor_y,
            items,
            hovered_index: None,
        }
    }

    pub fn move_focus(&mut self, delta: isize) {
        if self.items.is_empty() {
            self.hovered_index = None;
            return;
        }
        let len = self.items.len() as isize;
        let cur = self.hovered_index.map_or(-1, |i| i as isize);
        let next = if cur < 0 {
            if delta >= 0 { 0 } else { len - 1 }
        } else {
            (cur + delta).rem_euclid(len)
        };
        self.hovered_index = Some(next as usize);
    }

    #[must_use]
    pub fn activate_index(&self, index: usize) -> Option<UiAction> {
        self.items.get(index).map(|item| item.action.clone())
    }
}

/// Renders an open context menu clamped strictly inside `viewport`.
pub fn render_context_menu(
    draw_list: &mut UiDrawList,
    viewport: RectF,
    menu: &ContextMenuState,
    interaction: UiInteraction<'_>,
    theme: &Theme,
) {
    if menu.items.is_empty() || viewport.is_empty() {
        return;
    }

    let tokens = theme.effective_ui();

    // Transparent full-viewport backdrop so clicking anywhere outside closes the menu cleanly.
    draw_list.push_hit(
        viewport,
        HitTarget::ContextMenuBackdrop,
        z_layer::CONTEXT_MENU_BACKDROP,
    );

    const MENU_W: f32 = 224.0;
    const ROW_H: f32 = 30.0;
    const SEP_H: f32 = 9.0;
    const PAD: f32 = 6.0;

    let sep_count = menu.items.iter().filter(|i| i.separator_before).count();
    let total_h = PAD * 2.0 + menu.items.len() as f32 * ROW_H + sep_count as f32 * SEP_H;
    let w = MENU_W.min((viewport.w - 16.0).max(0.0));
    let h = total_h.min((viewport.h - 16.0).max(0.0));

    let x = if menu.anchor_x + w > viewport.right() - 8.0 {
        (viewport.right() - w - 8.0).max(viewport.x + 8.0)
    } else {
        menu.anchor_x.max(viewport.x + 8.0)
    };
    let y = if menu.anchor_y + h > viewport.bottom() - 8.0 {
        (viewport.bottom() - h - 8.0).max(viewport.y + 8.0)
    } else {
        menu.anchor_y.max(viewport.y + 8.0)
    };
    let card = RectF::new(x.round(), y.round(), w.round(), h.round());

    draw_list.push_rect(
        card,
        tokens.surface_modal,
        Some(tokens.border_modal),
        1.0,
        cap_radius(9.0, theme.rounding),
        Some(ShadowSpec {
            offset_x: 0.0,
            offset_y: 12.0,
            blur_radius: 32.0,
            spread: 0.0,
            color: tokens.shadow_color,
        }),
    );

    draw_list.push_clip(card);
    let mut row_y = card.y + PAD;
    let row_r = cap_radius(6.0, theme.rounding);

    for (idx, item) in menu.items.iter().enumerate() {
        if item.separator_before {
            draw_list.push_rect(
                RectF::new(card.x + 10.0, row_y + 4.0, (card.w - 20.0).max(0.0), 1.0),
                tokens.border_hairline,
                None,
                0.0,
                0.0,
                None,
            );
            row_y += SEP_H;
        }
        let row_rect = RectF::new(card.x + PAD, row_y, (card.w - PAD * 2.0).max(0.0), ROW_H);
        let item_target = HitTarget::ContextMenuItem { index: idx };
        let highlighted = menu.hovered_index == Some(idx) || interaction.is_hovered(&item_target);
        let pressed = interaction.is_pressed(&item_target);
        if highlighted || pressed {
            let fill = if item.danger {
                tokens
                    .status_error
                    .with_alpha_f32(if pressed { 0.24 } else { 0.14 })
            } else if pressed {
                tokens.surface_row_active
            } else {
                tokens.surface_row_selected
            };
            draw_list.push_rect(row_rect, fill, None, 0.0, row_r, None);
        }

        let fg = if item.danger {
            tokens.status_error
        } else if highlighted {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        };

        let mut text_x = row_rect.x + 8.0;
        if let Some(icon) = item.icon {
            draw_list.push_icon(RectF::new(text_x, row_rect.y + 8.0, 14.0, 14.0), icon, fg);
            text_x += 22.0;
        }

        let shortcut_w = item
            .shortcut
            .as_deref()
            .map_or(0.0, |s| (s.len() as f32 * 6.2 + 8.0).min(86.0));
        draw_list.push_text(
            RectF::new(
                text_x,
                row_rect.y + 7.0,
                (row_rect.right() - text_x - shortcut_w - 6.0).max(0.0),
                16.0,
            ),
            &item.label,
            fg,
            FontKind::Sans,
            12.0,
            FontWeight::Medium,
            TextAlign::Left,
            true,
        );

        if let Some(sc) = &item.shortcut {
            draw_list.push_text(
                RectF::new(
                    (row_rect.right() - shortcut_w - 4.0).max(text_x),
                    row_rect.y + 8.0,
                    shortcut_w,
                    14.0,
                ),
                sc,
                tokens.fg_muted,
                FontKind::Mono,
                10.0,
                FontWeight::Regular,
                TextAlign::Right,
                false,
            );
        }

        draw_list.push_hit(
            row_rect,
            HitTarget::ContextMenuItem { index: idx },
            z_layer::CONTEXT_MENU,
        );
        row_y += ROW_H;
    }
    draw_list.pop_clip();
}

/// Hover tooltip / image preview card displayed over hovered terminal file paths or images.
#[derive(Debug, Clone, PartialEq)]
pub enum HoverPreviewState {
    FilePath {
        path: String,
        line: Option<u32>,
        column: Option<u32>,
        anchor_x: f32,
        anchor_y: f32,
    },
    ImageCard {
        caption: String,
        image_width: u32,
        image_height: u32,
        anchor_x: f32,
        anchor_y: f32,
    },
}

/// Renders a file-path hover tooltip or image preview card frame.
///
/// For `ImageCard`, returns the inner pixel [`RectF`] where `gpu-renderer::image_pass` should draw
/// the preview texture.
pub fn render_hover_preview(
    draw_list: &mut UiDrawList,
    viewport: RectF,
    preview: &HoverPreviewState,
    theme: &Theme,
) -> Option<RectF> {
    let tokens = theme.effective_ui();
    let shadow = ShadowSpec {
        offset_x: 0.0,
        offset_y: 8.0,
        blur_radius: 24.0,
        spread: 0.0,
        color: tokens.shadow_color,
    };

    match preview {
        HoverPreviewState::FilePath {
            path,
            line,
            column,
            anchor_x,
            anchor_y,
        } => {
            let loc = match (line, column) {
                (Some(l), Some(c)) => format!("{path}:{l}:{c}"),
                (Some(l), None) => format!("{path}:{l}"),
                _ => path.clone(),
            };
            let w = ((loc.chars().count() as f32 * 6.8 + 44.0).clamp(160.0, 420.0))
                .min((viewport.w - 16.0).max(0.0));
            let h = 44.0;
            let x = (*anchor_x).clamp(
                viewport.x + 8.0,
                (viewport.right() - w - 8.0).max(viewport.x + 8.0),
            );
            let y = if *anchor_y - h - 10.0 >= viewport.y + 8.0 {
                *anchor_y - h - 10.0
            } else {
                (*anchor_y + 18.0).min((viewport.bottom() - h - 8.0).max(viewport.y + 8.0))
            };
            let card = RectF::new(x.round(), y.round(), w.round(), h);
            draw_list.push_rect(
                card,
                tokens.surface_modal,
                Some(tokens.border_subtle),
                1.0,
                cap_radius(7.0, theme.rounding),
                Some(shadow),
            );
            draw_list.push_icon(
                RectF::new(card.x + 8.0, card.y + 8.0, 14.0, 14.0),
                IconId::Document,
                tokens.accent_primary,
            );
            draw_list.push_text(
                RectF::new(card.x + 28.0, card.y + 6.0, (card.w - 36.0).max(0.0), 16.0),
                loc,
                tokens.fg_primary,
                FontKind::Mono,
                11.5,
                FontWeight::Medium,
                TextAlign::Left,
                true,
            );
            draw_list.push_text(
                RectF::new(card.x + 28.0, card.y + 23.0, (card.w - 36.0).max(0.0), 14.0),
                "Ctrl+Click to open",
                tokens.fg_muted,
                FontKind::Sans,
                10.5,
                FontWeight::Regular,
                TextAlign::Left,
                true,
            );
            None
        }
        HoverPreviewState::ImageCard {
            caption,
            image_width,
            image_height,
            anchor_x,
            anchor_y,
        } => {
            let aspect = if *image_height > 0 {
                (*image_width as f32 / *image_height as f32).clamp(0.4, 2.5)
            } else {
                1.5
            };
            let inner_w = 240.0_f32.min((viewport.w - 32.0).max(60.0));
            let inner_h = (inner_w / aspect).clamp(90.0, 220.0);
            let card_w = inner_w + 16.0;
            let card_h = inner_h + 36.0;
            let x = (*anchor_x).clamp(
                viewport.x + 8.0,
                (viewport.right() - card_w - 8.0).max(viewport.x + 8.0),
            );
            let y = if *anchor_y - card_h - 10.0 >= viewport.y + 8.0 {
                *anchor_y - card_h - 10.0
            } else {
                (*anchor_y + 18.0).min((viewport.bottom() - card_h - 8.0).max(viewport.y + 8.0))
            };
            let card = RectF::new(x.round(), y.round(), card_w.round(), card_h.round());
            draw_list.push_rect(
                card,
                tokens.surface_modal,
                Some(tokens.border_modal),
                1.0,
                cap_radius(9.0, theme.rounding),
                Some(shadow),
            );
            let image_slot =
                RectF::new(card.x + 8.0, card.y + 8.0, inner_w.round(), inner_h.round());
            draw_list.push_rect(
                image_slot,
                tokens.surface_pane,
                Some(tokens.border_hairline),
                1.0,
                cap_radius(5.0, theme.rounding),
                None,
            );
            draw_list.push_text(
                RectF::new(
                    card.x + 10.0,
                    image_slot.bottom() + 6.0,
                    (card.w - 20.0).max(0.0),
                    16.0,
                ),
                format!("{caption} · {image_width}×{image_height}"),
                tokens.fg_muted,
                FontKind::Mono,
                10.5,
                FontWeight::Regular,
                TextAlign::Left,
                true,
            );
            Some(image_slot)
        }
    }
}

/// Renders a sleek, viewport-clamped hover tooltip pill anchored near an icon-only control.
pub fn render_tooltip(
    draw_list: &mut UiDrawList,
    viewport: RectF,
    anchor_rect: RectF,
    label: &str,
    theme: &Theme,
) {
    if label.is_empty() || viewport.is_empty() || anchor_rect.is_empty() {
        return;
    }
    let tokens = theme.effective_ui();
    let w = ((label.chars().count() as f32) * 6.5 + 18.0)
        .clamp(60.0, 268.0)
        .min((viewport.w - 16.0).max(40.0));
    let h = 24.0;
    let (acx, _) = anchor_rect.center();
    let x = (acx - w * 0.5).clamp(
        viewport.x + 8.0,
        (viewport.right() - w - 8.0).max(viewport.x + 8.0),
    );
    let below_y = anchor_rect.bottom() + 6.0;
    let above_y = anchor_rect.y - h - 6.0;
    let y = if below_y + h <= viewport.bottom() - 8.0 {
        below_y
    } else if above_y >= viewport.y + 8.0 {
        above_y
    } else {
        (viewport.bottom() - h - 8.0).max(viewport.y + 8.0)
    };
    let card = RectF::new(x.round(), y.round(), w.round(), h);
    draw_list.push_rect(
        card,
        tokens.surface_modal,
        Some(tokens.border_modal),
        1.0,
        cap_radius(6.0, theme.rounding),
        Some(ShadowSpec {
            offset_x: 0.0,
            offset_y: 6.0,
            blur_radius: 18.0,
            spread: 0.0,
            color: tokens.shadow_color,
        }),
    );
    draw_list.push_text(
        RectF::new(card.x + 8.0, card.y + 4.0, (card.w - 16.0).max(0.0), 16.0),
        label,
        tokens.fg_primary,
        FontKind::Sans,
        11.0,
        FontWeight::Medium,
        TextAlign::Center,
        true,
    );
}

/// Renders specialized chrome for `tr3x`'s `/pr` floating drawer (`sup` review/merge overlay).
pub fn render_pr_drawer_chrome(
    draw_list: &mut UiDrawList,
    geo: &FloatingPaneGeometry<String>,
    pr_subtitle: Option<&str>,
    interaction: UiInteraction<'_>,
    tokens: &UiTokens,
    theme_rounding: f32,
    z: u16,
) {
    let radius = if geo.is_right_drawer {
        0.0
    } else {
        cap_radius(10.0, theme_rounding)
    };
    let shadow = ShadowSpec {
        offset_x: if geo.is_right_drawer { -10.0 } else { 0.0 },
        offset_y: if geo.is_right_drawer { 0.0 } else { 14.0 },
        blur_radius: 36.0,
        spread: 0.0,
        color: tokens.shadow_color,
    };

    draw_list.push_rect(
        geo.card_rect,
        tokens.surface_pane,
        Some(tokens.border_subtle),
        1.0,
        radius,
        Some(shadow),
    );

    // Accent left highlight edge on full-height right-docked `/pr` drawer
    if geo.is_right_drawer {
        draw_list.push_rect(
            RectF::new(geo.card_rect.x, geo.card_rect.y, 2.0, geo.card_rect.h),
            tokens.accent_primary,
            None,
            0.0,
            0.0,
            None,
        );
    }

    // Header bar (`30px`) with branch glyph, "Pull Request", subtitle, `sup` key hints, dock + close
    let hdr = geo.header_rect;
    draw_list.push_rect(hdr, tokens.surface_pane_header, None, 0.0, 0.0, None);
    draw_list.push_rect(
        RectF::new(hdr.x, (hdr.bottom() - 1.0).max(hdr.y), hdr.w, 1.0),
        tokens.border_hairline,
        None,
        0.0,
        0.0,
        None,
    );

    draw_list.push_icon(
        RectF::new(hdr.x + 12.0, hdr.y + 8.0, 14.0, 14.0),
        IconId::Branch,
        tokens.accent_primary,
    );
    let title = match pr_subtitle {
        Some(sub) if !sub.is_empty() => format!("Pull Request · {sub}"),
        _ => "Pull Request".to_owned(),
    };
    let hints_w = if hdr.w >= 380.0 { 164.0 } else { 0.0 };
    let title_w = (hdr.w - 32.0 - hints_w - 54.0).max(0.0);
    draw_list.push_text(
        RectF::new(hdr.x + 32.0, hdr.y + 6.0, title_w, 18.0),
        title,
        tokens.fg_primary,
        FontKind::Sans,
        12.0,
        FontWeight::SemiBold,
        TextAlign::Left,
        true,
    );

    if hints_w > 0.0 {
        draw_list.push_text(
            RectF::new(
                (hdr.right() - 54.0 - hints_w).max(hdr.x),
                hdr.y + 7.0,
                hints_w,
                16.0,
            ),
            "m merge · A approve · q close",
            tokens.fg_muted,
            FontKind::Mono,
            10.0,
            FontWeight::Regular,
            TextAlign::Right,
            true,
        );
    }

    let close_btn = RectF::new((hdr.right() - 26.0).max(hdr.x), hdr.y + 5.0, 20.0, 20.0);
    let dock_btn = RectF::new((close_btn.x - 24.0).max(hdr.x), hdr.y + 5.0, 20.0, 20.0);
    let dock_target = HitTarget::PaneButton {
        block_id: geo.id.clone(),
        button: PaneHeaderButton::FloatOrDock,
    };
    let close_target = HitTarget::PaneButton {
        block_id: geo.id.clone(),
        button: PaneHeaderButton::Close,
    };
    let btn_r = cap_radius(4.0, theme_rounding);
    if let Some((fill, border)) = interaction.button_surface(&dock_target, false, tokens) {
        draw_list.push_rect(dock_btn, fill, border, 1.0, btn_r, None);
    }
    draw_list.push_icon(
        dock_btn.inset_uniform(3.0),
        IconId::Dock,
        if interaction.is_hovered(&dock_target) {
            tokens.fg_primary
        } else {
            tokens.fg_secondary
        },
    );
    let (close_bg, close_tint) =
        interaction.danger_button_style(&close_target, tokens.fg_secondary, tokens);
    if let Some((fill, border)) = close_bg {
        draw_list.push_rect(close_btn, fill, border, 1.0, btn_r, None);
    }
    draw_list.push_icon(close_btn.inset_uniform(3.0), IconId::Close, close_tint);
    draw_list.push_hit(dock_btn, dock_target, z + 2);
    draw_list.push_hit(close_btn, close_target, z + 2);
}
