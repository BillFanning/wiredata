//! Stateless presentation helpers for the GUI: small reusable widgets, the
//! mapping to the shared serial lines, the interface-config editor, and pure
//! formatting/utility functions. Nothing here holds application state — the
//! `ListenerApp` panels in the parent module call into these. Grouped by
//! responsibility into submodules, re-exported flat so callers keep using
//! `widgets::<name>`.

mod match_rule_editor;
mod recording_editor;
mod status;

pub(super) use match_rule_editor::edit_mark_rules;
pub(super) use recording_editor::{edit_display_recording, edit_raw_recording};
pub(super) use status::{
    paint_glyph, recording_facts, recording_glyph_size, recording_indicator, start_button,
    status_color, status_glyph, status_words, stop_enabled,
};
pub(super) use wiredata_ui::format::human_bytes;
pub(super) use wiredata_ui::serial::LineClick;

use crate::config::{
    templates, ChannelConfig, DataBits, FlowControl, InterfaceConfig, Parity, SerialConfig,
    StopBits, UdpConfig,
};
use crate::core::ChannelId;
use crate::transport::udp::UdpMode;
use crate::transport::SerialControlLines;
use wiredata_ui::hover::{Hover, ShowHover};
use wiredata_ui::serial as shared;

use super::bind_scope::{bind_choice_label, bind_choices, LocalAddresses, ALL_INTERFACES_ADDRESS};

/// Why a typed channel name cannot be committed, or `None` when it can: it
/// duplicates another channel's name (§6, ADR-014; case-insensitively), or a
/// rotating recording could not use it in a filename (§59, §71). Mirrors
/// validation, so the GUI cannot save a profile that would not load.
pub(super) fn rename_problem<'a>(
    name: &str,
    mut others: impl Iterator<Item = &'a str>,
    must_be_filesystem_safe: bool,
) -> Option<&'static str> {
    if others.any(|other| other.eq_ignore_ascii_case(name)) {
        return Some("⚠ name already in use — names must be unique");
    }
    if must_be_filesystem_safe && !crate::record::is_filesystem_safe(name) {
        return Some(
            "⚠ recording file names use this name: keep it to 64 characters, without \
             / \\ : * ? \" < > | or a trailing space or dot, and not a device name like CON",
        );
    }
    None
}

/// Which interface a new channel uses, in the Add menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum AddKind {
    Udp,
    Serial,
}

impl AddKind {
    pub const ADD_MENU: [Self; 2] = [Self::Udp, Self::Serial];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Udp => "UDP",
            Self::Serial => "Serial",
        }
    }
}

/// A simple preset color scheme for the message view (#6 — simpler than a picker).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColorScheme {
    BlackOnWhite,
    GreenOnBlack,
    /// A softer, less saturated phosphor green — easier on the eyes than the bright
    /// `GreenOnBlack` for long sessions.
    GreenOnBlackDim,
    AmberOnBlack,
    WhiteOnBlack,
}

impl ColorScheme {
    pub(super) fn label(self) -> &'static str {
        match self {
            ColorScheme::BlackOnWhite => "Black on white",
            ColorScheme::GreenOnBlack => "Green on black",
            ColorScheme::GreenOnBlackDim => "Green on black (dim)",
            ColorScheme::AmberOnBlack => "Amber on black",
            ColorScheme::WhiteOnBlack => "White on black",
        }
    }
    pub(super) fn fg(self) -> egui::Color32 {
        match self {
            ColorScheme::BlackOnWhite => egui::Color32::from_gray(20),
            ColorScheme::GreenOnBlack => egui::Color32::from_rgb(60, 230, 60),
            ColorScheme::GreenOnBlackDim => egui::Color32::from_rgb(90, 170, 100),
            ColorScheme::AmberOnBlack => egui::Color32::from_rgb(255, 190, 70),
            ColorScheme::WhiteOnBlack => egui::Color32::from_gray(235),
        }
    }
    pub(super) fn bg(self) -> egui::Color32 {
        match self {
            ColorScheme::BlackOnWhite => egui::Color32::from_gray(252),
            _ => egui::Color32::from_gray(16),
        }
    }

    /// A stable token for persisting the preset in a profile. The scheme bundles fg+bg,
    /// so it round-trips as one token (stored in `DisplayViewConfig.foreground_color`)
    /// rather than two free color strings. Pairs with [`from_name`](Self::from_name).
    pub(super) fn name(self) -> &'static str {
        match self {
            ColorScheme::BlackOnWhite => "black_on_white",
            ColorScheme::GreenOnBlack => "green_on_black",
            ColorScheme::GreenOnBlackDim => "green_on_black_dim",
            ColorScheme::AmberOnBlack => "amber_on_black",
            ColorScheme::WhiteOnBlack => "white_on_black",
        }
    }

    /// Parse a persisted token back to a preset; unknown/None falls back to the default
    /// (BlackOnWhite).
    pub(super) fn from_name(name: Option<&str>) -> Self {
        match name {
            Some("green_on_black") => ColorScheme::GreenOnBlack,
            Some("green_on_black_dim") => ColorScheme::GreenOnBlackDim,
            Some("amber_on_black") => ColorScheme::AmberOnBlack,
            Some("white_on_black") => ColorScheme::WhiteOnBlack,
            _ => ColorScheme::BlackOnWhite,
        }
    }
}

/// Font sizes offered in the message-view size dropdown (#5).
pub(super) const MSG_FONT_SIZES: &[f32] = &[
    8.0, 10.0, 11.0, 12.0, 13.0, 14.0, 16.0, 18.0, 20.0, 24.0, 28.0, 36.0, 48.0, 72.0,
];

