//! Deterministic, zero-allocation spring physics and slide/fade animation primitives
//! for active pane focus borders, sidebar reveals, modal transitions, and the docked viewer.

use std::time::Instant;

use crate::layout::{RectF, ViewerSplitGeometry};
use crate::modal::ModalState;
use crate::viewer::ViewerState;

/// Critically/near-critically damped 1D spring integrator with automatic settle snapping
/// so idle frames consume 0% CPU.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spring1D {
    pub value: f32,
    pub target: f32,
    pub velocity: f32,
    pub stiffness: f32,
    pub damping: f32,
    pub value_eps: f32,
    pub vel_eps: f32,
}

impl Spring1D {
    #[must_use]
    pub const fn new(
        value: f32,
        target: f32,
        stiffness: f32,
        damping: f32,
        value_eps: f32,
        vel_eps: f32,
    ) -> Self {
        Self {
            value,
            target,
            velocity: 0.0,
            stiffness,
            damping,
            value_eps,
            vel_eps,
        }
    }

    #[must_use]
    pub const fn settled(
        value: f32,
        stiffness: f32,
        damping: f32,
        value_eps: f32,
        vel_eps: f32,
    ) -> Self {
        Self::new(value, value, stiffness, damping, value_eps, vel_eps)
    }

    /// Critically damped normalized `0.0..=1.0` transition spring for sidebar/modal/viewer slides.
    #[must_use]
    pub const fn transition_settled(value: f32) -> Self {
        Self::settled(value, 360.0, 38.0, 0.004, 0.04)
    }

    #[must_use]
    pub const fn transition_from(value: f32, target: f32) -> Self {
        Self::new(value, target, 360.0, 38.0, 0.004, 0.04)
    }

    pub fn set_target(&mut self, target: f32) {
        self.target = target;
    }

    pub fn snap(&mut self, value: f32) {
        self.value = value;
        self.target = value;
        self.velocity = 0.0;
    }

    #[must_use]
    pub fn is_settled(&self) -> bool {
        (self.value - self.target).abs() <= self.value_eps && self.velocity.abs() <= self.vel_eps
    }

    /// Advances the spring by `dt_secs` using bounded sub-steps (`<= 1/240 s`).
    /// Returns `true` if still animating, `false` once settled.
    pub fn step(&mut self, dt_secs: f32) -> bool {
        if self.is_settled() {
            self.value = self.target;
            self.velocity = 0.0;
            return false;
        }
        let dt = dt_secs.clamp(0.0, 0.05);
        if dt <= 0.0 {
            return true;
        }
        let sub_steps = (dt * 240.0).ceil().clamp(1.0, 12.0) as u32;
        let sub_dt = dt / (sub_steps as f32);
        for _ in 0..sub_steps {
            let force = -self.stiffness * (self.value - self.target);
            let accel = force - self.damping * self.velocity;
            self.velocity += accel * sub_dt;
            self.value += self.velocity * sub_dt;
        }
        if self.is_settled() {
            self.value = self.target;
            self.velocity = 0.0;
            false
        } else {
            true
        }
    }
}

/// 4-axis spring rectangle for smooth active-pane border focus transitions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpringRect {
    pub x: Spring1D,
    pub y: Spring1D,
    pub w: Spring1D,
    pub h: Spring1D,
}

impl SpringRect {
    #[must_use]
    pub const fn from_rect(r: RectF) -> Self {
        const STIFFNESS: f32 = 420.0;
        const DAMPING: f32 = 36.0;
        const VALUE_EPS: f32 = 0.35;
        const VEL_EPS: f32 = 4.0;
        Self {
            x: Spring1D::settled(r.x, STIFFNESS, DAMPING, VALUE_EPS, VEL_EPS),
            y: Spring1D::settled(r.y, STIFFNESS, DAMPING, VALUE_EPS, VEL_EPS),
            w: Spring1D::settled(r.w, STIFFNESS, DAMPING, VALUE_EPS, VEL_EPS),
            h: Spring1D::settled(r.h, STIFFNESS, DAMPING, VALUE_EPS, VEL_EPS),
        }
    }

    pub fn set_target(&mut self, r: RectF) {
        self.x.set_target(r.x);
        self.y.set_target(r.y);
        self.w.set_target(r.w);
        self.h.set_target(r.h);
    }

    pub fn snap(&mut self, r: RectF) {
        self.x.snap(r.x);
        self.y.snap(r.y);
        self.w.snap(r.w);
        self.h.snap(r.h);
    }

    #[must_use]
    pub const fn current(&self) -> RectF {
        RectF::new(self.x.value, self.y.value, self.w.value, self.h.value)
    }

