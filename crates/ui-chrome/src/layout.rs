//! Taffy-backed window chrome layout, binary split geometry, floating layers, capsules, and DnD hit-testing.

use taffy::prelude::*;

/// Axis-aligned rectangle in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RectF {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl RectF {
    #[must_use]
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    #[must_use]
    pub fn right(self) -> f32 {
        self.x + self.w
    }

    #[must_use]
    pub fn bottom(self) -> f32 {
        self.y + self.h
    }

    #[must_use]
    pub fn center(self) -> (f32, f32) {
        (self.x + self.w * 0.5, self.y + self.h * 0.5)
    }

    #[must_use]
    pub fn is_empty(self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    #[must_use]
    pub fn contains(self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }

    #[must_use]
    pub fn inset(self, left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self {
            x: self.x + left,
            y: self.y + top,
            w: (self.w - left - right).max(0.0),
            h: (self.h - top - bottom).max(0.0),
        }
    }

    #[must_use]
    pub fn inset_uniform(self, pad: f32) -> Self {
        self.inset(pad, pad, pad, pad)
    }

    #[must_use]
    pub fn intersect(self, other: Self) -> Self {
        let x1 = self.x.max(other.x);
        let y1 = self.y.max(other.y);
        let x2 = self.right().min(other.right());
        let y2 = self.bottom().min(other.bottom());
        Self {
            x: x1,
            y: y1,
            w: (x2 - x1).max(0.0),
            h: (y2 - y1).max(0.0),
        }
    }

    fn from_taffy(layout: &Layout, origin_x: f32, origin_y: f32) -> Self {
        Self {
            x: origin_x + layout.location.x,
            y: origin_y + layout.location.y,
            w: layout.size.width.max(0.0),
            h: layout.size.height.max(0.0),
        }
    }
}

/// Which edge of the window hosts the Rex toolbar (sidebar or horizontal tab bar).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolbarPosition {
    #[default]
    Left,
    Right,
    Top,
    Bottom,
}

impl ToolbarPosition {
    #[must_use]
    pub const fn is_sidebar(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }
}

/// Visibility rule for the toolbar (`interface.toolbar.auto_hide_toolbar`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolbarBehavior {
    #[default]
    Always,
    OnHover,
    Hidden,
}

/// Pane density preset (`interface.panes.density`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaneDensity {
    /// Inset rounded pane cards (`6px` outer margin, `6px` split gap, `8px` default corner radius).
    #[default]
    Comfortable,
    /// Tight edge-to-edge splits (`0px` outer margin, `1px` hairline divider, `0px` corner radius).
    Compact,
}

/// Pane header visibility mode (`interface.panes.headers`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaneHeaderMode {
    Always,
    #[default]
    OnHover,
    Hidden,
}

/// Anchor corner for minimized floating pane capsules (`floating_windows.minimized_corner`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MinimizedCorner {
    #[default]
    BottomRight,
    BottomLeft,
    TopRight,
    TopLeft,
}

/// Layout configuration parameters matching Rex's GUI spec and Omarchy Hyprland defaults.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromeLayoutConfig {
    pub toolbar_position: ToolbarPosition,
    pub toolbar_behavior: ToolbarBehavior,
    /// Desired sidebar width in logical px (clamped to `180.0..=380.0`).
    pub sidebar_width: f32,
    /// Explicit user collapse toggle (`client.sidebar.toggle`).
    pub sidebar_collapsed: bool,
    /// True while pointer hovers the hot edge or `client.toolbar.reveal` is active.
    pub toolbar_revealed: bool,
    /// Height of horizontal tab bar (`top`/`bottom`), default `38.0`.
    pub tab_bar_height: f32,
    /// Height of pane headers when visible, default `30.0`.
    pub pane_header_height: f32,
    pub density: PaneDensity,
    pub pane_header_mode: PaneHeaderMode,
    /// Optional Hyprland `decoration:rounding` override (`Some(0.0)` gives crisp sharp corners).
    pub hyprland_rounding: Option<f32>,
    pub minimized_corner: MinimizedCorner,
}

impl Default for ChromeLayoutConfig {
    fn default() -> Self {
        Self {
            toolbar_position: ToolbarPosition::Left,
            toolbar_behavior: ToolbarBehavior::Always,
            sidebar_width: 248.0,
            sidebar_collapsed: false,
            toolbar_revealed: false,
            tab_bar_height: 38.0,
            pane_header_height: 30.0,
            density: PaneDensity::Comfortable,
            pane_header_mode: PaneHeaderMode::Always,
            hyprland_rounding: None,
            minimized_corner: MinimizedCorner::BottomRight,
        }
    }
}

impl ChromeLayoutConfig {
    pub const MIN_SIDEBAR_WIDTH: f32 = 180.0;
    pub const MAX_SIDEBAR_WIDTH: f32 = 380.0;
    /// Maximum fraction of the window width a docked sidebar may occupy on narrow viewports.
    pub const MAX_SIDEBAR_VIEWPORT_RATIO: f32 = 0.30;
    /// Viewport width threshold below which a docked sidebar automatically collapses
    /// (and restores automatically when widened back, unless manually collapsed).
    pub const AUTO_COLLAPSE_VIEWPORT_WIDTH: f32 = 720.0;
    pub const HOT_EDGE_THICKNESS: f32 = 6.0;
    pub const RESIZE_HANDLE_WIDTH: f32 = 6.0;

    #[must_use]
    pub fn effective_sidebar_width(&self) -> f32 {
        self.sidebar_width
            .clamp(Self::MIN_SIDEBAR_WIDTH, Self::MAX_SIDEBAR_WIDTH)
    }

