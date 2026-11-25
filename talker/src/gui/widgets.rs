//! Shared field renderers and small pure helpers for the talker GUI, split
//! out of `mod.rs` in the master–detail restructure (spec §3.2). Everything
//! here is either a stateless `fn(ui, …)` widget or a pure validation /
//! formatting helper — app state stays in [`super::TalkerApp`].

use std::{net::Ipv4Addr, ops::ControlFlow, sync::Arc};

use egui::{Align, Layout};
use wiredata_ui::diagnostics::{dismissible_attention_callout, SignalTone};
use wiredata_ui::serial::{self as shared, LineClick, RtsByFlow};

use crate::core::channel::{
    ChannelId, ControlLines, FlowControl, InterfaceConfig, LocalAddress, OutputLine,
    RTS_HIGH_UNDER_RTS_CTS,
};
use crate::core::message::{
    code_page_encodes, decode_codepage_byte, repair_after_edit, segments, ChecksumAlgorithm,
    CodePage, Segment,
};

use super::diagnostics::LIVE_UPDATE_QUEUE_TOOLTIP;
use super::display::{ChannelDisplay, ControlStyle, DisplayMode};
use wiredata_ui::hover::{Hover, ShowHover};

/// The runner→UI live-update queue as reported beside its drop warning.
///
/// Grouped rather than passed as three loose numbers: depth is only readable
/// against its capacity, and a peak means nothing without both.
pub(super) struct LiveUpdateQueueGauge {
    pub len: usize,
    pub peak: usize,
    pub capacity: usize,
}

use super::draft::{
    multicast_ttl, ConnDraft, ConnKind, PayloadKind, PortHold, ScheduleDraft, UdpModeDraft,
};
use super::notice::DismissedNotice;
use super::{MessageAnalysisCache, MessageDraftAnalysis};

/// Foreground/background pair for a lossy code-page substitution. The shared
/// palette's amber is a readable foreground in light mode but too dark behind
/// black text, so the light theme uses a pale amber field with dark-brown text.
fn replacement_highlight_colors(ui: &egui::Ui) -> (egui::Color32, egui::Color32) {
    if ui.visuals().dark_mode {
        (
            egui::Color32::BLACK,
            wiredata_ui::palette::active(ui).warning,
        )
    } else {
        (
            egui::Color32::from_rgb(45, 30, 0),
            egui::Color32::from_rgb(255, 225, 150),
        )
    }
}

/// Lifecycle presentation shared by the detail header and the channel-list
/// rows: listener's glyph set and palette colors mapped onto talker's channel
/// states, plus a status word. Talker has no Faulted lifecycle state — a
/// channel with an error is either still running (sending with errors) or
/// stopped (its open failed / it exited) — so the ⚠ marks "has an error" in
/// both cases and the word carries the run state.
pub(super) fn lifecycle_indicator(
    running: bool,
    has_error: bool,
    pal: &wiredata_ui::palette::Palette,
) -> (&'static str, egui::Color32, &'static str) {
    use wiredata_ui::glyphs;
    match (running, has_error) {
        (true, false) => (glyphs::RUNNING, pal.running, "running"),
        (true, true) => (glyphs::FAULT, pal.fault, "running"),
        (false, true) => (glyphs::FAULT, pal.fault, "faulted"),
        (false, false) => (glyphs::STOPPED, pal.idle, "stopped"),
    }
}

/// The Start-side lifecycle button's label and enabled state — listener's
/// `start_button` decision mapped onto talker's states, so the two detail
/// panes carry drift and recovery the same way. Pure, unit-tested; the
/// detail pane renders the result and defers the click:
/// - not running, no error → "Start channel", enabled iff the draft is valid
/// - not running, error → "Retry channel" (the open/run failed), iff valid
/// - running + pending edits (interface or messages) → "Apply & Restart",
///   enabled iff the complete replacement draft is valid
/// - running, no edits → "Start channel", **disabled** (nothing to do)
pub(super) fn start_button(
    running: bool,
    has_error: bool,
    drift: bool,
    can_start: bool,
) -> (&'static str, bool) {
    match (running, drift) {
        (true, true) => ("Apply & Restart", can_start),
        (true, false) => ("Start channel", false),
        (false, _) => (
            if has_error {
                "Retry channel"
            } else {
                "Start channel"
            },
            can_start,
        ),
    }
}

// ── Interface summary / start blockers ────────────────────────────────────────

/// The hover of the line in a channel's header that gives its interface.
const INTERFACE_SUMMARY_HOVER: Hover = Hover::new(
    "This channel's interface, as Configure interface sets it.\n\n\
     For UDP: the mode, then the address and port the datagrams go to, and the local \
     port they are sent from when one is set.\n\n\
     For serial: the port, then the baud rate, data bits, parity and stop bits, as in \
     9600,8,N,1, and the flow control.\n\n\
     A ? marks a setting that is missing or not valid yet. The channel cannot start \
     until it is filled in.",
);

/// Render [`interface_summary`] in a channel header, splitting on `?`
/// markers so unknown / unfilled fields show up as a **bold red** glyph
/// rather than blending into the rest of the weak-grey summary text. Every
/// piece carries the line's hover.
pub(super) fn show_interface_summary(ui: &mut egui::Ui, conn: &ConnDraft) {
    let text = interface_summary(conn);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let mut buf = String::new();
        let flush = |ui: &mut egui::Ui, buf: &mut String| {
            if !buf.is_empty() {
                ui.weak(std::mem::take(buf))
                    .on_hover(&INTERFACE_SUMMARY_HOVER);
            }
        };
        // Render the ? as inline text with a red background — RichText
        // sits on the same text baseline as the surrounding weak-grey
        // labels, so the badge looks the same size and alignment in
        // every channel (a Frame-wrapped version offsets vertically
        // and reads as a different chrome than the rest of the line).
        // Padded with thin spaces so the background extends past the
        // glyph instead of clinging to it.
        let red_bg = egui::Color32::from_rgb(200, 50, 50);
        for c in text.chars() {
            if c == '?' {
                flush(ui, &mut buf);
                ui.label(
                    egui::RichText::new("\u{2009}?\u{2009}")
                        .color(egui::Color32::WHITE)
                        .strong()
                        .background_color(red_bg),
                )
                .on_hover(&INTERFACE_SUMMARY_HOVER);
            } else {
                buf.push(c);
            }
        }
        flush(ui, &mut buf);
    });
}

/// One-line, human-readable summary of a channel's selected interface and
/// its parameters, shown in the channel list and detail header so the active
/// config is visible at a glance without expanding the editor. Uses the
/// draft's current strings — invalid or missing parts show as `?`.
pub(super) fn interface_summary(conn: &ConnDraft) -> String {
    /// Show `s` if it's non-empty AND `valid(s)` is true; otherwise
    /// the `?` placeholder — which [`show_interface_summary`] paints
    /// as a red pill. Drives the at-a-glance "this channel can't
    /// start yet" cue for both missing AND malformed values.
    fn or_q(s: &str, valid: impl Fn(&str) -> bool) -> &str {
        if s.is_empty() || !valid(s) {
            "?"
        } else {
            s
        }
    }
    // Validators line up with `channel_blockers` so the summary's `?`s
    // match the disabled-Start tooltip exactly.
    let ok_ipv4 = |s: &str| s.parse::<Ipv4Addr>().is_ok();
    let ok_port = |s: &str| s.parse::<u16>().is_ok();
    let ok_any = |_: &str| true;
    let lp = if conn.local_port.is_empty() {
        String::new()
    } else {
        format!(" (local {})", conn.local_port)
    };
    match conn.kind() {
        ConnKind::Serial => {
            let data = conn.data_bits;
            // Conventional serial notation — `9600,8,N,1` — matching listener's
            // status line. Spelling the parity out made the one field that has
            // a standard abbreviation the odd one in the group.
            let parity = match conn.parity {
                1 => "O",
                2 => "E",
                _ => "N",
            };
            let stop = conn.stop_bits;
            let flow = match conn.flow_control {
                1 => "XON/XOFF",
                2 => "RTS/CTS",
                _ => "None",
            };
            format!(
                "Serial: {} {},{},{},{} flow:{}",
                or_q(&conn.serial_port, ok_any),
                conn.baud_rate,
                data,
                parity,
                stop,
                flow,
            )
        }
        ConnKind::Udp => {
            let (label, pair) = match conn.udp_mode {
                UdpModeDraft::Unicast => ("unicast", &conn.udp_unicast),
                UdpModeDraft::Broadcast => ("broadcast", &conn.udp_broadcast),
                UdpModeDraft::Multicast => ("multicast", &conn.udp_multicast),
            };
            format!(
                "UDP {label} {}:{}{lp}",
                or_q(&pair.addr, ok_ipv4),
                or_q(&pair.port, ok_port),
            )
        }
    }
}

/// A start-blocker sink: receives each blocker as a **lazy** formatter (so a
/// caller that only asks "is there any?" never formats or allocates) and can
/// stop the walk early by returning `Break`.
type BlockerSink<'s> = dyn FnMut(&dyn Fn() -> String) -> ControlFlow<()> + 's;

/// Enumerate the specific reasons the Start button is disabled for a
/// channel — one human-readable line per problem. Returned in the same
/// order they appear in the editor (channel fields first, then per-message
/// issues from top to bottom).
pub(super) fn start_blockers_analyzed(
    conn: &ConnDraft,
    messages: &[ScheduleDraft],
    analyses: &[MessageAnalysisCache],
) -> Vec<String> {
    let mut out = Vec::new();
    let _ = visit_start_blockers(conn, messages, analyses, &mut |blocker| {
        out.push(blocker());
        ControlFlow::Continue(())
    });
    out
}

/// Whether any start blocker exists — [`start_blockers_analyzed`]'s predicate
/// without its strings. This runs for **every** channel on every frame (the
/// Start-all gate); the walk stops at the first blocker and, because blockers
/// reach the sink lazily, formats nothing.
pub(super) fn any_start_blocker_analyzed(
    conn: &ConnDraft,
    messages: &[ScheduleDraft],
    analyses: &[MessageAnalysisCache],
) -> bool {
    visit_start_blockers(conn, messages, analyses, &mut |_| ControlFlow::Break(())).is_break()
}

/// The single source of truth both frontends walk.
fn visit_start_blockers(
    conn: &ConnDraft,
    messages: &[ScheduleDraft],
    analyses: &[MessageAnalysisCache],
    sink: &mut BlockerSink,
) -> ControlFlow<()> {
    channel_blockers(conn, sink)?;
    if messages.is_empty() {
        sink(&|| "No messages defined — add at least one".to_string())?;
    } else {
        let mut any_complete = false;
        for (i, message) in messages.iter().enumerate() {
            let Some(analysis) = analyses.get(i).and_then(|cached| cached.analysis.as_ref()) else {
                return sink(&|| "Message analysis is not ready".to_string());
            };
            any_complete |= analysis.config.is_some();
            message_blockers(i, message, analysis, sink)?;
        }
        if !any_complete {
            sink(&|| "No message is fully filled in".to_string())?;
        }
    }
    ControlFlow::Continue(())
}

fn channel_blockers(conn: &ConnDraft, sink: &mut BlockerSink) -> ControlFlow<()> {
    match conn.kind() {
        ConnKind::Serial => {
            if conn.serial_port.is_empty() {
                sink(&|| "Channel: select a serial port".to_string())?;
            }
            // The editor applies only a positive rate, so zero can come only
            // from a saved profile.
            if conn.baud_rate == 0 {
                sink(&|| "Channel: baud rate must be a positive number".to_string())?;
            }
        }
        ConnKind::Udp => {
            let (mode_label, pair, addr_label) = match conn.udp_mode {
                UdpModeDraft::Unicast => ("destination", &conn.udp_unicast, "address"),
                UdpModeDraft::Broadcast => ("broadcast", &conn.udp_broadcast, "address"),
                UdpModeDraft::Multicast => ("multicast", &conn.udp_multicast, "group"),
            };
            if pair.addr.is_empty() || pair.addr.parse::<Ipv4Addr>().is_err() {
                sink(&|| format!("Channel: {mode_label} {addr_label} must be IPv4"))?;
            }
            if pair.port.is_empty() || pair.port.parse::<u16>().is_err() {
                sink(&|| format!("Channel: {mode_label} port must be 1–65535"))?;
            }
            if invalid_parse::<u16>(&conn.local_port) {
                sink(&|| "Channel: local port must be 1–65535".to_string())?;
            }
            if conn.udp_mode == UdpModeDraft::Multicast
                && multicast_ttl(&conn.udp_multicast_ttl).is_none()
            {
                sink(&|| "Channel: multicast TTL must be 0–255".to_string())?;
            }
        }
    }
    ControlFlow::Continue(())
}

fn message_blockers(
    idx: usize,
    entry: &ScheduleDraft,
    analysis: &MessageDraftAnalysis,
    sink: &mut BlockerSink,
) -> ControlFlow<()> {
    let n = idx + 1;
    // Track whether this message reported a field-level blocker: the
    // analysis-level fallbacks below only apply to messages whose fields
    // all individually pass (same precedence the Vec-building code had).
    let mut any = false;
    let mut emit = |blocker: &dyn Fn() -> String| -> ControlFlow<()> {
        any = true;
        sink(blocker)
    };
    if entry.interval_ms.is_empty() {
        emit(&|| format!("Message {n}: interval is empty"))?;
    } else if entry.interval_ms.parse::<u64>().is_err() {
        emit(&|| format!("Message {n}: interval must be a whole number"))?;
    }
    match entry.payload_kind {
        PayloadKind::Hex if !hex_valid(&entry.hex_data) => {
            emit(&|| format!("Message {n}: hex is empty or invalid"))?;
        }
        PayloadKind::Nmea => {
            if entry.nmea_talker.is_empty() {
                emit(&|| format!("Message {n}: NMEA talker is empty"))?;
            }
            if entry.nmea_sentence_type.is_empty() {
                emit(&|| format!("Message {n}: NMEA sentence type is empty"))?;
            }
        }
        // UTF-8 / UTF-16 / ASCII payloads accept any string at this layer.
        _ => {}
    }
    if !any {
        match (&analysis.config, &analysis.validation_error) {
            (Some(_), Some(error)) => sink(&|| format!("Message {n}: {error}"))?,
            (Some(_), None) => {}
            (None, _) => sink(&|| format!("Message {n}: configuration is incomplete or invalid"))?,
        }
    }
    ControlFlow::Continue(())
}

// ── Interface field editors (Serial / UDP) ───────────────────────────────────

/// What an interface editor reports for one frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct InterfaceEdit {
    /// A value was committed. A running channel takes it at once.
    pub changed: bool,
    /// The serial port list should be read again.
    pub refresh_ports: bool,
    /// This computer's network address list should be read again.
    pub refresh_addresses: bool,
    /// An output line's button was clicked, on a serial channel.
    pub click: Option<LineClick>,
}

/// The id a serial channel's editor keeps its drop-downs and typed text under,
/// so two channels never share a half-typed port or baud rate. `None` is a
/// slot that does not exist, which the detail pane never draws.
pub(super) fn serial_editor_id(channel: Option<ChannelId>) -> egui::Id {
    egui::Id::new(("serial_editor", channel))
}

