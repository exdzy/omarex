//! Safe wrappers for `GhosttySelectionGesture`, plain-text selection extraction,
//! OSC 8 hyperlink lookup, and incremental terminal scrollback search (`search.h`).

use std::ops::Range;
use std::ptr;
use std::sync::Weak;

use parking_lot::Mutex;

use crate::images::KittyPlacement;
use crate::{Terminal, TerminalInner, VtError, ffi};

// ---------------------------------------------------------------------------
// Selection gestures & text/hyperlink extraction
// ---------------------------------------------------------------------------

/// Selection granularity chosen for single/double/triple click.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(i32)]
pub enum SelectionBehavior {
    Cell = ffi::GHOSTTY_SELECTION_GESTURE_BEHAVIOR_CELL,
    Word = ffi::GHOSTTY_SELECTION_GESTURE_BEHAVIOR_WORD,
    Line = ffi::GHOSTTY_SELECTION_GESTURE_BEHAVIOR_LINE,
    Output = ffi::GHOSTTY_SELECTION_GESTURE_BEHAVIOR_OUTPUT,
}

/// Autoscroll direction requested while dragging past top/bottom pane edges.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SelectionAutoscroll {
    None,
    Up,
    Down,
}

/// Surface geometry passed during selection drag/autoscroll events.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct SelectionGeometry {
    pub columns: u32,
    pub cell_width: u32,
    pub padding_left: u32,
    pub screen_height: u32,
}

impl SelectionGeometry {
    fn to_ffi(self) -> ffi::GhosttySelectionGestureGeometry {
        ffi::GhosttySelectionGestureGeometry {
            columns: self.columns.max(1),
            cell_width: self.cell_width.max(1),
            padding_left: self.padding_left,
            screen_height: self.screen_height.max(1),
        }
    }
}

/// Safe RAII wrapper around `GhosttySelectionGesture` supporting single/double/triple-click
/// (char/word/line/output), drag, rectangular Alt+drag, and edge autoscroll.
pub struct SelectionGesture {
    gesture: ffi::GhosttySelectionGesture,
    ev_press: ffi::GhosttySelectionGestureEvent,
    ev_drag: ffi::GhosttySelectionGestureEvent,
    ev_release: ffi::GhosttySelectionGestureEvent,
    ev_autoscroll: ffi::GhosttySelectionGestureEvent,
    terminal: Weak<Mutex<TerminalInner>>,
}

// SAFETY: `SelectionGesture` exclusively owns its opaque C gesture/event handles and
// serializes all terminal accesses via the `Weak<Mutex<TerminalInner>>`.
unsafe impl Send for SelectionGesture {}

impl SelectionGesture {
    pub fn new(terminal: &Terminal) -> Result<Self, VtError> {
        let mut gesture: ffi::GhosttySelectionGesture = ptr::null_mut();
        // SAFETY: NULL allocator uses libghostty default allocator.
        let rc = unsafe { ffi::ghostty_selection_gesture_new(ptr::null(), &raw mut gesture) };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }

        let mk_event = |ty| -> Result<ffi::GhosttySelectionGestureEvent, VtError> {
            let mut ev: ffi::GhosttySelectionGestureEvent = ptr::null_mut();
            // SAFETY: NULL allocator uses default allocator; `ty` is a valid enum constant.
            let rc =
                unsafe { ffi::ghostty_selection_gesture_event_new(ptr::null(), &raw mut ev, ty) };
            if rc != ffi::GHOSTTY_SUCCESS {
                Err(VtError::Ffi(rc))
            } else {
                Ok(ev)
            }
        };

        let ev_press = match mk_event(ffi::GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_PRESS) {
            Ok(e) => e,
            Err(err) => {
                // SAFETY: `gesture` was just created and not yet bound to any terminal state.
                unsafe { ffi::ghostty_selection_gesture_free(gesture, ptr::null_mut()) };
                return Err(err);
            }
        };
        let ev_drag = match mk_event(ffi::GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_DRAG) {
            Ok(e) => e,
            Err(err) => {
                unsafe {
                    ffi::ghostty_selection_gesture_event_free(ev_press);
                    ffi::ghostty_selection_gesture_free(gesture, ptr::null_mut());
                }
                return Err(err);
            }
        };
        let ev_release = match mk_event(ffi::GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_RELEASE) {
            Ok(e) => e,
            Err(err) => {
                unsafe {
                    ffi::ghostty_selection_gesture_event_free(ev_drag);
                    ffi::ghostty_selection_gesture_event_free(ev_press);
                    ffi::ghostty_selection_gesture_free(gesture, ptr::null_mut());
                }
                return Err(err);
            }
        };
        let ev_autoscroll =
            match mk_event(ffi::GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_AUTOSCROLL_TICK) {
                Ok(e) => e,
                Err(err) => {
                    unsafe {
                        ffi::ghostty_selection_gesture_event_free(ev_release);
                        ffi::ghostty_selection_gesture_event_free(ev_drag);
                        ffi::ghostty_selection_gesture_event_free(ev_press);
                        ffi::ghostty_selection_gesture_free(gesture, ptr::null_mut());
                    }
                    return Err(err);
                }
            };