    /// Returns `true` when the sidebar is responsively auto-collapsed due to a narrow viewport
    /// (`viewport_w < 720px`) while the user has not manually collapsed it.
    #[must_use]
    pub fn is_sidebar_auto_collapsed(&self, viewport_w: f32) -> bool {
        self.toolbar_position.is_sidebar()
            && self.toolbar_behavior == ToolbarBehavior::Always
            && !self.sidebar_collapsed
            && viewport_w < Self::AUTO_COLLAPSE_VIEWPORT_WIDTH
    }

    /// Computes the docked sidebar width adapted proportionally to `viewport_w`, never exceeding
    /// `min(configured_width, viewport_w * 0.30)`.
    #[must_use]
    pub fn responsive_sidebar_width(&self, viewport_w: f32) -> f32 {
        let vw = viewport_w.max(0.0);
        if vw <= 0.0 {
            return 0.0;
        }
        self.effective_sidebar_width()
            .min(vw * Self::MAX_SIDEBAR_VIEWPORT_RATIO)
            .floor()
            .max(0.0)
    }

    /// Corner radius for inset pane cards (`0.0` in `Compact`, `8.0` default in `Comfortable`
    /// even when Hyprland outer window rounding is `0`, harmonizing up to `16.0` when `> 0`).
    #[must_use]
    pub fn pane_corner_radius(&self) -> f32 {
        match self.density {
            PaneDensity::Compact => 0.0,
            PaneDensity::Comfortable => match self.hyprland_rounding {
                Some(r) if r > 0.0 => r.clamp(4.0, 16.0),
                _ => 8.0,
            },
        }
    }

    #[must_use]
    pub const fn outer_margin(&self) -> f32 {
        match self.density {
            PaneDensity::Comfortable => 10.0,
            PaneDensity::Compact => 0.0,
        }
    }

    #[must_use]
    pub const fn split_gap(&self) -> f32 {
        match self.density {
            PaneDensity::Comfortable => 8.0,
            PaneDensity::Compact => 1.0,
        }
    }
}

/// Top-level window chrome geometry computed via `taffy`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ChromeRects {
    pub viewport: RectF,
    /// Left/Right sidebar rect when visible (docked or hover overlay).
    pub sidebar: Option<RectF>,
    /// Drag-resize hit strip along the inner edge of the sidebar.
    pub sidebar_resize_handle: Option<RectF>,
    /// Top/Bottom horizontal tab strip rect when visible (docked or hover overlay).
    pub tab_bar: Option<RectF>,
    /// Workspace area available for tiled panes and floating layers.
    pub workspace: RectF,
    /// 6px edge trigger zone when `ToolbarBehavior::OnHover` or narrow auto-collapse is active.
    pub hot_edge: Option<RectF>,
    /// True when the visible toolbar floats over `workspace` instead of shrinking it.
    pub toolbar_is_overlay: bool,
    /// True when the sidebar was auto-collapsed solely because `viewport_w < 720px`
    /// (and will automatically restore once widened back unless manually collapsed).
    pub sidebar_auto_collapsed: bool,
}