/// A serial config's port, rate and framing as the shared lines edit them
/// (listener ADR-074). The shared lists offer only what a port opens with, so
/// Mark and Space parity and 1.5 stop bits, which a profile can still hold,
/// map to [`shared::Choice::Unsupported`] and show as themselves.
pub(super) fn serial_settings(serial: &SerialConfig) -> shared::SerialSettings {
    use shared::Choice::{Offered, Unsupported};
    shared::SerialSettings {
        port: serial.port.clone(),
        baud: serial.baud_rate,
        data_bits: match serial.data_bits {
            DataBits::Five => shared::DataBits::Five,
            DataBits::Six => shared::DataBits::Six,
            DataBits::Seven => shared::DataBits::Seven,
            DataBits::Eight => shared::DataBits::Eight,
        },
        parity: match serial.parity {
            Parity::None => Offered(shared::Parity::None),
            Parity::Even => Offered(shared::Parity::Even),
            Parity::Odd => Offered(shared::Parity::Odd),
            Parity::Mark => Unsupported("Mark"),
            Parity::Space => Unsupported("Space"),
        },
        stop_bits: match serial.stop_bits {
            StopBits::One => Offered(shared::StopBits::One),
            StopBits::Two => Offered(shared::StopBits::Two),
            StopBits::OnePointFive => Unsupported("1.5"),
        },
        flow: match serial.flow_control {
            FlowControl::None => shared::FlowControl::None,
            FlowControl::XonXoff => shared::FlowControl::XonXoff,
            FlowControl::RtsCts => shared::FlowControl::RtsCts,
        },
    }
}

/// Take what the shared lines edited back into a serial config. A parity or
/// stop bits value still [`shared::Choice::Unsupported`] leaves the config's
/// value as it was, so opening a profile never changes it unasked. RTS and DTR
/// are not the lines' to set and stay as they are.
pub(super) fn apply_serial_settings(serial: &mut SerialConfig, settings: &shared::SerialSettings) {
    serial.port.clone_from(&settings.port);
    serial.baud_rate = settings.baud;
    serial.data_bits = match settings.data_bits {
        shared::DataBits::Five => DataBits::Five,
        shared::DataBits::Six => DataBits::Six,
        shared::DataBits::Seven => DataBits::Seven,
        shared::DataBits::Eight => DataBits::Eight,
    };
    if let shared::Choice::Offered(parity) = settings.parity {
        serial.parity = match parity {
            shared::Parity::None => Parity::None,
            shared::Parity::Even => Parity::Even,
            shared::Parity::Odd => Parity::Odd,
        };
    }
    if let shared::Choice::Offered(stop_bits) = settings.stop_bits {
        serial.stop_bits = match stop_bits {
            shared::StopBits::One => StopBits::One,
            shared::StopBits::Two => StopBits::Two,
        };
    }
    serial.flow_control = match settings.flow {
        shared::FlowControl::None => FlowControl::None,
        shared::FlowControl::XonXoff => FlowControl::XonXoff,
        shared::FlowControl::RtsCts => FlowControl::RtsCts,
    };
}

/// The transport's control lines as the shared lines show them.
pub(super) fn control_lines(lines: SerialControlLines) -> shared::ControlLines {
    shared::ControlLines {
        rts: lines.rts,
        dtr: lines.dtr,
        cts: lines.cts,
        dsr: lines.dsr,
        dcd: lines.dcd,
        ri: lines.ri,
    }
}

/// What stands in for the RTS button: `None` unless the interface's flow
/// control sets RTS (RTS/CTS), then whether its level is known on this
/// platform ([`crate::transport::serial::RTS_HIGH_UNDER_RTS_CTS`]).
pub(super) fn rts_by_flow(interface: &InterfaceConfig) -> Option<shared::RtsByFlow> {
    status::rts_set_by_flow_control(interface).then_some(
        if crate::transport::serial::RTS_HIGH_UNDER_RTS_CTS {
            shared::RtsByFlow::High
        } else {
            shared::RtsByFlow::Paced
        },
    )
}

/// The id a channel's serial lines keep their drop-downs and typed text under,
/// so two channels' editors never share a half-typed port or baud rate.
fn serial_editor_id(channel_id: ChannelId) -> egui::Id {
    egui::Id::new(("serial_editor", channel_id))
}

/// An incrementing port box (talker-style): `[−] [text] [+]`, no drag-to-increment
/// or resize cursor. The text field parses on change; the buttons step by one. A
/// port of 0 shows empty (a fresh channel has no port yet, #3).
fn port_field(ui: &mut egui::Ui, id: &str, port: &mut u16) {
    ui.horizontal(|ui| {
        if ui.small_button("\u{2212}").clicked() {
            *port = port.saturating_sub(1);
        }
        let mut text = if *port == 0 {
            String::new()
        } else {
            port.to_string()
        };
        if ui
            .add(
                egui::TextEdit::singleline(&mut text)
                    .id_salt(id)
                    .desired_width(64.0),
            )
            .changed()
        {
            if let Ok(value) = text.trim().parse::<u16>() {
                *port = value;
            }
        }
        if ui.small_button("+").clicked() {
            *port = port.saturating_add(1);
        }
    });
}

const BROADCAST_HOVER: Hover = Hover::new(
    "Receives datagrams that a sender addresses to the whole subnet.\n\n\
     Set the binding address to All interfaces to accept them on any of this \
     computer's network interfaces.",
);
const UNICAST_HOVER: Hover = Hover::new(
    "Receives datagrams addressed directly to this computer.\n\n\
     Set the binding address to All interfaces to listen on every network \
     interface, or to one local address to listen on only that one.",
);
const MULTICAST_HOVER: Hover = Hover::new(
    "Joins a multicast group and receives the datagrams sent to it.\n\n\
     Enter the group's address in Group. Listener joins the group on this \
     computer's default network interface; the binding address does not choose \
     which interface joins it.",
);

const BROADCAST_BIND_HOVER: Hover = Hover::new(
    "The local network interface to receive on.\n\n\
     All interfaces (0.0.0.0) is the usual choice for broadcast, because the sender \
     addresses the whole subnet rather than one computer.",
);
const UNICAST_BIND_HOVER: Hover = Hover::new(
    "The local network interface to receive on.\n\n\
     All interfaces (0.0.0.0) accepts datagrams on every network adapter. Choose one \
     local address to receive only on that adapter, or 127.0.0.1 to receive only \
     what programs on this computer send.",
);
const MULTICAST_BIND_HOVER: Hover = Hover::new(
    "The local address this channel binds to.\n\n\
     All interfaces (0.0.0.0) is the usual choice. Listener joins the multicast \
     group on this computer's default network interface, and the address chosen here \
     does not change that.",
);