/// The transport's control lines as the shared lines show them.
pub(super) fn control_lines(lines: ControlLines) -> shared::ControlLines {
    shared::ControlLines {
        rts: lines.rts,
        dtr: lines.dtr,
        cts: lines.cts,
        dsr: lines.dsr,
        dcd: lines.dcd,
        ri: lines.ri,
    }
}

/// The lines to show live: the last reading, while the channel runs. A stopped
/// channel's last reading is stale, and a running one without a reading (the
/// first is not in yet, or the port cannot be read) has nothing to show, so
/// the shared lines grey out and say why.
pub(super) fn live_lines(
    running: bool,
    reading: Option<ControlLines>,
) -> Option<shared::ControlLines> {
    reading.filter(|_| running).map(control_lines)
}

/// What stands in for the RTS button: `None` unless the open port's flow
/// control sets RTS (RTS/CTS), then whether its level is known on this
/// platform (`RTS_HIGH_UNDER_RTS_CTS`). `open` is the interface the channel's
/// runner confirmed open, which is the port the lines belong to, not the
/// draft.
pub(super) fn rts_by_flow(open: Option<&InterfaceConfig>) -> Option<RtsByFlow> {
    match open {
        Some(InterfaceConfig::Serial(serial)) if serial.flow_control == FlowControl::Hardware => {
            Some(if RTS_HIGH_UNDER_RTS_CTS {
                RtsByFlow::High
            } else {
                RtsByFlow::Paced
            })
        }
        _ => None,
    }
}

/// The line and level a click on an output line's button asks for.
pub(super) fn output_request(click: LineClick) -> (OutputLine, bool) {
    match click {
        LineClick::Rts(high) => (OutputLine::Rts, high),
        LineClick::Dtr(high) => (OutputLine::Dtr, high),
    }
}

/// A serial channel's editor, the two lines Linemon and Listener show
/// (talker ADR-071), each wrapping to fit the detail pane: the port with its
/// baud rate and framing, then Flow with the control lines. A change to a
/// setting is reported at once, and a typed port or baud rate once its box is
/// left or Enter is pressed.
///
/// The control lines are live, not part of the draft: `live` is the running
/// port's last reading (`None` while the channel is not running), and
/// `rts_by_flow` says whether the open port's flow control sets RTS. `id`
/// keeps this channel's typed text apart from every other's.
pub(super) fn show_serial_fields(
    ui: &mut egui::Ui,
    id: egui::Id,
    conn: &mut ConnDraft,
    ports: Option<&[String]>,
    live: Option<shared::ControlLines>,
    rts_by_flow: Option<RtsByFlow>,
) -> InterfaceEdit {
    let mut settings = conn.serial_settings();
    let first = ui
        .horizontal_wrapped(|ui| shared::settings_line(ui, id, &mut settings, ports))
        .inner;
    let second = ui
        .horizontal_wrapped(|ui| {
            shared::flow_and_lines(ui, id, &mut settings.flow, live, rts_by_flow)
        })
        .inner;
    let changed = first.changed || second.changed;
    if changed {
        conn.apply_serial_settings(&settings);
    }
    InterfaceEdit {
        changed,
        refresh_ports: first.refresh_ports,
        click: second.click,
        ..InterfaceEdit::default()
    }
}

/// A UDP channel's editor: lines that each wrap to fit the detail pane. The
/// first is where the datagrams go, as an address and port, followed by the
/// local port; the second is the mode; in Multicast a third gives the outgoing
/// interface and the TTL. `changed` is set when a value was committed or the
/// mode changed, which a running channel takes at once. `addresses` is this
/// computer's network addresses, `None` until they are first listed.
pub(super) fn show_udp_fields(
    ui: &mut egui::Ui,
    conn: &mut ConnDraft,
    addresses: Option<&[LocalAddress]>,
) -> InterfaceEdit {
    let before_mode = conn.udp_mode;
    let mut apply = false;
    let mut refresh_addresses = false;

    // Per-mode namespace even though all three modes render the same
    // [addr] [port] [local-port] shape — kept as defence-in-depth so any
    // auto-derived (un-salted) widget id inside the lines lives in its
    // own namespace per mode, the same trick `message_grid_<kind>` uses
    // in the message editor.
    let namespace = match conn.udp_mode {
        UdpModeDraft::Unicast => "udp_lines_unicast",
        UdpModeDraft::Broadcast => "udp_lines_broadcast",
        UdpModeDraft::Multicast => "udp_lines_multicast",
    };
    ui.push_id(namespace, |ui| {
        ui.horizontal_wrapped(|ui| {
            apply |= show_udp_destination(ui, conn);
            apply |= show_udp_local_port(ui, conn);
        });
        ui.horizontal_wrapped(|ui| show_udp_mode(ui, conn));
        if conn.udp_mode == UdpModeDraft::Multicast {
            ui.horizontal_wrapped(|ui| {
                let (changed, refresh) = show_multicast_interface(ui, conn, addresses);
                apply |= changed;
                refresh_addresses = refresh;
                apply |= show_multicast_ttl(ui, conn);
            });
        }
    });

    InterfaceEdit {
        changed: apply || (conn.udp_mode != before_mode),
        refresh_addresses,
        ..InterfaceEdit::default()
    }
}

/// How the multicast interface choice names letting the system choose.
const MULTICAST_SYSTEM_DEFAULT: &str = "System default";

const BROADCAST_DESTINATION_HOVER: Hover = Hover::new(
    "Where the broadcast datagrams go.\n\n\
     255.255.255.255 reaches every device on the local network, but leaves through \
     one network adapter only: the one the system routes it through, usually the \
     adapter with the default route.\n\n\
     On a computer with more than one network adapter, use the broadcast address of \
     the network you want instead: its address with the host part all ones, such as \
     192.168.1.255 for a 192.168.1.x network with mask 255.255.255.0. The datagrams \
     then leave through the adapter on that network.\n\n\
     While the channel runs, Enter applies a change at once; leaving the box \
     any other way keeps it for Apply & Restart.",
);

const MULTICAST_INTERFACE_HOVER: Hover = Hover::new(
    "The network adapter the multicast datagrams leave through.\n\n\
     System default lets the system choose, usually the adapter with the default \
     route. On a computer with more than one network adapter, choose the address of \
     the adapter on the receivers' network.\n\n\
     The list shows this computer's IPv4 addresses on adapters that are up, and is \
     read again each time it is opened.",
);

const MULTICAST_TTL_HOVER: Hover = Hover::new(
    "How many routers the multicast datagrams may cross.\n\n\
     Left empty, it is 1, which keeps them on the local network. 0 keeps them on \
     this computer. Raise it only when the receivers are on another network and the \
     routers between forward multicast.\n\n\
     While the channel runs, Enter applies a change at once; leaving the box \
     any other way keeps it for Apply & Restart.",
);

/// The outgoing interface choices, as (interface, label): the system default
/// first, then each local address. The configured address is always among
/// them, so it is never silently replaced; once the addresses are listed, one
/// that is not among them says so. `listed` is `None` until they are.
pub(super) fn multicast_interface_choices(
    configured: Option<Ipv4Addr>,
    listed: Option<&[LocalAddress]>,
) -> Vec<(Option<Ipv4Addr>, String)> {
    let mut choices = vec![(None, MULTICAST_SYSTEM_DEFAULT.to_owned())];
    for address in listed.unwrap_or_default() {
        let label = if address.ip.is_loopback() {
            format!("{} \u{2014} this computer only", address.ip)
        } else {
            format!("{} \u{2014} {}", address.ip, address.interface)
        };
        choices.push((Some(address.ip), label));
    }
    if let Some(ip) = configured {
        if !choices.iter().any(|(choice, _)| *choice == Some(ip)) {
            let label = match listed {
                Some(_) => format!("{ip} \u{2014} not on this computer now"),
                None => ip.to_string(),
            };
            choices.push((Some(ip), label));
        }
    }
    choices
}

/// The outgoing interface's label and drop-down. Returns whether the choice
/// changed, and whether the address list should be read again, which opening
/// the drop-down asks for.
fn show_multicast_interface(
    ui: &mut egui::Ui,
    conn: &mut ConnDraft,
    addresses: Option<&[LocalAddress]>,
) -> (bool, bool) {
    let choices = multicast_interface_choices(conn.udp_multicast_interface, addresses);
    // As wide as the longest choice, so the TTL after it stays put whichever
    // is chosen.
    let widest = choices
        .iter()
        .map(|(_, label)| text_width(ui, egui::TextStyle::Button, label))
        .fold(0.0, f32::max);
    // Never wider than the pane: a long adapter name is cut with an ellipsis
    // in the closed box, and shown whole in the open list.
    let box_width = (widest + ui.spacing().icon_width + 3.0 * ui.spacing().button_padding.x)
        .min(ui.max_rect().width());
    let label = "Outgoing interface";
    keep_together(
        ui,
        row_width(
            ui,
            &[text_width(ui, egui::TextStyle::Body, label), box_width],
        ),
    );
    ui.label(label).on_hover(&MULTICAST_INTERFACE_HOVER);
    // A drop-down does not wrap on its own, so when it cannot sit beside its
    // label it starts the next row.
    keep_together(ui, box_width);
    let selected = choices
        .iter()
        .find(|(choice, _)| *choice == conn.udp_multicast_interface)
        .map_or(MULTICAST_SYSTEM_DEFAULT, |(_, label)| label.as_str())
        .to_owned();
    let before = conn.udp_multicast_interface;
    let response = egui::ComboBox::from_id_salt("udp_multicast_interface")
        .width(box_width)
        .truncate()
        .selected_text(selected)
        .show_ui(ui, |ui| {
            for (choice, label) in &choices {
                ui.selectable_value(&mut conn.udp_multicast_interface, *choice, label);
            }
        });
    (
        conn.udp_multicast_interface != before,
        response
            .response
            .on_hover(&MULTICAST_INTERFACE_HOVER)
            .clicked(),
    )
}

/// The multicast TTL field. Returns `true` when a value was committed.
fn show_multicast_ttl(ui: &mut egui::Ui, conn: &mut ConnDraft) -> bool {
    let bad = multicast_ttl(&conn.udp_multicast_ttl).is_none();
    keep_together(
        ui,
        row_width(
            ui,
            &[
                text_width(ui, egui::TextStyle::Body, "TTL"),
                TTL_FIELD_WIDTH,
            ],
        ),
    );
    ui.label("TTL").on_hover(&MULTICAST_TTL_HOVER);
    let r = red_bordered(ui, bad, "enter a TTL from 0 to 255", |ui| {
        ui.add(
            egui::TextEdit::singleline(&mut conn.udp_multicast_ttl)
                .id_salt("udp_multicast_ttl")
                .desired_width(TTL_FIELD_WIDTH)
                .char_limit(3)
                .hint_text("1"),
        )
    })
    .on_hover(&MULTICAST_TTL_HOVER);
    enter_committed(&r, ui)
}

fn show_udp_mode(ui: &mut egui::Ui, conn: &mut ConnDraft) {
    ui.label("Mode").on_hover(&UDP_MODE_HOVER);
    ui.radio_value(&mut conn.udp_mode, UdpModeDraft::Broadcast, "Broadcast")
        .on_hover(&UDP_MODE_HOVER);
    ui.radio_value(&mut conn.udp_mode, UdpModeDraft::Unicast, "Unicast")
        .on_hover(&UDP_MODE_HOVER);
    ui.radio_value(&mut conn.udp_mode, UdpModeDraft::Multicast, "Multicast")
        .on_hover(&UDP_MODE_HOVER);
}

const UDP_MODE_HOVER: Hover = Hover::new(
    "Who receives the datagrams.\n\n\
     Broadcast: every device on the local network.\n\n\
     Unicast: the one computer or device at the destination address.\n\n\
     Multicast: the receivers that have joined the multicast group.\n\n\
     Each mode keeps its own destination, so switching modes does not lose what was \
     typed for another.",
);

const UNICAST_DESTINATION_HOVER: Hover = Hover::new(
    "The IPv4 address of the one computer or device the datagrams go to.\n\n\
     While the channel runs, Enter applies a change at once; leaving the box \
     any other way keeps it for Apply & Restart.",
);

const MULTICAST_GROUP_HOVER: Hover = Hover::new(
    "An IPv4 multicast group address, from 224.0.0.0 to 239.255.255.255.\n\n\
     Receivers must join the same group and port to see these datagrams. Addresses \
     in 239.x.x.x are the usual choice on a local network.\n\n\
     While the channel runs, Enter applies a change at once; leaving the box \
     any other way keeps it for Apply & Restart.",
);

const UDP_PORT_HOVER: Hover = Hover::new(
    "The UDP port the datagrams are sent to. The receiver must listen on this port.\n\n\
     A number from 1 to 65535. The minus and plus buttons step it, faster the \
     longer they are held.\n\n\
     While the channel runs, Enter or a button applies a change at once; leaving \
     the box any other way keeps it for Apply & Restart.",
);

const LOCAL_PORT_HOVER: Hover = Hover::new(
    "The UDP port the datagrams are sent from.\n\n\
     Left empty, the system picks a free port each time the channel opens. Set one \
     only when the receiver accepts datagrams from one port.\n\n\
     While the channel runs, Enter applies a change at once; leaving the box \
     any other way keeps it for Apply & Restart.",
);

/// All three UDP modes have the same shape — an IPv4 address plus a port
/// — so they share one helper. The per-mode differences (label, hint
/// text, validation message, tooltip, id salts, and which pair of
/// `udp_unicast` / `udp_broadcast` / `udp_multicast` strings to point
/// at) are looked up from a single match.
fn show_udp_destination(ui: &mut egui::Ui, conn: &mut ConnDraft) -> bool {
    let mode = conn.udp_mode;
    // Compute validation flags up front while we still hold an immutable
    // borrow — the mutable destructure below precludes re-reading.
    let pair_ref = match mode {
        UdpModeDraft::Unicast => &conn.udp_unicast,
        UdpModeDraft::Broadcast => &conn.udp_broadcast,
        UdpModeDraft::Multicast => &conn.udp_multicast,
    };
    // Two-mode validation. *Lenient* before the user has submitted
    // (no red on empty, no red on partial IPv4 like `192.168.1.`),
    // *strict* after — empty AND any malformed value go red. The
    // submit flag flips on the first Enter, Start, or profile load.
    let (bad_addr, bad_port) = if pair_ref.submitted {
        (
            pair_ref.addr.parse::<Ipv4Addr>().is_err(),
            pair_ref.port.parse::<u16>().is_err(),
        )
    } else {
        (
            invalid_ipv4(&pair_ref.addr),
            invalid_parse::<u16>(&pair_ref.port),
        )
    };

    let (label, hover, hint, invalid_msg, addr_salt, port_salt) = match mode {
        UdpModeDraft::Unicast => (
            "Destination",
            UNICAST_DESTINATION_HOVER,
            "192.168.1.100",
            "enter an IPv4 address — e.g. 192.168.1.100",
            "udp_unicast_addr",
            "udp_unicast_port",
        ),
        UdpModeDraft::Broadcast => (
            "Destination",
            BROADCAST_DESTINATION_HOVER,
            "255.255.255.255",
            "enter an IPv4 address — e.g. 255.255.255.255",
            "udp_broadcast_addr",
            "udp_broadcast_port",
        ),
        UdpModeDraft::Multicast => (
            "Multicast group",
            MULTICAST_GROUP_HOVER,
            "239.0.0.1",
            "enter IPv4 multicast address — e.g. 239.0.0.1",
            "udp_multicast_addr",
            "udp_multicast_port",
        ),
    };
    ui.label(label).on_hover(&hover);

    let pair = match mode {
        UdpModeDraft::Unicast => &mut conn.udp_unicast,
        UdpModeDraft::Broadcast => &mut conn.udp_broadcast,
        UdpModeDraft::Multicast => &mut conn.udp_multicast,
    };
    let apply = show_addr_port_row(
        ui,
        AddrPortRow {
            addr_field: &mut pair.addr,
            addr_id_salt: addr_salt,
            addr_hint: hint,
            addr_invalid_msg: invalid_msg,
            addr_hover: &hover,
            bad_addr,
            port_field: &mut pair.port,
            port_id_salt: port_salt,
            bad_port,
            port_hold: &mut conn.udp_port_hold,
        },
    );
    // First explicit commit (Enter on either field, or a ± port
    // click that changes the value) flips the pair into strict
    // validation. Stays flipped for the life of the channel.
    if apply {
        pair.submitted = true;
    }
    apply
}