/// Computes outer window chrome geometry using a `taffy::TaffyTree`.
///
/// Pure function of `(viewport_w, viewport_h, cfg)` — never caches stale viewport dimensions.
#[must_use]
pub fn compute_chrome_layout(
    viewport_w: f32,
    viewport_h: f32,
    cfg: &ChromeLayoutConfig,
) -> ChromeRects {
    let vw = viewport_w.max(0.0);
    let vh = viewport_h.max(0.0);
    let viewport = RectF::new(0.0, 0.0, vw, vh);

    let sidebar_auto_collapsed = cfg.is_sidebar_auto_collapsed(vw);

    let toolbar_docked = match cfg.toolbar_behavior {
        ToolbarBehavior::Always => {
            !(cfg.toolbar_position.is_sidebar()
                && (cfg.sidebar_collapsed || sidebar_auto_collapsed))
        }
        ToolbarBehavior::OnHover | ToolbarBehavior::Hidden => false,
    };

    let toolbar_overlay = match cfg.toolbar_behavior {
        ToolbarBehavior::Always => sidebar_auto_collapsed && cfg.toolbar_revealed,
        ToolbarBehavior::OnHover | ToolbarBehavior::Hidden => {
            cfg.toolbar_revealed && !(cfg.toolbar_position.is_sidebar() && cfg.sidebar_collapsed)
        }
    };

    let docked_sidebar_w = cfg.responsive_sidebar_width(vw);
    let overlay_sidebar_w = cfg
        .effective_sidebar_width()
        .min((vw - 32.0).max(0.0))
        .min(vw * 0.78)
        .floor()
        .max(0.0);
    let sidebar_w = if toolbar_overlay {
        overlay_sidebar_w
    } else {
        docked_sidebar_w
    };
    let tab_bar_h = cfg.tab_bar_height.max(0.0).min(vh * 0.5);

    // Solve docked layout through Taffy flexbox.
    let mut tree: TaffyTree<()> = TaffyTree::new();
    let workspace_node = tree
        .new_leaf(Style {
            flex_grow: 1.0,
            flex_shrink: 1.0,
            size: Size {
                width: auto(),
                height: auto(),
            },
            ..Style::DEFAULT
        })
        .expect("taffy leaf");

    let toolbar_node = if toolbar_docked {
        let size = if cfg.toolbar_position.is_sidebar() {
            Size {
                width: length(sidebar_w),
                height: percent(1.0),
            }
        } else {
            Size {
                width: percent(1.0),
                height: length(tab_bar_h),
            }
        };
        Some(
            tree.new_leaf(Style {
                flex_shrink: 0.0,
                size,
                ..Style::DEFAULT
            })
            .expect("taffy toolbar leaf"),
        )
    } else {
        None
    };

    let (flex_direction, children): (FlexDirection, Vec<NodeId>) =
        match (cfg.toolbar_position, toolbar_node) {
            (ToolbarPosition::Left, Some(tb)) => (FlexDirection::Row, vec![tb, workspace_node]),
            (ToolbarPosition::Right, Some(tb)) => (FlexDirection::Row, vec![workspace_node, tb]),
            (ToolbarPosition::Top, Some(tb)) => (FlexDirection::Column, vec![tb, workspace_node]),
            (ToolbarPosition::Bottom, Some(tb)) => {
                (FlexDirection::Column, vec![workspace_node, tb])
            }
            (pos, None) => (
                if pos.is_sidebar() {
                    FlexDirection::Row
                } else {
                    FlexDirection::Column
                },
                vec![workspace_node],
            ),
        };

    let root = tree
        .new_with_children(
            Style {
                display: Display::Flex,
                flex_direction,
                size: Size {
                    width: length(vw),
                    height: length(vh),
                },
                ..Style::DEFAULT
            },
            &children,
        )
        .expect("taffy root");

    tree.compute_layout(
        root,
        Size {
            width: AvailableSpace::Definite(vw),
            height: AvailableSpace::Definite(vh),
        },
    )
    .expect("taffy compute_layout");

    let workspace = RectF::from_taffy(
        tree.layout(workspace_node).expect("workspace layout"),
        0.0,
        0.0,
    );

    let toolbar_rect = if let Some(tb) = toolbar_node {
        Some(RectF::from_taffy(
            tree.layout(tb).expect("toolbar layout"),
            0.0,
            0.0,
        ))
    } else if toolbar_overlay {
        Some(match cfg.toolbar_position {
            ToolbarPosition::Left => RectF::new(0.0, 0.0, sidebar_w, vh),
            ToolbarPosition::Right => RectF::new((vw - sidebar_w).max(0.0), 0.0, sidebar_w, vh),
            ToolbarPosition::Top => RectF::new(0.0, 0.0, vw, tab_bar_h),
            ToolbarPosition::Bottom => RectF::new(0.0, (vh - tab_bar_h).max(0.0), vw, tab_bar_h),
        })
    } else {
        None
    };

    let (sidebar, tab_bar) = if cfg.toolbar_position.is_sidebar() {
        (toolbar_rect, None)
    } else {
        (None, toolbar_rect)
    };

    let sidebar_resize_handle = sidebar.map(|sb| {
        let half = ChromeLayoutConfig::RESIZE_HANDLE_WIDTH * 0.5;
        let edge_x = match cfg.toolbar_position {
            ToolbarPosition::Left => sb.right() - half,
            ToolbarPosition::Right => sb.x - half,
            _ => sb.right() - half,
        };
        RectF::new(
            edge_x.max(0.0),
            sb.y,
            ChromeLayoutConfig::RESIZE_HANDLE_WIDTH,
            sb.h,
        )
    });

    let hot_edge = if (cfg.toolbar_behavior == ToolbarBehavior::OnHover || sidebar_auto_collapsed)
        && toolbar_rect.is_none()
    {
        let t = ChromeLayoutConfig::HOT_EDGE_THICKNESS;
        Some(match cfg.toolbar_position {
            ToolbarPosition::Left => RectF::new(0.0, 0.0, t.min(vw), vh),
            ToolbarPosition::Right => RectF::new((vw - t).max(0.0), 0.0, t.min(vw), vh),
            ToolbarPosition::Top => RectF::new(0.0, 0.0, vw, t.min(vh)),
            ToolbarPosition::Bottom => RectF::new(0.0, (vh - t).max(0.0), vw, t.min(vh)),
        })
    } else {
        None
    };

    ChromeRects {
        viewport,
        sidebar,
        sidebar_resize_handle,
        tab_bar,
        workspace,
        hot_edge,
        toolbar_is_overlay: toolbar_overlay,
        sidebar_auto_collapsed,
    }
}

/// Direction of a binary split container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitAxis {
    /// Children placed side-by-side (left | right).
    Horizontal,
    /// Children stacked top-to-bottom (top / bottom).
    Vertical,
}

/// Recursive binary split tree representing a Rex window's base layer.
#[derive(Debug, Clone, PartialEq)]
pub enum SplitNode<Id> {
    Leaf {
        id: Id,
    },
    Split {
        axis: SplitAxis,
        /// Fraction of available space assigned to `first`, clamped to `0.1..=0.9`.
        ratio: f32,
        first: Box<SplitNode<Id>>,
        second: Box<SplitNode<Id>>,
    },
}

impl<Id> SplitNode<Id> {
    #[must_use]
    pub fn leaf(id: Id) -> Self {
        Self::Leaf { id }
    }