const GROUP_HINT: Hover = Hover::new(
    "The multicast group address to join.\n\n\
     It must be from 224.0.0.0 to 239.255.255.255, and it must be the same group the \
     sender uses.\n\n\
     Addresses from 239.0.0.0 to 239.255.255.255 are the private block meant for use \
     inside one organization, and are a good default. Avoid addresses that start \
     with 224.0.0, which are reserved for local network control traffic.",
);

const SHARED_HINT: Hover = Hover::new(
    "Lets another program receive on the same UDP port at the same time.\n\n\
     Before it binds the port, Listener asks the operating system to allow \
     sharing. The Diagnostics section shows whether the system applied the request. \
     The other program usually has to ask for sharing as well.\n\n\
     Both programs then receive the same broadcast or multicast datagrams. This is \
     not offered for Unicast, because there the operating system delivers each \
     datagram to only one of the programs sharing a port.",
);

const KERNEL_TIMESTAMP_HINT: Hover = Hover::new(
    "Asks the operating system to take each UDP datagram's arrival time itself, \
     instead of Listener taking it after the read.\n\n\
     This works on Linux only. Windows and macOS report the request as unavailable \
     and take the time after the read, as they do when this is off.\n\n\
     The arrival times used by timestamp marks and written to the timestamp sidecar \
     then leave out the delay between a datagram reaching this computer and Listener \
     reading it. The Handoff and Read gap figures in Diagnostics do not change.\n\n\
     The time is software time. It is not the device's clock, not hardware time, \
     and not the arrival time of any single byte on the wire. Linux supplies it in \
     nanosecond fields, which does not mean it is accurate to a nanosecond.",
);

/// A list the interface editor asks to have listed again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Refresh {
    SerialPorts,
    LocalAddresses,
}

/// Edit a channel's interface config in place (§74/§75). Presentation only —
/// applied via a §13 Reconfigure. Returns the list to refresh, if its ⭮ button
/// was clicked or its choice opened, and the control line button clicked, if
/// any. `channel_id` keeps each channel's typed text apart from every other's.
///
/// A serial port is the two lines Linemon shows (listener ADR-074), each
/// wrapping to fit the detail pane: the port with its baud rate and framing,
/// then Flow with the control lines. The control lines are live, not part of
/// the draft: `lines` is the running port's last reading (`None` while it is
/// not running), and `rts_by_flow` says whether the open port's flow control
/// sets RTS.
pub(super) fn edit_interface(
    ui: &mut egui::Ui,
    channel_id: ChannelId,
    config: &mut ChannelConfig,
    serial_ports: Option<&[String]>,
    local_addresses: &LocalAddresses,
    lines: Option<SerialControlLines>,
    rts_by_flow: Option<shared::RtsByFlow>,
) -> (Option<Refresh>, Option<LineClick>) {
    let mut refresh = None;
    let mut click = None;

    ui.separator();

    match &mut config.interface {
        InterfaceConfig::Udp(udp) => {
            edit_udp(ui, udp, local_addresses, &mut refresh);
        }
        InterfaceConfig::Serial(serial) => {
            let id = serial_editor_id(channel_id);
            let mut settings = serial_settings(serial);
            let first = ui
                .horizontal_wrapped(|ui| shared::settings_line(ui, id, &mut settings, serial_ports))
                .inner;
            if first.refresh_ports {
                refresh = Some(Refresh::SerialPorts);
            }
            let second = ui
                .horizontal_wrapped(|ui| {
                    shared::flow_and_lines(
                        ui,
                        id,
                        &mut settings.flow,
                        lines.map(control_lines),
                        rts_by_flow,
                    )
                })
                .inner;
            click = second.click;
            if first.changed || second.changed {
                apply_serial_settings(serial, &settings);
            }
        }
    }
    edit_reconnect(ui, &mut config.reconnect);
    (refresh, click)
}

/// A UDP channel's settings: the port line, the mode line and the arrival
/// timing, in a 2-column grid (label | controls) that keeps the controls
/// aligned, so they don't shift left or right when the mode changes. The port
/// line and the mode line wrap to fit a narrow detail pane. Returns the
/// rectangles the port line and the mode line took, in that order.
fn edit_udp(
    ui: &mut egui::Ui,
    udp: &mut UdpConfig,
    local_addresses: &LocalAddresses,
    refresh: &mut Option<Refresh>,
) -> (egui::Rect, egui::Rect) {
    egui::Grid::new("udp_grid")
        .num_columns(2)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            // The port line, then the mode line (port sharing and the
            // multicast group follow the mode on it).
            ui.label("Port").on_hover(&UDP_PORT_HOVER);
            let port = ui
                .horizontal_wrapped(|ui| {
                    port_field(ui, "udp_port", &mut udp.port);
                    udp_bind_choice(ui, udp, local_addresses, refresh);
                })
                .response
                .rect;
            ui.end_row();
            ui.label("Mode").on_hover(&UDP_MODE_HOVER);
            let mode = ui
                .horizontal_wrapped(|ui| udp_mode_and_sharing(ui, udp))
                .response
                .rect;
            ui.end_row();

            ui.label("Arrival timing").on_hover_text(
                "Choose who takes the arrival time of each UDP datagram: Listener \
                 after it reads the datagram, or the operating system when it \
                 receives it.",
            );
            ui.checkbox(&mut udp.kernel_timestamps, "Kernel timestamp")
                .on_hover(&KERNEL_TIMESTAMP_HINT);
            ui.end_row();
            (port, mode)
        })
        .inner
}

/// What the "Port" label says on hover, in the channel editor and Linemon.
pub(super) const UDP_PORT_HOVER: Hover = Hover::new(
    "The UDP port to listen on.\n\n\
     It is the port the sending machine sends to.",
);

/// What the "Mode" label says on hover, in the channel editor and Linemon.
pub(super) const UDP_MODE_HOVER: Hover = Hover::new("How this UDP socket receives datagrams.");

/// The widest the UDP binding address choice is: wide enough for its longest
/// choice, "All interfaces (reachable from the network)", without clipping.
const BIND_CHOICE_MAX: f32 = 280.0;
/// The narrowest it shrinks to in a narrow pane. What a choice says is cut
/// short there, and the whole choice is in the list when it is opened.
const BIND_CHOICE_MIN: f32 = 140.0;
const _: () = assert!(BIND_CHOICE_MIN < BIND_CHOICE_MAX);
/// The width of the box that takes a typed binding address, when the system
/// cannot list its addresses.
const TYPED_BIND_WIDTH: f32 = 130.0;
/// The width of the multicast group's text box.
const GROUP_BOX_WIDTH: f32 = 130.0;