fn show_udp_local_port(ui: &mut egui::Ui, conn: &mut ConnDraft) -> bool {
    let bad_local = invalid_parse::<u16>(&conn.local_port);
    keep_together(ui, local_port_width(ui));
    ui.label("Local port").on_hover(&LOCAL_PORT_HOVER);
    let r = red_bordered(ui, bad_local, "enter a port number 1–65535", |ui| {
        ui.add(
            egui::TextEdit::singleline(&mut conn.local_port)
                .id_salt("udp_local_port")
                .desired_width(LOCAL_PORT_FIELD_WIDTH)
                .hint_text("auto"),
        )
    })
    .on_hover(&LOCAL_PORT_HOVER);
    enter_committed(&r, ui)
}

const ADDR_FIELD_WIDTH: f32 = 140.0;
const PORT_FIELD_WIDTH: f32 = 60.0;
const LOCAL_PORT_FIELD_WIDTH: f32 = 80.0;
const TTL_FIELD_WIDTH: f32 = 40.0;

/// How wide `text` is in `style`, laid out on one line.
fn text_width(ui: &egui::Ui, style: egui::TextStyle, text: &str) -> f32 {
    ui.painter()
        .layout_no_wrap(
            text.to_owned(),
            style.resolve(ui.style()),
            egui::Color32::PLACEHOLDER,
        )
        .size()
        .x
}

/// How wide controls of these `widths` are side by side on one row, with the
/// space between them.
fn row_width(ui: &egui::Ui, widths: &[f32]) -> f32 {
    let gaps = widths.len().saturating_sub(1);
    widths.iter().sum::<f32>() + ui.spacing().item_spacing.x * gaps as f32
}

/// How wide a small button showing `glyph` is.
fn small_button_width(ui: &egui::Ui, glyph: &str) -> f32 {
    text_width(ui, egui::TextStyle::Button, glyph) + 2.0 * ui.spacing().button_padding.x
}

/// The width of the port stepper: its label, the two ± buttons and the field
/// between them.
fn port_stepper_width(ui: &egui::Ui) -> f32 {
    row_width(
        ui,
        &[
            text_width(ui, egui::TextStyle::Body, "Port:"),
            small_button_width(ui, "\u{2212}"),
            PORT_FIELD_WIDTH,
            small_button_width(ui, "+"),
        ],
    )
}

/// The width of the local port's label and field.
fn local_port_width(ui: &egui::Ui) -> f32 {
    row_width(
        ui,
        &[
            text_width(ui, egui::TextStyle::Body, "Local port"),
            LOCAL_PORT_FIELD_WIDTH,
        ],
    )
}

/// In a wrapping line, start a new row first when the controls about to be
/// added, `width` wide together, would not fit on the rest of this one, so a
/// label is never left at the end of a row and its field at the start of the
/// next. At the start of a row they are added where they are.
fn keep_together(ui: &mut egui::Ui, width: f32) {
    let at_row_start = ui.available_rect_before_wrap().left() <= ui.max_rect().left() + 0.5;
    if !at_row_start && ui.available_size_before_wrap().x < width {
        ui.end_row();
    }
}

/// Parameters for [`show_addr_port_row`] — shared by all three UDP-mode
/// editors. Renders the `[addr] Port: [-] [port] [+]` strip with the same
/// hold-to-repeat ± behaviour, against per-mode fields / ids / hints /
/// validation messages.
struct AddrPortRow<'a> {
    addr_field: &'a mut String,
    addr_id_salt: &'a str,
    addr_hint: &'a str,
    addr_invalid_msg: &'a str,
    addr_hover: &'a Hover,
    bad_addr: bool,
    port_field: &'a mut String,
    port_id_salt: &'a str,
    bad_port: bool,
    port_hold: &'a mut Option<PortHold>,
}

/// Add a UDP destination's controls to the caller's line: address TextEdit,
/// "Port:" label, hold-to-repeat ± buttons around the port TextEdit. The
/// caller chooses the layout; on a wrapping line the port label, buttons and
/// field move to a new row together. Returns `true` if the user committed an
/// edit (Enter on either field, or a ± click that changed the port).
fn show_addr_port_row(ui: &mut egui::Ui, p: AddrPortRow) -> bool {
    let mut apply = false;
    let addr_r = red_bordered(ui, p.bad_addr, p.addr_invalid_msg, |ui| {
        ui.add(
            egui::TextEdit::singleline(p.addr_field)
                .id_salt(p.addr_id_salt)
                .desired_width(ADDR_FIELD_WIDTH)
                .hint_text(p.addr_hint),
        )
    })
    .on_hover(p.addr_hover);
    if enter_committed(&addr_r, ui) {
        apply = true;
    }
    keep_together(ui, port_stepper_width(ui));
    ui.label("Port:").on_hover(&UDP_PORT_HOVER);
    let r_minus = ui
        .small_button("\u{2212}")
        .on_hover_text("Decrement port (hold to accelerate)");
    let port_r = red_bordered(ui, p.bad_port, "enter a port number 1–65535", |ui| {
        ui.add(
            egui::TextEdit::singleline(p.port_field)
                .id_salt(p.port_id_salt)
                .desired_width(PORT_FIELD_WIDTH),
        )
    })
    .on_hover(&UDP_PORT_HOVER);
    if enter_committed(&port_r, ui) {
        apply = true;
    }
    let r_plus = ui
        .small_button("+")
        .on_hover_text("Increment port (hold to accelerate)");
    if drive_port_hold(ui, p.port_hold, p.port_field, &r_minus, &r_plus) {
        apply = true;
    }
    apply
}

// ── Hold-to-repeat port ± buttons ─────────────────────────────────────────────

/// Drive one frame of hold-to-repeat for the broadcast port's ± buttons.
///
/// - A simple click changes the port by exactly 1.
/// - Holding either button fires once immediately, then waits ~250 ms, then
///   auto-repeats at a rate that *accelerates* the longer the button is
///   held (see [`port_repeat_interval`]).
/// - Switching from one button to the other while held resets the state.
///
/// Uses absolute `Instant` deadlines (no per-frame `dt` accumulation), so the
/// cadence stays correct even when the framerate is jittery. Schedules the
/// next egui repaint precisely at the next fire instant via
/// `request_repaint_after`, so the loop keeps running without depending on
/// other input events.
///
/// Returns `true` if the port value changed this frame.
fn drive_port_hold(
    ui: &egui::Ui,
    hold: &mut Option<PortHold>,
    port_field: &mut String,
    r_minus: &egui::Response,
    r_plus: &egui::Response,
) -> bool {
    use std::time::{Duration, Instant};

    let mut changed = false;
    let now = Instant::now();

    // Use *global* pointer state, not Response::is_pointer_button_down_on,
    // because that per-widget flag depends on the widget's egui id being
    // present every frame — a single frame where it isn't tracked drops
    // the flag and ends the hold. Global primary_down stays true while the
    // mouse button is physically down regardless of what egui can or can't
    // see about the widget.
    let primary_pressed = ui.input(|i| i.pointer.primary_pressed());
    let primary_down = ui.input(|i| i.pointer.primary_down());

    // Initial press: pointer was just pressed AND was hovering one of our
    // buttons. Fire once and start the hold.
    if primary_pressed {
        let direction: i8 = if r_minus.hovered() {
            -1
        } else if r_plus.hovered() {
            1
        } else {
            0
        };
        if direction != 0 {
            changed |= port_step(port_field, direction);
            *hold = Some(PortHold {
                direction,
                started: now,
                next_fire_at: now + Duration::from_millis(250),
            });
        }
    }

    // Ongoing hold.
    if let Some(mut h) = *hold {
        if !primary_down {
            *hold = None;
        } else {
            // Catch up any deadlines that have already passed in a single
            // frame (handles slow frames cleanly).
            while now >= h.next_fire_at {
                let interval = port_repeat_interval(now.saturating_duration_since(h.started));
                h.next_fire_at += interval;
                changed |= port_step(port_field, h.direction);
            }
            *hold = Some(h);
            // Wake egui up exactly when the next fire is due, so the loop
            // keeps running without depending on any other input event.
            ui.ctx()
                .request_repaint_after(h.next_fire_at.saturating_duration_since(now));
        }
    }

    changed
}

/// Step a port-number string by `direction` (±1), clamped to 1..=65535.
/// Returns `true` if the value actually changed.
///
/// An empty field bootstraps to `1` on either button — otherwise the
/// buttons would silently do nothing until the user typed a starting
/// number. A non-empty but unparseable value (e.g. `444444444`) is left
/// alone so the user's typo isn't trashed.
fn port_step(port_field: &mut String, direction: i8) -> bool {
    if port_field.is_empty() {
        *port_field = "1".to_string();
        return true;
    }
    let Ok(p) = port_field.parse::<u16>() else {
        return false;
    };
    let new = match direction {
        -1 if p > 1 => p - 1,
        1 if p < u16::MAX => p + 1,
        _ => return false,
    };
    *port_field = new.to_string();
    true
}

/// Acceleration curve for the ± port hold-to-repeat.
/// Time-elapsed-since-press → delay until the next repeat.
///
/// Tiered (not exponential) so the cadence is predictable when the user is
/// targeting a specific port number. The initial 250 ms delay before the
/// first auto-repeat is handled separately in [`drive_port_hold`].
fn port_repeat_interval(elapsed: std::time::Duration) -> std::time::Duration {
    use std::time::Duration;
    match elapsed.as_secs_f32() {
        t if t < 1.0 => Duration::from_millis(100), // 10 / s for the first second
        t if t < 3.0 => Duration::from_millis(50),  // 20 / s next two seconds
        t if t < 6.0 => Duration::from_millis(25),  // 40 / s next three seconds
        _ => Duration::from_millis(10),             // 100 / s after that
    }
}

// ── Byte previews ─────────────────────────────────────────────────────────────

/// Render `bytes` as a single-line preview string.
///
/// Every printable ASCII byte (`0x20..=0x7E`) is emitted as-is; **every
/// other byte** — control characters, CR/LF, anything ≥ 0x80, and the
/// individual bytes of any multi-byte UTF-8 sequence — becomes a `‹XX›`
/// marker. This guarantees that the bundled fonts can render every glyph the
/// preview emits, so nothing tofus. The tradeoff: pretty Unicode display
/// is lost in the preview — `café` shows as `caf‹C3›‹A9›` — but the user
/// can see the exact bytes that will go on the wire, which matters more
/// for a tool like this.
///
/// Embedded `\r` and `\n` therefore appear as `‹0D›‹0A›` (visible, no
/// real line break), so the preview always renders on a single line and
/// no separate trim step is needed.
pub(super) fn preview_text(bytes: &[u8]) -> String {
    // Printable ASCII passes through; anything else (control bytes
    // and high bytes alike) is opaque to this preview, so render it
    // as the familiar `‹XX›` byte marker.
    preview_with(bytes, |b| (0x20..=0x7E).contains(&b).then_some(b as char))
}

/// Code-page preview with provenance: fallback `?` bytes receive an amber
/// background, while literal question marks remain ordinary text.
pub(super) fn preview_ascii_layout_job(
    ui: &egui::Ui,
    bytes: &[u8],
    code_page: CodePage,
    replacement_wire_offsets: &[usize],
) -> egui::text::LayoutJob {
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let normal = ui.visuals().text_color();
    let (replacement_text, replacement_background) = replacement_highlight_colors(ui);
    let mut job = egui::text::LayoutJob::default();

    for (offset, &byte) in bytes.iter().enumerate() {
        let text = match byte {
            0x00..=0x1F | 0x7F => format!("\u{2039}{byte:02X}\u{203A}"),
            _ => decode_codepage_byte(byte, code_page).to_string(),
        };
        let replaced = replacement_wire_offsets.binary_search(&offset).is_ok();
        job.append(
            &text,
            0.0,
            egui::TextFormat {
                font_id: font.clone(),
                color: if replaced { replacement_text } else { normal },
                background: if replaced {
                    replacement_background
                } else {
                    egui::Color32::TRANSPARENT
                },
                ..Default::default()
            },
        );
    }
    job
}

/// Shared byte-preview loop: emit either the caller-supplied glyph or a
/// visible `‹XX›` marker when the caller returns `None`.
fn preview_with<F: Fn(u8) -> Option<char>>(bytes: &[u8], decode: F) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match decode(b) {
            Some(c) => out.push(c),
            None => out.push_str(&format!("\u{2039}{b:02X}\u{203A}")),
        }
    }
    out
}

// ── Marker-aware text editing ─────────────────────────────────────────────────

/// `egui::TextBuffer` wrapper around a `&mut String` that **uppercases every
/// character on insert**, so a hex field's value can never momentarily contain
/// a lowercase letter (no one-frame flash between keystroke and post-hoc
/// `to_ascii_uppercase`). Used for the message editor's Hex data field.
pub(super) struct UppercaseHex<'a>(pub(super) &'a mut String);

impl egui::TextBuffer for UppercaseHex<'_> {
    fn is_mutable(&self) -> bool {
        true
    }
    fn as_str(&self) -> &str {
        self.0.as_str()
    }
    fn insert_text(&mut self, text: &str, char_index: usize) -> usize {
        let upper = text.to_ascii_uppercase();
        let byte_idx = self
            .0
            .char_indices()
            .nth(char_index)
            .map_or(self.0.len(), |(i, _)| i);
        self.0.insert_str(byte_idx, &upper);
        upper.chars().count()
    }
    fn delete_char_range(&mut self, char_range: std::ops::Range<usize>) {
        let start = self
            .0
            .char_indices()
            .nth(char_range.start)
            .map_or(self.0.len(), |(i, _)| i);
        let end = self
            .0
            .char_indices()
            .nth(char_range.end)
            .map_or(self.0.len(), |(i, _)| i);
        self.0.replace_range(start..end, "");
    }
    fn type_id(&self) -> std::any::TypeId {
        // `UppercaseHex<'a>` isn't `'static`, so we can't use `TypeId::of::<Self>()`.
        // Use a `'static` marker — egui only needs *some* stable TypeId.
        struct UppercaseHexMarker;
        std::any::TypeId::of::<UppercaseHexMarker>()
    }
}

