use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, OnceLock,
};

use crossbeam_channel::{Sender, TrySendError};
use tracing::{Event, Subscriber};
use tracing_subscriber::{layer::Context, Layer};

use crate::core::channel::ChannelId;

/// A log event forwarded to the GUI Log pane.
#[derive(Debug, Clone)]
pub struct LogEvent {
    pub level: tracing::Level,
    pub target: String,
    pub message: String,
    pub timestamp: chrono::DateTime<chrono::Local>,
    /// The **stable id** of the channel this event is about (ADR-020), when
    /// the emitter attributed it via a structured `channel = <id>` tracing
    /// field (the runner, the supervisor, and the GUI lifecycle logs do).
    /// Drives the per-channel info/warn/error counts on the channel-list
    /// rows — keyed by id, so a runner below a removed channel keeps
    /// counting into its own row. `None` for app-level events.
    pub channel: Option<ChannelId>,
}

/// A [`Layer`] that forwards tracing events to a GUI thread via a channel.
///
/// Construct with [`GuiLogLayer::new`] and install alongside the other layers
/// in [`super::init`]. The matching [`Receiver`][crossbeam_channel::Receiver]
/// is read by the GUI Log pane to display live log output.
///
/// Sends are best-effort (`try_send`): a full channel increments the matching
/// [`GuiLogHealth`] counter rather than blocking the calling thread. A dropped
/// receiver is normal during shutdown and is ignored.
pub struct GuiLogLayer {
    sender: Sender<LogEvent>,
    health: GuiLogHealth,
}

impl GuiLogLayer {
    pub fn new(sender: Sender<LogEvent>) -> Self {
        Self::with_health(sender, GuiLogHealth::new())
    }

    pub(super) fn with_health(sender: Sender<LogEvent>, health: GuiLogHealth) -> Self {
        Self { sender, health }
    }

    fn forward(&self, event: LogEvent) {
        match self.sender.try_send(event) {
            Ok(()) => self.health.notify(),
            Err(TrySendError::Full(_)) => {
                self.health.dropped_events.fetch_add(1, Ordering::Relaxed);
                self.health.notify();
            }
            Err(TrySendError::Disconnected(_)) => {
                // The GUI receiver is dropped during normal shutdown. There
                // is nobody left to show either the event or a loss notice.
            }
        }
    }
}

type Notify = Arc<dyn Fn() + Send + Sync>;

/// Session-wide health of the bounded GUI-pane log transport.
///
/// This is separate from file-log health: pane loss does not imply file loss,
/// and a full file queue does not imply pane loss.
#[derive(Clone)]
pub struct GuiLogHealth {
    dropped_events: Arc<AtomicU64>,
    notify: Arc<OnceLock<Notify>>,
}

impl GuiLogHealth {
    pub(super) fn new() -> Self {
        Self {
            dropped_events: Arc::new(AtomicU64::new(0)),
            notify: Arc::new(OnceLock::new()),
        }
    }

    /// Number of entries not delivered to the GUI pane because its queue was
    /// full. The count is cumulative for this application session.
    pub fn dropped_events(&self) -> u64 {
        self.dropped_events.load(Ordering::Relaxed)
    }

    /// Install the lightweight callback used to wake the GUI after an accepted
    /// or queue-full event.
    ///
    /// Installation may happen after this health handle is constructed. The
    /// first callback wins; a duplicate installation is ignored so active log
    /// producers never see their wake target replaced.
    pub fn set_notify(&self, notify: Notify) {
        let _ = self.notify.set(notify);
    }

    fn notify(&self) {
        if let Some(callback) = self.notify.get() {
            callback();
        }
    }
}

impl<S: Subscriber> Layer<S> for GuiLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);

        let log_event = LogEvent {
            level: *meta.level(),
            target: meta.target().to_string(),
            message: visitor.message,
            timestamp: chrono::Local::now(),
            channel: visitor.channel,
        };

        self.forward(log_event);
    }
}

#[derive(Default)]
struct EventVisitor {
    message: String,
    /// Value of a structured `channel` field, when present (a stable
    /// [`ChannelId`], carried as its raw u64 — ADR-020).
    channel: Option<ChannelId>,
}