        Ok(Self {
            gesture,
            ev_press,
            ev_drag,
            ev_release,
            ev_autoscroll,
            terminal: Weak::clone(&std::sync::Arc::downgrade(&terminal.inner)),
        })
    }

    /// Handles a mouse press at viewport cell `(col, row)` and surface pixel position `(px_x, px_y)`.
    ///
    /// Returns `Ok(true)` if a selection was installed on the terminal (e.g. double/triple click),
    /// or `Ok(false)` if single-click merely anchored a pending drag (clearing any prior selection).
    pub fn press(
        &mut self,
        col: u16,
        row: u32,
        px_x: f64,
        px_y: f64,
        time_ns: u64,
    ) -> Result<bool, VtError> {
        let Some(arc) = self.terminal.upgrade() else {
            return Err(VtError::TerminalDropped);
        };
        let guard = arc.lock();

        let mut grid_ref = ffi::GhosttyGridRef::default();
        // SAFETY: `guard.raw` is locked; `grid_ref` has initialized `size`.
        let rc = unsafe {
            ffi::ghostty_terminal_grid_ref(
                guard.raw,
                ffi::GhosttyPoint::viewport(col, row),
                &raw mut grid_ref,
            )
        };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }

        let pos = ffi::GhosttySurfacePosition { x: px_x, y: px_y };
        let behaviors = ffi::GhosttySelectionGestureBehaviors {
            single_click: ffi::GHOSTTY_SELECTION_GESTURE_BEHAVIOR_CELL,
            double_click: ffi::GHOSTTY_SELECTION_GESTURE_BEHAVIOR_WORD,
            triple_click: ffi::GHOSTTY_SELECTION_GESTURE_BEHAVIOR_LINE,
        };
        let repeat_interval_ns: u64 = 500_000_000;
        let repeat_distance: f64 = 6.0;

        // SAFETY: All pointers match documented option types in `selection.h`.
        unsafe {
            ffi::ghostty_selection_gesture_event_set(
                self.ev_press,
                ffi::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF,
                (&raw const grid_ref).cast(),
            );
            ffi::ghostty_selection_gesture_event_set(
                self.ev_press,
                ffi::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION,
                (&raw const pos).cast(),
            );
            ffi::ghostty_selection_gesture_event_set(
                self.ev_press,
                ffi::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_TIME_NS,
                (&raw const time_ns).cast(),
            );
            ffi::ghostty_selection_gesture_event_set(
                self.ev_press,
                ffi::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_INTERVAL_NS,
                (&raw const repeat_interval_ns).cast(),
            );
            ffi::ghostty_selection_gesture_event_set(
                self.ev_press,
                ffi::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_DISTANCE,
                (&raw const repeat_distance).cast(),
            );
            ffi::ghostty_selection_gesture_event_set(
                self.ev_press,
                ffi::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_BEHAVIORS,
                (&raw const behaviors).cast(),
            );
        }

        let mut selection = ffi::GhosttySelection::default();
        // SAFETY: `self.gesture`, `guard.raw`, and `self.ev_press` are valid while locked.
        let rc = unsafe {
            ffi::ghostty_selection_gesture_event(
                self.gesture,
                guard.raw,
                self.ev_press,
                &raw mut selection,
            )
        };
        match rc {
            ffi::GHOSTTY_SUCCESS => {
                // SAFETY: `selection` is a fresh valid snapshot for `guard.raw`.
                let rc2 = unsafe {
                    ffi::ghostty_terminal_set(
                        guard.raw,
                        ffi::GHOSTTY_TERMINAL_OPT_SELECTION,
                        (&raw const selection).cast(),
                    )
                };
                if rc2 != ffi::GHOSTTY_SUCCESS {
                    return Err(VtError::Ffi(rc2));
                }
                Ok(true)
            }
            ffi::GHOSTTY_NO_VALUE => {
                // SAFETY: Passing NULL clears the terminal's active selection.
                let _ = unsafe {
                    ffi::ghostty_terminal_set(
                        guard.raw,
                        ffi::GHOSTTY_TERMINAL_OPT_SELECTION,
                        ptr::null(),
                    )
                };
                Ok(false)
            }
            other => Err(VtError::Ffi(other)),
        }
    }

    /// Handles a pointer drag to viewport cell `(col, row)`. Set `rectangle = true` for Alt+drag.
    pub fn drag(
        &mut self,
        col: u16,
        row: u32,
        px_x: f64,
        px_y: f64,
        geometry: SelectionGeometry,
        rectangle: bool,
    ) -> Result<bool, VtError> {
        let Some(arc) = self.terminal.upgrade() else {
            return Err(VtError::TerminalDropped);
        };
        let guard = arc.lock();

        let mut grid_ref = ffi::GhosttyGridRef::default();
        // SAFETY: `guard.raw` is locked.
        let rc = unsafe {
            ffi::ghostty_terminal_grid_ref(
                guard.raw,
                ffi::GhosttyPoint::viewport(col, row),
                &raw mut grid_ref,
            )
        };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }

        let pos = ffi::GhosttySurfacePosition { x: px_x, y: px_y };
        let geom = geometry.to_ffi();

        // SAFETY: Option pointers match `selection.h` types.
        unsafe {
            ffi::ghostty_selection_gesture_event_set(
                self.ev_drag,
                ffi::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF,
                (&raw const grid_ref).cast(),
            );
            ffi::ghostty_selection_gesture_event_set(
                self.ev_drag,
                ffi::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION,
                (&raw const pos).cast(),
            );
            ffi::ghostty_selection_gesture_event_set(
                self.ev_drag,
                ffi::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_GEOMETRY,
                (&raw const geom).cast(),
            );
            ffi::ghostty_selection_gesture_event_set(
                self.ev_drag,
                ffi::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_RECTANGLE,
                (&raw const rectangle).cast(),
            );
        }

        let mut selection = ffi::GhosttySelection::default();
        // SAFETY: All handles are valid and `guard` serializes terminal access.
        let rc = unsafe {
            ffi::ghostty_selection_gesture_event(
                self.gesture,
                guard.raw,
                self.ev_drag,
                &raw mut selection,
            )
        };
        match rc {
            ffi::GHOSTTY_SUCCESS => {
                // SAFETY: Installing fresh snapshot selection on the same locked terminal.
                let rc2 = unsafe {
                    ffi::ghostty_terminal_set(
                        guard.raw,
                        ffi::GHOSTTY_TERMINAL_OPT_SELECTION,
                        (&raw const selection).cast(),
                    )
                };
                if rc2 != ffi::GHOSTTY_SUCCESS {
                    return Err(VtError::Ffi(rc2));
                }
                Ok(true)
            }
            ffi::GHOSTTY_NO_VALUE => Ok(false),
            other => Err(VtError::Ffi(other)),
        }
    }

    /// Handles pointer release, finalizing gesture click tracking.
    pub fn release(&mut self, cell: Option<(u16, u32)>) -> Result<(), VtError> {
        let Some(arc) = self.terminal.upgrade() else {
            return Err(VtError::TerminalDropped);
        };
        let guard = arc.lock();

        let mut grid_ref = ffi::GhosttyGridRef::default();
        // SAFETY: Sets or clears `OPT_REF` on `self.ev_release`.
        unsafe {
            if let Some((col, row)) = cell {
                let rc = ffi::ghostty_terminal_grid_ref(
                    guard.raw,
                    ffi::GhosttyPoint::viewport(col, row),
                    &raw mut grid_ref,
                );
                if rc == ffi::GHOSTTY_SUCCESS {
                    ffi::ghostty_selection_gesture_event_set(
                        self.ev_release,
                        ffi::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF,
                        (&raw const grid_ref).cast(),
                    );
                } else {
                    ffi::ghostty_selection_gesture_event_set(
                        self.ev_release,
                        ffi::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF,
                        ptr::null(),
                    );
                }
            } else {
                ffi::ghostty_selection_gesture_event_set(
                    self.ev_release,
                    ffi::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF,
                    ptr::null(),
                );
            }
            let rc = ffi::ghostty_selection_gesture_event(
                self.gesture,
                guard.raw,
                self.ev_release,
                ptr::null_mut(),
            );
            if rc != ffi::GHOSTTY_SUCCESS && rc != ffi::GHOSTTY_NO_VALUE {
                return Err(VtError::Ffi(rc));
            }
        }
        Ok(())
    }

    /// Advances selection during an autoscroll timer tick when dragging past pane bounds.
    pub fn autoscroll_tick(
        &mut self,
        col: u16,
        row: u32,
        px_x: f64,
        px_y: f64,
        geometry: SelectionGeometry,
        rectangle: bool,
    ) -> Result<bool, VtError> {
        let Some(arc) = self.terminal.upgrade() else {
            return Err(VtError::TerminalDropped);
        };
        let guard = arc.lock();

        let vp = ffi::GhosttyPointCoordinate { x: col, y: row };
        let pos = ffi::GhosttySurfacePosition { x: px_x, y: px_y };
        let geom = geometry.to_ffi();

        // SAFETY: All option pointers match documented types in `selection.h`.
        unsafe {
            ffi::ghostty_selection_gesture_event_set(
                self.ev_autoscroll,
                ffi::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_VIEWPORT,
                (&raw const vp).cast(),
            );
            ffi::ghostty_selection_gesture_event_set(
                self.ev_autoscroll,
                ffi::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION,
                (&raw const pos).cast(),
            );
            ffi::ghostty_selection_gesture_event_set(
                self.ev_autoscroll,
                ffi::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_GEOMETRY,
                (&raw const geom).cast(),
            );
            ffi::ghostty_selection_gesture_event_set(
                self.ev_autoscroll,
                ffi::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_RECTANGLE,
                (&raw const rectangle).cast(),
            );
        }

        let mut selection = ffi::GhosttySelection::default();
        // SAFETY: Serialized by `guard`.
        let rc = unsafe {
            ffi::ghostty_selection_gesture_event(
                self.gesture,
                guard.raw,
                self.ev_autoscroll,
                &raw mut selection,
            )
        };
        match rc {
            ffi::GHOSTTY_SUCCESS => {
                // SAFETY: Installing fresh snapshot selection on `guard.raw`.
                let rc2 = unsafe {
                    ffi::ghostty_terminal_set(
                        guard.raw,
                        ffi::GHOSTTY_TERMINAL_OPT_SELECTION,
                        (&raw const selection).cast(),
                    )
                };
                if rc2 != ffi::GHOSTTY_SUCCESS {
                    return Err(VtError::Ffi(rc2));
                }
                Ok(true)
            }
            ffi::GHOSTTY_NO_VALUE => Ok(false),
            other => Err(VtError::Ffi(other)),
        }
    }

    /// Queries the current autoscroll direction requested by the gesture.
    pub fn autoscroll_direction(&self) -> Result<SelectionAutoscroll, VtError> {
        let Some(arc) = self.terminal.upgrade() else {
            return Ok(SelectionAutoscroll::None);
        };
        let guard = arc.lock();
        let mut raw: ffi::GhosttySelectionGestureAutoscroll =
            ffi::GHOSTTY_SELECTION_GESTURE_AUTOSCROLL_NONE;
        // SAFETY: `raw` matches `GHOSTTY_SELECTION_GESTURE_DATA_AUTOSCROLL`.
        let rc = unsafe {
            ffi::ghostty_selection_gesture_get(
                self.gesture,
                guard.raw,
                ffi::GHOSTTY_SELECTION_GESTURE_DATA_AUTOSCROLL,
                (&raw mut raw).cast(),
            )
        };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        Ok(match raw {
            ffi::GHOSTTY_SELECTION_GESTURE_AUTOSCROLL_UP => SelectionAutoscroll::Up,
            ffi::GHOSTTY_SELECTION_GESTURE_AUTOSCROLL_DOWN => SelectionAutoscroll::Down,
            _ => SelectionAutoscroll::None,
        })
    }

    /// Resets any active click sequence and releases tracked gesture anchors.
    pub fn reset(&mut self) {
        if let Some(arc) = self.terminal.upgrade() {
            let guard = arc.lock();
            // SAFETY: `guard.raw` is alive and locked.
            unsafe { ffi::ghostty_selection_gesture_reset(self.gesture, guard.raw) };
        }
    }
}

impl Drop for SelectionGesture {
    fn drop(&mut self) {
        // SAFETY: Per `selection.h:667-673`, pass the live terminal if still alive so tracked
        // refs are released cleanly, or NULL if the terminal has already been freed.
        if let Some(arc) = self.terminal.upgrade() {
            let guard = arc.lock();
            unsafe { ffi::ghostty_selection_gesture_free(self.gesture, guard.raw) };
        } else {
            unsafe { ffi::ghostty_selection_gesture_free(self.gesture, ptr::null_mut()) };
        }
        unsafe {
            ffi::ghostty_selection_gesture_event_free(self.ev_autoscroll);
            ffi::ghostty_selection_gesture_event_free(self.ev_release);
            ffi::ghostty_selection_gesture_event_free(self.ev_drag);
            ffi::ghostty_selection_gesture_event_free(self.ev_press);
        }
    }
}

/// Extracts the terminal's active selection formatted as plain UTF-8 text.
///
/// Returns `Ok(None)` when no selection is active.
pub fn selection_text(
    terminal: &Terminal,
    unwrap: bool,
    trim: bool,
) -> Result<Option<String>, VtError> {
    let guard = terminal.inner.lock();
    let opts = ffi::GhosttyTerminalSelectionFormatOptions {
        size: std::mem::size_of::<ffi::GhosttyTerminalSelectionFormatOptions>(),
        emit: ffi::GHOSTTY_FORMATTER_FORMAT_PLAIN,
        unwrap,
        trim,
        selection: ptr::null(),
    };

    let mut required: usize = 0;
    // SAFETY: Passing NULL `buf` queries exact byte length or returns `GHOSTTY_NO_VALUE`.
    let rc = unsafe {
        ffi::ghostty_terminal_selection_format_buf(
            guard.raw,
            opts,
            ptr::null_mut(),
            0,
            &raw mut required,
        )
    };
    if rc == ffi::GHOSTTY_NO_VALUE {
        return Ok(None);
    }
    if rc != ffi::GHOSTTY_SUCCESS && rc != ffi::GHOSTTY_OUT_OF_SPACE {
        return Err(VtError::Ffi(rc));
    }
    if required == 0 {
        return Ok(Some(String::new()));
    }

    let mut buf = vec![0u8; required];
    let mut written: usize = 0;
    // SAFETY: `buf` has exact capacity `required` while `guard` holds the terminal lock.
    let rc2 = unsafe {
        ffi::ghostty_terminal_selection_format_buf(
            guard.raw,
            opts,
            buf.as_mut_ptr(),
            buf.len(),
            &raw mut written,
        )
    };
    if rc2 != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(rc2));
    }
    buf.truncate(written);
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

/// Selects the word under viewport cell `(col, row)` and installs it as active selection.
pub fn select_word(terminal: &Terminal, col: u16, row: u32) -> Result<bool, VtError> {
    let guard = terminal.inner.lock();
    let mut grid_ref = ffi::GhosttyGridRef::default();
    // SAFETY: `guard.raw` is locked.
    let rc = unsafe {
        ffi::ghostty_terminal_grid_ref(
            guard.raw,
            ffi::GhosttyPoint::viewport(col, row),
            &raw mut grid_ref,
        )
    };
    if rc != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(rc));
    }
    let opts = ffi::GhosttyTerminalSelectWordOptions {
        size: std::mem::size_of::<ffi::GhosttyTerminalSelectWordOptions>(),
        ref_: grid_ref,
        boundary_codepoints: ptr::null(),
        boundary_codepoints_len: 0,
    };
    let mut sel = ffi::GhosttySelection::default();
    // SAFETY: `opts` and `sel` are properly sized.
    let rc = unsafe { ffi::ghostty_terminal_select_word(guard.raw, &raw const opts, &raw mut sel) };
    match rc {
        ffi::GHOSTTY_SUCCESS => {
            // SAFETY: Installs snapshot selection on the same locked terminal.
            let rc2 = unsafe {
                ffi::ghostty_terminal_set(
                    guard.raw,
                    ffi::GHOSTTY_TERMINAL_OPT_SELECTION,
                    (&raw const sel).cast(),
                )
            };
            if rc2 != ffi::GHOSTTY_SUCCESS {
                return Err(VtError::Ffi(rc2));
            }
            Ok(true)
        }
        ffi::GHOSTTY_NO_VALUE => Ok(false),
        other => Err(VtError::Ffi(other)),
    }
}

