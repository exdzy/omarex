//! Safe wrappers for `GhosttySelectionGesture`, plain-text selection extraction,
//! OSC 8 hyperlink lookup, and incremental terminal scrollback search (`search.h`).

use std::ptr;
use std::sync::Weak;

use parking_lot::Mutex;

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