/// The width of the binding address choice, given the room it has: all of it,
/// between [`BIND_CHOICE_MIN`] and [`BIND_CHOICE_MAX`].
fn bind_choice_width(room: f32) -> f32 {
    room.clamp(BIND_CHOICE_MIN, BIND_CHOICE_MAX)
}

/// The width `text` takes in `style`'s font.
fn text_width(ui: &egui::Ui, text: &str, style: egui::TextStyle) -> f32 {
    let font = style.resolve(ui.style());
    ui.painter()
        .layout_no_wrap(text.to_owned(), font, egui::Color32::PLACEHOLDER)
        .size()
        .x
}

/// The room left on this row: in a wrapping line what is left before the
/// wrap, not the width of a whole row that `Ui::available_width` gives there.
fn room(ui: &egui::Ui) -> f32 {
    ui.available_size_before_wrap().x
}

/// The UDP binding address choice and its refresh button, with no label: it
/// follows the port number on the port line, in the channel editor and in
/// Linemon. Sets `refresh` when the address list should be listed again.
///
/// The choice takes the room the line has, between [`BIND_CHOICE_MIN`] and
/// [`BIND_CHOICE_MAX`]. In a wrapping line that has too little room left for
/// it, it starts the next row, where it takes what that row has.
pub(super) fn udp_bind_choice(
    ui: &mut egui::Ui,
    udp: &mut UdpConfig,
    local_addresses: &LocalAddresses,
    refresh: &mut Option<Refresh>,
) {
    // The bind-address tip is mode-aware: All interfaces is the right default
    // in every mode (accept on any local interface), but *why* differs.
    let bind_hint = match udp.mode {
        UdpMode::Broadcast => BROADCAST_BIND_HOVER,
        UdpMode::Unicast => UNICAST_BIND_HOVER,
        UdpMode::Multicast => MULTICAST_BIND_HOVER,
    };
    // Two spaces' width between the port number and the binding address, so
    // the two read as separate settings.
    let two_spaces = text_width(ui, "  ", egui::TextStyle::Body);
    let spacing = ui.spacing().item_spacing.x;
    // What the refresh button after the choice takes.
    let after = spacing
        + text_width(ui, "\u{2B6E}", egui::TextStyle::Button)
        + ui.spacing().button_padding.x * 2.0;
    let typed = matches!(local_addresses, LocalAddresses::Failed(_));
    let least = if typed {
        TYPED_BIND_WIDTH
    } else {
        BIND_CHOICE_MIN
    };
    if ui.layout().main_wrap && room(ui) < two_spaces + spacing + least + after {
        ui.end_row();
    } else {
        ui.add_space(two_spaces);
    }
    // The choices say who can reach the socket (§15, ADR-047). If the OS
    // cannot list its addresses, the address is typed.
    if let LocalAddresses::Failed(why) = local_addresses {
        ui.add(
            egui::TextEdit::singleline(&mut udp.bind_address)
                .id_salt("udp_bind")
                .desired_width(TYPED_BIND_WIDTH)
                .hint_text(ALL_INTERFACES_ADDRESS),
        )
        .on_hover(&bind_hint.clone().with_paragraph(&format!(
            "This computer's addresses could not be listed, so type \
             the address instead. The system reported: {why}"
        )));
    } else {
        let listed = local_addresses.listed();
        // The choice is as wide as the room, and cuts what it says short
        // rather than growing to fit it: a drop-down's width is only its
        // minimum, and its text would otherwise set the rest.
        let width = bind_choice_width(room(ui) - after);
        let combo = ui
            .scope(|ui| {
                ui.set_max_width(width);
                egui::ComboBox::from_id_salt("udp_bind")
                    .selected_text(bind_choice_label(&udp.bind_address, listed))
                    .width(width)
                    .truncate()
                    .show_ui(ui, |ui| {
                        for (address, label) in bind_choices(&udp.bind_address, listed) {
                            ui.selectable_value(&mut udp.bind_address, address, label);
                        }
                    })
            })
            .inner;
        // Listed again as the choice opens, as the serial port list is: an
        // adapter may have come or gone.
        if combo.response.on_hover(&bind_hint).clicked() {
            *refresh = Some(Refresh::LocalAddresses);
        }
    }
    if ui
        .small_button("\u{2B6E}")
        .on_hover_text("Refresh this computer's addresses")
        .clicked()
    {
        *refresh = Some(Refresh::LocalAddresses);
    }
}

/// The UDP mode radios, then port sharing and, for multicast, the group, with
/// no labels but the caller's "Mode": the mode line in the channel editor and
/// in Linemon. In a wrapping line the group moves to the next row as a whole
/// when it does not fit what is left of this one.
///
/// Port sharing is always shown, so it does not come and go as the mode
/// changes. It is not offered for unicast (§15, ADR-047): there the box is
/// disabled and unticked, and words say so, so the grey is not the only sign.
/// Switching to unicast clears the request rather than leaving a profile that
/// fails validation.
pub(super) fn udp_mode_and_sharing(ui: &mut egui::Ui, udp: &mut UdpConfig) {
    ui.radio_value(&mut udp.mode, UdpMode::Broadcast, "Broadcast")
        .on_hover(&BROADCAST_HOVER);
    ui.radio_value(&mut udp.mode, UdpMode::Unicast, "Unicast")
        .on_hover(&UNICAST_HOVER);
    ui.radio_value(&mut udp.mode, UdpMode::Multicast, "Multicast")
        .on_hover(&MULTICAST_HOVER);
    ui.separator();
    let unicast = udp.mode == UdpMode::Unicast;
    if unicast {
        udp.shared_port = false;
    }
    ui.add_enabled(
        !unicast,
        egui::Checkbox::new(&mut udp.shared_port, "Request shared port"),
    )
    .on_hover(&SHARED_HINT);
    if unicast {
        ui.weak("Only for Broadcast and Multicast.");
    }
    // The group is laid out in every mode and shown only in Multicast: its
    // text box is taller than the checkbox, so leaving it out would make the
    // line grow, and move, when Multicast is chosen.
    let multicast = udp.mode == UdpMode::Multicast;
    let group_ui = if multicast {
        egui::UiBuilder::new()
    } else {
        egui::UiBuilder::new().invisible()
    };
    // Its separator, its label and its box, with the space between them.
    let group_width = ui.spacing().item_spacing.x * 4.0
        + text_width(ui, "Group", egui::TextStyle::Body)
        + GROUP_BOX_WIDTH;
    if ui.layout().main_wrap && room(ui) < group_width {
        ui.end_row();
    }
    ui.scope_builder(group_ui, |ui| {
        ui.horizontal(|ui| {
            ui.separator();
            ui.label("Group").on_hover(&GROUP_HINT);
            let mut group = udp.multicast_group.clone().unwrap_or_default();
            if ui
                .add(
                    egui::TextEdit::singleline(&mut group)
                        .id_salt("udp_group")
                        .desired_width(GROUP_BOX_WIDTH)
                        .hint_text("239.0.0.1"),
                )
                .on_hover(&GROUP_HINT)
                .changed()
            {
                let g = group.trim();
                udp.multicast_group = (!g.is_empty()).then(|| g.to_string());
            }
        });
    });
}

