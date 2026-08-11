use crossbeam_channel::Sender;
use tracing::{Event, Subscriber};
use tracing_subscriber::{layer::Context, Layer};

use crate::core::channel::ChannelId;

/// A log event forwarded to the GUI status pane.
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
/// is read by the GUI status pane to display live log output.
///
/// Sends are best-effort (`try_send`): if the channel is full or the receiver
/// has been dropped the event is silently discarded rather than blocking the
/// calling thread.
pub struct GuiLogLayer {
    sender: Sender<LogEvent>,
}

impl GuiLogLayer {
    pub fn new(sender: Sender<LogEvent>) -> Self {
        Self { sender }
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

        let _ = self.sender.try_send(log_event);
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
    use crossbeam_channel::unbounded;

    use super::*;

    fn make_layer() -> (GuiLogLayer, crossbeam_channel::Receiver<LogEvent>) {
        let (tx, rx) = unbounded();
        (GuiLogLayer::new(tx), rx)
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
    fn disconnected_receiver_does_not_panic() {
        let (tx, rx) = unbounded::<LogEvent>();
        let layer = GuiLogLayer::new(tx);
        drop(rx); // disconnect receiver
                  // Sending to a disconnected channel must not panic — try_send discards.
        drop(layer);
    }
}
