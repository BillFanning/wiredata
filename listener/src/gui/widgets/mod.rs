//! Stateless presentation helpers for the GUI: small reusable widgets, the serial
//! option tables, the interface-config editor, and pure formatting/utility
//! functions. Nothing here holds application state — the `ListenerApp` panels in
//! the parent module call into these. Grouped by responsibility into submodules,
//! re-exported flat so callers keep using `widgets::<name>`.

mod match_rule_editor;
mod recording_editor;
mod status;

pub(super) use match_rule_editor::edit_mark_rules;
pub(super) use recording_editor::{edit_display_recording, edit_raw_recording};
pub(super) use status::{
    line_indicator, line_toggle, paint_glyph, recording_facts, recording_glyph_size,
    recording_indicator, start_button, status_color, status_glyph, status_words, stop_enabled,
};
pub(super) use wiredata_ui::format::human_bytes;

use crate::config::{
    templates, ChannelConfig, DataBits, FlowControl, InterfaceConfig, Parity, StopBits,
};
use crate::core::ChannelId;
use crate::transport::udp::UdpMode;

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
    /// The TCP Listener is not offered in this release (§4.1, ADR-047).
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

/// Serial option tables (value, label) for the radio rows (§74).
/// Widest first: 8 is the overwhelmingly common setting, so it sits leftmost
/// where the eye lands, matching talker's row.
const DATA_BITS: &[(DataBits, &str)] = &[
    (DataBits::Eight, "8"),
    (DataBits::Seven, "7"),
    (DataBits::Six, "6"),
    (DataBits::Five, "5"),
];
const PARITY: &[(Parity, &str)] = &[
    (Parity::None, "None"),
    (Parity::Even, "Even"),
    (Parity::Odd, "Odd"),
    (Parity::Mark, "Mark"),
    (Parity::Space, "Space"),
];
const STOP_BITS: &[(StopBits, &str)] = &[
    (StopBits::One, "1"),
    (StopBits::OnePointFive, "1.5"),
    (StopBits::Two, "2"),
];
/// Flow control: None first, RTS/CTS last (§74).
const FLOW_CONTROL: &[(FlowControl, &str)] = &[
    (FlowControl::None, "None"),
    (FlowControl::XonXoff, "Xon/Xoff"),
    (FlowControl::RtsCts, "RTS/CTS"),
];
/// Common serial baud rates for the baud radio row (§14.4).
const BAUD_RATES: &[u32] = &[4800, 9600, 19200, 38400, 57600, 115200, 230400];

/// One labelled row of radios inside the serial grid, bound to an enum value
/// with a fixed option table.
///
/// Grid rather than `horizontal` so the label occupies its own column: with a
/// per-row `horizontal`, every row's controls started wherever that row's label
/// happened to end, so "Flow" and "Data bits" put their first radio in
/// different places. In a grid they all line up under the port selector, as in
/// talker's `serial_grid`.
fn radio_row<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut T,
    options: &[(T, &str)],
) {
    ui.label(label);
    ui.horizontal(|ui| {
        for (v, s) in options {
            ui.radio_value(value, *v, *s);
        }
    });
    ui.end_row();
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

/// A list the interface editor asks to have listed again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Refresh {
    SerialPorts,
    LocalAddresses,
}