/// Build the editor layout, including marker color and optional code-page
/// replacement backgrounds.
fn marker_layout_job(
    ui: &egui::Ui,
    text: &str,
    code_page: Option<CodePage>,
) -> egui::text::LayoutJob {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let normal = ui.visuals().text_color();
    let marker = egui::Color32::from_rgb(110, 170, 255);
    let (replacement_text, replacement_background) = replacement_highlight_colors(ui);
    let mut job = egui::text::LayoutJob::default();
    // Soft wrapping would make long pasted lines grow vertically and displace
    // unrelated controls. The surrounding two-axis ScrollArea handles long
    // lines, while LayoutJob still honors explicit newline characters.
    job.wrap.max_width = f32::INFINITY;
    for (range, segment) in segments(text) {
        match (segment, code_page) {
            (Segment::Byte(_), _) => {
                job.append(
                    &text[range],
                    0.0,
                    egui::TextFormat {
                        font_id: font.clone(),
                        color: marker,
                        ..Default::default()
                    },
                );
            }
            (Segment::Text, None) => {
                job.append(
                    &text[range],
                    0.0,
                    egui::TextFormat {
                        font_id: font.clone(),
                        color: normal,
                        ..Default::default()
                    },
                );
            }
            (Segment::Text, Some(code_page)) => {
                let chunk = &text[range];
                let mut run_start = 0;
                let mut run_replaced = None;
                for (offset, character) in chunk.char_indices() {
                    let replaced = !matches!(character, '\u{2039}' | '\u{203A}')
                        && !code_page_encodes(character, code_page);
                    if run_replaced.is_some_and(|current| current != replaced) {
                        append_code_page_run(
                            &mut job,
                            &chunk[run_start..offset],
                            run_replaced.unwrap_or(false),
                            &font,
                            normal,
                            replacement_text,
                            replacement_background,
                        );
                        run_start = offset;
                    }
                    run_replaced = Some(replaced);
                }
                append_code_page_run(
                    &mut job,
                    &chunk[run_start..],
                    run_replaced.unwrap_or(false),
                    &font,
                    normal,
                    replacement_text,
                    replacement_background,
                );
            }
        }
    }
    job
}

fn append_code_page_run(
    job: &mut egui::text::LayoutJob,
    text: &str,
    replaced: bool,
    font: &egui::FontId,
    normal: egui::Color32,
    replacement_text: egui::Color32,
    replacement_background: egui::Color32,
) {
    if text.is_empty() {
        return;
    }
    job.append(
        text,
        0.0,
        egui::TextFormat {
            font_id: font.clone(),
            color: if replaced { replacement_text } else { normal },
            background: if replaced {
                replacement_background
            } else {
                egui::Color32::TRANSPARENT
            },
            ..Default::default()
        },
    );
}

#[derive(Clone, Copy)]
enum MessageEditorLayout {
    MarkerAware(Option<CodePage>),
    Plain,
}

fn plain_editor_layout_job(ui: &egui::Ui, text: &str) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    job.append(
        text,
        0.0,
        egui::TextFormat {
            font_id: egui::TextStyle::Body.resolve(ui.style()),
            color: ui.visuals().text_color(),
            ..Default::default()
        },
    );
    job
}

/// Lay out one editor value without soft wrapping. The resulting galley is
/// both the source of truth for horizontal extent and the layout TextEdit
/// consumes, avoiding a separate glyph-by-glyph width traversal.
fn message_editor_galley(
    ui: &egui::Ui,
    text: &str,
    layout: MessageEditorLayout,
) -> Arc<egui::Galley> {
    let job = match layout {
        MessageEditorLayout::MarkerAware(code_page) => marker_layout_job(ui, text, code_page),
        MessageEditorLayout::Plain => plain_editor_layout_job(ui, text),
    };
    ui.fonts_mut(|fonts| fonts.layout_job(job))
}

/// Supplies the precomputed galley to TextEdit's first layout request. If an
/// input event changes the text, TextEdit asks again and receives a fresh
/// galley for the new value.
struct MessageEditorLayouter {
    layout: MessageEditorLayout,
    initial: Option<Arc<egui::Galley>>,
}

impl MessageEditorLayouter {
    fn new(layout: MessageEditorLayout, initial: Arc<egui::Galley>) -> Self {
        Self {
            layout,
            initial: Some(initial),
        }
    }

    fn layout(&mut self, ui: &egui::Ui, text: &str) -> Arc<egui::Galley> {
        self.initial
            .take()
            .unwrap_or_else(|| message_editor_galley(ui, text, self.layout))
    }
}

const MESSAGE_EDITOR_MIN_ROWS: usize = 3;
const MESSAGE_EDITOR_MAX_ROWS: usize = 8;

fn message_editor_visible_rows(text: &str) -> usize {
    text.split('\n')
        .count()
        .clamp(MESSAGE_EDITOR_MIN_ROWS, MESSAGE_EDITOR_MAX_ROWS)
}

fn message_editor_content_width(
    ui: &egui::Ui,
    text: &str,
    galley: &egui::Galley,
    viewport_width: f32,
) -> f32 {
    let natural_width = if galley.size().x > 0.0 {
        galley.size().x
    } else {
        // Headless tests and a temporarily unavailable fallback font can
        // report a zero-width galley. Use a conservative byte-count estimate
        // only in that exceptional path; normal frames never rescan text.
        let longest_line = text.split('\n').map(str::len).max().unwrap_or(0);
        longest_line as f32 * egui::TextStyle::Body.resolve(ui.style()).size
    };
    // TextEdit's horizontal frame margin needs a little room beyond glyphs.
    (natural_width + 12.0).max(viewport_width)
}

pub(super) fn message_editor_max_height(ui: &egui::Ui, text: &str) -> f32 {
    message_editor_height_for_rows(ui, message_editor_visible_rows(text))
}

fn message_editor_height_for_rows(ui: &egui::Ui, rows: usize) -> f32 {
    let rows = rows as f32;
    let text_height = rows * ui.text_style_height(&egui::TextStyle::Body);
    // Frame margins plus room for a horizontal scrollbar when a line is long.
    text_height + 8.0 + ui.spacing().scroll.allocated_width()
}

fn message_editor_content<R>(
    ui: &mut egui::Ui,
    salt: &'static str,
    width: f32,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let mut content_rect = ui.available_rect_before_wrap();
    content_rect.max.x = content_rect.min.x + width;
    ui.scope_builder(
        egui::UiBuilder::new()
            .id_salt(("message_editor_content", salt))
            .max_rect(content_rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        add_contents,
    )
    .inner
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TextEditContextMenuState {
    has_selection: bool,
    has_text: bool,
}

impl TextEditContextMenuState {
    fn new(text: &str, cursor_range: Option<egui::text::CCursorRange>) -> Self {
        Self {
            has_selection: cursor_range.is_some_and(|range| !range.is_empty()),
            has_text: !text.is_empty(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TextEditContextAction {
    Cut,
    Copy,
    Paste,
    SelectAll,
}

impl TextEditContextAction {
    fn viewport_command(self) -> Option<egui::ViewportCommand> {
        match self {
            Self::Cut => Some(egui::ViewportCommand::RequestCut),
            Self::Copy => Some(egui::ViewportCommand::RequestCopy),
            Self::Paste => Some(egui::ViewportCommand::RequestPaste),
            Self::SelectAll => None,
        }
    }
}

fn text_edit_context_button(
    ui: &mut egui::Ui,
    label: &str,
    key: egui::Key,
    enabled: bool,
) -> egui::Response {
    let shortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, key);
    let shortcut_text = ui.ctx().format_shortcut(&shortcut);
    ui.add_enabled(
        enabled,
        egui::Button::new(label).shortcut_text(shortcut_text),
    )
}

fn apply_text_edit_context_action(
    ui: &mut egui::Ui,
    output: &mut egui::text_edit::TextEditOutput,
    action: TextEditContextAction,
) {
    let response = &output.response.response;
    response.request_focus();

    if let Some(command) = action.viewport_command() {
        // Let TextEdit process the native clipboard event on the next frame.
        // This preserves its undo history and marker-aware post-edit repair.
        ui.ctx().send_viewport_cmd(command);
        return;
    }

    let range = egui::text::CCursorRange::select_all(&output.galley);
    output.cursor_range = Some(range);
    output.state.cursor.set_char_range(Some(range));
    output.state.clone().store(ui.ctx(), response.id);
}

fn show_text_edit_context_menu(
    ui: &mut egui::Ui,
    text: &str,
    output: &mut egui::text_edit::TextEditOutput,
) {
    let state = TextEditContextMenuState::new(text, output.cursor_range);
    let response = output.response.response.clone();
    let mut action = None;

    response.context_menu(|ui| {
        if text_edit_context_button(ui, "Cut", egui::Key::X, state.has_selection).clicked() {
            action = Some(TextEditContextAction::Cut);
            ui.close();
        }
        if text_edit_context_button(ui, "Copy", egui::Key::C, state.has_selection).clicked() {
            action = Some(TextEditContextAction::Copy);
            ui.close();
        }
        if text_edit_context_button(ui, "Paste", egui::Key::V, true).clicked() {
            action = Some(TextEditContextAction::Paste);
            ui.close();
        }
        ui.separator();
        if text_edit_context_button(ui, "Select All", egui::Key::A, state.has_text).clicked() {
            action = Some(TextEditContextAction::SelectAll);
            ui.close();
        }
    });

    // A secondary click does not inherently focus TextEdit. Focus it when the
    // menu opens and again when applying an action so Paste targets this field.
    if response.secondary_clicked() {
        response.request_focus();
    }
    if let Some(action) = action {
        apply_text_edit_context_action(ui, output, action);
    }
}

fn bounded_multiline_text_edit(
    ui: &mut egui::Ui,
    text: &mut String,
    salt: &'static str,
    layout: MessageEditorLayout,
    width: f32,
    hint: &str,
) -> egui::text_edit::TextEditOutput {
    let viewport_width = (width - ui.spacing().scroll.allocated_width()).max(64.0);
    let rows = message_editor_visible_rows(text);
    let max_height = message_editor_height_for_rows(ui, rows);

    // This is the layout TextEdit needs anyway. Read its natural width before
    // constructing the scroll content, then reuse the same Arc on TextEdit's
    // first layout request instead of walking every glyph a second time.
    let initial_galley = message_editor_galley(ui, text, layout);
    let content_width = message_editor_content_width(ui, text, &initial_galley, viewport_width);
    let mut editor_layouter = MessageEditorLayouter::new(layout, initial_galley);
    let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap_width: f32| {
        editor_layouter.layout(ui, buf.as_str())
    };

    let mut output = egui::ScrollArea::both()
        .id_salt(("message_editor_scroll", salt))
        .max_width(width)
        .max_height(max_height)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            message_editor_content(ui, salt, content_width, |ui| {
                egui::TextEdit::multiline(text)
                    .id_salt(salt)
                    .desired_width(content_width)
                    .desired_rows(rows)
                    .hint_text(hint)
                    .layouter(&mut layouter)
                    .show(ui)
            })
        })
        .inner;

    show_text_edit_context_menu(ui, text, &mut output);
    output
}

#[derive(Clone, Default)]
struct MarkerEditSnapshot {
    text: Arc<str>,
}

impl MarkerEditSnapshot {
    /// Synchronize programmatic changes (profile/channel switches and Insert
    /// popup actions) before TextEdit can mutate the value. Returns whether a
    /// copy was needed; an unchanged repaint only performs a byte comparison.
    fn sync_external(&mut self, current: &str) -> bool {
        if self.text.as_ref() == current {
            false
        } else {
            self.text = Arc::from(current);
            true
        }
    }

    fn repair_and_commit(&mut self, current: &mut String, changed: bool) -> bool {
        if changed {
            repair_after_edit(&self.text, current);
            self.text = Arc::from(current.as_str());
            true
        } else {
            false
        }
    }
}

/// Bounded multiline TextEdit for text that may contain `‹XX›` byte markers.
///
/// Three marker-aware behaviours layered on a plain `TextEdit`:
///
///  1. Coloured-marker highlighting via [`message_editor_galley`].
///  2. *Atomic* marker deletion via [`repair_after_edit`]: a single
///     keystroke that disturbs a complete marker removes the whole
///     4-character unit rather than leaving an orphan `‹` / `›`.
///  3. *Cursor jump*: if the caret lands strictly inside a marker
///     (typed-through, clicked-into, etc.), it snaps to the marker's
///     near edge — direction of movement when known, closer edge on a
///     fresh click. Markers behave as single atoms for navigation.
///
/// The pre-edit text, previous cursor position, and the widget id are
/// stashed in `egui::Memory` so [`show_insert_byte_button`] (rendered
/// from a different ui parent — the popup) can read the target's
/// cursor and write a new one after inserting markers.
pub(super) fn marker_aware_text_edit(
    ui: &mut egui::Ui,
    text: &mut String,
    salt: &'static str,
    code_page: Option<CodePage>,
    width: f32,
    hint: &str,
) -> egui::Response {
    let stash_prev_text = ui.id().with("marker_prev").with(salt);
    let stash_prev_cursor = ui.id().with("marker_prev_cursor").with(salt);
    // Shared (parent-independent) ids the insert-byte popup uses.
    let shared_cursor_id = egui::Id::new("marker_target_cursor").with(salt);
    let shared_widget_id = egui::Id::new("marker_target_widget").with(salt);

    // Cloning the snapshot clones only an Arc. Leave the stored value untouched
    // on unchanged frames; actual or programmatic edits replace it below.
    let mut snapshot = ui
        .memory(|m| m.data.get_temp::<MarkerEditSnapshot>(stash_prev_text))
        .unwrap_or_default();
    let mut store_snapshot = snapshot.sync_external(text);
    let prev_cursor: Option<usize> = ui.memory(|m| m.data.get_temp(stash_prev_cursor));

    let output = bounded_multiline_text_edit(
        ui,
        text,
        salt,
        MessageEditorLayout::MarkerAware(code_page),
        width,
        hint,
    );

    // TextEdit::show returns AtomLayoutResponse wrapping the actual
    // Response — unwrap once here so the rest reads naturally.
    let resp = output.response.response;
    store_snapshot |= snapshot.repair_and_commit(text, resp.changed());
    if store_snapshot {
        ui.memory_mut(|m| m.data.insert_temp(stash_prev_text, snapshot));
    }

    let widget_id = resp.id;
    ui.memory_mut(|m| m.data.insert_temp(shared_widget_id, widget_id));

    if let Some(range) = output.cursor_range {
        // Only snap when there's no active selection — otherwise we'd
        // yank the user's shift-arrow selection sideways.
        let has_selection = range.primary != range.secondary;
        let cursor_char = range.primary.index;
        let cursor_byte = char_to_byte(text, cursor_char);
        let mut effective_cursor_char = cursor_char;
        if !has_selection {
            for (mrange, seg) in segments(text) {
                if !matches!(seg, Segment::Byte(_)) {
                    continue;
                }
                if mrange.start < cursor_byte && cursor_byte < mrange.end {
                    let target_byte = match prev_cursor.map(|p| char_to_byte(text, p)) {
                        Some(p) if p < cursor_byte => mrange.end,
                        Some(p) if p > cursor_byte => mrange.start,
                        // Fresh click or stationary — closer edge, end on tie.
                        _ => {
                            if cursor_byte - mrange.start < mrange.end - cursor_byte {
                                mrange.start
                            } else {
                                mrange.end
                            }
                        }
                    };
                    let target_char = byte_to_char(text, target_byte);
                    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), widget_id) {
                        state
                            .cursor
                            .set_char_range(Some(egui::text::CCursorRange::one(
                                egui::text::CCursor::new(target_char),
                            )));
                        state.store(ui.ctx(), widget_id);
                    }
                    effective_cursor_char = target_char;
                    break;
                }
            }
        }
        ui.memory_mut(|m| {
            m.data.insert_temp(stash_prev_cursor, effective_cursor_char);
            m.data.insert_temp(shared_cursor_id, effective_cursor_char);
        });
    }

    resp
}

/// Plain bounded multiline TextEdit that stashes its cursor + widget id
/// under the same shared ids [`marker_aware_text_edit`] uses, so the
/// matching `Insert …` popup can find them. Use this for fields
/// that don't recognise `‹XX›` markers (UTF-16 in its default
/// Unicode mode).
pub(super) fn plain_text_edit_with_cursor(
    ui: &mut egui::Ui,
    text: &mut String,
    salt: &'static str,
    width: f32,
    hint: &str,
) -> egui::Response {
    let shared_cursor_id = egui::Id::new("marker_target_cursor").with(salt);
    let shared_widget_id = egui::Id::new("marker_target_widget").with(salt);
    let output =
        bounded_multiline_text_edit(ui, text, salt, MessageEditorLayout::Plain, width, hint);
    let resp = output.response.response;
    ui.memory_mut(|m| m.data.insert_temp(shared_widget_id, resp.id));
    if let Some(range) = output.cursor_range {
        ui.memory_mut(|m| m.data.insert_temp(shared_cursor_id, range.primary.index));
    }
    resp
}

/// Byte position of the character at `char_idx` in `text`. Saturates to
/// `text.len()` for indices past the end (treat as the after-last position).
fn char_to_byte(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(text.len())
}

/// Character position of the byte at `byte_idx`. Clamps `byte_idx` to
/// `text.len()` first so callers don't have to.
fn byte_to_char(text: &str, byte_idx: usize) -> usize {
    let byte_idx = byte_idx.min(text.len());
    text[..byte_idx].chars().count()
}

// ── Insert Byte / Insert Code Unit popups ─────────────────────────────────────

/// Parse the Insert Byte popup's hex input — single byte (`1B`) or a
/// space- and/or comma-separated list (`1B 0D 0A`, `1B,0D,0A`,
/// `1B, 0D 0A`). On failure the `Err` is the disabled-button hover text.
fn parse_hex_bytes(input: &str) -> Result<Vec<u8>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(
            "Type 1–2 hex digits — single byte (1B) or several separated by \
             spaces / commas (1B 0D 0A)"
                .to_string(),
        );
    }
    let pieces: Vec<&str> = trimmed
        .split([' ', ',', '\t'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if pieces.is_empty() {
        // All separators, no values — e.g. " , , ".
        return Err("No hex digits found between separators".to_string());
    }
    let mut out = Vec::with_capacity(pieces.len());
    for piece in &pieces {
        if piece.len() > 2 {
            return Err(format!(
                "`{piece}` is more than 2 hex digits — split bytes with a \
                 space or comma (1B 0D 0A)"
            ));
        }
        match u8::from_str_radix(piece, 16) {
            Ok(b) => out.push(b),
            Err(_) => {
                return Err(format!(
                    "`{piece}` is not a valid hex byte — use 1–2 digits 0–9 / A–F"
                ))
            }
        }
    }
    Ok(out)
}

/// Parse the UTF-16 Insert popup's input — one or more 4-hex-digit
/// code units, optionally space/comma separated (`0E16`,
/// `0E16 1F62`, `0E16, 1F62`). Each piece must be exactly 4 hex
/// digits; the active byte order is applied by the caller, not here.
fn parse_hex_units(input: &str) -> Result<Vec<u16>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(
            "Type 4 hex digits — single code unit (0E16) or several separated \
             by spaces / commas (0E16 1F62)"
                .to_string(),
        );
    }
    let pieces: Vec<&str> = trimmed
        .split([' ', ',', '\t'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if pieces.is_empty() {
        return Err("No hex digits found between separators".to_string());
    }
    let mut out = Vec::with_capacity(pieces.len());
    for piece in &pieces {
        if piece.len() != 4 {
            return Err(format!(
                "`{piece}` must be exactly 4 hex digits (one UTF-16 code unit) \
                 — use spaces or commas to separate units (0E16 1F62)"
            ));
        }
        match u16::from_str_radix(piece, 16) {
            Ok(u) => out.push(u),
            Err(_) => {
                return Err(format!(
                    "`{piece}` is not a valid hex code unit — use digits 0–9 / A–F"
                ))
            }
        }
    }
    Ok(out)
}

/// Chrome strings for the insert popup — bundled so
/// [`show_insert_popup`] doesn't take a parade of `&'static str`s.
struct InsertChrome {
    button_label: &'static str,
    field_label: &'static str,
    hint: &'static str,
}

/// "Insert Byte" button — popup inserts one or more raw bytes (each
/// wrapped in a `‹XX›` marker) at the target field's cursor. Used by
/// UTF-8, ASCII, and UTF-16 (with raw-bytes mode on) payloads.
pub(super) fn show_insert_byte_button(
    ui: &mut egui::Ui,
    text: &mut String,
    hex: &mut String,
    target_salt: &'static str,
) -> bool {
    show_insert_popup(
        ui,
        text,
        hex,
        target_salt,
        InsertChrome {
            button_label: "Insert Byte",
            field_label: "Byte value(s) (hex):",
            hint: "1B  or  1B 0D 0A",
        },
        |s| Ok(bytes_to_markers(&parse_hex_bytes(s)?)),
    )
}

/// "Insert Code Unit" button — UTF-16 variant. Each unit is 4 hex
/// digits (one `u16`). What gets inserted depends on `allow_raw_bytes`:
///
///  - `false` (default): the units decode as UTF-16 to actual
///    Unicode characters and are inserted verbatim. `0E16` inserts
///    `ฃ`, surrogate pairs are recognised, lone surrogates error.
///  - `true`: each unit splits into two raw bytes per `big_endian`
///    and is inserted as a pair of `‹XX›` markers.
pub(super) fn show_insert_unit_button(
    ui: &mut egui::Ui,
    text: &mut String,
    hex: &mut String,
    target_salt: &'static str,
    big_endian: bool,
    allow_raw_bytes: bool,
) -> bool {
    show_insert_popup(
        ui,
        text,
        hex,
        target_salt,
        InsertChrome {
            // `0E16` is Thai `ฃ` — renders because the shared font stack
            // bundles Noto Sans Thai as a fallback. CJK is *not* bundled,
            // so codepoints in U+4E00–9FFF still show as tofu.
            button_label: "Insert Code Unit",
            field_label: "Code unit(s) (4 hex):",
            hint: "0E16  or  0E16 1F62",
        },
        move |s| {
            let units = parse_hex_units(s)?;
            if allow_raw_bytes {
                let mut bytes = Vec::with_capacity(units.len() * 2);
                for u in &units {
                    if big_endian {
                        bytes.extend_from_slice(&u.to_be_bytes());
                    } else {
                        bytes.extend_from_slice(&u.to_le_bytes());
                    }
                }
                Ok(bytes_to_markers(&bytes))
            } else {
                String::from_utf16(&units).map_err(|_| {
                    "lone surrogate — pair high (D800–DBFF) and low (DC00–DFFF) \
                     surrogates together (e.g. D83D DE00 for 😀)"
                        .to_string()
                })
            }
        },
    )
}

/// Wrap each byte in a `‹XX›` marker (uppercase hex). The string is
/// what gets inserted into a marker-aware text field.
fn bytes_to_markers(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("\u{2039}{b:02X}\u{203A}"))
        .collect()
}