    #[must_use]
    pub const fn target(&self) -> RectF {
        RectF::new(self.x.target, self.y.target, self.w.target, self.h.target)
    }

    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.x.is_settled() && self.y.is_settled() && self.w.is_settled() && self.h.is_settled()
    }

    pub fn step(&mut self, dt_secs: f32) -> bool {
        let sx = self.x.step(dt_secs);
        let sy = self.y.step(dt_secs);
        let sw = self.w.step(dt_secs);
        let sh = self.h.step(dt_secs);
        sx || sy || sw || sh
    }
}

/// Complete UI chrome animation state owned by [`crate::UiTree`].
pub(crate) struct UiAnimationState {
    pub active_border_rect: Option<SpringRect>,
    pub active_border_block: Option<String>,
    pub active_border_viewport: Option<(f32, f32)>,
    pub active_border_zoomed: Option<String>,
    pub sidebar_progress: Spring1D,
    pub sidebar_initialized: bool,
    pub last_sidebar_auto_collapsed: bool,
    pub last_sidebar_rect: Option<RectF>,
    pub last_sidebar_is_right: bool,
    pub last_sidebar_is_overlay: bool,
    pub viewer_progress: Spring1D,
    pub viewer_initialized: bool,
    pub last_viewer_split: Option<ViewerSplitGeometry>,
    pub closing_viewer: Option<(ViewerSplitGeometry, ViewerState, Spring1D)>,
    pub modal_open_progress: Spring1D,
    pub closing_modal: Option<(ModalState, Spring1D)>,
    pub last_tick: Option<Instant>,
}

impl Default for UiAnimationState {
    fn default() -> Self {
        Self {
            active_border_rect: None,
            active_border_block: None,
            active_border_viewport: None,
            active_border_zoomed: None,
            sidebar_progress: Spring1D::transition_settled(1.0),
            sidebar_initialized: false,
            last_sidebar_auto_collapsed: false,
            last_sidebar_rect: None,
            last_sidebar_is_right: false,
            last_sidebar_is_overlay: false,
            viewer_progress: Spring1D::transition_settled(0.0),
            viewer_initialized: false,
            last_viewer_split: None,
            closing_viewer: None,
            modal_open_progress: Spring1D::transition_settled(1.0),
            closing_modal: None,
            last_tick: None,
        }
    }
}

impl UiAnimationState {
    #[must_use]
    pub fn is_layout_animating(&self) -> bool {
        (!self.sidebar_progress.is_settled() && !self.last_sidebar_is_overlay)
            || !self.viewer_progress.is_settled()
            || self.closing_viewer.is_some()
    }

    #[must_use]
    pub fn has_active_animations(&self) -> bool {
        self.active_border_rect
            .as_ref()
            .is_some_and(|r| !r.is_settled())
            || !self.sidebar_progress.is_settled()
            || !self.viewer_progress.is_settled()
            || self.closing_viewer.is_some()
            || !self.modal_open_progress.is_settled()
            || self.closing_modal.is_some()
    }

    pub fn snap_all(&mut self) {
        if let Some(rect) = &mut self.active_border_rect {
            let t = rect.target();
            rect.snap(t);
        }
        let sp = self.sidebar_progress.target;
        self.sidebar_progress.snap(sp);
        let vp = self.viewer_progress.target;
        self.viewer_progress.snap(vp);
        let mp = self.modal_open_progress.target;
        self.modal_open_progress.snap(mp);
        self.closing_viewer = None;
        self.closing_modal = None;
        self.last_tick = None;
    }

    pub fn tick(&mut self, now: Instant) -> bool {
        if !self.has_active_animations() {
            self.last_tick = None;
            return false;
        }
        let dt = match self.last_tick {
            Some(prev) => now
                .saturating_duration_since(prev)
                .as_secs_f32()
                .clamp(0.0, 0.05),
            None => 1.0 / 120.0,
        };
        self.last_tick = Some(now);

        let mut active = false;
        if let Some(rect) = &mut self.active_border_rect {
            active |= rect.step(dt);
        }
        active |= self.sidebar_progress.step(dt);
        active |= self.viewer_progress.step(dt);
        active |= self.modal_open_progress.step(dt);

        if let Some((_, _, sp)) = &mut self.closing_viewer {
            if sp.step(dt) && sp.value > 0.005 {
                active = true;
            } else {
                self.closing_viewer = None;
            }
        }
        if let Some((_, sp)) = &mut self.closing_modal {
            if sp.step(dt) && sp.value > 0.005 {
                active = true;
            } else {
                self.closing_modal = None;
            }
        }

        if !active {
            self.last_tick = None;
        }
        true
    }
}