/// The "Reconnect automatically" choice (§9.1, ADR-045), with what it does in
/// words. Applied live; it needs no restart.
pub(super) fn edit_reconnect(ui: &mut egui::Ui, policy: &mut crate::config::ReconnectPolicy) {
    let hover = reconnect_hover();
    ui.checkbox(&mut policy.enabled, "Reconnect automatically")
        .on_hover(&hover);
}

/// The "Reconnect automatically" hover. It reads the same ticked or not, and
/// says what each state does, with the pace from `crate::retry`.
fn reconnect_hover() -> Hover {
    let wait = |d: std::time::Duration| wiredata_ui::format::wait(d);
    let longest = wait(crate::retry::RETRY_MAX);
    Hover::from_paragraphs([
        "Reopens the channel by itself when its port or device fails.".to_owned(),
        format!(
            "When this is ticked, the channel tries again after {}, then waits longer \
             between tries until the wait reaches {longest}, and keeps trying every \
             {longest} until it works.",
            wait(crate::retry::RETRY_FIRST)
        ),
        "When it is not ticked, the channel stays down until you start it again.".to_owned(),
    ])
}

/// A fresh template config for the given interface kind. New UDP channels default
/// to Broadcast (the first/most-common option for this tool).
pub(super) fn template_for(kind: AddKind) -> ChannelConfig {
    match kind {
        AddKind::Udp => {
            let mut config = templates::udp_template();
            if let InterfaceConfig::Udp(udp) = &mut config.interface {
                udp.mode = UdpMode::Broadcast;
            }
            config
        }
        AddKind::Serial => templates::serial_template(),
    }
}

/// Whether a channel's config is missing the minimum needed to start: no serial
/// port, or a 0 bind port for UDP. Drives auto-opening the Configure section
/// when such a channel takes focus (task 1).
pub(super) fn config_incomplete(config: &ChannelConfig) -> bool {
    match &config.interface {
        InterfaceConfig::Udp(u) => u.port == 0,
        InterfaceConfig::Serial(s) => s.port.trim().is_empty(),
    }
}

/// Whether an edited config draft differs from the channel's committed config in a way
/// that needs an Apply & Restart. Used to switch the Start button to "Apply & Restart"
/// while a running channel has pending edits. Pure / unit-tested.
///
/// Fields that are applied **live** (no restart) are neutralized before comparing, so
/// editing them doesn't flip the lifecycle button:
/// - `name` — renames live (§6).
/// - `raw_recording` and `display_recording` — the live Record/Stop toggles
///   (ADR-012/-013); destination/rotation/etc. take effect when recording is
///   (re)started, not via a channel restart.
/// - `display` (view settings) and `retention` (scroll buffer) — synced live via
///   `SetViewConfig` (§78, §87), no restart.
/// - `reconnect` — read only by the runtime's retry loop, so set live (§9.1).
///
/// The `interface` still counts: it is applied via a §13 Reconfigure, i.e. a
/// restart.
pub(super) fn config_needs_restart(draft: &ChannelConfig, committed: &ChannelConfig) -> bool {
    let mut a = draft.clone();
    // Neutralize the live-applied fields so only restart-worthy edits register.
    a.name = committed.name.clone();
    a.raw_recording = committed.raw_recording.clone();
    a.display_recording = committed.display_recording.clone();
    a.display = committed.display.clone();
    a.retention = committed.retention.clone();
    a.reconnect = committed.reconnect;
    &a != committed
}

#[cfg(test)]
mod tests {
    use super::super::state::ChannelStatus;
    use super::*;

    /// The Name field holds back what validation would refuse (§6, §59, §71),
    /// so a saved profile always loads every channel it shows.
    #[test]
    fn a_rename_is_held_back_when_validation_would_refuse_it() {
        let others = ["Feed", "AIS"];
        assert_eq!(rename_problem("GPS", others.into_iter(), true), None);
        assert_eq!(
            rename_problem("feed", others.into_iter(), false),
            Some("⚠ name already in use — names must be unique")
        );
        let unsafe_name = rename_problem("GPS/feed", others.into_iter(), true);
        assert!(
            unsafe_name.is_some_and(|problem| problem.contains("recording file names")),
            "{unsafe_name:?}"
        );
        // Nothing rotates, so nothing names a file after it.
        assert_eq!(rename_problem("GPS/feed", others.into_iter(), false), None);
    }

    #[test]
    fn add_menu_defines_the_shared_transport_order() {
        assert_eq!(AddKind::ADD_MENU.map(AddKind::label), ["UDP", "Serial"]);
    }

    #[test]
    fn config_incomplete_flags_missing_port_or_serial_device() {
        // A real port → complete; a 0 port (the fresh template default) → incomplete.
        let mut udp = templates::udp_template();
        if let InterfaceConfig::Udp(u) = &mut udp.interface {
            u.port = 18180;
        }
        assert!(!config_incomplete(&udp));
        if let InterfaceConfig::Udp(u) = &mut udp.interface {
            u.port = 0;
        }
        assert!(config_incomplete(&udp));

        // A fresh serial template has no port selected → incomplete.
        let serial = templates::serial_template();
        assert!(config_incomplete(&serial));
    }