    #[must_use]
    pub fn split(axis: SplitAxis, ratio: f32, first: Self, second: Self) -> Self {
        Self::Split {
            axis,
            ratio: ratio.clamp(0.1, 0.9),
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    #[must_use]
    pub fn leaf_count(&self) -> usize {
        match self {
            Self::Leaf { .. } => 1,
            Self::Split { first, second, .. } => first.leaf_count() + second.leaf_count(),
        }
    }

    #[must_use]
    pub fn contains_leaf(&self, target: &Id) -> bool
    where
        Id: PartialEq,
    {
        match self {
            Self::Leaf { id } => id == target,
            Self::Split { first, second, .. } => {
                first.contains_leaf(target) || second.contains_leaf(target)
            }
        }
    }
}

/// Resolved pixel geometry for a single tiled pane.
#[derive(Debug, Clone, PartialEq)]
pub struct PaneGeometry<Id> {
    pub id: Id,
    /// Outer inset card rect (for background, border, and active focus highlight).
    pub card_rect: RectF,
    /// Header strip rect if visible (`Always` or hovered/revealed under `OnHover`).
    pub header_rect: Option<RectF>,
    /// Inner terminal viewport rect (below the header if docked).
    pub terminal_rect: RectF,
    pub corner_radius: f32,
    pub is_zoomed: bool,
}

/// Resolved geometry for a draggable divider between two split subtrees.
#[derive(Debug, Clone, PartialEq)]
pub struct DividerGeometry {
    pub axis: SplitAxis,
    /// Visual gap/hairline rect.
    pub visual_rect: RectF,
    /// Expanded `6px` pointer hit-test rect centered on `visual_rect`.
    pub hit_rect: RectF,
    /// Path from tree root to this split node (`false` = first child, `true` = second child).
    pub split_path: Vec<bool>,
    /// Bounding rect of the parent split container (used to convert pointer drags into ratios).
    pub parent_rect: RectF,
    pub current_ratio: f32,
}

/// Output of laying out a split tree inside `workspace`.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitLayoutResult<Id> {
    pub panes: Vec<PaneGeometry<Id>>,
    pub dividers: Vec<DividerGeometry>,
}

/// Computes pixel geometry for all tiled panes and split dividers inside `workspace`.
///
/// If `zoomed_pane` matches a leaf in `root`, that pane fills 100% of the padded workspace
/// and `dividers` is empty (leaving the underlying split tree untouched).
#[must_use]
pub fn layout_splits<Id: Clone + PartialEq>(
    workspace: RectF,
    root: &SplitNode<Id>,
    cfg: &ChromeLayoutConfig,
    zoomed_pane: Option<&Id>,
    hovered_header_pane: Option<&Id>,
) -> SplitLayoutResult<Id> {
    let margin = cfg.outer_margin();
    // Avoid doubling margin against a docked sidebar edge; keep clean balanced frame.
    let content = workspace.inset_uniform(margin);
    let corner_radius = cfg.pane_corner_radius();

    if let Some(zoom_id) = zoomed_pane
        && root.contains_leaf(zoom_id)
    {
        let pane = build_pane_geometry(
            zoom_id.clone(),
            content,
            cfg,
            corner_radius,
            true,
            hovered_header_pane == Some(zoom_id),
        );
        return SplitLayoutResult {
            panes: vec![pane],
            dividers: Vec::new(),
        };
    }

    let mut panes = Vec::with_capacity(root.leaf_count());
    let mut dividers = Vec::new();
    let mut path = Vec::new();

    layout_split_recursive(
        content,
        root,
        cfg,
        corner_radius,
        hovered_header_pane,
        &mut path,
        &mut panes,
        &mut dividers,
    );

    SplitLayoutResult { panes, dividers }
}

#[allow(clippy::too_many_arguments)]
fn layout_split_recursive<Id: Clone + PartialEq>(
    rect: RectF,
    node: &SplitNode<Id>,
    cfg: &ChromeLayoutConfig,
    corner_radius: f32,
    hovered_header_pane: Option<&Id>,
    path: &mut Vec<bool>,
    panes: &mut Vec<PaneGeometry<Id>>,
    dividers: &mut Vec<DividerGeometry>,
) {
    match node {
        SplitNode::Leaf { id } => {
            panes.push(build_pane_geometry(
                id.clone(),
                rect,
                cfg,
                corner_radius,
                false,
                hovered_header_pane == Some(id),
            ));
        }
        SplitNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let clamped_ratio = ratio.clamp(0.1, 0.9);
            let gap = cfg.split_gap();
            let hit_thickness = 6.0_f32.max(gap);

            let (first_rect, second_rect, visual_rect, hit_rect) = match axis {
                SplitAxis::Horizontal => {
                    let avail = (rect.w - gap).max(0.0);
                    let w1 = (avail * clamped_ratio).round();
                    let w2 = (avail - w1).max(0.0);
                    let div_x = rect.x + w1;
                    let r1 = RectF::new(rect.x, rect.y, w1, rect.h);
                    let r2 = RectF::new(div_x + gap, rect.y, w2, rect.h);
                    let vis = RectF::new(div_x, rect.y, gap, rect.h);
                    let hit = RectF::new(
                        div_x + gap * 0.5 - hit_thickness * 0.5,
                        rect.y,
                        hit_thickness,
                        rect.h,
                    );
                    (r1, r2, vis, hit)
                }
                SplitAxis::Vertical => {
                    let avail = (rect.h - gap).max(0.0);
                    let h1 = (avail * clamped_ratio).round();
                    let h2 = (avail - h1).max(0.0);
                    let div_y = rect.y + h1;
                    let r1 = RectF::new(rect.x, rect.y, rect.w, h1);
                    let r2 = RectF::new(rect.x, div_y + gap, rect.w, h2);
                    let vis = RectF::new(rect.x, div_y, rect.w, gap);
                    let hit = RectF::new(
                        rect.x,
                        div_y + gap * 0.5 - hit_thickness * 0.5,
                        rect.w,
                        hit_thickness,
                    );
                    (r1, r2, vis, hit)
                }
            };

            dividers.push(DividerGeometry {
                axis: *axis,
                visual_rect,
                hit_rect,
                split_path: path.clone(),
                parent_rect: rect,
                current_ratio: clamped_ratio,
            });

            path.push(false);
            layout_split_recursive(
                first_rect,
                first,
                cfg,
                corner_radius,
                hovered_header_pane,
                path,
                panes,
                dividers,
            );
            path.pop();

            path.push(true);
            layout_split_recursive(
                second_rect,
                second,
                cfg,
                corner_radius,
                hovered_header_pane,
                path,
                panes,
                dividers,
            );
            path.pop();
        }
    }
}