/// Edit a channel's interface config in place, laid out like talker (§74/§75).
/// Presentation only — applied via a §13 Reconfigure. Returns the list to
/// refresh, if its ⟳ button was clicked or its choice opened. `channel_id` keys
/// the per-channel custom-baud text buffer so it survives frames and resets on
/// switch.
pub(super) fn edit_interface(
    ui: &mut egui::Ui,
    channel_id: ChannelId,
    config: &mut ChannelConfig,
    serial_ports: &[String],
    local_addresses: &LocalAddresses,
) -> Option<Refresh> {
    let mut refresh = None;

    ui.separator();

    match &mut config.interface {
        InterfaceConfig::Udp(udp) => {
            // A 2-column grid (label | controls) keeps the address/port columns
            // aligned, so they don't shift left/right when the mode changes.
            egui::Grid::new("udp_grid")
                .num_columns(2)
                .spacing([8.0, 4.0])
                .show(ui, |ui| {
                    ui.label("Mode")
                        .on_hover_text("How this UDP socket receives datagrams.");
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut udp.mode, UdpMode::Broadcast, "Broadcast")
                            .on_hover_text(
                                "Receive broadcast datagrams (sent to the subnet \
                                 broadcast address). Bind to All interfaces to accept \
                                 them on any local interface.",
                            );
                        ui.radio_value(&mut udp.mode, UdpMode::Unicast, "Unicast")
                            .on_hover_text(
                                "Receive datagrams addressed directly to this host. \
                                 Bind to All interfaces to listen on every local \
                                 interface, or one local address to listen on just that \
                                 one.",
                            );
                        ui.radio_value(&mut udp.mode, UdpMode::Multicast, "Multicast")
                            .on_hover_text(
                                "Join a multicast group and receive its datagrams. Bind \
                                 to All interfaces to join on any interface; set the \
                                 Group address below.",
                            );
                    });
                    ui.end_row();

                    // Binding address + port — always directly under Mode, so it
                    // never moves when switching modes (#2, #3). The multicast Group
                    // row appears *below* it.
                    // The bind-address tip is mode-aware: All interfaces is the right
                    // default in every mode (accept on any local interface), but *why*
                    // differs.
                    let bind_hint = match udp.mode {
                        UdpMode::Broadcast => {
                            "The local network interface(s) to receive on. All interfaces \
                             (0.0.0.0) is the usual choice for broadcast, since the sender \
                             targets the subnet, not a specific host."
                        }
                        UdpMode::Unicast => {
                            "The local network interface(s) to receive on. All interfaces \
                             (0.0.0.0) accepts on every NIC; choose one local address to \
                             receive only on that NIC, or 127.0.0.1 for this computer only."
                        }
                        UdpMode::Multicast => {
                            "The local network interface to join the group on. All \
                             interfaces (0.0.0.0) is fine for most setups; choose one local \
                             address to join on just that NIC."
                        }
                    };
                    ui.label("Binding address").on_hover_text(bind_hint);
                    ui.horizontal(|ui| {
                        // The choices say who can reach the socket (§15, ADR-047).
                        // If the OS cannot list its addresses, the address is typed.
                        if let LocalAddresses::Failed(why) = local_addresses {
                            ui.add(
                                egui::TextEdit::singleline(&mut udp.bind_address)
                                    .id_salt("udp_bind")
                                    .desired_width(130.0)
                                    .hint_text(ALL_INTERFACES_ADDRESS),
                            )
                            .on_hover_text(format!(
                                "{bind_hint}\n\nThis computer's addresses could not be \
                                 listed: {why}"
                            ));
                        } else {
                            let listed = local_addresses.listed();
                            let combo = egui::ComboBox::from_id_salt("udp_bind")
                                .selected_text(bind_choice_label(&udp.bind_address, listed))
                                .width(280.0)
                                .show_ui(ui, |ui| {
                                    for (address, label) in bind_choices(&udp.bind_address, listed)
                                    {
                                        ui.selectable_value(&mut udp.bind_address, address, label);
                                    }
                                });
                            // Listed again as the choice opens, as the serial
                            // port list is: an adapter may have come or gone.
                            if combo.response.on_hover_text(bind_hint).clicked() {
                                refresh = Some(Refresh::LocalAddresses);
                            }
                        }
                        if ui
                            .small_button("\u{2B6E}")
                            .on_hover_text("Refresh this computer's addresses")
                            .clicked()
                        {
                            refresh = Some(Refresh::LocalAddresses);
                        }
                        ui.label("port").on_hover_text(
                            "The UDP port to listen on — i.e. the destination port the \
                             sending machine sends to.",
                        );
                        port_field(ui, "udp_port", &mut udp.port);
                    });
                    ui.end_row();

                    if udp.mode == UdpMode::Multicast {
                        const GROUP_HINT: &str = "The multicast group address to join, in \
                            the 224.0.0.0–239.255.255.255 range. 239.0.0.0/8 is the \
                            administratively-scoped (private) block — a good default; \
                            avoid 224.0.0.x, which is reserved for local control traffic. \
                            Must match the sender's group.";
                        ui.label("Group").on_hover_text(GROUP_HINT);
                        let mut group = udp.multicast_group.clone().unwrap_or_default();
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut group)
                                    .id_salt("udp_group")
                                    .desired_width(130.0)
                                    .hint_text("239.0.0.1"),
                            )
                            .on_hover_text(GROUP_HINT)
                            .changed()
                        {
                            let g = group.trim();
                            udp.multicast_group = (!g.is_empty()).then(|| g.to_string());
                        }
                        ui.end_row();
                    }

                    // Not offered for unicast (§15, ADR-047), so switching to it
                    // clears the request rather than leaving a profile that fails
                    // validation.
                    if udp.mode == UdpMode::Unicast {
                        udp.shared_port = false;
                    } else {
                        const SHARED_HINT: &str = "Let another program bind this port \
                            too, and receive the same broadcast or multicast datagrams. \
                            Listener sets the OS's address-reuse option before binding; \
                            Diagnostics says whether the OS applied it. The other program \
                            usually has to request sharing as well. Not offered for \
                            unicast, where the OS delivers each datagram to only one of \
                            the programs sharing a port.";
                        ui.label("Port sharing").on_hover_text(SHARED_HINT);
                        ui.checkbox(&mut udp.shared_port, "Request shared port")
                            .on_hover_text(SHARED_HINT);
                        ui.end_row();
                    }

                    ui.label("Arrival timing").on_hover_text(
                        "Choose whether UDP arrival wall-clock timestamps are captured after the read or requested from the OS receive path.",
                    );
                    ui.checkbox(&mut udp.kernel_timestamps, "Kernel timestamp")
                        .on_hover_text(
                            "Request a kernel software timestamp for each UDP datagram. Linux SO_TIMESTAMPNS returns a software timestamp represented with nanosecond fields; that representation does not guarantee nanosecond accuracy. It removes post-receive userland scheduling delay from recorded wall-clock capture but does not change Handoff or Read gap. Windows and macOS report the request as unavailable and use post-read capture. This is not hardware, device, or per-byte wire timing.",
                        );
                    ui.end_row();
                });
        }
        InterfaceConfig::TcpListener(tcp) => {
            egui::Grid::new("tcp_grid")
                .num_columns(2)
                .spacing([8.0, 4.0])
                .show(ui, |ui| {
                    ui.label("Binding address");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut tcp.bind_address)
                                .id_salt("tcp_bind")
                                .desired_width(130.0)
                                .hint_text("0.0.0.0"),
                        );
                        ui.label("port");
                        port_field(ui, "tcp_port", &mut tcp.port);
                    });
                    ui.end_row();
                });
        }
        InterfaceConfig::Serial(serial) => {
            // One grid so every row's controls begin at the same x, under the
            // port selector — talker's `serial_grid` shape.
            egui::Grid::new("serial_grid")
                .num_columns(2)
                .spacing([8.0, 4.0])
                .show(ui, |ui| {
                    // Port dropdown + refresh.
                    ui.label("Port");
                    ui.horizontal(|ui| {
                        // The closed box shows the *configured* port, which
                        // outlives the hardware — it is saved in the profile.
                        // Say so when the port is not currently enumerated, or
                        // the box reads as though the device were present while
                        // the list behind it is empty.
                        let label = if serial.port.is_empty() {
                            "select port\u{2026}".to_string()
                        } else if serial_ports.contains(&serial.port) {
                            serial.port.clone()
                        } else {
                            format!("{} (not found)", serial.port)
                        };
                        let combo = egui::ComboBox::from_id_salt("serial_port")
                            .selected_text(label)
                            .width(150.0)
                            .show_ui(ui, |ui| {
                                // A way back to "no port". Without it a
                                // configured port can never be unset: the list
                                // holds only real ports, so when none are
                                // present it holds nothing selectable at all,
                                // and a port whose hardware has gone is stuck.
                                if !serial.port.is_empty() {
                                    ui.selectable_value(
                                        &mut serial.port,
                                        String::new(),
                                        "(clear selection)",
                                    );
                                }
                                if serial_ports.is_empty() {
                                    ui.weak("No ports found");
                                } else {
                                    for port in serial_ports {
                                        ui.selectable_value(&mut serial.port, port.clone(), port);
                                    }
                                }
                            });
                        // Re-enumerate as the list is opened, not only at
                        // startup and on the refresh button: otherwise the
                        // choices are a snapshot from launch, and a port
                        // unplugged since then still looks selectable.
                        // Enumeration is not free, so this fires on the click
                        // that opens the list rather than every frame it is open.
                        if combo.response.clicked() {
                            refresh = Some(Refresh::SerialPorts);
                        }
                        if ui
                            .small_button("\u{2B6E}")
                            .on_hover_text("Refresh port list")
                            .clicked()
                        {
                            refresh = Some(Refresh::SerialPorts);
                        }
                    });
                    ui.end_row();
                    // Baud — radios for common rates plus a free-entry box for any custom
                    // rate. The box keeps its own buffer (egui temp memory, keyed per
                    // channel) so typing isn't clobbered each frame: the old
                    // re-derive-from-state box cleared itself the instant a standard rate was
                    // typed, and couldn't hold a custom one. A radio click resyncs the box;
                    // otherwise the typed text wins.
                    let buf_id = egui::Id::new(("baud_custom_text", channel_id));
                    let mut text = ui
                        .data(|d| d.get_temp::<String>(buf_id))
                        .unwrap_or_else(|| serial.baud_rate.to_string());
                    ui.label("Baud");
                    ui.horizontal(|ui| {
                        let before = serial.baud_rate;
                        for &baud in BAUD_RATES {
                            ui.radio_value(&mut serial.baud_rate, baud, baud.to_string());
                        }
                        if serial.baud_rate != before {
                            // A radio set the rate — mirror it into the custom box.
                            text = serial.baud_rate.to_string();
                        }
                        ui.label("custom");
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut text)
                                    .id_salt("baud_custom")
                                    .desired_width(80.0)
                                    .hint_text("e.g. 250000"),
                            )
                            .changed()
                        {
                            if let Ok(baud) = text.trim().parse::<u32>() {
                                if baud > 0 {
                                    serial.baud_rate = baud;
                                }
                            }
                        }
                    });
                    ui.end_row();
                    ui.data_mut(|d| d.insert_temp(buf_id, text));
                    radio_row(ui, "Data bits", &mut serial.data_bits, DATA_BITS);
                    radio_row(ui, "Parity", &mut serial.parity, PARITY);
                    radio_row(ui, "Stop bits", &mut serial.stop_bits, STOP_BITS);
                    radio_row(ui, "Flow", &mut serial.flow_control, FLOW_CONTROL);
                });
        }
    }
    ui.separator();
    edit_reconnect(ui, &mut config.reconnect);
    refresh
}