    #[test]
    fn start_button_reflects_state_and_pending_edits() {
        // Stopped → plain Start, enabled.
        assert_eq!(
            start_button(ChannelStatus::Stopped, false),
            ("Start channel", true)
        );
        // Running with no edits → disabled (nothing to apply).
        assert_eq!(
            start_button(ChannelStatus::Running, false),
            ("Start channel", false)
        );
        // Running with pending edits → Apply & Restart, enabled.
        assert_eq!(
            start_button(ChannelStatus::Running, true),
            ("Apply & Restart", true)
        );
        // Faulted → Retry, enabled (config_changed irrelevant).
        assert_eq!(
            start_button(ChannelStatus::Faulted, false),
            ("Retry channel", true)
        );
        assert_eq!(
            start_button(ChannelStatus::Faulted, true),
            ("Retry channel", true)
        );
        // Reconnecting → disabled: a Start would be an illegal Reconnecting→Starting.
        assert_eq!(
            start_button(ChannelStatus::Reconnecting, false),
            ("Start channel", false)
        );
    }

    #[test]
    fn stop_enabled_only_for_stoppable_states() {
        // Stop is legal from Running/Faulted/Reconnecting, illegal from Stopped.
        assert!(stop_enabled(ChannelStatus::Running));
        assert!(stop_enabled(ChannelStatus::Faulted));
        assert!(stop_enabled(ChannelStatus::Reconnecting));
        assert!(!stop_enabled(ChannelStatus::Stopped));
    }

    #[test]
    fn recording_indicator_maps_state_to_label() {
        use crate::core::RecordingState;
        let pal = &wiredata_ui::palette::LIGHT;
        assert_eq!(
            recording_indicator(Some(RecordingState::Enabled), pal).2,
            "recording"
        );
        assert_eq!(
            recording_indicator(Some(RecordingState::Faulted), pal).2,
            "faulted"
        );
        assert_eq!(
            recording_indicator(Some(RecordingState::Disabled), pal).2,
            "off"
        );
        assert_eq!(recording_indicator(None, pal).2, "off");
    }

    #[test]
    fn reconnect_status_is_stated_in_words() {
        // ADR-045: "Reconnecting — attempt 3, next try in 8 s" — never colour
        // alone.
        use crate::runtime::ReconnectProgress;
        use std::time::Duration;
        let retrying = ReconnectProgress::Retrying {
            attempt: 3,
            next_try_in: Duration::from_millis(7_200),
        };
        assert_eq!(
            status_words(ChannelStatus::Reconnecting, Some(retrying)),
            "Reconnecting — attempt 3, next try in 8 s"
        );
        assert_eq!(status_words(ChannelStatus::Running, None), "running");
    }

    #[test]
    fn the_reconnect_hover_describes_both_states() {
        assert_eq!(
            reconnect_hover().text(),
            "Reopens the channel by itself when its port or device fails.\n\n\
             When this is ticked, the channel tries again after 1 s, then waits longer \
             between tries until the wait reaches 30 s, and keeps trying every 30 s until \
             it works.\n\n\
             When it is not ticked, the channel stays down until you start it again."
        );
    }

    #[test]
    fn the_reconnect_choice_applies_without_a_restart() {
        let committed = templates::udp_template();
        let mut draft = committed.clone();
        draft.reconnect.enabled = !committed.reconnect.enabled;
        assert!(!config_needs_restart(&draft, &committed));
    }

    #[test]
    fn recording_facts_name_the_file_its_size_the_disk_and_the_next_rotation() {
        use crate::runtime::RecordingStatus;
        use std::time::{Duration, SystemTime};
        let now = SystemTime::now();
        let status = RecordingStatus {
            current_file: Some(std::path::PathBuf::from("rec").join("GPS_2026-09-30_08.raw")),
            bytes_written: 1_500,
            total_size: Some(2_000_000),
            free_space: Some(3_000_000_000),
            next_rotation: Some(now + Duration::from_secs(23 * 60 + 30)),
            ..RecordingStatus::default()
        };
        assert_eq!(
            recording_facts(&status, now),
            "Writing GPS_2026-09-30_08.raw · 1.500 kB in this file · 2.000 MB in all its \
             files · 3.000 GB free · next file in 23 min"
        );
        // Before the first disk check lands, and in a gap, it says what it knows.
        let gap = RecordingStatus::default();
        assert_eq!(recording_facts(&gap, now), "No file open");
        let daily = RecordingStatus {
            next_rotation: Some(now + Duration::from_secs(5 * 3600 + 7 * 60)),
            ..RecordingStatus::default()
        };
        assert!(recording_facts(&daily, now).ends_with("next file in 5 h 7 min"));
    }

    #[test]
    fn a_recording_gap_has_its_own_glyph_and_names_what_it_waits_for() {
        // Shape and words carry the state, never colour alone: a gap is on but
        // not writing, so it takes the half-filled glyph, not the running dot.
        use crate::core::{GapReason, RecordingState};
        let pal = &wiredata_ui::palette::LIGHT;
        let (glyph, _, label) =
            recording_indicator(Some(RecordingState::Gap(GapReason::WriteFailed)), pal);
        assert_eq!(glyph, wiredata_ui::glyphs::RECONNECTING);
        assert_eq!(label, "gap — retrying");
        assert_eq!(
            recording_indicator(
                Some(RecordingState::Gap(GapReason::DestinationMissing)),
                pal
            )
            .2,
            "gap — waiting for the folder"
        );
    }