fn build_pane_geometry<Id>(
    id: Id,
    card_rect: RectF,
    cfg: &ChromeLayoutConfig,
    corner_radius: f32,
    is_zoomed: bool,
    header_hover_revealed: bool,
) -> PaneGeometry<Id> {
    let header_h = cfg.pane_header_height.min(card_rect.h);
    match cfg.pane_header_mode {
        PaneHeaderMode::Always => {
            let header_rect = RectF::new(card_rect.x, card_rect.y, card_rect.w, header_h);
            let terminal_rect = RectF::new(
                card_rect.x,
                card_rect.y + header_h,
                card_rect.w,
                (card_rect.h - header_h).max(0.0),
            );
            PaneGeometry {
                id,
                card_rect,
                header_rect: Some(header_rect),
                terminal_rect,
                corner_radius,
                is_zoomed,
            }
        }
        PaneHeaderMode::OnHover => {
            // Under OnHover, the header floats over the top of the card without reflowing terminal rows.
            let header_rect = header_hover_revealed
                .then(|| RectF::new(card_rect.x, card_rect.y, card_rect.w, header_h));
            PaneGeometry {
                id,
                card_rect,
                header_rect,
                terminal_rect: card_rect,
                corner_radius,
                is_zoomed,
            }
        }
        PaneHeaderMode::Hidden => PaneGeometry {
            id,
            card_rect,
            header_rect: None,
            terminal_rect: card_rect,
            corner_radius,
            is_zoomed,
        },
    }
}

/// Finds the split divider hit by pointer coordinates `(px, py)`.
#[must_use]
pub fn hit_test_divider(
    dividers: &[DividerGeometry],
    px: f32,
    py: f32,
) -> Option<&DividerGeometry> {
    dividers.iter().find(|d| d.hit_rect.contains(px, py))
}

/// Converts a pointer drag coordinate into a clamped split ratio in `0.1..=0.9`.
#[must_use]
pub fn compute_drag_ratio(divider: &DividerGeometry, px: f32, py: f32) -> f32 {
    let raw = match divider.axis {
        SplitAxis::Horizontal => {
            if divider.parent_rect.w <= 0.0 {
                return divider.current_ratio;
            }
            (px - divider.parent_rect.x) / divider.parent_rect.w
        }
        SplitAxis::Vertical => {
            if divider.parent_rect.h <= 0.0 {
                return divider.current_ratio;
            }
            (py - divider.parent_rect.y) / divider.parent_rect.h
        }
    };
    raw.clamp(0.1, 0.9)
}

/// Normalized floating layer specification (`session.new_layer` `bounds = [x, y, w, h]`).
#[derive(Debug, Clone, PartialEq)]
pub struct FloatingLayerSpec<Id> {
    pub id: Id,
    /// Normalized `[x, y, w, h]` in `0.0..=1.0` relative to `workspace`.
    pub bounds: [f32; 4],
    pub minimized: bool,
}

impl<Id> FloatingLayerSpec<Id> {
    /// Minimum terminal column width for `tr3x`'s side-by-side `/pr` drawer (from `tr3x/src/pr.rs`).
    pub const TR3X_PR_WIDE_COLS: u16 = 140;

    /// Exact `tr3x` `/pr` drawer bounds:
    /// - `>= 140` columns: `[0.45, 0.0, 0.55, 1.0]` full-height right drawer
    /// - `< 140` columns: `[0.03, 0.03, 0.94, 0.94]` near-fullscreen floating card
    #[must_use]
    pub const fn tr3x_pr_drawer(id: Id, window_columns: u16) -> Self {
        let bounds = if window_columns >= Self::TR3X_PR_WIDE_COLS {
            [0.45, 0.0, 0.55, 1.0]
        } else {
            [0.03, 0.03, 0.94, 0.94]
        };
        Self {
            id,
            bounds,
            minimized: false,
        }
    }

    /// Fallback `tr3x` Rename/Delete floating prompt bounds `[0.2, 0.3, 0.6, 0.3]`.
    #[must_use]
    pub const fn tr3x_prompt(id: Id) -> Self {
        Self {
            id,
            bounds: [0.2, 0.3, 0.6, 0.3],
            minimized: false,
        }
    }

    /// Fallback `tr3x` Find Thread floating bounds `[0.2, 0.2, 0.6, 0.6]`.
    #[must_use]
    pub const fn tr3x_find(id: Id) -> Self {
        Self {
            id,
            bounds: [0.2, 0.2, 0.6, 0.6],
            minimized: false,
        }
    }

    /// `tr3x` Pair / Update floating bounds `[0.15, 0.2, 0.7, 0.5]`.
    #[must_use]
    pub const fn tr3x_utility(id: Id) -> Self {
        Self {
            id,
            bounds: [0.15, 0.2, 0.7, 0.5],
            minimized: false,
        }
    }
}

/// Resolved pixel geometry for a visible floating layer.
#[derive(Debug, Clone, PartialEq)]
pub struct FloatingPaneGeometry<Id> {
    pub id: Id,
    pub card_rect: RectF,
    pub header_rect: RectF,
    pub terminal_rect: RectF,
    pub corner_radius: f32,
    /// True when docked flush to the right edge at full height (like `tr3x` `/pr` wide drawer).
    pub is_right_drawer: bool,
}