/// Shared MenuButton + popup chrome that the byte / code-unit insert
/// buttons hang off. `parse` turns the popup's text into the literal
/// string to splice into the target field at the cursor — bytes get
/// marker-wrapped by the caller's parser, glyph mode produces real
/// Unicode characters. Labels / hint live in [`InsertChrome`]; the
/// cursor bookkeeping is common to every caller.
///
/// `target_salt` must match the salt passed to
/// [`marker_aware_text_edit`] for the field this drives — that's how the
/// popup (rendered under a different ui parent) finds the target's
/// cursor and widget id in egui memory.
fn show_insert_popup<F>(
    ui: &mut egui::Ui,
    text: &mut String,
    hex: &mut String,
    target_salt: &'static str,
    chrome: InsertChrome,
    parse: F,
) -> bool
where
    F: Fn(&str) -> Result<String, String>,
{
    let shared_cursor_id = egui::Id::new("marker_target_cursor").with(target_salt);
    let shared_widget_id = egui::Id::new("marker_target_widget").with(target_salt);

    // Default menu close behavior is `CloseOnClick`, which closes the
    // menu the moment the user clicks anywhere inside — including the
    // TextEdit (which has to be clicked to gain focus). Switch to
    // `CloseOnClickOutside` so the popup stays open while the user
    // types the hex value.
    let mut changed = false;
    egui::containers::menu::MenuButton::new(chrome.button_label)
        .config(
            egui::containers::menu::MenuConfig::new()
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
        )
        .ui(ui, |ui| {
            // Three rows — label, hex entry, Insert button — in a fixed
            // 200-px wide child UI with a non-justified top-down layout.
            // The fixed width keeps the popup compact enough to fit
            // below the trigger button (egui's auto-placement flips
            // popups above when they'd be too wide for the space below).
            // A bare `ui.vertical` would inherit the menu's
            // `top_down_justified` layout, which stretches each row to
            // the full layout width — re-introducing the same flip.
            ui.allocate_ui_with_layout(
                egui::vec2(200.0, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.label(chrome.field_label);
                    let resp = ui.add(
                        egui::TextEdit::singleline(hex)
                            .desired_width(180.0)
                            .hint_text(chrome.hint),
                    );
                    // Auto-focus the hex field so the user can start typing
                    // right away. Idempotent — egui doesn't keep resetting
                    // the caret if the field already has focus.
                    resp.request_focus();
                    let parse_result = parse(hex);
                    let ok = parse_result.is_ok();
                    // Enter while the popup is open commits — gated on the
                    // input parsing, not on `resp.lost_focus()` (which
                    // doesn't always fire for popup-hosted TextEdits, so
                    // Enter would otherwise feel dead). Only consume Enter
                    // when the value parses.
                    let entered =
                        resp.has_focus() && ok && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let mut insert = ui.add_enabled(ok, egui::Button::new("Insert"));
                    if let Err(why) = &parse_result {
                        insert = insert.on_disabled_hover_text(why.clone());
                    }
                    if let Ok(insertion) = &parse_result {
                        if insert.clicked() || entered {
                            // Cursor byte position from memory; fall back
                            // to end-of-text if the field was never focused.
                            let cursor_char: Option<usize> =
                                ui.memory(|m| m.data.get_temp(shared_cursor_id));
                            let insert_byte = cursor_char
                                .map(|c| char_to_byte(text, c))
                                .unwrap_or(text.len());
                            text.insert_str(insert_byte, insertion);
                            changed = true;
                            // New cursor sits right after the inserted
                            // text. Update both the shared stash (so a
                            // subsequent Insert lands in the right place
                            // even if the field isn't re-focused first)
                            // and the actual TextEditState.
                            let new_cursor_char = byte_to_char(text, insert_byte + insertion.len());
                            ui.memory_mut(|m| {
                                m.data.insert_temp(shared_cursor_id, new_cursor_char);
                            });
                            let widget_id: Option<egui::Id> =
                                ui.memory(|m| m.data.get_temp(shared_widget_id));
                            if let Some(wid) = widget_id {
                                if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), wid) {
                                    state.cursor.set_char_range(Some(
                                        egui::text::CCursorRange::one(egui::text::CCursor::new(
                                            new_cursor_char,
                                        )),
                                    ));
                                    state.store(ui.ctx(), wid);
                                }
                            }
                            hex.clear();
                            ui.close();
                        }
                    }
                },
            );
        });
    changed
}

// ── Display pane ──────────────────────────────────────────────────────────────

fn output_layout_job(
    ui: &egui::Ui,
    text: &str,
    replacement_ranges: &[std::ops::Range<usize>],
) -> egui::text::LayoutJob {
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let normal = ui.visuals().text_color();
    let (replacement_text, replacement_background) = replacement_highlight_colors(ui);
    let mut job = egui::text::LayoutJob::default();
    let mut cursor = 0;
    for range in replacement_ranges {
        debug_assert!(range.start >= cursor && range.end <= text.len());
        if cursor < range.start {
            job.append(
                &text[cursor..range.start],
                0.0,
                egui::TextFormat {
                    font_id: font.clone(),
                    color: normal,
                    ..Default::default()
                },
            );
        }
        job.append(
            &text[range.clone()],
            0.0,
            egui::TextFormat {
                font_id: font.clone(),
                color: replacement_text,
                background: replacement_background,
                ..Default::default()
            },
        );
        cursor = range.end;
    }
    if cursor < text.len() || text.is_empty() {
        job.append(
            &text[cursor..],
            0.0,
            egui::TextFormat {
                font_id: font,
                color: normal,
                ..Default::default()
            },
        );
    }
    job
}