    #[test]
    fn config_change_detection_excludes_live_applied_fields() {
        let base = templates::udp_template();
        // Identical config = no change.
        assert!(!config_needs_restart(&base, &base));

        // Live-applied edits are NOT restart-worthy (they don't flip the lifecycle
        // button) — name (§6), raw recording (ADR-012), view settings + scroll buffer
        // (SetViewConfig, §78/§87).
        let mut renamed = base.clone();
        renamed.name = crate::core::ChannelName::new("different");
        assert!(!config_needs_restart(&renamed, &base));

        let mut raw = base.clone();
        raw.raw_recording.destination = Some(std::path::PathBuf::from("/tmp/x.raw"));
        raw.raw_recording.file_rotation = crate::record::FileRotationPolicy::Hourly;
        assert!(
            !config_needs_restart(&raw, &base),
            "editing raw recording must not require an Apply & Restart"
        );

        let mut disp = base.clone();
        disp.display_recording.destination = Some(std::path::PathBuf::from("/tmp/x.disp"));
        disp.display_recording.enabled = true;
        assert!(
            !config_needs_restart(&disp, &base),
            "editing display recording must not require an Apply & Restart (ADR-012)"
        );

        let mut view = base.clone();
        view.retention.byte_limit = Some(256 * 1024);
        view.display.view.font_size = Some(20.0);
        assert!(!config_needs_restart(&view, &base));

        // A real restart-worthy edit (the interface) DOES register.
        let mut iface = base.clone();
        if let crate::config::InterfaceConfig::Udp(udp) = &mut iface.interface {
            udp.port = udp.port.wrapping_add(1);
        }
        assert!(config_needs_restart(&iface, &base));
    }

    // ---- the shared serial lines ----

    fn serial_config() -> SerialConfig {
        match templates::serial_template().interface {
            InterfaceConfig::Serial(serial) => serial,
            InterfaceConfig::Udp(_) => unreachable!("the serial template is serial"),
        }
    }

    /// Every value the shared lists offer maps to its twin and back, and the
    /// settings the shared lines do not hold (RTS, DTR) are left as they were.
    #[test]
    fn serial_settings_map_to_the_shared_ones_and_back() {
        use wiredata_ui::serial::{self as shared, Choice};
        for (ours, theirs) in [
            (DataBits::Five, shared::DataBits::Five),
            (DataBits::Six, shared::DataBits::Six),
            (DataBits::Seven, shared::DataBits::Seven),
            (DataBits::Eight, shared::DataBits::Eight),
        ] {
            let mut config = serial_config();
            config.data_bits = ours;
            assert_eq!(serial_settings(&config).data_bits, theirs);
        }
        for (ours, theirs) in [
            (Parity::None, shared::Parity::None),
            (Parity::Even, shared::Parity::Even),
            (Parity::Odd, shared::Parity::Odd),
        ] {
            let mut config = serial_config();
            config.parity = ours;
            assert_eq!(serial_settings(&config).parity, Choice::Offered(theirs));
        }
        for (ours, theirs) in [
            (StopBits::One, shared::StopBits::One),
            (StopBits::Two, shared::StopBits::Two),
        ] {
            let mut config = serial_config();
            config.stop_bits = ours;
            assert_eq!(serial_settings(&config).stop_bits, Choice::Offered(theirs));
        }
        for (ours, theirs) in [
            (FlowControl::None, shared::FlowControl::None),
            (FlowControl::XonXoff, shared::FlowControl::XonXoff),
            (FlowControl::RtsCts, shared::FlowControl::RtsCts),
        ] {
            let mut config = serial_config();
            config.flow_control = ours;
            assert_eq!(serial_settings(&config).flow, theirs);
        }

        // Back again, from a config that differs in every field the lines hold.
        let mut original = serial_config();
        original.port = "COM9".to_owned();
        original.baud_rate = 250_000;
        original.data_bits = DataBits::Seven;
        original.parity = Parity::Odd;
        original.stop_bits = StopBits::Two;
        original.flow_control = FlowControl::XonXoff;
        original.rts = Some(true);
        original.dtr = Some(false);
        let settings = serial_settings(&original);
        assert_eq!(settings.port, "COM9");
        assert_eq!(settings.baud, 250_000);
        let mut other = SerialConfig {
            port: "COM1".to_owned(),
            baud_rate: 1200,
            data_bits: DataBits::Five,
            parity: Parity::Even,
            stop_bits: StopBits::One,
            flow_control: FlowControl::RtsCts,
            ..original.clone()
        };
        apply_serial_settings(&mut other, &settings);
        assert_eq!(other, original);
    }

    /// Mark, Space and 1.5 stop bits are in a profile but not in the shared
    /// lists: they are shown as themselves.
    #[test]
    fn what_the_shared_lists_lack_is_shown_as_itself() {
        use wiredata_ui::serial::Choice;
        let mut config = serial_config();
        config.parity = Parity::Mark;
        config.stop_bits = StopBits::OnePointFive;
        let settings = serial_settings(&config);
        assert_eq!(settings.parity, Choice::Unsupported("Mark"));
        assert_eq!(settings.stop_bits, Choice::Unsupported("1.5"));
        config.parity = Parity::Space;
        assert_eq!(
            serial_settings(&config).parity,
            Choice::Unsupported("Space")
        );
    }

    /// Opening a profile and changing something else never changes Mark, Space
    /// or 1.5; choosing another value does.
    #[test]
    fn an_unsupported_value_is_kept_until_another_is_chosen() {
        use wiredata_ui::serial::{self as shared, Choice};
        let mut config = serial_config();
        config.parity = Parity::Mark;
        config.stop_bits = StopBits::OnePointFive;
        config.rts = Some(true);

        let mut settings = serial_settings(&config);
        settings.baud = 19_200;
        apply_serial_settings(&mut config, &settings);
        assert_eq!(config.baud_rate, 19_200);
        assert_eq!(config.parity, Parity::Mark, "kept");
        assert_eq!(config.stop_bits, StopBits::OnePointFive, "kept");
        assert_eq!(config.rts, Some(true), "not the shared lines' to change");

        settings.parity = Choice::Offered(shared::Parity::Even);
        apply_serial_settings(&mut config, &settings);
        assert_eq!(config.parity, Parity::Even, "chosen");
        assert_eq!(config.stop_bits, StopBits::OnePointFive, "still kept");

        settings.stop_bits = Choice::Offered(shared::StopBits::Two);
        apply_serial_settings(&mut config, &settings);
        assert_eq!(config.stop_bits, StopBits::Two, "chosen");

        // Space is kept the same way.
        config.parity = Parity::Space;
        let settings = serial_settings(&config);
        let mut again = config.clone();
        apply_serial_settings(&mut again, &settings);
        assert_eq!(again.parity, Parity::Space);
    }

