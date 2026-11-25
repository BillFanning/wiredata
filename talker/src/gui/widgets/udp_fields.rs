//! A UDP channel's editor: destination, local port, mode, the multicast
//! interface and TTL, and the hold-to-repeat port buttons.

use std::net::Ipv4Addr;

use crate::core::channel::LocalAddress;

use wiredata_ui::hover::{Hover, ShowHover};

use crate::gui::draft::{multicast_ttl, ConnDraft, PortHold, UdpModeDraft};

use super::*;

/// A UDP channel's editor: lines that each wrap to fit the detail pane. The
/// first is where the datagrams go, as an address and port, followed by the
/// local port; the second is the mode; in Multicast a third gives the outgoing
/// interface and the TTL. `changed` is set when a value was committed or the
/// mode changed, which a running channel takes at once. `addresses` is this
/// computer's network addresses, `None` until they are first listed.
pub(in crate::gui) fn show_udp_fields(
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
pub(in crate::gui) fn multicast_interface_choices(
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

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::gui::draft::ConnKind;

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
}