/// Render a channel's real-time outbound display pane (spec §5.7).
///
/// Three lines can head the pane. The warning and gauge describe the shared
/// live-update queue, which can affect Output and diagnostic readouts; the
/// sampling note qualifies Output alone. The shared queue has one visible home
/// here rather than being repeated in the diagnostics card. Its warning is
/// dismissible because it is a run total that keeps its last value long after
/// the pressure that caused it passed; `dismissed` carries the count the reader
/// has already acknowledged (see `super::notice`).
///
/// An unacknowledged warning also marks the section's own header. The pane is
/// collapsed by default, and moving this warning here took away the diagnostics
/// badge that used to raise it, so stating it only inside would have put the
/// condition somewhere a reader can sit in front of and never see. The header
/// carries a stable `id_salt`, so a heading that changes with the count does not
/// read as a different section and collapse itself.
pub(super) fn show_display_pane(
    ui: &mut egui::Ui,
    display: &mut ChannelDisplay,
    dismissed: &mut DismissedNotice,
    accepted_total: u64,
    dropped_updates: u64,
    queue: LiveUpdateQueueGauge,
) {
    let unacknowledged = dropped_updates > 0 && dismissed.showing(dropped_updates);
    let heading = if unacknowledged {
        egui::RichText::new(format!("Output  \u{26A0} {dropped_updates} dropped"))
            .color(wiredata_ui::palette::active(ui).warning)
    } else {
        egui::RichText::new("Output")
    };
    egui::CollapsingHeader::new(heading)
        .id_salt("output_pane")
        .show(ui, |ui| {
            // Ordered by how much each notice qualifies the pane: dropped updates
            // can cost any kind of update, sampling only costs payload lines.
            // `showing` was consulted once above, for the header: it re-arms a
            // record left from a previous run, so calling it again here would be a
            // second mutation in one frame.
            if unacknowledged {
                let hover = LIVE_UPDATE_QUEUE_TOOLTIP.layout(ui.style());
                let acknowledged = dismissible_attention_callout(
                    ui,
                    "live_update_drop_attention",
                    format!(
                        "{dropped_updates} live updates dropped; Output may omit lines and live \
                         readouts may lag"
                    ),
                    SignalTone::Warning,
                    hover,
                );
                if acknowledged {
                    dismissed.dismiss(dropped_updates);
                }
                ui.add_space(4.0);
            }
            // The payload observer is rate-limited independently of cumulative
            // counters. Drive the badge from proven accepted-vs-sampled omission
            // so a throughput estimate can neither conceal nor invent sampling.
            let sample_hz = 1.0
                / crate::core::runner::ObserverPolicy::sampled()
                    .sample_interval
                    .as_secs_f32();
            // The gauge sits with the warning it explains. This shared queue
            // also feeds live diagnostic readouts (as the tooltip says); its
            // placement does not make it an Output-only measurement.
            ui.label(
                egui::RichText::new(format!(
                    // "of", not a slash: "/" on this screen means "per".
                    "live update queue at last screen check: {} of {} · peak {} · {} dropped",
                    queue.len, queue.capacity, queue.peak, dropped_updates
                ))
                .small()
                .color(ui.visuals().weak_text_color()),
            )
            .on_hover(&LIVE_UPDATE_QUEUE_TOOLTIP);
            if display.payload_samples_omitted(accepted_total) {
                // A standing note about how the pane works, not a state to act
                // on, so it recedes with the theme: no accent, no ⚠, and the
                // same quiet weight as the queue gauge above it. The callout is
                // what carries urgency when there is any.
                ui.label(
                    egui::RichText::new(format!(
                        "sampled output · not every sent payload is shown · limit ~{sample_hz:.0}/s"
                    ))
                    .small()
                    .color(ui.visuals().weak_text_color()),
                )
                .on_hover(&Hover::from(format!(
                    "Above about {sample_hz:.0} messages a second, Output shows a sample of the \
                     messages, not every one.\n\n\
                     That keeps the pane's drawing cost the same at any send rate. This line \
                     appears once the count of sent messages shows that at least one was left \
                     out, and stays for the rest of the run. A full live update queue can also \
                     leave one out.\n\n\
                     The run's counts are exact; live readouts can lag until a later update. \
                     None of these figures shows that the data reached the wire or a receiver."
                )));
                ui.separator();
            }
            ui.horizontal(|ui| {
                ui.label("View:").on_hover(&Hover::new(
                    "The view changes only how Output shows the bytes.\n\n\
                     What is sent is the same in every view.",
                ));
                ui.radio_value(&mut display.mode, DisplayMode::Hex, "Hex");
                ui.radio_value(&mut display.mode, DisplayMode::Rendered, "Rendered");
                // Raw comes last so the ctrl-chars sub-options below sit
                // immediately next to the radio they modify.
                ui.radio_value(&mut display.mode, DisplayMode::Raw, "Raw");
                // Wrap the conditional ctrl-chars block in a stable id scope so
                // its appearance / disappearance can't shift the auto-derived
                // ids of the surrounding widgets (Clear button, etc.) and trip
                // egui's "duplicate widget id" warnings on view-mode changes.
                ui.push_id("ctrl_chars_block", |ui| {
                    if display.mode == DisplayMode::Raw {
                        ui.separator();
                        ui.label("ctrl-chars:");
                        ui.radio_value(
                            &mut display.control_style,
                            ControlStyle::Pictures,
                            "Pictures (\u{240A})",
                        );
                        ui.radio_value(&mut display.control_style, ControlStyle::Brackets, "[LF]");
                        ui.radio_value(
                            &mut display.control_style,
                            ControlStyle::HexEscapes,
                            "<0A>",
                        );
                    }
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Clear").clicked() {
                        display.clear();
                    }
                });
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .max_height(150.0)
                .stick_to_bottom(true)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    // One logical label preserves exact selection and copying:
                    // egui's multi-widget selection inserts separators between
                    // labels and cannot retain virtualized-offscreen endpoints.
                    let (text, replacement_ranges) = display.rendered();
                    let job = output_layout_job(ui, text, replacement_ranges);
                    ui.add(egui::Label::new(job).wrap().selectable(true));
                });
        });
}

// ── Validation / chrome helpers ───────────────────────────────────────────────

/// "Broken value" red-box for any text-input field. Calls `add` to
/// render the field, and when `invalid` is true tints the fill pink,
/// paints a 2-px red outline just outside the widget rect, and
/// attaches `msg` as the hover tooltip. Returns the field's
/// [`egui::Response`] unchanged so callers can still chain `lost_focus()`,
/// `changed()`, etc.
///
/// Replaces an earlier "red error row below the field" pattern that
/// pushed surrounding controls around as the user typed.
pub(super) fn red_bordered<F>(ui: &mut egui::Ui, invalid: bool, msg: &str, add: F) -> egui::Response
where
    F: FnOnce(&mut egui::Ui) -> egui::Response,
{
    /// How strongly an invalid field is washed with the fault red. Low enough
    /// that the text the user is fixing stays the most legible thing in it.
    const TINT_ALPHA: u8 = 36;

    // The shared fault red, so an invalid field, a faulted channel and a failed
    // recording are all the same red — this used to be its own `220,80,80`,
    // which its own comment described as "the rest of the GUI's warning red".
    let red = wiredata_ui::palette::active(ui).fault;
    // Derived from the same red rather than named, so the wash cannot drift
    // away from the outline it sits inside, and it lands pale on the light
    // theme and deep on the dark one without a second constant.
    let tint = wiredata_ui::palette::tint(ui, red, TINT_ALPHA);

    // No `ui.scope`: a scope inherits a wrapping layout, and a field inside
    // it then shrinks to the room left on the row instead of wrapping to the
    // next one. The fill is set on the caller's own `Ui` and put back, so the
    // field's id never depends on whether it is invalid either.
    let resp = if invalid {
        let v = ui.visuals_mut();
        let saved = (v.text_edit_bg_color, v.extreme_bg_color);
        // Pink fill via the two fields TextEdit might read:
        //  - `text_edit_bg_color` is the explicit override
        //  - `extreme_bg_color` is the fallback when the former is `None`
        v.text_edit_bg_color = Some(tint);
        v.extreme_bg_color = tint;
        let resp = add(ui);
        let v = ui.visuals_mut();
        (v.text_edit_bg_color, v.extreme_bg_color) = saved;
        resp
    } else {
        add(ui)
    };
    if invalid {
        // Explicit outline outside the rect — guarantees a visible
        // 2-px red box regardless of which `Visuals` field a given
        // egui version's TextEdit uses for its border.
        ui.painter().rect_stroke(
            resp.rect,
            egui::CornerRadius::same(2),
            egui::Stroke::new(2.0_f32, red),
            egui::StrokeKind::Outside,
        );
        resp.on_hover_text(msg)
    } else {
        resp
    }
}

/// True when the user "committed" the contents of a TextEdit by pressing
/// Enter on the way out — the pattern we use to apply interface-field
/// changes to the running talker thread.
pub(super) fn enter_committed(r: &egui::Response, ui: &egui::Ui) -> bool {
    r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
}

/// True when `s` is a non-empty string that fails to parse as `T`.
/// Used to drive the red-border / start-blocker validation: empty means
/// "user hasn't typed anything here yet" (not an error to surface),
/// whereas non-empty + parse fail means the user typed something wrong.
pub(super) fn invalid_parse<T>(s: &str) -> bool
where
    T: std::str::FromStr,
{
    !s.is_empty() && s.parse::<T>().is_err()
}

/// "Broken value" check for an IPv4-address text field, tolerant of
/// partial typing.
///
/// Empty and not-yet-complete inputs are considered OK so the field
/// doesn't flash red while the user is mid-type. Only flags red once
/// the string is unambiguously garbage:
///
///  - any character that isn't a digit or `.`
///  - more than four dot-separated parts
///  - exactly four parts with none empty, but the whole string still
///    fails to parse as [`Ipv4Addr`] (e.g. `192.168.1.999`)
///
/// In particular the LAN-prefix default `192.168.1.` (4 parts, last
/// empty) is considered "still being typed" — no red.
fn invalid_ipv4(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if s.chars().any(|c| !c.is_ascii_digit() && c != '.') {
        return true;
    }
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() > 4 {
        return true;
    }
    parts.len() == 4
        && parts.iter().all(|p| !p.is_empty())
        && s.parse::<std::net::Ipv4Addr>().is_err()
}

pub(super) fn hex_valid(s: &str) -> bool {
    let stripped: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    !stripped.is_empty()
        && stripped.len().is_multiple_of(2)
        && stripped.chars().all(|c| c.is_ascii_hexdigit())
}

pub(super) fn code_page_label(code_page: CodePage) -> &'static str {
    match code_page {
        CodePage::Iso8859_1 => "ISO-8859-1",
        CodePage::Windows1252 => "Windows-1252",
        CodePage::Cp437 => "CP437",
        CodePage::MacRoman => "Mac OS Roman",
    }
}

