//! Native async D-Bus `org.freedesktop.Notifications` client (`zbus`) with
//! interactive `"Open"` click-to-focus routing, text sanitization, mute filtering,
//! and per-block rate limiting. Zero `notify-send` subprocess forks.

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use zbus::export::futures_core::Stream as _;
use zbus::wire::Variant;
use zbus::{Connection, MatchRule, MessageStream};

use crate::ThemeError;

const NOTIFY_DEST: &str = "org.freedesktop.Notifications";
const NOTIFY_PATH: &str = "/org/freedesktop/Notifications";
const NOTIFY_IFACE: &str = "org.freedesktop.Notifications";

/// Default per-block desktop notification cooldown (matches `docs/gui-spec.md §2.4`).
pub const DEFAULT_RATE_LIMIT: Duration = Duration::from_secs(3);

/// Identifies the exact Rex target to raise/focus when the user clicks a notification.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NotificationTarget {
    pub host: String,
    pub session_id: String,
    pub block_id: String,
}

/// Freedesktop notification urgency level (`urgency` hint byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum NotificationUrgency {
    Low = 0,
    #[default]
    Normal = 1,
    Critical = 2,
}

/// Request to display a desktop notification for a Rex block/session.
#[derive(Debug, Clone)]
pub struct NotificationRequest {
    pub target: NotificationTarget,
    pub title: String,
    pub body: String,
    pub urgency: NotificationUrgency,
    pub app_icon: Option<String>,
    /// Expiration timeout in milliseconds (`-1` = server default, `0` = never expire).
    pub expire_timeout_ms: i32,
}

impl NotificationRequest {
    pub fn new(
        target: NotificationTarget,
        title: impl Into<String>,
        body: impl Into<String>,
        urgency: NotificationUrgency,
    ) -> Self {
        Self {
            target,
            title: title.into(),
            body: body.into(),
            urgency,
            app_icon: Some("omarex".to_owned()),
            expire_timeout_ms: -1,
        }
    }
}

/// Sanitizes untrusted OSC 7501 / OSC 9 / OSC 777 strings before displaying them in
/// desktop notifications or GUI chrome (`docs/gui-spec.md §2.4`):
/// - Strips ASCII control characters (keeping space and converting `\n`/`\r`/`\t` to space)
/// - Strips Unicode BiDi override/isolate controls (`U+202A..=U+202E`, `U+2066..=U+2069`)
pub fn sanitize_notification_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        let code = ch as u32;
        if (0x202A..=0x202E).contains(&code) || (0x2066..=0x2069).contains(&code) {
            continue;
        }
        if ch == '\n' || ch == '\r' || ch == '\t' {
            out.push(' ');
            continue;
        }
        if ch.is_control() {
            continue;
        }
        out.push(ch);
    }
    out.trim().to_owned()
}

/// Mute and per-block rate-limit filter for desktop notifications.
#[derive(Debug)]
pub struct MuteFilter {
    enabled: bool,
    muted_sessions: HashSet<String>,
    muted_blocks: HashSet<String>,
    rate_limit: Duration,
    last_sent: HashMap<String, Instant>,
}

impl Default for MuteFilter {
    fn default() -> Self {
        Self::new(DEFAULT_RATE_LIMIT)
    }
}