/// The "Reconnect automatically" choice (§9.1, ADR-045), with what it does in
/// words. Applied live; it needs no restart.
pub(super) fn edit_reconnect(ui: &mut egui::Ui, policy: &mut crate::config::ReconnectPolicy) {
    ui.checkbox(&mut policy.enabled, "Reconnect automatically");
    ui.label(egui::RichText::new(reconnect_behaviour(policy)).weak());
}

/// What a reconnect policy does, in words (§9.1). Pure, unit-tested.
pub(super) fn reconnect_behaviour(policy: &crate::config::ReconnectPolicy) -> String {
    if !policy.enabled {
        return "If the port or device fails, the channel stays down until you start it \
                again."
            .to_owned();
    }
    let wait = |ms: u64| {
        if ms.is_multiple_of(1000) {
            format!("{} s", ms / 1000)
        } else {
            format!("{ms} ms")
        }
    };
    let until = match policy.max_attempts {
        None => "until it works".to_owned(),
        Some(1) => "once, then gives up".to_owned(),
        Some(n) => format!("up to {n} times, then gives up"),
    };
    format!(
        "If the port or device fails, the channel tries again after {}, waiting longer \
         each time up to {}, {until}.",
        wait(policy.initial_backoff_ms),
        wait(policy.max_backoff_ms)
    )
}