pub(super) fn checksum_label(algorithm: ChecksumAlgorithm) -> &'static str {
    match algorithm {
        ChecksumAlgorithm::Xor => "XOR",
        ChecksumAlgorithm::Crc8 => "CRC-8",
        ChecksumAlgorithm::Crc16Kermit => "CRC-16/KERMIT",
        ChecksumAlgorithm::Crc16Modbus => "CRC-16/MODBUS",
        ChecksumAlgorithm::Crc32 => "CRC-32",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::channel::{SerialConfig, UdpConfig};

    #[test]
    fn editor_galleys_preserve_newlines_without_soft_wrapping() {
        egui::__run_test_ui(|ui| {
            let text = format!("{}\nsecond line", "long ‹0D› ".repeat(500));
            for layout in [
                MessageEditorLayout::MarkerAware(None),
                MessageEditorLayout::Plain,
            ] {
                let galley = message_editor_galley(ui, &text, layout);
                assert_eq!(galley.rows.len(), 2, "long lines must not soft-wrap");
            }
        });
    }

    #[test]
    fn editor_layouter_reuses_the_galley_that_supplied_its_width() {
        egui::__run_test_ui(|ui| {
            let text = "a long editor line";
            let initial = message_editor_galley(ui, text, MessageEditorLayout::MarkerAware(None));
            let mut layouter = MessageEditorLayouter::new(
                MessageEditorLayout::MarkerAware(None),
                Arc::clone(&initial),
            );

            let first = layouter.layout(ui, text);
            assert!(
                Arc::ptr_eq(&first, &initial),
                "TextEdit's first request must reuse the width galley"
            );
            let second = layouter.layout(ui, "changed");
            assert_eq!(second.text(), "changed");
        });
    }

    #[test]
    fn text_edit_context_menu_enables_actions_from_editor_state() {
        let caret = egui::text::CCursorRange::one(egui::text::CCursor::new(2));
        let selection =
            egui::text::CCursorRange::two(egui::text::CCursor::new(1), egui::text::CCursor::new(3));

        assert_eq!(
            TextEditContextMenuState::new("", None),
            TextEditContextMenuState {
                has_selection: false,
                has_text: false,
            }
        );
        assert_eq!(
            TextEditContextMenuState::new("text", Some(caret)),
            TextEditContextMenuState {
                has_selection: false,
                has_text: true,
            }
        );
        assert_eq!(
            TextEditContextMenuState::new("text", Some(selection)),
            TextEditContextMenuState {
                has_selection: true,
                has_text: true,
            }
        );
    }

    #[test]
    fn text_edit_clipboard_actions_use_native_viewport_requests() {
        assert_eq!(
            TextEditContextAction::Cut.viewport_command(),
            Some(egui::ViewportCommand::RequestCut)
        );
        assert_eq!(
            TextEditContextAction::Copy.viewport_command(),
            Some(egui::ViewportCommand::RequestCopy)
        );
        assert_eq!(
            TextEditContextAction::Paste.viewport_command(),
            Some(egui::ViewportCommand::RequestPaste)
        );
        assert_eq!(TextEditContextAction::SelectAll.viewport_command(), None);
    }

    #[test]
    fn text_edit_select_all_uses_and_persists_galley_character_bounds() {
        egui::__run_test_ui(|ui| {
            let mut text = "Aé‹1B›\nZ".to_owned();
            let mut output = egui::TextEdit::multiline(&mut text).show(ui);
            let widget_id = output.response.response.id;
            let expected = egui::text::CCursorRange::select_all(&output.galley);

            apply_text_edit_context_action(ui, &mut output, TextEditContextAction::SelectAll);

            assert_eq!(output.cursor_range, Some(expected));
            let stored = egui::TextEdit::load_state(ui.ctx(), widget_id)
                .expect("Select All must persist the TextEdit cursor state");
            assert_eq!(stored.cursor.char_range(), Some(expected));
            assert!(ui.memory(|memory| memory.has_focus(widget_id)));
        });
    }

    #[test]
    fn marker_snapshot_copies_only_when_text_changes() {
        let mut snapshot = MarkerEditSnapshot::default();
        assert!(snapshot.sync_external("A‹1B›B"));
        let unchanged = Arc::clone(&snapshot.text);

        assert!(!snapshot.sync_external("A‹1B›B"));
        assert!(Arc::ptr_eq(&snapshot.text, &unchanged));

        let mut edited = "A‹X1B›B".to_string();
        assert!(snapshot.repair_and_commit(&mut edited, true));
        assert_eq!(edited, "AB");
        assert_eq!(snapshot.text.as_ref(), "AB");

        assert!(snapshot.sync_external("profile replacement"));
        assert!(!snapshot.sync_external("profile replacement"));
    }

    #[test]
    fn replacement_background_distinguishes_fallback_from_literal_question_mark() {
        egui::__run_test_ui(|ui| {
            let editor = marker_layout_job(ui, "?—", Some(CodePage::Iso8859_1));
            assert_eq!(&editor.text[editor.sections[0].byte_range.clone()], "?");
            assert_eq!(
                editor.sections[0].format.background,
                egui::Color32::TRANSPARENT
            );
            assert_eq!(&editor.text[editor.sections[1].byte_range.clone()], "—");
            assert_ne!(
                editor.sections[1].format.background,
                egui::Color32::TRANSPARENT
            );

            let preview = preview_ascii_layout_job(ui, b"??", CodePage::Iso8859_1, &[1]);
            assert_eq!(
                preview.sections[0].format.background,
                egui::Color32::TRANSPARENT
            );
            assert_ne!(
                preview.sections[1].format.background,
                egui::Color32::TRANSPARENT
            );
        });
    }

    #[test]
    fn replacement_background_has_contrast_in_both_themes_and_output() {
        egui::__run_test_ui(|ui| {
            ui.visuals_mut().dark_mode = false;
            let (light_text, light_background) = replacement_highlight_colors(ui);
            assert_eq!(light_text, egui::Color32::from_rgb(45, 30, 0));
            assert_eq!(light_background, egui::Color32::from_rgb(255, 225, 150));

            let replacement_range = 1..2;
            let output = output_layout_job(ui, "??", std::slice::from_ref(&replacement_range));
            assert_eq!(
                output.sections[0].format.background,
                egui::Color32::TRANSPARENT
            );
            assert_eq!(output.sections[1].format.color, light_text);
            assert_eq!(output.sections[1].format.background, light_background);

            ui.visuals_mut().dark_mode = true;
            let (dark_text, dark_background) = replacement_highlight_colors(ui);
            assert_eq!(dark_text, egui::Color32::BLACK);
            assert_eq!(dark_background, wiredata_ui::palette::DARK.warning);
        });
    }

    #[test]
    fn message_editor_grows_for_newlines_then_caps_its_viewport() {
        assert_eq!(message_editor_visible_rows("one line"), 3);
        assert_eq!(message_editor_visible_rows("1\n2\n3\n4\n5"), 5);
        assert_eq!(message_editor_visible_rows(&"line\n".repeat(20)), 8);

        egui::__run_test_ui(|ui| {
            ui.set_width(500.0);
            let mut text = format!("{}\n{}", "wide ".repeat(500), "line\n".repeat(20));
            let before = ui.min_rect().bottom();
            let max_height = message_editor_max_height(ui, &text);
            let galley = message_editor_galley(ui, &text, MessageEditorLayout::MarkerAware(None));
            let content_width = message_editor_content_width(ui, &text, &galley, 300.0);
            let response =
                marker_aware_text_edit(ui, &mut text, "bounded_editor_test", None, 300.0, "text");
            let consumed = ui.min_rect().bottom() - before;
            assert!(
                consumed <= max_height + 1.0,
                "editor consumed {consumed} px despite a {max_height} px cap"
            );
            assert!(
                response.rect.width() > 300.0,
                "long lines must scroll: response={} content={content_width}",
                response.rect.width()
            );
            assert!(
                response.rect.height() > 8.0 * ui.text_style_height(&egui::TextStyle::Body),
                "all explicit lines should remain in scrollable content"
            );
        });
    }

    #[test]
    fn plain_message_editor_uses_the_same_bounded_scroll_geometry() {
        egui::__run_test_ui(|ui| {
            ui.set_width(500.0);
            let mut text = format!("{}\n{}", "wide ".repeat(500), "line\n".repeat(20));
            let before = ui.min_rect().bottom();
            let max_height = message_editor_max_height(ui, &text);
            let response =
                plain_text_edit_with_cursor(ui, &mut text, "plain_editor_test", 300.0, "text");
            let consumed = ui.min_rect().bottom() - before;

            assert!(consumed <= max_height + 1.0);
            assert!(response.rect.width() > 300.0);
            assert!(response.rect.height() > 8.0 * ui.text_style_height(&egui::TextStyle::Body));
        });
    }

    // ── parse_hex_bytes ───────────────────────────────────────────────────────

    #[test]
    fn parse_single_byte() {
        assert_eq!(parse_hex_bytes("1B").unwrap(), vec![0x1B]);
        assert_eq!(parse_hex_bytes("ff").unwrap(), vec![0xFF]);
        assert_eq!(parse_hex_bytes("0").unwrap(), vec![0x00]);
    }

    #[test]
    fn parse_space_separated() {
        assert_eq!(parse_hex_bytes("1B 0D 0A").unwrap(), vec![0x1B, 0x0D, 0x0A]);
    }

    #[test]
    fn parse_comma_separated() {
        assert_eq!(parse_hex_bytes("1B,0D,0A").unwrap(), vec![0x1B, 0x0D, 0x0A]);
    }

    #[test]
    fn parse_mixed_separators_and_extra_whitespace() {
        assert_eq!(
            parse_hex_bytes("  1B,  0D 0A,   FF  ").unwrap(),
            vec![0x1B, 0x0D, 0x0A, 0xFF]
        );
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(parse_hex_bytes("").is_err());
        assert!(parse_hex_bytes("   ").is_err());
        assert!(parse_hex_bytes(" , , ").is_err());
    }

    #[test]
    fn parse_rejects_non_hex() {
        let err = parse_hex_bytes("1B XY 0D").unwrap_err();
        assert!(err.contains("XY"), "{err}");
    }

    #[test]
    fn parse_rejects_too_long_piece() {
        let err = parse_hex_bytes("1B0D").unwrap_err();
        assert!(err.contains("more than 2"), "{err}");
    }

    // ── parse_hex_units ───────────────────────────────────────────────────────

    #[test]
    fn parse_units_single_and_multiple() {
        assert_eq!(parse_hex_units("0E16").unwrap(), vec![0x0E16]);
        assert_eq!(parse_hex_units("0E16 1F62").unwrap(), vec![0x0E16, 0x1F62]);
        assert_eq!(parse_hex_units("0E16,1F62").unwrap(), vec![0x0E16, 0x1F62]);
        assert_eq!(
            parse_hex_units("  0E16,  1F62  ").unwrap(),
            vec![0x0E16, 0x1F62]
        );
    }

    #[test]
    fn parse_units_rejects_wrong_length() {
        // 3 digits, 5 digits, and a missing space between two units.
        for input in ["E16", "01F62", "0E161F62"] {
            let err = parse_hex_units(input).unwrap_err();
            assert!(err.contains("exactly 4 hex digits"), "{input}: {err}");
        }
    }

    #[test]
    fn parse_units_rejects_non_hex() {
        let err = parse_hex_units("XYZW").unwrap_err();
        assert!(err.contains("XYZW"), "{err}");
    }

    #[test]
    fn parse_units_rejects_empty() {
        assert!(parse_hex_units("").is_err());
        assert!(parse_hex_units("   ").is_err());
        assert!(parse_hex_units(", ,").is_err());
    }

    // ── char_to_byte / byte_to_char ───────────────────────────────────────────

    #[test]
    fn char_byte_round_trip_ascii() {
        let s = "hello";
        for i in 0..=s.len() {
            assert_eq!(byte_to_char(s, char_to_byte(s, i)), i.min(5));
        }
    }

    #[test]
    fn char_byte_handles_multibyte() {
        // 'A' (1 byte/char) + '‹' (3 bytes/1 char) + 'B' (1 byte/char)
        let s = "A\u{2039}B";
        assert_eq!(char_to_byte(s, 0), 0);
        assert_eq!(char_to_byte(s, 1), 1);
        assert_eq!(char_to_byte(s, 2), 4);
        assert_eq!(char_to_byte(s, 3), 5); // saturates to len
        assert_eq!(byte_to_char(s, 0), 0);
        assert_eq!(byte_to_char(s, 1), 1);
        assert_eq!(byte_to_char(s, 4), 2);
        assert_eq!(byte_to_char(s, 5), 3);
        assert_eq!(byte_to_char(s, 99), 3); // clamps past end
    }

    // ── invalid_ipv4 ──────────────────────────────────────────────────────────

    #[test]
    fn ipv4_empty_is_not_invalid() {
        assert!(!invalid_ipv4(""));
    }

    #[test]
    fn ipv4_partial_typing_is_not_invalid() {
        // The user is mid-typing; don't flash red yet.
        for s in [
            "1",
            "19",
            "192",
            "192.",
            "192.168",
            "192.168.1",
            "192.168.1.",
        ] {
            assert!(!invalid_ipv4(s), "{s:?} should be treated as partial");
        }
    }

    #[test]
    fn ipv4_complete_valid_is_not_invalid() {
        for s in ["0.0.0.0", "192.168.1.5", "255.255.255.255"] {
            assert!(!invalid_ipv4(s), "{s:?} parses as Ipv4Addr");
        }
    }

    #[test]
    fn ipv4_garbage_chars_are_invalid() {
        for s in ["abc", "192.168.1.a", "192-168-1-5", "192.168.1.5 "] {
            assert!(invalid_ipv4(s), "{s:?} contains non-IPv4 characters");
        }
    }

    #[test]
    fn ipv4_too_many_parts_or_out_of_range_is_invalid() {
        for s in ["192.168.1.5.6", "192.168.1.300", "1..2.3.4"] {
            assert!(invalid_ipv4(s), "{s:?} can never be a valid Ipv4Addr");
        }
    }

    #[test]
    fn lifecycle_indicator_maps_states_to_the_shared_glyph_set() {
        use wiredata_ui::glyphs;
        use wiredata_ui::palette::LIGHT;
        let (g, c, w) = lifecycle_indicator(true, false, &LIGHT);
        assert_eq!((g, w), (glyphs::RUNNING, "running"));
        assert_eq!(c, LIGHT.running);
        // Running with a live error: the ⚠ carries the alarm, the word stays
        // honest about the run state; the detail header prints the error below.
        let (g, c, w) = lifecycle_indicator(true, true, &LIGHT);
        assert_eq!((g, w), (glyphs::FAULT, "running"));
        assert_eq!(c, LIGHT.fault);
        let (g, _, w) = lifecycle_indicator(false, true, &LIGHT);
        assert_eq!((g, w), (glyphs::FAULT, "faulted"));
        let (g, c, w) = lifecycle_indicator(false, false, &LIGHT);
        assert_eq!((g, w), (glyphs::STOPPED, "stopped"));
        assert_eq!(c, LIGHT.idle);
    }

    #[test]
    fn start_button_matches_listener_decision_table() {
        // Stopped, valid → plain Start, enabled.
        assert_eq!(
            start_button(false, false, false, true),
            ("Start channel", true)
        );
        // Stopped, invalid draft → Start, disabled (blockers on hover).
        assert_eq!(
            start_button(false, false, false, false),
            ("Start channel", false)
        );
        // Stopped with an error from the last run/open → Retry.
        assert_eq!(
            start_button(false, true, false, true),
            ("Retry channel", true)
        );
        assert_eq!(
            start_button(false, true, false, false),
            ("Retry channel", false)
        );
        // Drift on a stopped channel is irrelevant — Start applies drafts anyway.
        assert_eq!(
            start_button(false, false, true, true),
            ("Start channel", true)
        );
        // Running with valid pending edits → the coordinated restart.
        assert_eq!(
            start_button(true, false, true, true),
            ("Apply & Restart", true)
        );
        // An invalid replacement must not interrupt the active run.
        assert_eq!(
            start_button(true, true, true, false),
            ("Apply & Restart", false)
        );
        // Running, nothing to apply → disabled Start.
        assert_eq!(
            start_button(true, false, false, false),
            ("Start channel", false)
        );
    }

    #[test]
    fn malformed_ascii_marker_is_an_exact_message_blocker() {
        let draft = ScheduleDraft {
            payload_kind: PayloadKind::Ascii,
            ascii_text: "active‹replacement".to_string(),
            ..ScheduleDraft::default()
        };

        let analysis = MessageDraftAnalysis::build(&draft);
        let mut blockers = Vec::new();
        let _ = message_blockers(0, &draft, &analysis, &mut |blocker| {
            blockers.push(blocker());
            ControlFlow::Continue(())
        });
        assert_eq!(blockers.len(), 1, "blockers: {blockers:?}");
        assert!(blockers[0].starts_with("Message 1:"));
        assert!(blockers[0].contains("complete ‹XX› byte marker"));
    }

    #[test]
    fn blocker_predicate_agrees_with_the_collected_list() {
        // The bool fast path (`any_start_blocker_analyzed`) and the
        // string-collecting path walk the same visitor; pin that they can't
        // drift apart across a few representative draft states.
        let valid_conn = || {
            let mut conn = ConnDraft::new(ConnKind::Udp);
            conn.udp_unicast.addr = "127.0.0.1".to_string();
            conn.udp_unicast.port = "9000".to_string();
            conn
        };
        let invalid_conn = || {
            let mut conn = ConnDraft::new(ConnKind::Udp);
            conn.udp_unicast.addr.clear(); // empty address
            conn
        };
        let msg = |interval: &str| ScheduleDraft {
            payload_kind: PayloadKind::Utf8,
            utf8_text: "hello".to_string(),
            interval_ms: interval.to_string(),
            ..ScheduleDraft::default()
        };
        for (conn, msgs) in [
            (valid_conn(), vec![msg("100")]),
            (valid_conn(), vec![msg("not-a-number")]),
            (invalid_conn(), vec![msg("100")]),
            (valid_conn(), vec![]),
            (valid_conn(), vec![msg("100"), msg("not-a-number")]),
        ] {
            let conn = &conn;
            let analyses: Vec<MessageAnalysisCache> = msgs
                .iter()
                .map(|d| {
                    let mut cache = MessageAnalysisCache::default();
                    cache.refresh(d);
                    cache
                })
                .collect();
            let listed = start_blockers_analyzed(conn, &msgs, &analyses);
            assert_eq!(
                any_start_blocker_analyzed(conn, &msgs, &analyses),
                !listed.is_empty(),
                "predicate disagrees with list {listed:?}"
            );
        }
    }

    #[test]
    fn serial_overload_is_advisory_and_does_not_disable_start() {
        let mut conn = ConnDraft::new(ConnKind::Serial);
        conn.serial_port = "COM1".to_owned();
        conn.baud_rate = 4_800;
        let message = ScheduleDraft {
            payload_kind: PayloadKind::Utf8,
            utf8_text: "X".repeat(10_000),
            interval_ms: "1".to_owned(),
            ..ScheduleDraft::default()
        };
        let mut analysis = MessageAnalysisCache::default();
        analysis.refresh(&message);

        assert!(start_blockers_analyzed(&conn, &[message], &[analysis]).is_empty());
    }

    // ── The serial editor on the shared lines ─────────────────────────────────

    fn channel_blocker_texts(conn: &ConnDraft) -> Vec<String> {
        let mut texts = Vec::new();
        let _ = channel_blockers(conn, &mut |blocker| {
            texts.push(blocker());
            ControlFlow::Continue(())
        });
        texts
    }

    /// The shared baud box applies only a positive rate, so a rate of zero can
    /// come only from a saved profile, and it still blocks Start.
    #[test]
    fn a_zero_baud_rate_blocks_start() {
        let mut conn = ConnDraft::new(ConnKind::Serial);
        conn.serial_port = "COM1".to_owned();
        assert!(channel_blocker_texts(&conn).is_empty());
        conn.baud_rate = 0;
        assert_eq!(
            channel_blocker_texts(&conn),
            ["Channel: baud rate must be a positive number"]
        );
    }

    fn serial_interface(flow: FlowControl) -> InterfaceConfig {
        let mut config = SerialConfig::new("COM3");
        config.flow_control = flow;
        InterfaceConfig::Serial(config)
    }

    /// RTS has words instead of a button only under RTS/CTS, and which words
    /// is the platform's fact (`RTS_HIGH_UNDER_RTS_CTS`). Only a running
    /// serial port has any.
    #[test]
    fn rts_has_words_only_under_rts_cts() {
        for flow in [FlowControl::None, FlowControl::Software] {
            assert_eq!(rts_by_flow(Some(&serial_interface(flow))), None, "{flow:?}");
        }
        let expected = if RTS_HIGH_UNDER_RTS_CTS {
            RtsByFlow::High
        } else {
            RtsByFlow::Paced
        };
        assert_eq!(
            rts_by_flow(Some(&serial_interface(FlowControl::Hardware))),
            Some(expected)
        );
        assert_eq!(rts_by_flow(None), None, "no running port");
        let udp = InterfaceConfig::Udp(UdpConfig::unicast("127.0.0.1:9".parse().unwrap()));
        assert_eq!(rts_by_flow(Some(&udp)), None, "a UDP channel has no RTS");
    }

    #[test]
    fn the_control_lines_map_field_for_field() {
        let lines = control_lines(ControlLines {
            rts: true,
            dtr: false,
            cts: true,
            dsr: false,
            dcd: true,
            ri: false,
        });
        assert_eq!(
            (lines.rts, lines.dtr, lines.cts, lines.dsr, lines.dcd, lines.ri),
            (true, false, true, false, true, false)
        );
        let flipped = control_lines(ControlLines {
            rts: false,
            dtr: true,
            cts: false,
            dsr: true,
            dcd: false,
            ri: true,
        });
        assert_eq!(
            (
                flipped.rts,
                flipped.dtr,
                flipped.cts,
                flipped.dsr,
                flipped.dcd,
                flipped.ri
            ),
            (false, true, false, true, false, true)
        );
    }

    /// The lines are live only while the channel runs and has a reading. A
    /// stopped channel's last reading is stale, and a running one with none
    /// (the first reading is not in, or the port cannot be read) has nothing
    /// to show.
    #[test]
    fn the_lines_are_live_only_while_the_channel_runs_with_a_reading() {
        let reading = ControlLines {
            rts: true,
            cts: true,
            ..ControlLines::default()
        };
        assert_eq!(
            live_lines(true, Some(reading)),
            Some(control_lines(reading)),
            "running with a reading"
        );
        assert_eq!(live_lines(false, Some(reading)), None, "not running");
        assert_eq!(live_lines(true, None), None, "running, no reading");
        assert_eq!(live_lines(false, None), None, "not running, no reading");
    }

    /// A click asks the supervisor for the line it was on and the level it
    /// named, which is the opposite of the level shown.
    #[test]
    fn a_click_asks_for_the_line_and_level_it_names() {
        assert_eq!(
            output_request(LineClick::Rts(true)),
            (OutputLine::Rts, true)
        );
        assert_eq!(
            output_request(LineClick::Rts(false)),
            (OutputLine::Rts, false)
        );
        assert_eq!(
            output_request(LineClick::Dtr(true)),
            (OutputLine::Dtr, true)
        );
        assert_eq!(
            output_request(LineClick::Dtr(false)),
            (OutputLine::Dtr, false)
        );
    }

    /// Each channel's typed port and baud text lives under its own id, so two
    /// channels' editors never share a half-typed value.
    #[test]
    fn two_channels_never_share_typed_text() {
        let (a, b) = (ChannelId::mint(), ChannelId::mint());
        assert_ne!(serial_editor_id(Some(a)), serial_editor_id(Some(b)));
        assert_eq!(serial_editor_id(Some(a)), serial_editor_id(Some(a)));
    }

    // ── The narrow detail pane ────────────────────────────────────────────────

    /// What a pane of some width shows of an editor drawn into it.
    struct Pane {
        /// Rows of controls the editor took.
        rows: usize,
        /// The right edge the editor reached.
        drawn_right: f32,
        /// The right edge the pane had.
        room_right: f32,
    }

    impl Pane {
        fn fits(&self) -> bool {
            self.drawn_right <= self.room_right + 0.5
        }
    }

    /// Draw an editor into a `width`-point pane, as the detail pane does.
    fn in_pane(width: f32, mut draw: impl FnMut(&mut egui::Ui)) -> Pane {
        let ctx = egui::Context::default();
        let mut pane = None;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.set_max_width(width);
            // Before drawing: a Ui grows its max rect to take in what overflows it.
            let room_right = ui.max_rect().right();
            draw(ui);
            let used = ui.min_rect();
            let row = ui.spacing().interact_size.y;
            let gap = ui.spacing().item_spacing.y;
            pane = Some(Pane {
                rows: ((used.height() + gap) / (row + gap)).round() as usize,
                drawn_right: used.right(),
                room_right,
            });
        });
        pane.expect("the frame ran")
    }

    fn serial_draft() -> ConnDraft {
        let mut conn = ConnDraft::new(ConnKind::Serial);
        conn.serial_port = "COM3".to_owned();
        conn.flow_control = 2;
        conn
    }

    /// The serial editor as the detail pane draws it, with the lines live and
    /// RTS in words (the widest case) or greyed out with their sentence.
    fn serial_pane(width: f32, live: bool) -> Pane {
        let mut conn = serial_draft();
        let ports = ["COM3".to_owned(), "COM14".to_owned()];
        in_pane(width, |ui| {
            let lines = live.then(shared::ControlLines::default);
            let words = live.then_some(RtsByFlow::High);
            let id = serial_editor_id(Some(ChannelId::mint()));
            show_serial_fields(ui, id, &mut conn, Some(&ports), lines, words);
        })
    }

    /// The UDP editor as the detail pane draws it, with every field valid or
    /// every field in the red-border state, and a long adapter name listed.
    fn udp_pane(width: f32, mode: UdpModeDraft, invalid: bool) -> Pane {
        let mut conn = ConnDraft::new(ConnKind::Udp);
        conn.udp_mode = mode;
        conn.local_port = if invalid { "99999" } else { "50000" }.to_owned();
        conn.udp_multicast_ttl = if invalid { "999" } else { "255" }.to_owned();
        let addresses = [
            LocalAddress {
                ip: Ipv4Addr::new(192, 168, 100, 200),
                interface: "vEthernet (Default Switch)".to_owned(),
            },
            LocalAddress {
                ip: Ipv4Addr::LOCALHOST,
                interface: "Loopback Pseudo-Interface 1".to_owned(),
            },
        ];
        conn.udp_multicast_interface = Some(addresses[0].ip);
        for pair in [
            &mut conn.udp_unicast,
            &mut conn.udp_broadcast,
            &mut conn.udp_multicast,
        ] {
            pair.addr = if invalid {
                "300.300.300.300"
            } else {
                "239.255.255.250"
            }
            .to_owned();
            pair.port = if invalid { "99999" } else { "65535" }.to_owned();
        }
        in_pane(width, |ui| {
            show_udp_fields(ui, &mut conn, Some(&addresses));
        })
    }

    /// In a channel's detail pane, about 320 points wide, a serial channel's
    /// two lines wrap rather than run off the edge.
    #[test]
    fn a_serial_channel_fits_a_narrow_pane() {
        for live in [false, true] {
            let pane = serial_pane(320.0, live);
            assert!(
                pane.fits(),
                "the editor runs past the pane (live: {live}): {} against {}",
                pane.drawn_right,
                pane.room_right
            );
            assert!(
                pane.rows > 2,
                "a pane this narrow wraps the two lines (live: {live}), took {} rows",
                pane.rows
            );
        }
    }

    /// With the room, each serial line is one row.
    #[test]
    fn each_serial_line_is_one_row_when_there_is_room() {
        for live in [false, true] {
            let pane = serial_pane(1400.0, live);
            assert!(pane.fits(), "the editor runs past the pane (live: {live})");
            assert_eq!(pane.rows, 2, "two lines, one row each (live: {live})");
        }
    }

    /// A UDP channel's two lines wrap in the same pane, in every mode.
    #[test]
    fn a_udp_channel_fits_a_narrow_pane() {
        for mode in [
            UdpModeDraft::Unicast,
            UdpModeDraft::Broadcast,
            UdpModeDraft::Multicast,
        ] {
            for invalid in [false, true] {
                let pane = udp_pane(320.0, mode, invalid);
                assert!(
                    pane.fits(),
                    "the editor runs past the pane (invalid: {invalid}): {} against {}",
                    pane.drawn_right,
                    pane.room_right
                );
                assert!(
                    pane.rows > 2,
                    "a pane this narrow wraps the two lines, took {} rows",
                    pane.rows
                );
            }
        }
    }

    /// With the room, each UDP line is one row: the address and ports, the
    /// mode, and in Multicast the outgoing interface and TTL.
    #[test]
    fn each_udp_line_is_one_row_when_there_is_room() {
        for mode in [
            UdpModeDraft::Unicast,
            UdpModeDraft::Broadcast,
            UdpModeDraft::Multicast,
        ] {
            for invalid in [false, true] {
                let pane = udp_pane(1400.0, mode, invalid);
                assert!(pane.fits());
                let lines = if mode == UdpModeDraft::Multicast {
                    3
                } else {
                    2
                };
                assert_eq!(pane.rows, lines, "one row per line");
            }
        }
    }

    /// The red-border tint is on the field and only the field: the style is
    /// put back, so the fields after it are not tinted.
    #[test]
    fn an_invalid_field_leaves_the_style_as_it_found_it() {
        egui::__run_test_ui(|ui| {
            let before = (
                ui.visuals().text_edit_bg_color,
                ui.visuals().extreme_bg_color,
            );
            let mut text = String::new();
            let mut during = None;
            red_bordered(ui, true, "bad", |ui| {
                during = Some((
                    ui.visuals().text_edit_bg_color,
                    ui.visuals().extreme_bg_color,
                ));
                ui.add(egui::TextEdit::singleline(&mut text))
            });
            assert_ne!(
                during,
                Some(before),
                "the field is tinted while it is drawn"
            );
            assert_eq!(
                (
                    ui.visuals().text_edit_bg_color,
                    ui.visuals().extreme_bg_color
                ),
                before,
                "and the tint is taken off after it"
            );
        });
    }

    /// Where a `width`-wide control lands in a 320-point wrapping line that
    /// already holds a `filler`-wide one, after asking to keep it together.
    /// Returns its top, and the line's top.
    fn kept_together(filler: f32, width: f32) -> (f32, f32) {
        let ctx = egui::Context::default();
        let mut tops = None;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.set_max_width(320.0);
            ui.horizontal_wrapped(|ui| {
                let top = ui.cursor().top();
                if filler > 0.0 {
                    ui.allocate_space(egui::vec2(filler, 20.0));
                }
                keep_together(ui, width);
                let placed = ui.allocate_space(egui::vec2(width, 20.0)).1;
                tops = Some((placed.top(), top));
            });
        });
        tops.expect("the frame ran")
    }

    /// Controls that belong together are moved to a new row whole, never
    /// split across two, and only when they do not fit on this one.
    #[test]
    fn a_group_that_does_not_fit_starts_a_new_row() {
        let (placed, top) = kept_together(100.0, 100.0);
        assert_eq!(placed, top, "there is room, so it stays on the row");
        let (placed, top) = kept_together(250.0, 100.0);
        assert!(placed > top, "no room, so it starts a row of its own");
        let (placed, top) = kept_together(0.0, 400.0);
        assert_eq!(
            placed, top,
            "at the start of a row it is not pushed down, however wide"
        );
    }

    /// The width the line allows for a group is the width the group takes,
    /// or it would wrap too early or split anyway.
    #[test]
    fn the_widths_the_line_allows_are_the_widths_it_takes() {
        let ctx = egui::Context::default();
        let mut measured = None;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.set_max_width(1400.0);
            let mut addr = String::new();
            let mut port = String::new();
            let mut hold = None;
            let mut conn = ConnDraft::new(ConnKind::Udp);
            let stepper = ui
                .horizontal_wrapped(|ui| {
                    show_addr_port_row(
                        ui,
                        AddrPortRow {
                            addr_field: &mut addr,
                            addr_id_salt: "a",
                            addr_hint: "",
                            addr_invalid_msg: "",
                            addr_hover: &UNICAST_DESTINATION_HOVER,
                            bad_addr: false,
                            port_field: &mut port,
                            port_id_salt: "p",
                            bad_port: false,
                            port_hold: &mut hold,
                        },
                    );
                })
                .response
                .rect
                .width();
            let local = ui
                .horizontal_wrapped(|ui| {
                    show_udp_local_port(ui, &mut conn);
                })
                .response
                .rect
                .width();
            let gap = ui.spacing().item_spacing.x;
            measured = Some((
                stepper - ADDR_FIELD_WIDTH - gap,
                port_stepper_width(ui),
                local,
                local_port_width(ui),
            ));
        });
        let (stepper, allowed_stepper, local, allowed_local) = measured.expect("the frame ran");
        assert!(
            (stepper - allowed_stepper).abs() < 0.5,
            "the port stepper takes {stepper}, the line allows {allowed_stepper}"
        );
        assert!(
            (local - allowed_local).abs() < 0.5,
            "the local port takes {local}, the line allows {allowed_local}"
        );
    }

    #[test]
    fn the_interface_choices_name_the_default_and_each_address() {
        let listed = [
            LocalAddress {
                ip: Ipv4Addr::new(192, 168, 1, 20),
                interface: "Ethernet".to_owned(),
            },
            LocalAddress {
                ip: Ipv4Addr::LOCALHOST,
                interface: "lo".to_owned(),
            },
        ];
        assert_eq!(
            multicast_interface_choices(None, Some(&listed)),
            [
                (None, "System default".to_owned()),
                (
                    Some(Ipv4Addr::new(192, 168, 1, 20)),
                    "192.168.1.20 \u{2014} Ethernet".to_owned()
                ),
                (
                    Some(Ipv4Addr::LOCALHOST),
                    "127.0.0.1 \u{2014} this computer only".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn a_configured_interface_is_kept_and_says_when_it_is_not_here() {
        let gone = Some(Ipv4Addr::new(10, 9, 9, 9));
        let listed = [LocalAddress {
            ip: Ipv4Addr::new(192, 168, 1, 20),
            interface: "Ethernet".to_owned(),
        }];
        assert_eq!(
            multicast_interface_choices(gone, Some(&listed)).last(),
            Some(&(
                gone,
                "10.9.9.9 \u{2014} not on this computer now".to_owned()
            ))
        );
        // Before the list arrives, nothing is known to be missing.
        assert_eq!(
            multicast_interface_choices(gone, None),
            [
                (None, "System default".to_owned()),
                (gone, "10.9.9.9".to_owned())
            ]
        );
    }

    /// A frame in which nothing was touched reports nothing, so it never
    /// applies a change to a running channel.
    #[test]
    fn an_untouched_editor_reports_no_change() {
        let ctx = egui::Context::default();
        let mut serial = serial_draft();
        let mut udp = ConnDraft::new(ConnKind::Udp);
        udp.udp_unicast.addr = "192.168.1.50".to_owned();
        udp.udp_unicast.port = "4000".to_owned();
        let before = serial.serial_settings();
        let id = serial_editor_id(Some(ChannelId::mint()));
        for _ in 0..3 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                let edit = show_serial_fields(ui, id, &mut serial, None, None, None);
                assert_eq!(edit, InterfaceEdit::default());
                assert_eq!(
                    show_udp_fields(ui, &mut udp, None),
                    InterfaceEdit::default()
                );
            });
        }
        assert_eq!(serial.serial_settings(), before);
    }
}