/// Computes pixel geometry for all non-minimized floating layers inside `workspace`.
#[must_use]
pub fn layout_floating_layers<Id: Clone>(
    workspace: RectF,
    layers: &[FloatingLayerSpec<Id>],
    cfg: &ChromeLayoutConfig,
) -> Vec<FloatingPaneGeometry<Id>> {
    let mut out = Vec::new();
    for layer in layers {
        if layer.minimized {
            continue;
        }
        let [nx, ny, nw, nh] = layer.bounds;
        let x_norm = nx.clamp(0.0, 0.95);
        let y_norm = ny.clamp(0.0, 0.95);
        let w_norm = nw.clamp(0.05, 1.0 - x_norm);
        let h_norm = nh.clamp(0.05, 1.0 - y_norm);

        let card_rect = RectF::new(
            (workspace.x + workspace.w * x_norm).round(),
            (workspace.y + workspace.h * y_norm).round(),
            (workspace.w * w_norm).round(),
            (workspace.h * h_norm).round(),
        );
        let is_right_drawer =
            (x_norm + w_norm - 1.0).abs() < 0.01 && y_norm < 0.01 && h_norm > 0.99;

        let header_h = cfg.pane_header_height.min(card_rect.h);
        let header_rect = RectF::new(card_rect.x, card_rect.y, card_rect.w, header_h);
        let terminal_rect = RectF::new(
            card_rect.x,
            card_rect.y + header_h,
            card_rect.w,
            (card_rect.h - header_h).max(0.0),
        );
        let corner_radius = if is_right_drawer {
            0.0
        } else {
            cfg.hyprland_rounding.unwrap_or(10.0).clamp(0.0, 16.0)
        };

        out.push(FloatingPaneGeometry {
            id: layer.id.clone(),
            card_rect,
            header_rect,
            terminal_rect,
            corner_radius,
            is_right_drawer,
        });
    }
    out
}

/// Resolved geometry for a corner-anchored minimized floating pane capsule.
#[derive(Debug, Clone, PartialEq)]
pub struct MinimizedCapsuleGeometry<Id> {
    pub id: Id,
    pub rect: RectF,
}

/// Stacks minimized floating pane capsules (`28px` tall, `164px` wide) in the configured corner.
#[must_use]
pub fn layout_minimized_capsules<Id: Clone>(
    workspace: RectF,
    layers: &[FloatingLayerSpec<Id>],
    corner: MinimizedCorner,
) -> Vec<MinimizedCapsuleGeometry<Id>> {
    const CAPSULE_W: f32 = 164.0;
    const CAPSULE_H: f32 = 28.0;
    const INSET: f32 = 12.0;
    const GAP: f32 = 6.0;

    let minimized_ids: Vec<Id> = layers
        .iter()
        .filter(|l| l.minimized)
        .map(|l| l.id.clone())
        .collect();

    let mut out = Vec::with_capacity(minimized_ids.len());
    let w = CAPSULE_W.min((workspace.w - INSET * 2.0).max(0.0));

    for (idx, id) in minimized_ids.into_iter().enumerate() {
        let offset = idx as f32 * (CAPSULE_H + GAP);
        let x = match corner {
            MinimizedCorner::BottomRight | MinimizedCorner::TopRight => {
                (workspace.right() - INSET - w).max(workspace.x)
            }
            MinimizedCorner::BottomLeft | MinimizedCorner::TopLeft => workspace.x + INSET,
        };
        let y = match corner {
            MinimizedCorner::BottomRight | MinimizedCorner::BottomLeft => {
                (workspace.bottom() - INSET - CAPSULE_H - offset).max(workspace.y)
            }
            MinimizedCorner::TopRight | MinimizedCorner::TopLeft => workspace.y + INSET + offset,
        };
        out.push(MinimizedCapsuleGeometry {
            id,
            rect: RectF::new(x.round(), y.round(), w.round(), CAPSULE_H),
        });
    }
    out
}

/// Interactive drag/resize target on a floating pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatingHitZone {
    MoveHeader,
    ResizeN,
    ResizeS,
    ResizeE,
    ResizeW,
    ResizeNE,
    ResizeNW,
    ResizeSE,
    ResizeSW,
    Content,
}

/// Hit-tests pointer coordinates against a floating pane's 8px border resize zones, header, or content.
#[must_use]
pub fn hit_test_floating_pane<Id>(
    pane: &FloatingPaneGeometry<Id>,
    px: f32,
    py: f32,
) -> Option<FloatingHitZone> {
    if !pane.card_rect.contains(px, py) {
        return None;
    }
    const EDGE: f32 = 8.0;
    let r = pane.card_rect;
    let on_left = px - r.x < EDGE;
    let on_right = r.right() - px < EDGE;
    let on_top = py - r.y < EDGE;
    let on_bottom = r.bottom() - py < EDGE;

    let zone = match (on_left, on_right, on_top, on_bottom) {
        (true, _, true, _) => FloatingHitZone::ResizeNW,
        (_, true, true, _) => FloatingHitZone::ResizeNE,
        (true, _, _, true) => FloatingHitZone::ResizeSW,
        (_, true, _, true) => FloatingHitZone::ResizeSE,
        (true, _, _, _) => FloatingHitZone::ResizeW,
        (_, true, _, _) => FloatingHitZone::ResizeE,
        (_, _, true, _) => FloatingHitZone::ResizeN,
        (_, _, _, true) => FloatingHitZone::ResizeS,
        _ if pane.header_rect.contains(px, py) => FloatingHitZone::MoveHeader,
        _ => FloatingHitZone::Content,
    };
    Some(zone)
}

/// Target zone when dragging a pane over another pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropZone {
    Top,
    Bottom,
    Left,
    Right,
    CenterSwap,
}

/// Result of pane drag-and-drop hit-testing, including the visual highlight rect to draw.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DropZoneHit {
    pub zone: DropZone,
    pub preview_rect: RectF,
}