/// Selects the line at viewport cell `(col, row)` and installs it as active selection.
pub fn select_line(terminal: &Terminal, col: u16, row: u32) -> Result<bool, VtError> {
    let guard = terminal.inner.lock();
    let mut grid_ref = ffi::GhosttyGridRef::default();
    // SAFETY: `guard.raw` is locked.
    let rc = unsafe {
        ffi::ghostty_terminal_grid_ref(
            guard.raw,
            ffi::GhosttyPoint::viewport(col, row),
            &raw mut grid_ref,
        )
    };
    if rc != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(rc));
    }
    let opts = ffi::GhosttyTerminalSelectLineOptions {
        size: std::mem::size_of::<ffi::GhosttyTerminalSelectLineOptions>(),
        ref_: grid_ref,
        whitespace: ptr::null(),
        whitespace_len: 0,
        semantic_prompt_boundary: false,
    };
    let mut sel = ffi::GhosttySelection::default();
    // SAFETY: `opts` and `sel` are properly sized.
    let rc = unsafe { ffi::ghostty_terminal_select_line(guard.raw, &raw const opts, &raw mut sel) };
    match rc {
        ffi::GHOSTTY_SUCCESS => {
            let rc2 = unsafe {
                ffi::ghostty_terminal_set(
                    guard.raw,
                    ffi::GHOSTTY_TERMINAL_OPT_SELECTION,
                    (&raw const sel).cast(),
                )
            };
            if rc2 != ffi::GHOSTTY_SUCCESS {
                return Err(VtError::Ffi(rc2));
            }
            Ok(true)
        }
        ffi::GHOSTTY_NO_VALUE => Ok(false),
        other => Err(VtError::Ffi(other)),
    }
}

/// Selects all selectable content in the active screen and scrollback.
pub fn select_all(terminal: &Terminal) -> Result<bool, VtError> {
    let guard = terminal.inner.lock();
    let mut sel = ffi::GhosttySelection::default();
    // SAFETY: `guard.raw` is locked; `sel` has initialized `size`.
    let rc = unsafe { ffi::ghostty_terminal_select_all(guard.raw, &raw mut sel) };
    match rc {
        ffi::GHOSTTY_SUCCESS => {
            let rc2 = unsafe {
                ffi::ghostty_terminal_set(
                    guard.raw,
                    ffi::GHOSTTY_TERMINAL_OPT_SELECTION,
                    (&raw const sel).cast(),
                )
            };
            if rc2 != ffi::GHOSTTY_SUCCESS {
                return Err(VtError::Ffi(rc2));
            }
            Ok(true)
        }
        ffi::GHOSTTY_NO_VALUE => Ok(false),
        other => Err(VtError::Ffi(other)),
    }
}

/// Clears any active terminal selection.
pub fn clear_selection(terminal: &Terminal) -> Result<(), VtError> {
    let guard = terminal.inner.lock();
    // SAFETY: Passing NULL for `GHOSTTY_TERMINAL_OPT_SELECTION` clears the selection.
    let rc = unsafe {
        ffi::ghostty_terminal_set(guard.raw, ffi::GHOSTTY_TERMINAL_OPT_SELECTION, ptr::null())
    };
    if rc != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(rc));
    }
    Ok(())
}

/// 0-based screen-absolute cell coordinate `(col, row)` covering scrollback + viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenPoint {
    pub col: u16,
    pub row: u32,
}

/// Selection geometry mode for Copy Mode range selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    Char,
    Line,
    Block,
}

/// Vim-style word motion operations on screen coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordMotion {
    NextStart,
    PrevStart,
    NextEnd,
}

#[inline]
fn screen_point(col: u16, row: u32) -> ffi::GhosttyPoint {
    ffi::GhosttyPoint {
        tag: ffi::GHOSTTY_POINT_TAG_SCREEN,
        value: ffi::GhosttyPointValue {
            coordinate: ffi::GhosttyPointCoordinate { x: col, y: row },
        },
    }
}

fn terminal_cols_and_total_rows(raw: ffi::GhosttyTerminal) -> Result<(u16, u32), VtError> {
    let mut cols: u16 = 0;
    let rc_c = unsafe {
        ffi::ghostty_terminal_get(raw, ffi::GHOSTTY_TERMINAL_DATA_COLS, (&raw mut cols).cast())
    };
    if rc_c != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(rc_c));
    }
    let mut sb = ffi::GhosttyTerminalScrollbar::default();
    let rc_s = unsafe {
        ffi::ghostty_terminal_get(
            raw,
            ffi::GHOSTTY_TERMINAL_DATA_SCROLLBAR,
            (&raw mut sb).cast(),
        )
    };
    if rc_s != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(rc_s));
    }
    let total = u32::try_from(sb.total).unwrap_or(u32::MAX);
    Ok((cols, total))
}

/// Returns `(cols, total_screen_rows)` covering scrollback history + active screen.
pub fn screen_row_bounds(terminal: &Terminal) -> Result<(u16, u32), VtError> {
    let guard = terminal.inner.lock();
    terminal_cols_and_total_rows(guard.raw)
}

/// Installs a terminal selection between two screen-absolute coordinates using `Char`, `Line`,
/// or `Block` (rectangular) mode.
pub fn select_range(
    terminal: &Terminal,
    anchor: ScreenPoint,
    cursor: ScreenPoint,
    mode: SelectionMode,
) -> Result<(), VtError> {
    let guard = terminal.inner.lock();
    let (cols, total_rows) = terminal_cols_and_total_rows(guard.raw)?;
    if cols == 0 || total_rows == 0 {
        return Ok(());
    }
    let max_col = cols.saturating_sub(1);
    let max_row = total_rows.saturating_sub(1);
    let a_row = anchor.row.min(max_row);
    let c_row = cursor.row.min(max_row);
    let a_col = anchor.col.min(max_col);
    let c_col = cursor.col.min(max_col);

    let (start_pt, end_pt, rectangle) = match mode {
        SelectionMode::Char => (
            screen_point(a_col, a_row),
            screen_point(c_col, c_row),
            false,
        ),
        SelectionMode::Line => {
            let top = a_row.min(c_row);
            let bot = a_row.max(c_row);
            (screen_point(0, top), screen_point(max_col, bot), false)
        }
        SelectionMode::Block => (screen_point(a_col, a_row), screen_point(c_col, c_row), true),
    };

    let mut ref0 = ffi::GhosttyGridRef::default();
    let mut ref1 = ffi::GhosttyGridRef::default();
    // SAFETY: `guard.raw` is locked and both screen points are clamped within bounds.
    let r0 = unsafe { ffi::ghostty_terminal_grid_ref(guard.raw, start_pt, &raw mut ref0) };
    if r0 != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(r0));
    }
    let r1 = unsafe { ffi::ghostty_terminal_grid_ref(guard.raw, end_pt, &raw mut ref1) };
    if r1 != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(r1));
    }

    let sel = ffi::GhosttySelection {
        size: std::mem::size_of::<ffi::GhosttySelection>(),
        start: ref0,
        end: ref1,
        rectangle,
    };
    // SAFETY: Installs snapshot selection on the same locked terminal.
    let rc = unsafe {
        ffi::ghostty_terminal_set(
            guard.raw,
            ffi::GHOSTTY_TERMINAL_OPT_SELECTION,
            (&raw const sel).cast(),
        )
    };
    if rc != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(rc));
    }
    Ok(())
}

fn read_screen_row_cells(
    raw: ffi::GhosttyTerminal,
    cols: u16,
    total_rows: u32,
    screen_row: u32,
) -> Result<Vec<(char, u16)>, VtError> {
    if cols == 0 || screen_row >= total_rows {
        return Ok(Vec::new());
    }
    let mut ref0 = ffi::GhosttyGridRef::default();
    let mut ref1 = ffi::GhosttyGridRef::default();
    let r0 =
        unsafe { ffi::ghostty_terminal_grid_ref(raw, screen_point(0, screen_row), &raw mut ref0) };
    let r1 = unsafe {
        ffi::ghostty_terminal_grid_ref(
            raw,
            screen_point(cols.saturating_sub(1), screen_row),
            &raw mut ref1,
        )
    };
    if r0 != ffi::GHOSTTY_SUCCESS || r1 != ffi::GHOSTTY_SUCCESS {
        return Ok(Vec::new());
    }
    let sel = ffi::GhosttySelection {
        size: std::mem::size_of::<ffi::GhosttySelection>(),
        start: ref0,
        end: ref1,
        rectangle: true,
    };
    let text = format_explicit_selection(raw, &sel, false, true)?;
    let mut out = Vec::with_capacity(text.chars().count());
    let mut col: u16 = 0;
    for ch in text.chars() {
        let w = char_display_width(ch);
        if w == 0 {
            continue;
        }
        out.push((ch, col));
        col = col.saturating_add(u16::from(w));
    }
    Ok(out)
}

/// Returns the column of the first non-whitespace character on `screen_row` (`^`), or `0` if blank.
pub fn screen_first_non_blank_col(terminal: &Terminal, screen_row: u32) -> Result<u16, VtError> {
    let guard = terminal.inner.lock();
    let (cols, total_rows) = terminal_cols_and_total_rows(guard.raw)?;
    let cells = read_screen_row_cells(guard.raw, cols, total_rows, screen_row)?;
    for (ch, col) in cells {
        if !ch.is_whitespace() {
            return Ok(col);
        }
    }
    Ok(0)
}

