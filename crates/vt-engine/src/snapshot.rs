//! Streaming progressive `GHOSTSNP` v1 snapshot decoder (`snapshot.h`).
//!
//! Decodes incoming `/api/data` chunks incrementally:
//! 1. Validates the 10-byte `"GHOSTSNP"` v1 envelope and frames CRC-protected records in `O(1)`.
//! 2. Returns `SnapshotFeedStatus::Ready(Terminal)` immediately when the `READY` record
//!    arrives (<5ms first paint!), with `COMPRESS_HISTORY = true` and `KITTY_IMAGE_STORAGE_LIMIT`
//!    configured before decode.
//! 3. Loads older scrollback pages and Kitty image pixels in bounded time slices via
//!    [`SnapshotDecoder::step_background`] while the UI thread renders concurrently.
//! 4. Preserves all trailing post-`FINISH` bytes (Rex `rex1;` APCs + live VT stream) verbatim.

use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::{Terminal, TerminalConfig, TerminalInner, VtError, ffi, init_sys};

const GHOSTSNP_MAGIC: &[u8; 8] = b"GHOSTSNP";
const ENVELOPE_LEN: usize = 10;
const RECORD_HEADER_LEN: usize = 10;

const RECORD_TAG_HISTORY: u16 = 4;
const RECORD_TAG_READY: u16 = 5;
const RECORD_TAG_FINISH: u16 = 6;

/// Result returned by [`SnapshotDecoder::feed`].
pub enum SnapshotFeedStatus {
    /// Still accumulating the prefix before the `READY` marker.
    NeedMoreData,
    /// `READY` record decoded! The returned [`Terminal`] is immediately renderable.
    /// Continue feeding remaining snapshot bytes and calling [`SnapshotDecoder::step_background`].
    Ready(Terminal),
    /// Decoder was already past `READY`; newly fed bytes are queued for `step_background()`
    /// (or trailing live VT bytes if `FINISH` was already reached).
    Buffered,
}

/// Result returned by [`SnapshotDecoder::step_background`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundStep {
    /// Waiting on more network bytes from `/api/data` before the next complete record can be read.
    NeedMoreData,
    /// Decoded one or more history pages / Kitty image payloads within the time budget.
    Progress {
        pages_decoded: usize,
        rows_prepended: usize,
        images_decoded: usize,
        image_bytes: usize,
        more_buffered: bool,
    },
    /// Reached `FINISH`: snapshot decode is 100% complete!
    ///
    /// `consumed_bytes` is the exact `GHOSTSNP` byte count (`SOURCE_OFFSET`).
    /// `trailing_bytes` holds any bytes already fed after `FINISH` (e.g. Rex `rex1;` APCs)
    /// that must be fed immediately to `terminal.write(&trailing_bytes)`.
    Finished {
        consumed_bytes: usize,
        trailing_bytes: Vec<u8>,
        pages_decoded: usize,
        rows_prepended: usize,
        images_decoded: usize,
        image_bytes: usize,
    },
}

/// Pinned byte buffer backing the `GhosttyReader` callback.
///
/// The `GhosttyReader` only ever sees bytes when our framing scanner has already verified
/// a complete record (or `READY`/`FINISH`), so `read_cb` never hits premature starvation.
struct StreamCursor {
    buf: Vec<u8>,
    read_pos: usize,
    /// Hard read limit for the active FFI call (`ready()` stops at `ready_end`, `next()` at `buf.len()`).
    read_limit: usize,
}