/// Hit-tests a dragged pane over `target_rect`.
///
/// Outer 25% bands trigger directional splits (resolved by closest edge in corners);
/// the inner 50%×50% region triggers `CenterSwap`.
#[must_use]
pub fn hit_test_drop_zone(target_rect: RectF, px: f32, py: f32) -> Option<DropZoneHit> {
    if !target_rect.contains(px, py) || target_rect.is_empty() {
        return None;
    }
    let rx = (px - target_rect.x) / target_rect.w;
    let ry = (py - target_rect.y) / target_rect.h;

    let d_left = rx;
    let d_right = 1.0 - rx;
    let d_top = ry;
    let d_bottom = 1.0 - ry;
    let min_edge = d_left.min(d_right).min(d_top).min(d_bottom);

    if min_edge >= 0.25 {
        return Some(DropZoneHit {
            zone: DropZone::CenterSwap,
            preview_rect: target_rect,
        });
    }

    let (zone, preview_rect) = if (min_edge - d_left).abs() < f32::EPSILON {
        (
            DropZone::Left,
            RectF::new(
                target_rect.x,
                target_rect.y,
                target_rect.w * 0.5,
                target_rect.h,
            ),
        )
    } else if (min_edge - d_right).abs() < f32::EPSILON {
        (
            DropZone::Right,
            RectF::new(
                target_rect.x + target_rect.w * 0.5,
                target_rect.y,
                target_rect.w * 0.5,
                target_rect.h,
            ),
        )
    } else if (min_edge - d_top).abs() < f32::EPSILON {
        (
            DropZone::Top,
            RectF::new(
                target_rect.x,
                target_rect.y,
                target_rect.w,
                target_rect.h * 0.5,
            ),
        )
    } else {
        (
            DropZone::Bottom,
            RectF::new(
                target_rect.x,
                target_rect.y + target_rect.h * 0.5,
                target_rect.w,
                target_rect.h * 0.5,
            ),
        )
    };

    Some(DropZoneHit { zone, preview_rect })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taffy_computes_all_four_toolbar_positions_and_hover_modes() {
        let mut cfg = ChromeLayoutConfig::default();

        // Left sidebar
        let left = compute_chrome_layout(1440.0, 900.0, &cfg);
        assert_eq!(left.sidebar, Some(RectF::new(0.0, 0.0, 248.0, 900.0)));
        assert_eq!(left.workspace, RectF::new(248.0, 0.0, 1192.0, 900.0));
        assert!(left.tab_bar.is_none());
        assert!(!left.toolbar_is_overlay);

        // Right sidebar
        cfg.toolbar_position = ToolbarPosition::Right;
        let right = compute_chrome_layout(1440.0, 900.0, &cfg);
        assert_eq!(right.workspace, RectF::new(0.0, 0.0, 1192.0, 900.0));
        assert_eq!(right.sidebar, Some(RectF::new(1192.0, 0.0, 248.0, 900.0)));

        // Top horizontal tabs
        cfg.toolbar_position = ToolbarPosition::Top;
        let top = compute_chrome_layout(1440.0, 900.0, &cfg);
        assert_eq!(top.tab_bar, Some(RectF::new(0.0, 0.0, 1440.0, 38.0)));
        assert_eq!(top.workspace, RectF::new(0.0, 38.0, 1440.0, 862.0));

        // Bottom horizontal tabs
        cfg.toolbar_position = ToolbarPosition::Bottom;
        let bottom = compute_chrome_layout(1440.0, 900.0, &cfg);
        assert_eq!(bottom.workspace, RectF::new(0.0, 0.0, 1440.0, 862.0));
        assert_eq!(bottom.tab_bar, Some(RectF::new(0.0, 862.0, 1440.0, 38.0)));

        // OnHover hidden -> exposes hot edge, full workspace
        cfg.toolbar_position = ToolbarPosition::Left;
        cfg.toolbar_behavior = ToolbarBehavior::OnHover;
        cfg.toolbar_revealed = false;
        let hover_hidden = compute_chrome_layout(1440.0, 900.0, &cfg);
        assert!(hover_hidden.sidebar.is_none());
        assert_eq!(hover_hidden.workspace, RectF::new(0.0, 0.0, 1440.0, 900.0));
        assert_eq!(
            hover_hidden.hot_edge,
            Some(RectF::new(0.0, 0.0, 6.0, 900.0))
        );

        // OnHover revealed -> overlay sidebar without resizing workspace
        cfg.toolbar_revealed = true;
        let hover_shown = compute_chrome_layout(1440.0, 900.0, &cfg);
        assert!(hover_shown.toolbar_is_overlay);
        assert_eq!(
            hover_shown.sidebar,
            Some(RectF::new(0.0, 0.0, 248.0, 900.0))
        );
        assert_eq!(hover_shown.workspace, RectF::new(0.0, 0.0, 1440.0, 900.0));
    }

    #[test]
    fn binary_splits_zoom_and_divider_drag_work() {
        let tree = SplitNode::split(
            SplitAxis::Horizontal,
            0.6,
            SplitNode::leaf("left"),
            SplitNode::split(
                SplitAxis::Vertical,
                0.5,
                SplitNode::leaf("top-right"),
                SplitNode::leaf("bot-right"),
            ),
        );
        let cfg = ChromeLayoutConfig {
            density: PaneDensity::Comfortable,
            pane_header_mode: PaneHeaderMode::Always,
            ..ChromeLayoutConfig::default()
        };
        let ws = RectF::new(248.0, 0.0, 1000.0, 800.0);
        let res = layout_splits(ws, &tree, &cfg, None, None);
        assert_eq!(res.panes.len(), 3);
        assert_eq!(res.dividers.len(), 2);
        assert_eq!(res.panes[0].corner_radius, 8.0);
        assert_eq!(res.panes[0].header_rect.unwrap().h, 30.0);
        assert_eq!(
            res.panes[0].terminal_rect.h,
            res.panes[0].card_rect.h - 30.0
        );

        // Divider hit test & drag ratio calculation
        let h_div = &res.dividers[0];
        let (cx, cy) = h_div.hit_rect.center();
        assert!(hit_test_divider(&res.dividers, cx, cy).is_some());
        let dragged =
            compute_drag_ratio(h_div, h_div.parent_rect.x + h_div.parent_rect.w * 0.25, cy);
        assert!((dragged - 0.25).abs() < 0.01);

        // Zoom override gives 100% workspace to zoomed leaf and hides dividers
        let zoomed = layout_splits(ws, &tree, &cfg, Some(&"bot-right"), None);
        assert_eq!(zoomed.panes.len(), 1);
        assert!(zoomed.dividers.is_empty());
        assert_eq!(zoomed.panes[0].id, "bot-right");
        assert!(zoomed.panes[0].is_zoomed);
        assert_eq!(
            zoomed.panes[0].card_rect,
            ws.inset_uniform(cfg.outer_margin())
        );
    }

    #[test]
    fn tr3x_pr_drawer_and_minimized_capsules_match_spec() {
        let ws = RectF::new(248.0, 0.0, 1200.0, 900.0);
        let cfg = ChromeLayoutConfig::default();

        // Wide >= 140 cols: right-docked 55% full-height drawer
        let wide_layers = vec![FloatingLayerSpec::tr3x_pr_drawer("pr", 160)];
        let wide_geo = layout_floating_layers(ws, &wide_layers, &cfg);
        assert_eq!(wide_geo.len(), 1);
        assert!(wide_geo[0].is_right_drawer);
        assert_eq!(wide_geo[0].corner_radius, 0.0);
        assert_eq!(wide_geo[0].card_rect, RectF::new(788.0, 0.0, 660.0, 900.0));

        // Narrow < 140 cols: near-fullscreen rounded overlay
        let narrow_layers = vec![FloatingLayerSpec::tr3x_pr_drawer("pr", 110)];
        let narrow_geo = layout_floating_layers(ws, &narrow_layers, &cfg);
        assert!(!narrow_geo[0].is_right_drawer);
        assert_eq!(narrow_geo[0].corner_radius, 10.0);

        // Minimized capsules stack cleanly in bottom-right corner
        let minimized = vec![
            FloatingLayerSpec {
                id: "f1",
                bounds: [0.2, 0.2, 0.5, 0.5],
                minimized: true,
            },
            FloatingLayerSpec {
                id: "f2",
                bounds: [0.2, 0.2, 0.5, 0.5],
                minimized: true,
            },
        ];
        assert!(layout_floating_layers(ws, &minimized, &cfg).is_empty());
        let caps = layout_minimized_capsules(ws, &minimized, MinimizedCorner::BottomRight);
        assert_eq!(caps.len(), 2);
        assert!(caps[1].rect.y < caps[0].rect.y);
        assert_eq!(caps[0].rect.right(), ws.right() - 12.0);
    }

    #[test]
    fn drop_zone_hit_testing_covers_all_five_zones() {
        let pane = RectF::new(100.0, 100.0, 400.0, 200.0);
        assert_eq!(
            hit_test_drop_zone(pane, 110.0, 200.0).unwrap().zone,
            DropZone::Left
        );
        assert_eq!(
            hit_test_drop_zone(pane, 490.0, 200.0).unwrap().zone,
            DropZone::Right
        );
        assert_eq!(
            hit_test_drop_zone(pane, 300.0, 110.0).unwrap().zone,
            DropZone::Top
        );
        assert_eq!(
            hit_test_drop_zone(pane, 300.0, 290.0).unwrap().zone,
            DropZone::Bottom
        );
        assert_eq!(
            hit_test_drop_zone(pane, 300.0, 200.0).unwrap().zone,
            DropZone::CenterSwap
        );
        assert!(hit_test_drop_zone(pane, 50.0, 50.0).is_none());
    }

    #[test]
    fn responsive_sidebar_clamps_proportionally_auto_collapses_and_restores_on_resize() {
        let mut cfg = ChromeLayoutConfig {
            sidebar_width: 340.0,
            ..ChromeLayoutConfig::default()
        };

        // 1. Wide 1920px: full configured width (340 <= 576)
        let wide = compute_chrome_layout(1920.0, 1080.0, &cfg);
        assert!(!wide.sidebar_auto_collapsed);
        assert_eq!(wide.sidebar.unwrap().w, 340.0);
        assert_eq!(wide.workspace.w, 1580.0);

        // 2. Halved 960px (side terminal opened in Hyprland): clamped to 30% (288px)
        let half = compute_chrome_layout(960.0, 1080.0, &cfg);
        assert!(!half.sidebar_auto_collapsed);
        assert_eq!(half.sidebar.unwrap().w, 288.0);
        assert_eq!(half.workspace.w, 672.0);

        // 3. Narrow 640px (< 720px threshold): auto-collapses cleanly without mutating user cfg
        let narrow = compute_chrome_layout(640.0, 1080.0, &cfg);
        assert!(narrow.sidebar_auto_collapsed);
        assert!(narrow.sidebar.is_none());
        assert_eq!(narrow.workspace, RectF::new(0.0, 0.0, 640.0, 1080.0));
        assert!(narrow.hot_edge.is_some());
        assert!(!cfg.sidebar_collapsed);

        // 4. Widening back to 1920px automatically restores configured 340px width
        let restored = compute_chrome_layout(1920.0, 1080.0, &cfg);
        assert!(!restored.sidebar_auto_collapsed);
        assert_eq!(restored.sidebar.unwrap().w, 340.0);
        assert_eq!(restored.workspace.w, 1580.0);

        // 5. Explicit manual collapse stays collapsed across narrow -> wide transitions
        cfg.sidebar_collapsed = true;
        let manual_narrow = compute_chrome_layout(640.0, 1080.0, &cfg);
        assert!(!manual_narrow.sidebar_auto_collapsed);
        assert!(manual_narrow.sidebar.is_none());
        let manual_wide = compute_chrome_layout(1920.0, 1080.0, &cfg);
        assert!(!manual_wide.sidebar_auto_collapsed);
        assert!(manual_wide.sidebar.is_none());
    }
}