/// Returns the column of the last non-whitespace character on `screen_row` (`$`), or `0` if blank.
pub fn screen_line_end_col(terminal: &Terminal, screen_row: u32) -> Result<u16, VtError> {
    let guard = terminal.inner.lock();
    let (cols, total_rows) = terminal_cols_and_total_rows(guard.raw)?;
    let cells = read_screen_row_cells(guard.raw, cols, total_rows, screen_row)?;
    for &(ch, col) in cells.iter().rev() {
        if !ch.is_whitespace() {
            return Ok(col);
        }
    }
    Ok(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VimCharClass {
    Space,
    Word,
    Punct,
}

#[inline]
fn classify_vim_char(ch: char) -> VimCharClass {
    if ch.is_whitespace() {
        VimCharClass::Space
    } else if ch.is_alphanumeric() || ch == '_' {
        VimCharClass::Word
    } else {
        VimCharClass::Punct
    }
}

/// Returns dense per-cell `(col, class)` up to the last non-blank char (empty vec if blank row).
fn screen_row_classes(
    raw: ffi::GhosttyTerminal,
    cols: u16,
    total_rows: u32,
    screen_row: u32,
) -> Result<Vec<(u16, VimCharClass)>, VtError> {
    let cells = read_screen_row_cells(raw, cols, total_rows, screen_row)?;
    let mut out: Vec<(u16, VimCharClass)> = cells
        .into_iter()
        .map(|(ch, col)| (col, classify_vim_char(ch)))
        .collect();
    while out
        .last()
        .is_some_and(|(_, cls)| *cls == VimCharClass::Space)
    {
        out.pop();
    }
    Ok(out)
}

/// Computes the next cursor [`ScreenPoint`] for Vim `w` (`NextStart`), `b` (`PrevStart`),
/// or `e` (`NextEnd`) across scrollback + viewport rows.
pub fn screen_word_motion(
    terminal: &Terminal,
    from: ScreenPoint,
    motion: WordMotion,
) -> Result<ScreenPoint, VtError> {
    let guard = terminal.inner.lock();
    let (cols, total_rows) = terminal_cols_and_total_rows(guard.raw)?;
    if cols == 0 || total_rows == 0 {
        return Ok(from);
    }
    let max_row = total_rows.saturating_sub(1);
    let mut row = from.row.min(max_row);
    let mut classes = screen_row_classes(guard.raw, cols, total_rows, row)?;

    match motion {
        WordMotion::NextStart => {
            // Find index at or after `from.col`.
            let idx = classes.iter().position(|&(c, _)| c >= from.col);
            if let Some(mut i) = idx {
                let start_cls = classes[i].1;
                if start_cls != VimCharClass::Space {
                    while i < classes.len() && classes[i].1 == start_cls {
                        i += 1;
                    }
                }
                while i < classes.len() && classes[i].1 == VimCharClass::Space {
                    i += 1;
                }
                if i < classes.len() {
                    return Ok(ScreenPoint {
                        col: classes[i].0,
                        row,
                    });
                }
            }
            // Advance to subsequent rows.
            while row < max_row {
                row += 1;
                classes = screen_row_classes(guard.raw, cols, total_rows, row)?;
                if classes.is_empty() {
                    return Ok(ScreenPoint { col: 0, row });
                }
                if let Some(&(col, _)) =
                    classes.iter().find(|&&(_, cls)| cls != VimCharClass::Space)
                {
                    return Ok(ScreenPoint { col, row });
                }
            }
            let end_col = classes.last().map_or(0, |&(c, _)| c);
            Ok(ScreenPoint { col: end_col, row })
        }
        WordMotion::PrevStart => {
            // Find index strictly before `from.col`.
            let mut idx_opt = classes.iter().rposition(|&(c, _)| c < from.col);
            loop {
                if let Some(mut i) = idx_opt {
                    while i > 0 && classes[i].1 == VimCharClass::Space {
                        i -= 1;
                    }
                    if classes[i].1 != VimCharClass::Space {
                        let target_cls = classes[i].1;
                        while i > 0 && classes[i - 1].1 == target_cls {
                            i -= 1;
                        }
                        return Ok(ScreenPoint {
                            col: classes[i].0,
                            row,
                        });
                    }
                }
                if row == 0 {
                    return Ok(ScreenPoint { col: 0, row: 0 });
                }
                row -= 1;
                classes = screen_row_classes(guard.raw, cols, total_rows, row)?;
                if classes.is_empty() {
                    return Ok(ScreenPoint { col: 0, row });
                }
                idx_opt = Some(classes.len() - 1);
            }
        }
        WordMotion::NextEnd => {
            // Start strictly after `from.col`.
            let mut idx = classes.iter().position(|&(c, _)| c > from.col);
            loop {
                if let Some(mut i) = idx {
                    while i < classes.len() && classes[i].1 == VimCharClass::Space {
                        i += 1;
                    }
                    if i < classes.len() {
                        let target_cls = classes[i].1;
                        while i + 1 < classes.len() && classes[i + 1].1 == target_cls {
                            i += 1;
                        }
                        return Ok(ScreenPoint {
                            col: classes[i].0,
                            row,
                        });
                    }
                }
                if row >= max_row {
                    let end_col = classes.last().map_or(0, |&(c, _)| c);
                    return Ok(ScreenPoint { col: end_col, row });
                }
                row += 1;
                classes = screen_row_classes(guard.raw, cols, total_rows, row)?;
                idx = if classes.is_empty() { None } else { Some(0) };
            }
        }
    }
}

/// Extracts the OSC 8 hyperlink URI at viewport coordinate `(col, row)`, if present.
pub fn hyperlink_at(terminal: &Terminal, col: u16, row: u32) -> Result<Option<String>, VtError> {
    let guard = terminal.inner.lock();
    let mut grid_ref = ffi::GhosttyGridRef::default();
    // SAFETY: `guard.raw` is locked for the full grid-ref lifetime.
    let rc = unsafe {
        ffi::ghostty_terminal_grid_ref(
            guard.raw,
            ffi::GhosttyPoint::viewport(col, row),
            &raw mut grid_ref,
        )
    };
    if rc == ffi::GHOSTTY_NO_VALUE || rc == ffi::GHOSTTY_INVALID_VALUE {
        return Ok(None);
    }
    if rc != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(rc));
    }

    let mut stack_buf = [0u8; 512];
    let mut len: usize = 0;
    // SAFETY: `grid_ref` is valid under `guard`.
    let rc = unsafe {
        ffi::ghostty_grid_ref_hyperlink_uri(
            &raw const grid_ref,
            stack_buf.as_mut_ptr(),
            stack_buf.len(),
            &raw mut len,
        )
    };
    match rc {
        ffi::GHOSTTY_SUCCESS => {
            if len == 0 {
                Ok(None)
            } else {
                Ok(Some(
                    String::from_utf8_lossy(&stack_buf[..len]).into_owned(),
                ))
            }
        }
        ffi::GHOSTTY_OUT_OF_SPACE => {
            let mut heap_buf = vec![0u8; len];
            let mut actual: usize = 0;
            // SAFETY: `heap_buf` has exact required capacity `len`.
            let rc2 = unsafe {
                ffi::ghostty_grid_ref_hyperlink_uri(
                    &raw const grid_ref,
                    heap_buf.as_mut_ptr(),
                    heap_buf.len(),
                    &raw mut actual,
                )
            };
            if rc2 != ffi::GHOSTTY_SUCCESS {
                return Err(VtError::Ffi(rc2));
            }
            heap_buf.truncate(actual);
            Ok(Some(String::from_utf8_lossy(&heap_buf).into_owned()))
        }
        other => Err(VtError::Ffi(other)),
    }
}

/// Formats an explicit `GhosttySelection` without mutating the terminal's active selection.
fn format_explicit_selection(
    raw: ffi::GhosttyTerminal,
    sel: &ffi::GhosttySelection,
    unwrap: bool,
    trim: bool,
) -> Result<String, VtError> {
    let opts = ffi::GhosttyTerminalSelectionFormatOptions {
        size: std::mem::size_of::<ffi::GhosttyTerminalSelectionFormatOptions>(),
        emit: ffi::GHOSTTY_FORMATTER_FORMAT_PLAIN,
        unwrap,
        trim,
        selection: sel as *const ffi::GhosttySelection,
    };

    let mut required: usize = 0;
    // SAFETY: Caller holds `terminal.inner` lock; `sel` lives across both calls.
    let rc = unsafe {
        ffi::ghostty_terminal_selection_format_buf(raw, opts, ptr::null_mut(), 0, &raw mut required)
    };
    if rc == ffi::GHOSTTY_NO_VALUE || required == 0 {
        return Ok(String::new());
    }
    if rc != ffi::GHOSTTY_SUCCESS && rc != ffi::GHOSTTY_OUT_OF_SPACE {
        return Err(VtError::Ffi(rc));
    }

    let mut buf = vec![0u8; required];
    let mut written: usize = 0;
    // SAFETY: `buf` has exact required length.
    let rc2 = unsafe {
        ffi::ghostty_terminal_selection_format_buf(
            raw,
            opts,
            buf.as_mut_ptr(),
            buf.len(),
            &raw mut written,
        )
    };
    if rc2 != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(rc2));
    }
    buf.truncate(written);
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Returns viewport-relative `(row, start_col..end_col)` spans for the active terminal selection.
///
/// Queries the terminal selection and screen coordinates directly without allocating or updating a
/// `RenderState`, so `GhosttyTerminal`'s incremental dirty-row flags are never consumed or cleared.
pub fn selection_viewport_spans(terminal: &Terminal) -> Result<Vec<RowCellSpan>, VtError> {
    let guard = terminal.inner.lock();

    let mut sel = ffi::GhosttySelection::default();
    // SAFETY: `guard.raw` is locked; `sel.size` is initialized by `Default`.
    let rc_sel = unsafe {
        ffi::ghostty_terminal_get(
            guard.raw,
            ffi::GHOSTTY_TERMINAL_DATA_SELECTION,
            (&raw mut sel).cast(),
        )
    };
    if rc_sel == ffi::GHOSTTY_NO_VALUE {
        return Ok(Vec::new());
    }
    if rc_sel != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(rc_sel));
    }

    let mut cols: u16 = 0;
    let mut rows: u16 = 0;
    let mut sb = ffi::GhosttyTerminalScrollbar::default();
    // SAFETY: `guard.raw` is locked; output pointers match documented C ABI types.
    unsafe {
        let rc_c = ffi::ghostty_terminal_get(
            guard.raw,
            ffi::GHOSTTY_TERMINAL_DATA_COLS,
            (&raw mut cols).cast(),
        );
        if rc_c != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc_c));
        }
        let rc_r = ffi::ghostty_terminal_get(
            guard.raw,
            ffi::GHOSTTY_TERMINAL_DATA_ROWS,
            (&raw mut rows).cast(),
        );
        if rc_r != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc_r));
        }
        let rc_sb = ffi::ghostty_terminal_get(
            guard.raw,
            ffi::GHOSTTY_TERMINAL_DATA_SCROLLBAR,
            (&raw mut sb).cast(),
        );
        if rc_sb != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc_sb));
        }
    }
    if cols == 0 || rows == 0 {
        return Ok(Vec::new());
    }

    let mut start_pt = ffi::GhosttyPointCoordinate::default();
    let mut end_pt = ffi::GhosttyPointCoordinate::default();
    // SAFETY: `sel.start` and `sel.end` are valid untracked grid refs while `guard` is held.
    let rc_s = unsafe {
        ffi::ghostty_terminal_point_from_grid_ref(
            guard.raw,
            &raw const sel.start,
            ffi::GHOSTTY_POINT_TAG_SCREEN,
            &raw mut start_pt,
        )
    };
    let rc_e = unsafe {
        ffi::ghostty_terminal_point_from_grid_ref(
            guard.raw,
            &raw const sel.end,
            ffi::GHOSTTY_POINT_TAG_SCREEN,
            &raw mut end_pt,
        )
    };
    if rc_s == ffi::GHOSTTY_NO_VALUE || rc_e == ffi::GHOSTTY_NO_VALUE {
        return Ok(Vec::new());
    }
    if rc_s != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(rc_s));
    }
    if rc_e != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(rc_e));
    }

    let vp_top = sb.offset;
    let vp_bot_excl = vp_top.saturating_add(u64::from(rows));
    let max_col = cols.saturating_sub(1);
    let mut spans = Vec::new();

    if sel.rectangle {
        let y_min = u64::from(start_pt.y.min(end_pt.y));
        let y_max = u64::from(start_pt.y.max(end_pt.y));
        let x_min = start_pt.x.min(end_pt.x).min(max_col);
        let x_max = start_pt.x.max(end_pt.x).min(max_col);
        let vis_start = y_min.max(vp_top);
        let vis_end = y_max.min(vp_bot_excl.saturating_sub(1));
        if vis_start <= vis_end && y_min < vp_bot_excl && y_max >= vp_top {
            for y in vis_start..=vis_end {
                spans.push(RowCellSpan {
                    row: (y - vp_top) as u32,
                    start_col: x_min,
                    end_col: x_max.saturating_add(1),
                });
            }
        }
    } else {
        let ((s_x, s_y), (e_x, e_y)) = if (start_pt.y, start_pt.x) <= (end_pt.y, end_pt.x) {
            (
                (start_pt.x.min(max_col), u64::from(start_pt.y)),
                (end_pt.x.min(max_col), u64::from(end_pt.y)),
            )
        } else {
            (
                (end_pt.x.min(max_col), u64::from(end_pt.y)),
                (start_pt.x.min(max_col), u64::from(start_pt.y)),
            )
        };
        let vis_start = s_y.max(vp_top);
        let vis_end = e_y.min(vp_bot_excl.saturating_sub(1));
        if vis_start <= vis_end && s_y < vp_bot_excl && e_y >= vp_top {
            for y in vis_start..=vis_end {
                let start_col = if y == s_y { s_x } else { 0 };
                let end_col_incl = if y == e_y { e_x } else { max_col };
                if end_col_incl >= start_col {
                    spans.push(RowCellSpan {
                        row: (y - vp_top) as u32,
                        start_col,
                        end_col: end_col_incl.saturating_add(1),
                    });
                }
            }
        }
    }

    Ok(spans)
}