/// The available serial port names, sorted (§14.4). Empty if enumeration fails.
pub(super) fn list_serial_ports() -> Vec<String> {
    let mut ports: Vec<String> = serialport::available_ports()
        .map(|list| list.into_iter().map(|p| p.port_name).collect())
        .unwrap_or_default();
    ports.sort();
    ports
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
/// port, or a 0 bind port for UDP/TCP. Drives auto-opening the Configure section
/// when such a channel takes focus (task 1).
pub(super) fn config_incomplete(config: &ChannelConfig) -> bool {
    match &config.interface {
        InterfaceConfig::Udp(u) => u.port == 0,
        InterfaceConfig::TcpListener(t) => t.port == 0,
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
            ("Start Channel", true)
        );
        // Running with no edits → disabled (nothing to apply).
        assert_eq!(
            start_button(ChannelStatus::Running, false),
            ("Start Channel", false)
        );
        // Running with pending edits → Apply & Restart, enabled.
        assert_eq!(
            start_button(ChannelStatus::Running, true),
            ("Apply & Restart", true)
        );
        // Faulted → Retry, enabled (config_changed irrelevant).
        assert_eq!(
            start_button(ChannelStatus::Faulted, false),
            ("Retry Channel", true)
        );
        assert_eq!(
            start_button(ChannelStatus::Faulted, true),
            ("Retry Channel", true)
        );
        // Reconnecting → disabled: a Start would be an illegal Reconnecting→Starting.
        assert_eq!(
            start_button(ChannelStatus::Reconnecting, false),
            ("Start Channel", false)
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
        // ADR-045: "Reconnecting — attempt 3, next try in 8 s", "Gave up after
        // N attempts" — never colour alone.
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
        assert_eq!(
            status_words(
                ChannelStatus::Faulted,
                Some(ReconnectProgress::GaveUp { attempts: 10 })
            ),
            "Faulted — gave up after 10 attempts"
        );
        assert_eq!(status_words(ChannelStatus::Running, None), "running");
    }

    #[test]
    fn the_reconnect_choice_says_what_it_does() {
        let mut policy = templates::udp_template().reconnect;
        policy.enabled = false;
        assert_eq!(
            reconnect_behaviour(&policy),
            "If the port or device fails, the channel stays down until you start it again."
        );
        policy.enabled = true;
        policy.initial_backoff_ms = 1_000;
        policy.max_backoff_ms = 30_000;
        policy.max_attempts = None;
        assert_eq!(
            reconnect_behaviour(&policy),
            "If the port or device fails, the channel tries again after 1 s, waiting longer \
             each time up to 30 s, until it works."
        );
        policy.max_attempts = Some(10);
        assert!(reconnect_behaviour(&policy).ends_with("up to 10 times, then gives up."));
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
            recording_indicator(Some(RecordingState::Gap(GapReason::LowDisk)), pal).2,
            "gap — waiting for disk space"
        );
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
        if let Some(v) = view.display.views.first_mut() {
            v.font_size = Some(20.0);
        }
        assert!(!config_needs_restart(&view, &base));

        // A real restart-worthy edit (the interface) DOES register.
        let mut iface = base.clone();
        if let crate::config::InterfaceConfig::Udp(udp) = &mut iface.interface {
            udp.port = udp.port.wrapping_add(1);
        }
        assert!(config_needs_restart(&iface, &base));
    }
}