    #[test]
    fn the_control_lines_map_field_for_field() {
        let lines = SerialControlLines {
            rts: true,
            dtr: false,
            cts: true,
            dsr: false,
            dcd: true,
            ri: false,
        };
        let shared = control_lines(lines);
        assert_eq!(
            (shared.rts, shared.dtr, shared.cts, shared.dsr, shared.dcd, shared.ri),
            (true, false, true, false, true, false)
        );
        let flipped = control_lines(SerialControlLines {
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

    /// RTS has words instead of a button only under RTS/CTS, and which words
    /// is the platform's fact (`RTS_HIGH_UNDER_RTS_CTS`).
    #[test]
    fn rts_has_words_only_under_rts_cts() {
        use wiredata_ui::serial::RtsByFlow;
        let mut serial = templates::serial_template().interface;
        for flow in [FlowControl::None, FlowControl::XonXoff] {
            if let InterfaceConfig::Serial(config) = &mut serial {
                config.flow_control = flow;
            }
            assert_eq!(rts_by_flow(&serial), None, "{flow:?}");
        }
        if let InterfaceConfig::Serial(config) = &mut serial {
            config.flow_control = FlowControl::RtsCts;
        }
        let expected = if crate::transport::serial::RTS_HIGH_UNDER_RTS_CTS {
            RtsByFlow::High
        } else {
            RtsByFlow::Paced
        };
        assert_eq!(rts_by_flow(&serial), Some(expected));
        assert_eq!(rts_by_flow(&templates::udp_template().interface), None);
    }

    /// Each channel's typed port and baud text lives under its own id, so two
    /// channels' editors never share a half-typed value.
    #[test]
    fn two_channels_never_share_typed_text() {
        let (a, b) = (ChannelId::new(), ChannelId::new());
        assert_ne!(serial_editor_id(a), serial_editor_id(b));
        assert_eq!(serial_editor_id(a), serial_editor_id(a));
    }

    // ---- the narrow detail pane ----

    fn multicast_config() -> ChannelConfig {
        let mut config = template_for(AddKind::Udp);
        if let InterfaceConfig::Udp(udp) = &mut config.interface {
            udp.port = 5005;
            udp.mode = UdpMode::Multicast;
            udp.multicast_group = Some("239.0.0.1".to_owned());
        }
        config
    }

    fn listed_addresses() -> LocalAddresses {
        LocalAddresses::Listed(vec![super::super::bind_scope::LocalAddress {
            ip: std::net::Ipv4Addr::new(192, 168, 10, 20),
            interface: "Ethernet".to_owned(),
        }])
    }

    /// Draw the UDP editor's two lines in a `width`-point pane: the room it
    /// had, the port line, the mode line, and everything the editor drew.
    fn lay_out_udp(width: f32) -> (egui::Rect, egui::Rect, egui::Rect, egui::Rect) {
        let ctx = egui::Context::default();
        let mut drawn = None;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.set_max_width(width);
            let room = ui.max_rect();
            let mut config = multicast_config();
            let InterfaceConfig::Udp(udp) = &mut config.interface else {
                unreachable!("the config is UDP");
            };
            let (port, mode) = edit_udp(ui, udp, &listed_addresses(), &mut None);
            drawn = Some((room, port, mode, ui.min_rect()));
        });
        drawn.expect("the frame ran")
    }

    /// In a channel's detail pane, about 320 points wide, a multicast
    /// channel's lines wrap rather than run off the edge, and so does
    /// everything else the editor draws.
    #[test]
    fn a_multicast_channel_fits_a_narrow_pane() {
        let (room, port, mode, _) = lay_out_udp(320.0);
        for (name, line) in [("port", port), ("mode", mode)] {
            assert!(
                line.right() <= room.right() + 0.5,
                "the {name} line runs past the pane: {line:?} in {room:?}"
            );
        }

        let ctx = egui::Context::default();
        let mut edge = None;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.set_max_width(320.0);
            // The room before drawing: a `Ui` widens to take in what overflows.
            let room = ui.max_rect().right();
            let mut config = multicast_config();
            edit_interface(
                ui,
                ChannelId::new(),
                &mut config,
                None,
                &listed_addresses(),
                None,
                None,
            );
            edge = Some((ui.min_rect().right(), room));
        });
        let (drawn, room) = edge.expect("the frame ran");
        assert!(
            drawn <= room + 0.5,
            "the editor runs past the pane: {drawn} against {room}"
        );
    }

    /// A serial channel's two lines wrap in the same pane.
    #[test]
    fn a_serial_channel_fits_a_narrow_pane() {
        let ctx = egui::Context::default();
        let mut edge = None;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.set_max_width(320.0);
            // The room before drawing: a `Ui` widens to take in what overflows.
            let room = ui.max_rect().right();
            let mut config = templates::serial_template();
            let ports = ["COM3".to_owned(), "COM14".to_owned()];
            edit_interface(
                ui,
                ChannelId::new(),
                &mut config,
                Some(&ports),
                &LocalAddresses::NotListed,
                None,
                None,
            );
            edge = Some((ui.min_rect().right(), room));
        });
        let (drawn, room) = edge.expect("the frame ran");
        assert!(
            drawn <= room + 0.5,
            "the editor runs past the pane: {drawn} against {room}"
        );
    }

    /// Where there is room, as in Linemon's wide window, each UDP line is one
    /// row, and the multicast group does not make the line taller.
    #[test]
    fn each_udp_line_is_one_row_when_there_is_room() {
        let (_, port, mode, _) = lay_out_udp(1400.0);
        let ctx = egui::Context::default();
        let mut row = 0.0;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            row = ui.spacing().interact_size.y;
        });
        for (name, line) in [("port", port), ("mode", mode)] {
            assert!(
                line.height() < row * 1.5,
                "the {name} line is more than one row: {line:?}"
            );
        }
        // With the room, the binding address keeps its full width: the port
        // box, the choice and its refresh button together are wider than the
        // choice alone.
        assert!(
            port.width() > BIND_CHOICE_MAX + 100.0,
            "the binding address choice shrank without need: {port:?}"
        );
    }

    /// The binding address drop-down takes the room it has, never more than
    /// it needs for a full address and never less than it can show.
    #[test]
    fn the_bind_choice_shrinks_to_the_room_it_has() {
        assert_eq!(bind_choice_width(1000.0), BIND_CHOICE_MAX);
        assert_eq!(bind_choice_width(200.0), 200.0);
        assert_eq!(bind_choice_width(10.0), BIND_CHOICE_MIN);
    }
}