/// Extracts the full unwrapped logical line containing viewport cell `(col, row)`, along with the
/// 0-based Unicode character index corresponding to `(col, row)` inside the returned string.
///
/// Does **not** modify the terminal's active text selection. Handles soft-wrapped multi-line paths
/// and wide/multibyte UTF-8 glyphs accurately.
pub fn logical_line_at(
    terminal: &Terminal,
    col: u16,
    row: u32,
) -> Result<Option<(String, usize)>, VtError> {
    let guard = terminal.inner.lock();
    let mut hover_ref = ffi::GhosttyGridRef::default();
    // SAFETY: `guard.raw` is locked for the full grid-ref lifetime.
    let rc = unsafe {
        ffi::ghostty_terminal_grid_ref(
            guard.raw,
            ffi::GhosttyPoint::viewport(col, row),
            &raw mut hover_ref,
        )
    };
    if rc == ffi::GHOSTTY_NO_VALUE || rc == ffi::GHOSTTY_INVALID_VALUE {
        return Ok(None);
    }
    if rc != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(rc));
    }

    let opts = ffi::GhosttyTerminalSelectLineOptions {
        size: std::mem::size_of::<ffi::GhosttyTerminalSelectLineOptions>(),
        ref_: hover_ref,
        whitespace: ptr::null(),
        whitespace_len: 0,
        semantic_prompt_boundary: false,
    };
    let mut line_sel = ffi::GhosttySelection::default();
    // SAFETY: `guard.raw` is locked; `opts` and `line_sel` have valid `.size`.
    let rc =
        unsafe { ffi::ghostty_terminal_select_line(guard.raw, &raw const opts, &raw mut line_sel) };
    match rc {
        ffi::GHOSTTY_NO_VALUE => return Ok(None),
        ffi::GHOSTTY_SUCCESS => {}
        other => return Err(VtError::Ffi(other)),
    }

    let line_text = format_explicit_selection(guard.raw, &line_sel, true, true)?;
    if line_text.is_empty() {
        return Ok(None);
    }

    let prefix_sel = ffi::GhosttySelection {
        size: std::mem::size_of::<ffi::GhosttySelection>(),
        start: line_sel.start,
        end: hover_ref,
        rectangle: false,
    };
    let prefix = format_explicit_selection(guard.raw, &prefix_sel, true, false)?;
    let hover_char_col = prefix.chars().count().saturating_sub(1);
    Ok(Some((line_text, hover_char_col)))
}

/// A contiguous cell span on a single viewport row (0-based `row`, half-open column range).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct RowCellSpan {
    pub row: u32,
    pub start_col: u16,
    pub end_col: u16,
}

/// Origin cell of one Unicode character inside an [`UnwrappedLine`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct CharCellOrigin {
    pub row: u32,
    pub col: u16,
    pub width: u8,
}

/// Logical line unwrapped across both VT soft-wraps and TUI box-gutter continuations
/// (such as `│ ` / `┃ ` code blocks in `tr3x`), with exact per-character viewport cell mapping
/// for multi-row hover underline rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnwrappedLine {
    pub text: String,
    /// 0-based Unicode character index in `text` corresponding to the hovered `(col, row)`.
    pub hover_char_col: usize,
    /// Parallel to `text.chars()` — exact viewport `(row, col, width)` for each character.
    pub char_cells: Vec<CharCellOrigin>,
}

impl UnwrappedLine {
    /// Converts a Unicode character range (such as `DetectedFilePath::char_span`) into one or
    /// more per-row [`RowCellSpan`]s suitable for rendering hover underlines across wrapped rows.
    #[must_use]
    pub fn cell_spans_for_char_range(&self, char_range: Range<usize>) -> Vec<RowCellSpan> {
        let mut spans: Vec<RowCellSpan> = Vec::new();
        let end = char_range.end.min(self.char_cells.len());
        if char_range.start >= end {
            return spans;
        }
        for cell in &self.char_cells[char_range.start..end] {
            let cell_end = cell.col.saturating_add(u16::from(cell.width.max(1)));
            if let Some(last) = spans.last_mut()
                && last.row == cell.row
                && cell.col <= last.end_col
            {
                last.end_col = last.end_col.max(cell_end);
            } else {
                spans.push(RowCellSpan {
                    row: cell.row,
                    start_col: cell.col,
                    end_col: cell_end,
                });
            }
        }
        spans
    }
}

/// Internal parsed representation of a single viewport row for TUI gutter stitching.
struct RawViewportRow {
    row: u32,
    chars: Vec<(char, u16, u8)>,
    vt_wraps_next: bool,
    gutter: Option<(char, u16)>,
    /// Leading space count before content (used when `gutter` is `None`).
    indent_cols: u16,
    /// Index into `chars` where non-gutter, non-indent content begins.
    content_start_idx: usize,
    /// Index into `chars` one past the last non-whitespace character.
    trimmed_end_idx: usize,
    /// 1-past-end column of the last non-whitespace character (0 if blank).
    trimmed_end_col: u16,
}

impl RawViewportRow {
    fn content_str(&self) -> String {
        if self.content_start_idx >= self.trimmed_end_idx {
            return String::new();
        }
        self.chars[self.content_start_idx..self.trimmed_end_idx]
            .iter()
            .map(|(ch, _, _)| *ch)
            .collect()
    }

    fn full_str(&self) -> String {
        self.chars.iter().map(|(ch, _, _)| *ch).collect()
    }
}

#[inline]
fn is_tui_gutter_char(ch: char) -> bool {
    matches!(
        ch,
        '│' | '┃' | '║' | '▏' | '▎' | '▍' | '▌' | '┆' | '┇' | '┊' | '┋' | '|'
    )
}

#[inline]
fn char_display_width(ch: char) -> u8 {
    let cp = ch as u32;
    if ch == '\0'
        || ch == '\u{200D}'
        || (0x0300..=0x036F).contains(&cp)
        || (0xFE00..=0xFE0F).contains(&cp)
    {
        return 0;
    }
    if (0x1100..=0x115F).contains(&cp)
        || (0x2E80..=0xA4CF).contains(&cp)
        || (0xAC00..=0xD7A3).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0xFE10..=0xFE19).contains(&cp)
        || (0xFE30..=0xFE6F).contains(&cp)
        || (0xFF00..=0xFF60).contains(&cp)
        || (0xFFE0..=0xFFE6).contains(&cp)
        || (0x1F300..=0x1FAFF).contains(&cp)
        || (0x20000..=0x3FFFD).contains(&cp)
    {
        2
    } else {
        1
    }
}