impl MuteFilter {
    pub fn new(rate_limit: Duration) -> Self {
        Self {
            enabled: true,
            muted_sessions: HashSet::new(),
            muted_blocks: HashSet::new(),
            rate_limit,
            last_sent: HashMap::new(),
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn mute_session(&mut self, session_id: impl Into<String>) {
        self.muted_sessions.insert(session_id.into());
    }

    pub fn unmute_session(&mut self, session_id: &str) {
        self.muted_sessions.remove(session_id);
    }

    pub fn toggle_session(&mut self, session_id: impl Into<String>) -> bool {
        let id = session_id.into();
        if self.muted_sessions.remove(&id) {
            false
        } else {
            self.muted_sessions.insert(id);
            true
        }
    }

    pub fn mute_block(&mut self, block_id: impl Into<String>) {
        self.muted_blocks.insert(block_id.into());
    }

    pub fn unmute_block(&mut self, block_id: &str) {
        self.muted_blocks.remove(block_id);
    }

    pub fn toggle_block(&mut self, block_id: impl Into<String>) -> bool {
        let id = block_id.into();
        if self.muted_blocks.remove(&id) {
            false
        } else {
            self.muted_blocks.insert(id);
            true
        }
    }

    pub fn is_muted(&self, session_id: &str, block_id: &str) -> bool {
        !self.enabled
            || self.muted_sessions.contains(session_id)
            || self.muted_blocks.contains(block_id)
    }

    /// Checks mute status and per-block cooldown. Records `now` when returning `true`.
    pub fn allow_and_record(&mut self, target: &NotificationTarget, now: Instant) -> bool {
        if self.is_muted(&target.session_id, &target.block_id) {
            return false;
        }
        let key = format!("{}|{}", target.host, target.block_id);
        if let Some(prev) = self.last_sent.get(&key)
            && now.duration_since(*prev) < self.rate_limit
        {
            return false;
        }
        self.last_sent.insert(key, now);
        true
    }
}

/// Async D-Bus desktop notification sender with interactive `"Open"` action callback channel.
pub struct DesktopNotifier {
    conn: Connection,
    app_name: String,
    filter: Arc<Mutex<MuteFilter>>,
    pending: Arc<Mutex<HashMap<u32, NotificationTarget>>>,
    listener: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for DesktopNotifier {
    fn drop(&mut self) {
        if let Some(task) = self.listener.take() {
            task.abort();
        }
    }
}

impl DesktopNotifier {
    /// Connects to the user session D-Bus and starts listening for `ActionInvoked` and
    /// `NotificationClosed` signals from `org.freedesktop.Notifications`.
    ///
    /// Returns the notifier and a receiver yielding [`NotificationTarget`] whenever the
    /// user clicks a notification or its `"Open"` action button.
    pub async fn connect(
        app_name: impl Into<String>,
        filter: Arc<Mutex<MuteFilter>>,
    ) -> Result<(Self, mpsc::Receiver<NotificationTarget>), ThemeError> {
        let conn = Connection::session()
            .await
            .map_err(|e| ThemeError::Dbus(e.to_string()))?;

        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(NOTIFY_IFACE)
            .path(NOTIFY_PATH)
            .build()
            .map_err(|e| ThemeError::Dbus(e.to_string()))?;

        let mut stream = MessageStream::for_match_rule(rule, &conn, None)
            .await
            .map_err(|e| ThemeError::Dbus(e.to_string()))?;

        let pending: Arc<Mutex<HashMap<u32, NotificationTarget>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_bg = Arc::clone(&pending);
        let (action_tx, action_rx) = mpsc::channel(32);

        let listener = tokio::spawn(async move {
            while let Some(item) =
                std::future::poll_fn(|cx| Pin::new(&mut stream).poll_next(cx)).await
            {
                let Ok(msg) = item else {
                    continue;
                };
                let member = msg.header().member().map(|m| m.as_str().to_owned());
                match member.as_deref() {
                    Some("ActionInvoked") => {
                        if let Ok((id, action_key)) = msg.body().deserialize::<(u32, String)>()
                            && (action_key == "default" || action_key == "open")
                        {
                            let target = pending_bg.lock().ok().and_then(|mut map| map.remove(&id));
                            if let Some(target) = target
                                && action_tx.send(target).await.is_err()
                            {
                                break;
                            }
                        }
                    }
                    Some("NotificationClosed") => {
                        if let Ok((id, _reason)) = msg.body().deserialize::<(u32, u32)>()
                            && let Ok(mut map) = pending_bg.lock()
                        {
                            map.remove(&id);
                        }
                    }
                    _ => {}
                }
            }
        });

        Ok((
            Self {
                conn,
                app_name: app_name.into(),
                filter,
                pending,
                listener: Some(listener),
            },
            action_rx,
        ))
    }

    /// Accesses the shared [`MuteFilter`].
    pub fn filter(&self) -> &Arc<Mutex<MuteFilter>> {
        &self.filter
    }

    /// Sends a desktop notification if permitted by the mute filter and rate limiter.
    ///
    /// Returns `Ok(None)` when suppressed by mute/rate-limit, or `Ok(Some(notification_id))`
    /// when dispatched to the desktop notification daemon.
    pub async fn send(&self, req: NotificationRequest) -> Result<Option<u32>, ThemeError> {
        let allowed = self
            .filter
            .lock()
            .map(|mut f| f.allow_and_record(&req.target, Instant::now()))
            .unwrap_or(false);
        if !allowed {
            return Ok(None);
        }

        let summary = sanitize_notification_text(&req.title);
        let body = sanitize_notification_text(&req.body);
        let icon = req.app_icon.as_deref().unwrap_or("omarex");

        // Freedesktop spec: alternating [action_key, localized_label].
        // Including both "default" (click on notification body) and "open" (explicit button)
        // works smoothly across mako, dunst, swaync, and Omarchy's notification daemon.
        let actions: Vec<&str> = vec!["default", "Open", "open", "Open"];

        let mut hints: HashMap<&str, Variant<'_>> = HashMap::with_capacity(2);
        hints.insert("urgency", Variant::U8(req.urgency as u8));
        hints.insert("desktop-entry", Variant::from("omarex"));

        let reply = self
            .conn
            .call_method(
                Some(NOTIFY_DEST),
                NOTIFY_PATH,
                Some(NOTIFY_IFACE),
                "Notify",
                &(
                    self.app_name.as_str(),
                    0u32, // replaces_id
                    icon,
                    summary.as_str(),
                    body.as_str(),
                    actions,
                    hints,
                    req.expire_timeout_ms,
                ),
            )
            .await
            .map_err(|e| ThemeError::Dbus(e.to_string()))?;

        let id: u32 = reply
            .body()
            .deserialize()
            .map_err(|e| ThemeError::Dbus(e.to_string()))?;

        if let Ok(mut map) = self.pending.lock() {
            map.insert(id, req.target);
        }

        Ok(Some(id))
    }

    /// Dismisses a previously sent notification by ID.
    pub async fn close(&self, id: u32) -> Result<(), ThemeError> {
        if let Ok(mut map) = self.pending.lock() {
            map.remove(&id);
        }
        self.conn
            .call_method(
                Some(NOTIFY_DEST),
                NOTIFY_PATH,
                Some(NOTIFY_IFACE),
                "CloseNotification",
                &(id,),
            )
            .await
            .map_err(|e| ThemeError::Dbus(e.to_string()))?;
        Ok(())
    }
}