impl tracing::field::Visit for EventVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        }
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if field.name() == "channel" {
            self.channel = Some(ChannelId::from_raw(value));
        }
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        if field.name() == "channel" {
            self.channel = u64::try_from(value).ok().map(ChannelId::from_raw);
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use crossbeam_channel::{bounded, unbounded};

    use super::*;

    fn make_layer() -> (GuiLogLayer, crossbeam_channel::Receiver<LogEvent>) {
        let (tx, rx) = unbounded();
        (GuiLogLayer::new(tx), rx)
    }

    fn event(message: &str) -> LogEvent {
        LogEvent {
            level: tracing::Level::INFO,
            target: "test".to_owned(),
            message: message.to_owned(),
            timestamp: chrono::Local::now(),
            channel: None,
        }
    }

    #[test]
    fn log_event_fields_are_accessible() {
        let event = LogEvent {
            level: tracing::Level::INFO,
            target: "my::module".to_string(),
            message: "hello world".to_string(),
            timestamp: chrono::Local::now(),
            channel: None,
        };
        assert_eq!(event.level, tracing::Level::INFO);
        assert_eq!(event.target, "my::module");
        assert_eq!(event.message, "hello world");
    }

    #[test]
    fn log_event_is_clone() {
        let event = LogEvent {
            level: tracing::Level::WARN,
            target: "t".to_string(),
            message: "msg".to_string(),
            timestamp: chrono::Local::now(),
            channel: None,
        };
        let cloned = event.clone();
        assert_eq!(cloned.level, event.level);
        assert_eq!(cloned.message, event.message);
    }

    #[test]
    fn layer_captures_the_channel_field_end_to_end() {
        // A real tracing event dispatched through the layer: the structured
        // `channel` field (a stable id's raw value, ADR-020) lands in
        // LogEvent::channel, and an event without one yields None.
        let (tx, rx) = crossbeam_channel::bounded(8);
        let id = ChannelId::mint();
        super::super::with_gui_test_subscriber(tx, || {
            tracing::info!(channel = id.as_u64(), "channel 3 running");
            tracing::warn!("app-level warning");
        });
        let first = rx.try_recv().unwrap();
        assert_eq!(first.channel, Some(id));
        assert_eq!(first.message, "channel 3 running");
        let second = rx.try_recv().unwrap();
        assert_eq!(second.channel, None);
        assert_eq!(second.level, tracing::Level::WARN);
    }

    #[test]
    fn gui_layer_can_be_constructed() {
        let (layer, _rx) = make_layer();
        // Just verify it constructs without panicking.
        drop(layer);
    }

    #[test]
    fn accepted_and_full_events_notify_while_only_full_events_count_as_dropped() {
        let (tx, rx) = bounded(1);
        let health = GuiLogHealth::new();
        let notifications = Arc::new(AtomicU64::new(0));
        let callback_count = Arc::clone(&notifications);
        health.set_notify(Arc::new(move || {
            callback_count.fetch_add(1, Ordering::Relaxed);
        }));
        let layer = GuiLogLayer::with_health(tx, health.clone());

        layer.forward(event("accepted"));
        layer.forward(event("full one"));
        layer.forward(event("full two"));

        assert_eq!(health.dropped_events(), 2);
        assert_eq!(notifications.load(Ordering::Relaxed), 3);
        assert_eq!(rx.try_recv().unwrap().message, "accepted");
    }

    #[test]
    fn notify_callback_installs_late_once_and_duplicate_install_is_ignored() {
        let (tx, rx) = bounded(1);
        let health = GuiLogHealth::new();
        let layer = GuiLogLayer::with_health(tx, health.clone());

        // Construction precedes GUI setup. Events before callback installation
        // remain valid; they simply have nobody to wake yet.
        layer.forward(event("before callback"));
        assert_eq!(rx.try_recv().unwrap().message, "before callback");

        let first_notifications = Arc::new(AtomicU64::new(0));
        let first_callback_count = Arc::clone(&first_notifications);
        health.set_notify(Arc::new(move || {
            first_callback_count.fetch_add(1, Ordering::Relaxed);
        }));
        let duplicate_notifications = Arc::new(AtomicU64::new(0));
        let duplicate_callback_count = Arc::clone(&duplicate_notifications);
        health.set_notify(Arc::new(move || {
            duplicate_callback_count.fetch_add(1, Ordering::Relaxed);
        }));

        layer.forward(event("after callback"));

        assert_eq!(first_notifications.load(Ordering::Relaxed), 1);
        assert_eq!(duplicate_notifications.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn disconnected_receiver_counts_nothing_and_does_not_notify() {
        let (tx, rx) = bounded::<LogEvent>(1);
        let health = GuiLogHealth::new();
        let notifications = Arc::new(AtomicU64::new(0));
        let callback_count = Arc::clone(&notifications);
        health.set_notify(Arc::new(move || {
            callback_count.fetch_add(1, Ordering::Relaxed);
        }));
        let layer = GuiLogLayer::with_health(tx, health.clone());
        drop(rx);

        layer.forward(event("after shutdown"));

        assert_eq!(health.dropped_events(), 0);
        assert_eq!(notifications.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn rendezvous_queue_without_receiver_reports_full_instead_of_blocking() {
        let (tx, _rx) = bounded::<LogEvent>(0);
        let health = GuiLogHealth::new();
        let layer = GuiLogLayer::with_health(tx, health.clone());

        for index in 0..1_000 {
            layer.forward(event(&format!("full {index}")));
        }

        assert_eq!(health.dropped_events(), 1_000);
    }
}