fn read_viewport_row(
    raw: ffi::GhosttyTerminal,
    cols: u16,
    rows: u16,
    row: u32,
) -> Result<Option<RawViewportRow>, VtError> {
    if cols == 0 || row >= u32::from(rows) {
        return Ok(None);
    }

    let mut ref0 = ffi::GhosttyGridRef::default();
    let mut ref1 = ffi::GhosttyGridRef::default();
    // SAFETY: `raw` is locked by caller; `(0, row)` and `(cols - 1, row)` are in-bounds.
    let r0 = unsafe {
        ffi::ghostty_terminal_grid_ref(raw, ffi::GhosttyPoint::viewport(0, row), &raw mut ref0)
    };
    let r1 = unsafe {
        ffi::ghostty_terminal_grid_ref(
            raw,
            ffi::GhosttyPoint::viewport(cols.saturating_sub(1), row),
            &raw mut ref1,
        )
    };
    if r0 != ffi::GHOSTTY_SUCCESS || r1 != ffi::GHOSTTY_SUCCESS {
        return Ok(None);
    }

    // Check if `libghostty-vt` considers this row soft-wrapped onto `row + 1`.
    let mut vt_wraps_next = false;
    if row + 1 < u32::from(rows) {
        let opts = ffi::GhosttyTerminalSelectLineOptions {
            size: std::mem::size_of::<ffi::GhosttyTerminalSelectLineOptions>(),
            ref_: ref0,
            whitespace: ptr::null(),
            whitespace_len: 0,
            semantic_prompt_boundary: false,
        };
        let mut line_sel = ffi::GhosttySelection::default();
        // SAFETY: `raw` is locked; `opts` and `line_sel` have valid `.size`.
        if unsafe { ffi::ghostty_terminal_select_line(raw, &raw const opts, &raw mut line_sel) }
            == ffi::GHOSTTY_SUCCESS
        {
            let mut end_pt = ffi::GhosttyPointCoordinate::default();
            if unsafe {
                ffi::ghostty_terminal_point_from_grid_ref(
                    raw,
                    &raw const line_sel.end,
                    ffi::GHOSTTY_POINT_TAG_VIEWPORT,
                    &raw mut end_pt,
                )
            } == ffi::GHOSTTY_SUCCESS
            {
                vt_wraps_next = end_pt.y > row;
            }
        }
    }

    let row_sel = ffi::GhosttySelection {
        size: std::mem::size_of::<ffi::GhosttySelection>(),
        start: ref0,
        end: ref1,
        rectangle: true,
    };
    let text = format_explicit_selection(raw, &row_sel, false, !vt_wraps_next)?;

    let mut chars = Vec::with_capacity(text.len());
    let mut cur_col: u16 = 0;
    for ch in text.chars() {
        let w = char_display_width(ch);
        if w == 0 {
            let prev_col = cur_col.saturating_sub(1);
            chars.push((ch, prev_col, 1));
        } else {
            chars.push((ch, cur_col, w));
            cur_col = cur_col.saturating_add(u16::from(w));
        }
    }

    let mut trimmed_end_idx = chars.len();
    while trimmed_end_idx > 0 && chars[trimmed_end_idx - 1].0.is_whitespace() {
        trimmed_end_idx -= 1;
    }
    let trimmed_end_col = if trimmed_end_idx > 0 {
        let (_, c, w) = chars[trimmed_end_idx - 1];
        c.saturating_add(u16::from(w))
    } else {
        0
    };

    // Detect leading whitespace and optional TUI box gutter (`│ `, `┃ `, etc.).
    let mut i = 0usize;
    while i < trimmed_end_idx && chars[i].0 == ' ' {
        i += 1;
    }
    let indent_cols = chars.get(i).map_or(0, |&(_, c, _)| c);

    let (gutter, content_start_idx) = if i < trimmed_end_idx
        && indent_cols <= 16
        && is_tui_gutter_char(chars[i].0)
        && chars.get(i + 1).is_some_and(|&(next, _, _)| next == ' ')
    {
        let g = (chars[i].0, chars[i].1);
        let mut j = i + 1;
        while j < trimmed_end_idx && chars[j].0 == ' ' {
            j += 1;
        }
        (Some(g), j)
    } else {
        (None, i)
    };

    Ok(Some(RawViewportRow {
        row,
        chars,
        vt_wraps_next,
        gutter,
        indent_cols,
        content_start_idx,
        trimmed_end_idx,
        trimmed_end_col,
    }))
}

#[inline]
fn is_path_or_url_continuation_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
        || matches!(
            ch,
            '_' | '-' | '.' | '/' | '~' | '%' | '+' | '@' | ':' | '=' | '&' | '?' | '#' | ','
        )
        || (!ch.is_ascii() && !ch.is_whitespace() && !is_tui_gutter_char(ch) && ch != '•')
}

/// Decides whether `prev` hard-wraps directly into `next` across a TUI gutter or margin.
fn should_stitch_tui_rows(prev: &RawViewportRow, next: &RawViewportRow, cols: u16) -> bool {
    if prev.vt_wraps_next {
        return true;
    }
    if prev.content_start_idx >= prev.trimmed_end_idx
        || next.content_start_idx >= next.trimmed_end_idx
    {
        return false;
    }

    // Must share the same TUI gutter (e.g. `│` at same column), or both be unguttered with
    // compatible indentation.
    match (prev.gutter, next.gutter) {
        (Some(g1), Some(g2)) if g1 == g2 => {}
        (None, None)
            if next.indent_cols >= prev.indent_cols
                && next.indent_cols <= prev.indent_cols.saturating_add(6) => {}
        _ => return false,
    }

    // Require `prev` to reach a plausible wrap boundary so two consecutive short path lines
    // (e.g. `│ /tmp/a.png` followed by `│ /tmp/b.png`) are never falsely concatenated.
    let near_wrap_edge = prev.trimmed_end_col.saturating_add(16) >= cols
        || u32::from(prev.trimmed_end_col) * 100 >= u32::from(cols.max(1)) * 55;
    if !near_wrap_edge {
        return false;
    }

    let prev_content = prev.content_str();
    let next_content = next.content_str();

    // Check trailing token of `prev_content`: must look like an active path or URL.
    let trailing_token = prev_content
        .rsplit(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '<' | '>' | '|'))
        .next()
        .unwrap_or("");
    let trailing_clean = trailing_token.trim_start_matches(['(', '[', '{']);
    let looks_like_path_or_url = trailing_clean.contains('/') || trailing_clean.contains("://");
    if !looks_like_path_or_url {
        return false;
    }
    let Some(last_ch) = trailing_clean.chars().last() else {
        return false;
    };
    if !is_path_or_url_continuation_char(last_ch) || matches!(last_ch, ',' | ';' | '!' | '?') {
        return false;
    }

    // Check leading content of `next_content`: must look like a path/URL continuation, not a new
    // bullet, shell prompt, heading, or brand-new root URL/path (unless right at the right edge).
    let Some(first_ch) = next_content.chars().next() else {
        return false;
    };
    if !is_path_or_url_continuation_char(first_ch) {
        return false;
    }
    if next_content.starts_with("- ")
        || next_content.starts_with("* ")
        || next_content.starts_with("# ")
        || next_content.starts_with("$ ")
        || next_content.starts_with("> ")
        || next_content.starts_with("http://")
        || next_content.starts_with("https://")
        || next_content.starts_with("file://")
        || next_content.starts_with("~/")
        || next_content.starts_with("./")
        || next_content.starts_with("../")
    {
        return false;
    }
    if first_ch == '/' && prev.trimmed_end_col.saturating_add(8) < cols {
        return false;
    }

    true
}

fn stitch_row_group(
    rows: &[RawViewportRow],
    hover_target: Option<(u32, u16)>,
) -> Option<(String, Vec<CharCellOrigin>, Option<usize>)> {
    if rows.is_empty() {
        return None;
    }
    let mut text = String::new();
    let mut char_cells = Vec::new();
    let mut hover_char_col = None;

    for (idx, r) in rows.iter().enumerate() {
        let start_idx = if idx == 0 || rows[idx - 1].vt_wraps_next {
            0
        } else {
            r.content_start_idx
        };
        let end_idx = if r.vt_wraps_next {
            r.chars.len()
        } else {
            r.trimmed_end_idx
        };
        if start_idx >= end_idx {
            continue;
        }
        for &(ch, c, w) in &r.chars[start_idx..end_idx] {
            if let Some((h_row, h_col)) = hover_target
                && r.row == h_row
                && h_col >= c
                && h_col < c.saturating_add(u16::from(w.max(1)))
            {
                hover_char_col = Some(char_cells.len());
            }
            text.push(ch);
            char_cells.push(CharCellOrigin {
                row: r.row,
                col: c,
                width: w,
            });
        }
    }

    if text.is_empty() {
        return None;
    }
    Some((text, char_cells, hover_char_col))
}

/// Extracts the unwrapped logical line at viewport `(col, row)`, stitching both VT soft-wraps
/// AND TUI hard-wrapped gutter lines (such as `│ ` fenced code blocks in `tr3x`), and returning
/// exact per-character viewport cell coordinates for hover underline rendering.
pub fn logical_line_with_tui_unwrap_at(
    terminal: &Terminal,
    col: u16,
    row: u32,
) -> Result<Option<UnwrappedLine>, VtError> {
    const MAX_STITCH_ROWS: u32 = 12;

    let guard = terminal.inner.lock();
    let mut cols: u16 = 0;
    let mut rows: u16 = 0;
    // SAFETY: `guard.raw` is locked.
    let rc_c = unsafe {
        ffi::ghostty_terminal_get(
            guard.raw,
            ffi::GHOSTTY_TERMINAL_DATA_COLS,
            (&raw mut cols).cast(),
        )
    };
    let rc_r = unsafe {
        ffi::ghostty_terminal_get(
            guard.raw,
            ffi::GHOSTTY_TERMINAL_DATA_ROWS,
            (&raw mut rows).cast(),
        )
    };
    if rc_c != ffi::GHOSTTY_SUCCESS || rc_r != ffi::GHOSTTY_SUCCESS || cols == 0 || rows == 0 {
        return Ok(None);
    }
    if col >= cols || row >= u32::from(rows) {
        return Ok(None);
    }

    let Some(center) = read_viewport_row(guard.raw, cols, rows, row)? else {
        return Ok(None);
    };
    if center.trimmed_end_idx == 0 && !center.vt_wraps_next {
        return Ok(None);
    }

    // Expand upward while the previous row stitches into the current first row.
    let mut above: Vec<RawViewportRow> = Vec::new();
    let mut cur_top = row;
    while cur_top > 0 && row - cur_top < MAX_STITCH_ROWS {
        let prev_row_idx = cur_top - 1;
        let Some(prev) = read_viewport_row(guard.raw, cols, rows, prev_row_idx)? else {
            break;
        };
        let next_ref = above.last().unwrap_or(&center);
        if should_stitch_tui_rows(&prev, next_ref, cols) {
            above.push(prev);
            cur_top = prev_row_idx;
        } else {
            break;
        }
    }
    above.reverse();

    // Expand downward while the current last row stitches into the next row.
    let mut below: Vec<RawViewportRow> = Vec::new();
    let mut cur_bot = row;
    while cur_bot + 1 < u32::from(rows) && cur_bot - row < MAX_STITCH_ROWS {
        let next_row_idx = cur_bot + 1;
        let Some(next) = read_viewport_row(guard.raw, cols, rows, next_row_idx)? else {
            break;
        };
        let prev_ref = below.last().unwrap_or(&center);
        if should_stitch_tui_rows(prev_ref, &next, cols) {
            below.push(next);
            cur_bot = next_row_idx;
        } else {
            break;
        }
    }

    let mut group = Vec::with_capacity(above.len() + 1 + below.len());
    group.extend(above);
    group.push(center);
    group.extend(below);

    let Some((text, char_cells, Some(hover_char_col))) = stitch_row_group(&group, Some((row, col)))
    else {
        return Ok(None);
    };

    Ok(Some(UnwrappedLine {
        text,
        hover_char_col,
        char_cells,
    }))
}

