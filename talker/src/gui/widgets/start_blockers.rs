//! The interface summary line and the reasons Start is disabled. Kept
//! together: the summary's `?` marks must match the blockers exactly.

use std::{net::Ipv4Addr, ops::ControlFlow};

use wiredata_ui::hover::{Hover, ShowHover};

use crate::gui::draft::{
    multicast_ttl, ConnDraft, ConnKind, PayloadKind, ScheduleDraft, UdpModeDraft,
};
use crate::gui::{MessageAnalysisCache, MessageDraftAnalysis};

use super::*;

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
pub(in crate::gui) fn show_interface_summary(ui: &mut egui::Ui, conn: &ConnDraft) {
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
pub(in crate::gui) fn interface_summary(conn: &ConnDraft) -> String {
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
pub(in crate::gui) fn start_blockers_analyzed(
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
pub(in crate::gui) fn any_start_blocker_analyzed(
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

#[cfg(test)]
mod tests {

    use super::*;

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
}