unsafe extern "C" fn cursor_read_cb(
    userdata: *mut c_void,
    buffer: *mut u8,
    capacity: usize,
    out_read: *mut usize,
) -> bool {
    if userdata.is_null() || buffer.is_null() || out_read.is_null() {
        return false;
    }
    // SAFETY: `userdata` is a pinned `Box<StreamCursor>` owned by `SnapshotDecoder` and
    // outlives the C `GhosttySnapshotDecoder` handle.
    let cursor = unsafe { &mut *userdata.cast::<StreamCursor>() };
    let available = cursor.read_limit.saturating_sub(cursor.read_pos);
    let to_copy = available.min(capacity);
    if to_copy > 0 {
        // SAFETY: `cursor.buf` has at least `cursor.read_limit` bytes; `buffer` has `capacity`.
        unsafe {
            ptr::copy_nonoverlapping(cursor.buf.as_ptr().add(cursor.read_pos), buffer, to_copy);
        }
        cursor.read_pos += to_copy;
    }
    // SAFETY: `out_read` is verified non-null above.
    unsafe {
        *out_read = to_copy;
    }
    true
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum DecoderPhase {
    BeforeReady,
    BackgroundHistory,
    Finished,
    Poisoned,
}

/// Streaming progressive `GHOSTSNP` decoder with `COMPRESS_HISTORY = true`.
pub struct SnapshotDecoder {
    raw: ffi::GhosttySnapshotDecoder,
    /// Pinned heap buffer referenced by `GhosttyReader::userdata`. Must be dropped AFTER `raw`.
    cursor: Box<StreamCursor>,
    /// Shared reference to the restored terminal so `step_background` locks safely and
    /// guarantees the terminal stays alive until `raw` is freed.
    terminal_inner: Option<Arc<Mutex<TerminalInner>>>,
    config: TerminalConfig,
    phase: DecoderPhase,
    envelope_checked: bool,
    /// Byte offset up to which complete records have been framed in `cursor.buf`.
    scan_pos: usize,
    /// End byte offset of the `READY` record once seen.
    ready_end: Option<usize>,
    /// End byte offset of the `FINISH` record once seen.
    finish_end: Option<usize>,
    /// Total snapshot bytes consumed once `FINISH` validates.
    consumed_bytes: usize,
    /// Trailing bytes fed after `FINISH` was already drained.
    trailing_overflow: Vec<u8>,
}

// SAFETY: `SnapshotDecoder` owns `raw` and `cursor` exclusively and locks `terminal_inner`
// around every call that mutates the terminal.
unsafe impl Send for SnapshotDecoder {}

impl SnapshotDecoder {
    /// Creates a new streaming `GHOSTSNP` decoder configured with `COMPRESS_HISTORY = true`
    /// and `config.kitty_image_storage_limit`.
    pub fn new(config: TerminalConfig) -> Result<Self, VtError> {
        init_sys();

        let mut cursor = Box::new(StreamCursor {
            buf: Vec::with_capacity(64 * 1024),
            read_pos: 0,
            read_limit: 0,
        });

        let reader = ffi::GhosttyReader {
            read: cursor_read_cb,
            userdata: (&raw mut *cursor).cast::<c_void>(),
        };

        let mut raw: ffi::GhosttySnapshotDecoder = ptr::null_mut();
        // SAFETY: `cursor` is heap-boxed so its address remains stable until after `raw` is freed.
        let rc = unsafe { ffi::ghostty_snapshot_decoder_new(ptr::null(), &raw mut raw, reader) };
        if rc != ffi::GHOSTTY_SUCCESS {
            return Err(VtError::Ffi(rc));
        }

        let compress_history = config.compress_history;
        let kitty_limit = config.kitty_image_storage_limit;
        // SAFETY: Options are set before decoding starts (`snapshot.h:147-149`).
        let rc_comp = unsafe {
            ffi::ghostty_snapshot_decoder_set(
                raw,
                ffi::GHOSTTY_SNAPSHOT_DECODER_OPT_COMPRESS_HISTORY,
                (&raw const compress_history).cast(),
            )
        };
        let rc_kitty = unsafe {
            ffi::ghostty_snapshot_decoder_set(
                raw,
                ffi::GHOSTTY_SNAPSHOT_DECODER_OPT_KITTY_IMAGE_STORAGE_LIMIT,
                (&raw const kitty_limit).cast(),
            )
        };
        if rc_comp != ffi::GHOSTTY_SUCCESS || rc_kitty != ffi::GHOSTTY_SUCCESS {
            // SAFETY: `raw` was created above and has not been freed.
            unsafe { ffi::ghostty_snapshot_decoder_free(raw) };
            return Err(VtError::Ffi(if rc_comp != ffi::GHOSTTY_SUCCESS {
                rc_comp
            } else {
                rc_kitty
            }));
        }

        Ok(Self {
            raw,
            cursor,
            terminal_inner: None,
            config,
            phase: DecoderPhase::BeforeReady,
            envelope_checked: false,
            scan_pos: 0,
            ready_end: None,
            finish_end: None,
            consumed_bytes: 0,
            trailing_overflow: Vec::new(),
        })
    }

    /// Feeds the next chunk of raw bytes from `/api/data` into the streaming decoder.
    ///
    /// As soon as the `READY` record arrives, this decodes the renderable active state
    /// immediately and returns `SnapshotFeedStatus::Ready(terminal)`.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<SnapshotFeedStatus, VtError> {
        if self.phase == DecoderPhase::Poisoned {
            return Err(VtError::InvalidSnapshot(
                "decoder is poisoned after prior error",
            ));
        }

        if !chunk.is_empty() {
            if self.phase == DecoderPhase::Finished {
                self.trailing_overflow.extend_from_slice(chunk);
                return Ok(SnapshotFeedStatus::Buffered);
            }
            self.cursor.buf.extend_from_slice(chunk);
            self.advance_record_scanner()?;
        }

        if self.phase == DecoderPhase::BeforeReady {
            if let Some(ready_end) = self.ready_end {
                self.cursor.read_limit = ready_end;
                let mut raw_term: ffi::GhosttyTerminal = ptr::null_mut();
                // SAFETY: `ready_end` guarantees the complete prefix through `READY` is buffered.
                let rc =
                    unsafe { ffi::ghostty_snapshot_decoder_ready(self.raw, &raw mut raw_term) };
                if rc != ffi::GHOSTTY_SUCCESS {
                    self.phase = DecoderPhase::Poisoned;
                    return Err(VtError::Ffi(rc));
                }
                let terminal = Terminal::from_raw(raw_term, self.config.clone())?;
                self.terminal_inner = Some(Arc::clone(&terminal.inner));
                self.phase = DecoderPhase::BackgroundHistory;
                return Ok(SnapshotFeedStatus::Ready(terminal));
            }
            return Ok(SnapshotFeedStatus::NeedMoreData);
        }

        Ok(SnapshotFeedStatus::Buffered)
    }

    /// Progressively decodes buffered history pages and Kitty image payloads for up to `budget`.
    ///
    /// Each individual record locks the restored [`Terminal`] briefly and releases it between
    /// records so the UI thread stays responsive at 120–300 Hz.
    pub fn step_background(&mut self, budget: Duration) -> Result<BackgroundStep, VtError> {
        match self.phase {
            DecoderPhase::BeforeReady => return Ok(BackgroundStep::NeedMoreData),
            DecoderPhase::Poisoned => {
                return Err(VtError::InvalidSnapshot(
                    "decoder is poisoned after prior error",
                ));
            }
            DecoderPhase::Finished => {
                let trailing = self.take_trailing_bytes();
                return Ok(BackgroundStep::Finished {
                    consumed_bytes: self.consumed_bytes,
                    trailing_bytes: trailing,
                    pages_decoded: 0,
                    rows_prepended: 0,
                    images_decoded: 0,
                    image_bytes: 0,
                });
            }
            DecoderPhase::BackgroundHistory => {}
        }

        let Some(term_arc) = self.terminal_inner.as_ref().map(Arc::clone) else {
            return Err(VtError::InvalidSnapshot("missing restored terminal"));
        };

        let deadline = Instant::now() + budget;
        let mut pages_decoded = 0usize;
        let mut rows_prepended = 0usize;
        let mut images_decoded = 0usize;
        let mut image_bytes = 0usize;

        loop {
            if !self.has_safe_next_step() {
                if pages_decoded > 0 || images_decoded > 0 {
                    return Ok(BackgroundStep::Progress {
                        pages_decoded,
                        rows_prepended,
                        images_decoded,
                        image_bytes,
                        more_buffered: false,
                    });
                }
                return Ok(BackgroundStep::NeedMoreData);
            }

            // Allow the reader to read up to the last fully framed record (or FINISH).
            self.cursor.read_limit = self.finish_end.unwrap_or(self.scan_pos);

            let rc = {
                // Hold terminal lock strictly for one `next()` step (one page or one image).
                let _guard = term_arc.lock();
                // SAFETY: `self.raw` is in post-READY state and `_guard` serializes terminal mutation.
                unsafe { ffi::ghostty_snapshot_decoder_next(self.raw) }
            };

            match rc {
                ffi::GHOSTTY_SUCCESS => {
                    let mut kind: ffi::GhosttySnapshotDecoderProgressKind =
                        ffi::GHOSTTY_SNAPSHOT_DECODER_PROGRESS_KIND_PAGE;
                    // SAFETY: Progress metadata is valid right after `next() == SUCCESS`.
                    let _ = unsafe {
                        ffi::ghostty_snapshot_decoder_get(
                            self.raw,
                            ffi::GHOSTTY_SNAPSHOT_DECODER_DATA_PROGRESS_KIND,
                            (&raw mut kind).cast(),
                        )
                    };
                    if kind == ffi::GHOSTTY_SNAPSHOT_DECODER_PROGRESS_KIND_IMAGE {
                        let mut bytes: usize = 0;
                        let _ = unsafe {
                            ffi::ghostty_snapshot_decoder_get(
                                self.raw,
                                ffi::GHOSTTY_SNAPSHOT_DECODER_DATA_PROGRESS_IMAGE_BYTES,
                                (&raw mut bytes).cast(),
                            )
                        };
                        images_decoded += 1;
                        image_bytes += bytes;
                    } else {
                        let mut rows: usize = 0;
                        let _ = unsafe {
                            ffi::ghostty_snapshot_decoder_get(
                                self.raw,
                                ffi::GHOSTTY_SNAPSHOT_DECODER_DATA_PROGRESS_ROWS,
                                (&raw mut rows).cast(),
                            )
                        };
                        pages_decoded += 1;
                        rows_prepended += rows;
                    }

                    if Instant::now() >= deadline {
                        let more_buffered = self.has_safe_next_step();
                        return Ok(BackgroundStep::Progress {
                            pages_decoded,
                            rows_prepended,
                            images_decoded,
                            image_bytes,
                            more_buffered,
                        });
                    }
                }
                ffi::GHOSTTY_NO_VALUE => {
                    let mut offset: usize = self.cursor.read_pos;
                    // SAFETY: `SOURCE_OFFSET` is exact at FINISH (`snapshot.h:285-288`).
                    let _ = unsafe {
                        ffi::ghostty_snapshot_decoder_get(
                            self.raw,
                            ffi::GHOSTTY_SNAPSHOT_DECODER_DATA_SOURCE_OFFSET,
                            (&raw mut offset).cast(),
                        )
                    };
                    self.consumed_bytes = offset;
                    self.phase = DecoderPhase::Finished;
                    let trailing_bytes = self.take_trailing_bytes();
                    return Ok(BackgroundStep::Finished {
                        consumed_bytes: offset,
                        trailing_bytes,
                        pages_decoded,
                        rows_prepended,
                        images_decoded,
                        image_bytes,
                    });
                }
                other => {
                    self.phase = DecoderPhase::Poisoned;
                    return Err(VtError::Ffi(other));
                }
            }
        }
    }

    /// Convenience one-shot decode of an in-memory `GHOSTSNP` slice (with optional trailing bytes).
    ///
    /// Returns `(Terminal, consumed_snapshot_bytes)`.
    pub fn decode_slice(data: &[u8], config: TerminalConfig) -> Result<(Terminal, usize), VtError> {
        let mut decoder = Self::new(config)?;
        let term = match decoder.feed(data)? {
            SnapshotFeedStatus::Ready(t) => t,
            SnapshotFeedStatus::NeedMoreData => {
                return Err(VtError::InvalidSnapshot("truncated GHOSTSNP before READY"));
            }
            SnapshotFeedStatus::Buffered => {
                return Err(VtError::InvalidSnapshot("unexpected decoder phase"));
            }
        };
        loop {
            match decoder.step_background(Duration::from_secs(10))? {
                BackgroundStep::Finished { consumed_bytes, .. } => {
                    return Ok((term, consumed_bytes));
                }
                BackgroundStep::Progress { .. } => continue,
                BackgroundStep::NeedMoreData => {
                    return Err(VtError::InvalidSnapshot("truncated GHOSTSNP before FINISH"));
                }
            }
        }
    }

    /// Advisory primary screen history row count (available once `READY` has decoded).
    pub fn history_rows_primary(&self) -> Option<u64> {
        if matches!(
            self.phase,
            DecoderPhase::BeforeReady | DecoderPhase::Poisoned
        ) {
            return None;
        }
        let mut rows: u64 = 0;
        // SAFETY: Valid post-READY data query.
        let rc = unsafe {
            ffi::ghostty_snapshot_decoder_get(
                self.raw,
                ffi::GHOSTTY_SNAPSHOT_DECODER_DATA_HISTORY_ROWS_PRIMARY,
                (&raw mut rows).cast(),
            )
        };
        (rc == ffi::GHOSTTY_SUCCESS).then_some(rows)
    }

    /// Pending Kitty graphics pixel bytes reported at `READY` for the primary screen.
    pub fn kitty_pending_bytes_primary(&self) -> Option<u64> {
        if matches!(
            self.phase,
            DecoderPhase::BeforeReady | DecoderPhase::Poisoned
        ) {
            return None;
        }
        let mut bytes: u64 = 0;
        // SAFETY: Valid post-READY data query.
        let rc = unsafe {
            ffi::ghostty_snapshot_decoder_get(
                self.raw,
                ffi::GHOSTTY_SNAPSHOT_DECODER_DATA_KITTY_PENDING_BYTES_PRIMARY,
                (&raw mut bytes).cast(),
            )
        };
        (rc == ffi::GHOSTTY_SUCCESS).then_some(bytes)
    }

    /// Incremental O(1)-per-record framing scanner over `self.cursor.buf`.
    fn advance_record_scanner(&mut self) -> Result<(), VtError> {
        if self.finish_end.is_some() {
            return Ok(());
        }

        if !self.envelope_checked {
            if self.cursor.buf.len() < ENVELOPE_LEN {
                return Ok(());
            }
            if &self.cursor.buf[..8] != GHOSTSNP_MAGIC {
                self.phase = DecoderPhase::Poisoned;
                return Err(VtError::InvalidSnapshot("bad GHOSTSNP magic"));
            }
            self.envelope_checked = true;
            self.scan_pos = ENVELOPE_LEN;
        }

        while self.cursor.buf.len().saturating_sub(self.scan_pos) >= RECORD_HEADER_LEN {
            let hdr = &self.cursor.buf[self.scan_pos..self.scan_pos + RECORD_HEADER_LEN];
            let tag = u16::from_le_bytes([hdr[0], hdr[1]]);
            let payload_len = u32::from_le_bytes([hdr[2], hdr[3], hdr[4], hdr[5]]) as usize;
            let Some(rec_end) = self
                .scan_pos
                .checked_add(RECORD_HEADER_LEN)
                .and_then(|v| v.checked_add(payload_len))
            else {
                self.phase = DecoderPhase::Poisoned;
                return Err(VtError::InvalidSnapshot("record length overflow"));
            };
            if self.cursor.buf.len() < rec_end {
                break;
            }
            self.scan_pos = rec_end;
            if tag == RECORD_TAG_READY && self.ready_end.is_none() {
                self.ready_end = Some(rec_end);
            } else if tag == RECORD_TAG_FINISH {
                self.finish_end = Some(rec_end);
                break;
            }
        }

        Ok(())
    }

    /// Returns `true` when enough complete records are buffered after `cursor.read_pos` that
    /// one call to `ghostty_snapshot_decoder_next()` is guaranteed not to hit premature EOF.
    fn has_safe_next_step(&self) -> bool {
        if self.finish_end.is_some() {
            return true;
        }
        // Walk complete records starting at `cursor.read_pos`:
        // - Skip any leading HISTORY manifest (tag 4), since `next()` consumes HISTORY + its
        //   first PAGE (or next record) together.
        // - Require at least one complete data record PLUS one following complete record (in case
        //   a PAGE is immediately followed by an optional KITTY_IMAGE_PAGE_PLACEMENTS record).
        let mut pos = self.cursor.read_pos;
        let mut non_manifest_records = 0usize;
        while pos + RECORD_HEADER_LEN <= self.scan_pos {
            let hdr = &self.cursor.buf[pos..pos + RECORD_HEADER_LEN];
            let tag = u16::from_le_bytes([hdr[0], hdr[1]]);
            let payload_len = u32::from_le_bytes([hdr[2], hdr[3], hdr[4], hdr[5]]) as usize;
            let next_pos = pos + RECORD_HEADER_LEN + payload_len;
            if next_pos > self.scan_pos {
                break;
            }
            pos = next_pos;
            if tag == RECORD_TAG_FINISH {
                return true;
            }
            if tag != RECORD_TAG_HISTORY {
                non_manifest_records += 1;
                if non_manifest_records >= 2 {
                    return true;
                }
            }
        }
        false
    }

    /// Returns `true` once `FINISH` has been validated.
    pub fn is_finished(&self) -> bool {
        self.phase == DecoderPhase::Finished
    }

    /// Drains any trailing post-`FINISH` bytes accumulated in the decoder.
    pub fn drain_trailing_bytes(&mut self) -> Vec<u8> {
        if self.phase != DecoderPhase::Finished {
            return Vec::new();
        }
        let mut out = if self.consumed_bytes < self.cursor.buf.len() {
            self.cursor.buf[self.consumed_bytes..].to_vec()
        } else {
            Vec::new()
        };
        self.cursor.buf.clear();
        self.cursor.read_pos = 0;
        self.cursor.read_limit = 0;
        if !self.trailing_overflow.is_empty() {
            out.extend_from_slice(&self.trailing_overflow);
            self.trailing_overflow.clear();
        }
        out
    }

    fn take_trailing_bytes(&mut self) -> Vec<u8> {
        self.drain_trailing_bytes()
    }
}

impl Drop for SnapshotDecoder {
    fn drop(&mut self) {
        // SAFETY: Free `GhosttySnapshotDecoder` FIRST, while both `self.cursor` and
        // `self.terminal_inner` are still alive (`snapshot.h:616-620`).
        unsafe { ffi::ghostty_snapshot_decoder_free(self.raw) };
    }
}