/// Returns all non-empty visible viewport lines with VT soft-wraps and TUI gutter continuations
/// stitched together (handy for resolving bare filenames against full paths visible on screen).
pub fn viewport_text_lines(terminal: &Terminal) -> Result<Vec<String>, VtError> {
    let guard = terminal.inner.lock();
    let mut cols: u16 = 0;
    let mut rows: u16 = 0;
    // SAFETY: `guard.raw` is locked.
    let rc_c = unsafe {
        ffi::ghostty_terminal_get(
            guard.raw,
            ffi::GHOSTTY_TERMINAL_DATA_COLS,
            (&raw mut cols).cast(),
        )
    };
    let rc_r = unsafe {
        ffi::ghostty_terminal_get(
            guard.raw,
            ffi::GHOSTTY_TERMINAL_DATA_ROWS,
            (&raw mut rows).cast(),
        )
    };
    if rc_c != ffi::GHOSTTY_SUCCESS || rc_r != ffi::GHOSTTY_SUCCESS || cols == 0 || rows == 0 {
        return Ok(Vec::new());
    }

    let mut raw_rows = Vec::with_capacity(usize::from(rows));
    for r in 0..u32::from(rows) {
        if let Some(parsed) = read_viewport_row(guard.raw, cols, rows, r)? {
            raw_rows.push(parsed);
        }
    }

    let mut out = Vec::new();
    let mut i = 0usize;
    while i < raw_rows.len() {
        let mut end = i;
        while end + 1 < raw_rows.len()
            && should_stitch_tui_rows(&raw_rows[end], &raw_rows[end + 1], cols)
        {
            end += 1;
        }
        if let Some((stitched, _, _)) = stitch_row_group(&raw_rows[i..=end], None) {
            let trimmed = stitched.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_owned());
            }
        } else {
            let fallback = raw_rows[i].full_str();
            let trimmed = fallback.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_owned());
            }
        }
        i = end + 1;
    }

    Ok(out)
}

/// Returns the topmost Kitty image placement covering viewport cell `(col, row)`, if any.
///
/// Placements are compared by `(z_layer, z_index)` so foreground images take priority over
/// background images when overlapping.
#[must_use]
pub fn kitty_placement_at(
    placements: &[KittyPlacement],
    col: u16,
    row: u32,
) -> Option<KittyPlacement> {
    let col_i = i32::from(col);
    let row_i = i32::try_from(row).ok()?;
    let mut best: Option<KittyPlacement> = None;

    for &p in placements {
        let gcols = i32::try_from(p.grid_cols.max(1)).unwrap_or(i32::MAX);
        let grows = i32::try_from(p.grid_rows.max(1)).unwrap_or(i32::MAX);
        let col_end = p.viewport_col.saturating_add(gcols);
        let row_end = p.viewport_row.saturating_add(grows);
        if col_i >= p.viewport_col && col_i < col_end && row_i >= p.viewport_row && row_i < row_end
        {
            let replace = match best {
                None => true,
                Some(prev) => (p.z_layer, p.z_index) >= (prev.z_layer, prev.z_index),
            };
            if replace {
                best = Some(p);
            }
        }
    }
    best
}

/// Checks whether the cell span `[col_start..col_end)` on an arbitrary signed row offset relative
/// to the top of the current viewport (`viewport_row`, which may be negative to inspect scrollback
/// rows above the viewport) contains only blank/whitespace characters.
///
/// Returns `Ok(None)` if the requested row lies outside the terminal's history bounds.
pub fn viewport_row_span_is_blank(
    terminal: &Terminal,
    viewport_row: i32,
    col_start: u16,
    col_end: u16,
) -> Result<Option<bool>, VtError> {
    if col_end <= col_start {
        return Ok(Some(true));
    }
    let guard = terminal.inner.lock();
    let mut sb = ffi::GhosttyTerminalScrollbar::default();
    // SAFETY: `guard.raw` is locked.
    let rc = unsafe {
        ffi::ghostty_terminal_get(
            guard.raw,
            ffi::GHOSTTY_TERMINAL_DATA_SCROLLBAR,
            (&raw mut sb).cast(),
        )
    };
    if rc != ffi::GHOSTTY_SUCCESS {
        return Err(VtError::Ffi(rc));
    }
    let abs_row_i64 = (sb.offset as i64) + i64::from(viewport_row);
    if abs_row_i64 < 0 || (abs_row_i64 as u64) >= sb.total {
        return Ok(None);
    }
    let mut cols: u16 = 0;
    let rc_c = unsafe {
        ffi::ghostty_terminal_get(
            guard.raw,
            ffi::GHOSTTY_TERMINAL_DATA_COLS,
            (&raw mut cols).cast(),
        )
    };
    if rc_c != ffi::GHOSTTY_SUCCESS || cols == 0 {
        return Ok(None);
    }
    let c0 = col_start.min(cols.saturating_sub(1));
    let c1 = col_end.saturating_sub(1).min(cols.saturating_sub(1));
    if c1 < c0 {
        return Ok(Some(true));
    }

    let screen_y = abs_row_i64 as u32;
    let pt0 = ffi::GhosttyPoint {
        tag: ffi::GHOSTTY_POINT_TAG_SCREEN,
        value: ffi::GhosttyPointValue {
            coordinate: ffi::GhosttyPointCoordinate { x: c0, y: screen_y },
        },
    };
    let pt1 = ffi::GhosttyPoint {
        tag: ffi::GHOSTTY_POINT_TAG_SCREEN,
        value: ffi::GhosttyPointValue {
            coordinate: ffi::GhosttyPointCoordinate { x: c1, y: screen_y },
        },
    };
    let mut ref0 = ffi::GhosttyGridRef::default();
    let mut ref1 = ffi::GhosttyGridRef::default();
    // SAFETY: `guard.raw` is locked; screen coordinates are verified in-bounds.
    let r0 = unsafe { ffi::ghostty_terminal_grid_ref(guard.raw, pt0, &raw mut ref0) };
    let r1 = unsafe { ffi::ghostty_terminal_grid_ref(guard.raw, pt1, &raw mut ref1) };
    if r0 != ffi::GHOSTTY_SUCCESS || r1 != ffi::GHOSTTY_SUCCESS {
        return Ok(None);
    }

    let span_sel = ffi::GhosttySelection {
        size: std::mem::size_of::<ffi::GhosttySelection>(),
        start: ref0,
        end: ref1,
        rectangle: true,
    };
    let text = format_explicit_selection(guard.raw, &span_sel, false, true)?;
    Ok(Some(text.trim().is_empty()))
}

// ---------------------------------------------------------------------------
// Incremental Scrollback Search (`search.h`)
// ---------------------------------------------------------------------------

/// Progress state of an incremental [`TerminalSearch`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SearchStatus {
    /// `tick()` can make progress lock-free without touching the terminal.
    Running,
    /// `feed()` must be called to pull the next terminal chunk.
    FeedRequired,
    /// Caught up with the terminal as of the last feed.
    Complete,
}

impl SearchStatus {
    fn from_ffi(raw: ffi::GhosttySearchStatus) -> Self {
        match raw {
            ffi::GHOSTTY_SEARCH_STATUS_RUNNING => Self::Running,
            ffi::GHOSTTY_SEARCH_STATUS_FEED_REQUIRED => Self::FeedRequired,
            _ => Self::Complete,
        }
    }
}

/// Viewport scroll policy when moving between search matches.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(i32)]
pub enum SearchScroll {
    IfNeeded = ffi::GHOSTTY_SEARCH_SCROLL_IF_NEEDED,
    None = ffi::GHOSTTY_SEARCH_SCROLL_NONE,
}

/// A search match projected into viewport `(col, row)` coordinates for GPU highlight quads.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ViewportMatch {
    pub start_col: u16,
    pub start_row: u32,
    pub end_col: u16,
    pub end_row: u32,
}

/// Safe RAII wrapper around `GhosttySearch` supporting lock-free background `tick()`
/// and short-lock `feed()` over primary + alternate screens and compressed scrollback.
pub struct TerminalSearch {
    raw: ffi::GhosttySearch,
    terminal: Weak<Mutex<TerminalInner>>,
    scratch: Vec<ffi::GhosttySelection>,
}

// SAFETY: `TerminalSearch` owns `raw` exclusively and serializes terminal-touching
// operations through `Weak<Mutex<TerminalInner>>`.
unsafe impl Send for TerminalSearch {}

impl TerminalSearch {
    pub fn new(terminal: &Terminal) -> Result<Self, VtError> {
        let guard = terminal.inner.lock();
        let mut raw: ffi::GhosttySearch = ptr::null_mut();
        // SAFETY: `guard.raw` is locked while registering the search with the terminal.
        let rc = unsafe { ffi::ghostty_search_new(ptr::null(), &raw mut raw, guard.raw) };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        Ok(Self {
            raw,
            terminal: std::sync::Arc::downgrade(&terminal.inner),
            scratch: Vec::new(),
        })
    }

    /// Sets (or clears if empty) the ASCII case-insensitive search needle.
    pub fn set_needle(&mut self, needle: &str) -> Result<(), VtError> {
        let Some(arc) = self.terminal.upgrade() else {
            return Err(VtError::TerminalDropped);
        };
        let _guard = arc.lock();
        let rc = if needle.is_empty() {
            // SAFETY: Passing NULL clears the needle while terminal access is serialized.
            unsafe {
                ffi::ghostty_search_set(self.raw, ffi::GHOSTTY_SEARCH_OPT_NEEDLE, ptr::null())
            }
        } else {
            let gs = ffi::GhosttyString {
                ptr: needle.as_ptr(),
                len: needle.len(),
            };
            // SAFETY: `gs` is copied internally by `libghostty-vt`.
            unsafe {
                ffi::ghostty_search_set(
                    self.raw,
                    ffi::GHOSTTY_SEARCH_OPT_NEEDLE,
                    (&raw const gs).cast(),
                )
            }
        };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        Ok(())
    }

    pub fn set_scroll_policy(&mut self, policy: SearchScroll) -> Result<(), VtError> {
        let raw = policy as ffi::GhosttySearchScroll;
        // SAFETY: `OPT_SELECT_SCROLL` only touches search-owned state.
        let rc = unsafe {
            ffi::ghostty_search_set(
                self.raw,
                ffi::GHOSTTY_SEARCH_OPT_SELECT_SCROLL,
                (&raw const raw).cast(),
            )
        };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        Ok(())
    }

