//! A serial channel's editor: the shared settings lines (talker ADR-071)
//! and the control lines.

use wiredata_ui::serial::{self as shared, LineClick, RtsByFlow};

use crate::core::channel::{
    ChannelId, ControlLines, FlowControl, InterfaceConfig, OutputLine, RTS_HIGH_UNDER_RTS_CTS,
};

use crate::gui::draft::ConnDraft;

use super::*;

/// The id a serial channel's editor keeps its drop-downs and typed text under,
/// so two channels never share a half-typed port or baud rate. `None` is a
/// slot that does not exist, which the detail pane never draws.
pub(in crate::gui) fn serial_editor_id(channel: Option<ChannelId>) -> egui::Id {
    egui::Id::new(("serial_editor", channel))
}

/// The transport's control lines as the shared lines show them.
pub(in crate::gui) fn control_lines(lines: ControlLines) -> shared::ControlLines {
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
pub(in crate::gui) fn live_lines(
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
pub(in crate::gui) fn rts_by_flow(open: Option<&InterfaceConfig>) -> Option<RtsByFlow> {
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
pub(in crate::gui) fn output_request(click: LineClick) -> (OutputLine, bool) {
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
pub(in crate::gui) fn show_serial_fields(
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

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::core::channel::{SerialConfig, UdpConfig};
    use crate::gui::draft::ConnKind;

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