    /// Advances search work completely lock-free without touching the terminal.
    pub fn tick(&mut self) -> Result<SearchStatus, VtError> {
        let mut status: ffi::GhosttySearchStatus = ffi::GHOSTTY_SEARCH_STATUS_COMPLETE;
        // SAFETY: `ghostty_search_tick` never reads the terminal (`search.h:349-352`).
        let rc = unsafe { ffi::ghostty_search_tick(self.raw, &raw mut status) };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        Ok(SearchStatus::from_ffi(status))
    }

    /// Briefly locks the terminal to copy the next bounded chunk of data into the searcher.
    pub fn feed(&mut self) -> Result<(), VtError> {
        let Some(arc) = self.terminal.upgrade() else {
            return Err(VtError::TerminalDropped);
        };
        let _guard = arc.lock();
        // SAFETY: Terminal lock is held for `ghostty_search_feed`.
        let rc = unsafe { ffi::ghostty_search_feed(self.raw) };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        Ok(())
    }

    /// Blocking convenience that feeds and ticks until caught up with the terminal.
    pub fn run(&mut self) -> Result<(), VtError> {
        let Some(arc) = self.terminal.upgrade() else {
            return Err(VtError::TerminalDropped);
        };
        let _guard = arc.lock();
        // SAFETY: Terminal lock is held for `ghostty_search_run`.
        let rc = unsafe { ffi::ghostty_search_run(self.raw) };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        Ok(())
    }

    /// Moves selection to the next match (toward older history). Returns `Ok(false)` if no matches.
    pub fn select_next(&mut self) -> Result<bool, VtError> {
        let Some(arc) = self.terminal.upgrade() else {
            return Err(VtError::TerminalDropped);
        };
        let _guard = arc.lock();
        // SAFETY: `OPT_SELECT_NEXT` requires NULL value and serialized terminal access.
        let rc = unsafe {
            ffi::ghostty_search_set(self.raw, ffi::GHOSTTY_SEARCH_OPT_SELECT_NEXT, ptr::null())
        };
        match rc {
            ffi::GHOSTTY_SUCCESS => Ok(true),
            ffi::GHOSTTY_NO_VALUE => Ok(false),
            other => Err(VtError::Ffi(other)),
        }
    }

    /// Moves selection to the previous match (toward newer output). Returns `Ok(false)` if no matches.
    pub fn select_prev(&mut self) -> Result<bool, VtError> {
        let Some(arc) = self.terminal.upgrade() else {
            return Err(VtError::TerminalDropped);
        };
        let _guard = arc.lock();
        // SAFETY: `OPT_SELECT_PREV` requires NULL value and serialized terminal access.
        let rc = unsafe {
            ffi::ghostty_search_set(self.raw, ffi::GHOSTTY_SEARCH_OPT_SELECT_PREV, ptr::null())
        };
        match rc {
            ffi::GHOSTTY_SUCCESS => Ok(true),
            ffi::GHOSTTY_NO_VALUE => Ok(false),
            other => Err(VtError::Ffi(other)),
        }
    }

    pub fn status(&self) -> Result<SearchStatus, VtError> {
        let mut status: ffi::GhosttySearchStatus = ffi::GHOSTTY_SEARCH_STATUS_COMPLETE;
        // SAFETY: `ghostty_search_get` only touches search-owned memory.
        let rc = unsafe {
            ffi::ghostty_search_get(
                self.raw,
                ffi::GHOSTTY_SEARCH_DATA_STATUS,
                (&raw mut status).cast(),
            )
        };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        Ok(SearchStatus::from_ffi(status))
    }

    pub fn total_matches(&self) -> Result<usize, VtError> {
        let mut count: usize = 0;
        // SAFETY: `count` matches `size_t *` for `GHOSTTY_SEARCH_DATA_TOTAL_MATCHES`.
        let rc = unsafe {
            ffi::ghostty_search_get(
                self.raw,
                ffi::GHOSTTY_SEARCH_DATA_TOTAL_MATCHES,
                (&raw mut count).cast(),
            )
        };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }
        Ok(count)
    }

    /// 0-based index of the selected match (0 = newest), or `None` if nothing is selected.
    pub fn selected_index(&self) -> Result<Option<usize>, VtError> {
        let mut idx: usize = 0;
        // SAFETY: `idx` matches `size_t *`.
        let rc = unsafe {
            ffi::ghostty_search_get(
                self.raw,
                ffi::GHOSTTY_SEARCH_DATA_SELECTED_INDEX,
                (&raw mut idx).cast(),
            )
        };
        match rc {
            ffi::GHOSTTY_SUCCESS => Ok(Some(idx)),
            ffi::GHOSTTY_NO_VALUE => Ok(None),
            other => Err(VtError::Ffi(other)),
        }
    }

    /// Returns the currently selected match as `(start, end)` screen coordinates, or `None` if
    /// nothing is selected.
    pub fn selected_match_screen_range(
        &self,
        terminal: &Terminal,
    ) -> Result<Option<(ScreenPoint, ScreenPoint)>, VtError> {
        let guard = terminal.inner.lock();
        let mut sel = ffi::GhosttySelection::default();
        // SAFETY: `GHOSTTY_SEARCH_DATA_SELECTED_MATCH` writes `GhosttySelection`.
        let rc = unsafe {
            ffi::ghostty_search_get(
                self.raw,
                ffi::GHOSTTY_SEARCH_DATA_SELECTED_MATCH,
                (&raw mut sel).cast(),
            )
        };
        match rc {
            ffi::GHOSTTY_NO_VALUE => return Ok(None),
            ffi::GHOSTTY_SUCCESS => {}
            other => return Err(VtError::Ffi(other)),
        }
        let mut start_pt = ffi::GhosttyPointCoordinate::default();
        let mut end_pt = ffi::GhosttyPointCoordinate::default();
        // SAFETY: `sel.start`/`sel.end` are valid snapshots under `guard.raw`.
        let rc_s = unsafe {
            ffi::ghostty_terminal_point_from_grid_ref(
                guard.raw,
                &raw const sel.start,
                ffi::GHOSTTY_POINT_TAG_SCREEN,
                &raw mut start_pt,
            )
        };
        let rc_e = unsafe {
            ffi::ghostty_terminal_point_from_grid_ref(
                guard.raw,
                &raw const sel.end,
                ffi::GHOSTTY_POINT_TAG_SCREEN,
                &raw mut end_pt,
            )
        };
        if rc_s != ffi::GHOSTTY_SUCCESS || rc_e != ffi::GHOSTTY_SUCCESS {
            return Ok(None);
        }
        Ok(Some((
            ScreenPoint {
                col: start_pt.x,
                row: start_pt.y,
            },
            ScreenPoint {
                col: end_pt.x,
                row: end_pt.y,
            },
        )))
    }

    /// Returns all currently visible viewport search matches projected to `(col, row)` coordinates.
    pub fn viewport_matches(&mut self) -> Result<Vec<ViewportMatch>, VtError> {
        let Some(arc) = self.terminal.upgrade() else {
            return Ok(Vec::new());
        };
        let guard = arc.lock();

        let mut rows: u16 = 0;
        // SAFETY: `guard.raw` is locked.
        let _ = unsafe {
            ffi::ghostty_terminal_get(
                guard.raw,
                ffi::GHOSTTY_TERMINAL_DATA_ROWS,
                (&raw mut rows).cast(),
            )
        };

        let mut buf = ffi::GhosttySelectionBuffer {
            ptr: ptr::null_mut(),
            cap: 0,
            len: 0,
        };
        // SAFETY: NULL ptr with cap 0 queries required count in `buf.len`.
        let rc = unsafe {
            ffi::ghostty_search_get(
                self.raw,
                ffi::GHOSTTY_SEARCH_DATA_VIEWPORT_MATCHES,
                (&raw mut buf).cast(),
            )
        };
        if rc == ffi::GHOSTTY_NO_VALUE || (rc == ffi::GHOSTTY_SUCCESS && buf.len == 0) {
            return Ok(Vec::new());
        }
        if rc != ffi::GHOSTTY_SUCCESS && rc != ffi::GHOSTTY_OUT_OF_SPACE {
            return Err(VtError::Ffi(rc));
        }

        let needed = buf.len;
        self.scratch
            .resize(needed, ffi::GhosttySelection::default());
        let mut filled = ffi::GhosttySelectionBuffer {
            ptr: self.scratch.as_mut_ptr(),
            cap: self.scratch.len(),
            len: 0,
        };
        // SAFETY: `self.scratch` has capacity `needed`.
        let rc2 = unsafe {
            ffi::ghostty_search_get(
                self.raw,
                ffi::GHOSTTY_SEARCH_DATA_VIEWPORT_MATCHES,
                (&raw mut filled).cast(),
            )
        };
        if rc2 != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc2));
        }

        let mut out = Vec::with_capacity(filled.len);
        for sel in &self.scratch[..filled.len] {
            let mut start_pt = ffi::GhosttyPointCoordinate::default();
            let mut end_pt = ffi::GhosttyPointCoordinate::default();
            // SAFETY: `sel.start`/`sel.end` are valid snapshots under `guard`.
            let rc_s = unsafe {
                ffi::ghostty_terminal_point_from_grid_ref(
                    guard.raw,
                    &raw const sel.start,
                    ffi::GHOSTTY_POINT_TAG_VIEWPORT,
                    &raw mut start_pt,
                )
            };
            let rc_e = unsafe {
                ffi::ghostty_terminal_point_from_grid_ref(
                    guard.raw,
                    &raw const sel.end,
                    ffi::GHOSTTY_POINT_TAG_VIEWPORT,
                    &raw mut end_pt,
                )
            };
            if rc_s == ffi::GHOSTTY_SUCCESS
                && rc_e == ffi::GHOSTTY_SUCCESS
                && start_pt.y < u32::from(rows)
            {
                out.push(ViewportMatch {
                    start_col: start_pt.x,
                    start_row: start_pt.y,
                    end_col: end_pt.x,
                    end_row: end_pt.y,
                });
            }
        }
        Ok(out)
    }
}

impl Drop for TerminalSearch {
    fn drop(&mut self) {
        // SAFETY: Per `search.h:334-339`, serialize `ghostty_search_free` with the terminal
        // if still alive; if the terminal was already freed, freeing search memory is still safe.
        if let Some(arc) = self.terminal.upgrade() {
            let _guard = arc.lock();
            unsafe { ffi::ghostty_search_free(self.raw) };
        } else {
            unsafe { ffi::ghostty_search_free(self.raw) };
        }
    }
}
