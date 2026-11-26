//! The serial port's settings and control lines, laid out once for Linemon,
//! a Listener channel and a talker channel (talker ADR-071, listener ADR-074).
//!
//! Two lines, Linemon's: the port with its baud rate and framing, then flow
//! control with the control lines. Each app keeps its own serial types and maps
//! them to and from the neutral ones here, so this module stays presentational
//! and depends on egui alone.
//!
//! The widgets add their pieces to the caller's `Ui` and choose no layout. The
//! caller decides: `horizontal` where the line must stay one row and the window
//! grows to fit (Linemon), `horizontal_wrapped` where the pane is narrow (a
//! channel's detail pane).

use crate::hover::{Hover, ShowHover};

/// Data bits per character.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataBits {
    Five,
    Six,
    Seven,
    Eight,
}

/// Parity. Only what a port opens with: Mark and Space are refused by the
/// serial library, so they are not offered (see [`Choice::Unsupported`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parity {
    None,
    Even,
    Odd,
}

/// Stop bits. 1.5 is refused by the serial library, so it is not offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopBits {
    One,
    Two,
}

/// Flow control, named as on the wire (talker ADR-070).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowControl {
    None,
    XonXoff,
    RtsCts,
}

/// Eight first: the usual choice is where the eye lands.
pub const DATA_BITS: &[(DataBits, &str)] = &[
    (DataBits::Eight, "8"),
    (DataBits::Seven, "7"),
    (DataBits::Six, "6"),
    (DataBits::Five, "5"),
];
pub const PARITY: &[(Parity, &str)] = &[
    (Parity::None, "None"),
    (Parity::Even, "Even"),
    (Parity::Odd, "Odd"),
];
pub const STOP_BITS: &[(StopBits, &str)] = &[(StopBits::One, "1"), (StopBits::Two, "2")];
pub const FLOW_CONTROL: &[(FlowControl, &str)] = &[
    (FlowControl::None, "None"),
    (FlowControl::XonXoff, "XON/XOFF"),
    (FlowControl::RtsCts, "RTS/CTS"),
];
/// The baud rates offered: few enough that the list opens without scrolling.
/// Any other rate, faster or slower, is typed into the box at the bottom of the
/// list.
pub const BAUD_RATES: &[u32] = &[4800, 9600, 19200, 38400, 57600, 115200];

/// A setting shown in a drop-down: a value it offers, or one a profile holds
/// that it does not. An unsupported value is shown as itself and kept until
/// another is chosen, so opening a profile never changes it unasked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice<T> {
    Offered(T),
    /// What the profile holds, in words, such as "Mark".
    Unsupported(&'static str),
}

/// The serial settings the two lines edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerialSettings {
    /// Empty when no port is chosen.
    pub port: String,
    pub baud: u32,
    pub data_bits: DataBits,
    pub parity: Choice<Parity>,
    pub stop_bits: Choice<StopBits>,
    pub flow: FlowControl,
}

/// The six control lines, as last read or set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ControlLines {
    pub rts: bool,
    pub dtr: bool,
    pub cts: bool,
    pub dsr: bool,
    pub dcd: bool,
    pub ri: bool,
}

/// A click on an output line's button: the line, and the level asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineClick {
    Rts(bool),
    Dtr(bool),
}

/// RTS under RTS/CTS flow control, where the flow control sets it and there is
/// no button. Which applies is a fact about the platform's driver, so the
/// caller says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtsByFlow {
    /// The driver keeps RTS high (Windows).
    High,
    /// The driver raises and lowers RTS to pace the sender; its level is not
    /// known.
    Paced,
}

/// What the first line reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SettingsResponse {
    /// A setting changed this frame. A typed port or baud counts only once it
    /// is committed: the box is left or Enter is pressed.
    pub changed: bool,
    /// The port list should be read again: the drop-down was opened or the
    /// refresh button clicked.
    pub refresh_ports: bool,
}

/// What the second line reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinesResponse {
    /// Flow control changed this frame.
    pub changed: bool,
    /// An output line's button was clicked.
    pub click: Option<LineClick>,
}

/// The first line: Port (with its refresh button), Baud, Data bits, Parity and
/// Stop bits. `ports` is the port list, `None` until one has been read. `id`
/// keeps this instance's drop-downs and typed text apart from any other's.
pub fn settings_line(
    ui: &mut egui::Ui,
    id: egui::Id,
    settings: &mut SerialSettings,
    ports: Option<&[String]>,
) -> SettingsResponse {
    let before = settings.clone();
    let mut refresh_ports = false;

    let label = port_label(&settings.port, ports);
    let listed = ports.unwrap_or_default();
    let width = combo_width(
        ui,
        [label.as_str(), OTHER_PORT_HINT, PORT_WIDTH_SAMPLE]
            .into_iter()
            .chain(listed.iter().map(String::as_str)),
    );
    // The refresh button stays on the drop-down's row.
    let refresh_width = ui.spacing().item_spacing.x
        + text_width(ui, REFRESH_GLYPH, egui::TextStyle::Button)
        + ui.spacing().button_padding.x * 2.0;
    label_for_combo(ui, "Port", &PORT_HOVER, width + refresh_width);
    let text_id = id.with("port_text");
    let mut typed = ui
        .data(|d| d.get_temp::<String>(text_id))
        .unwrap_or_default();
    let combo = egui::ComboBox::from_id_salt(id.with("port"))
        .selected_text(label.as_str())
        .width(width)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show_ui(ui, |ui| {
            let mut chosen = false;
            if !settings.port.is_empty() {
                chosen |= ui
                    .selectable_value(&mut settings.port, String::new(), "(clear selection)")
                    .clicked();
            }
            match ports {
                None => {
                    ui.weak("Listing ports\u{2026}");
                }
                Some([]) => {
                    ui.weak("No ports found");
                }
                Some(ports) => {
                    for port in ports {
                        chosen |= ui
                            .selectable_value(&mut settings.port, port.clone(), port)
                            .clicked();
                    }
                }
            }
            ui.separator();
            // A port the system did not list, such as a virtual one, typed by
            // name (listener §14.4).
            let field = ui.add(
                egui::TextEdit::singleline(&mut typed)
                    .id_salt(id.with("port_typed"))
                    .desired_width(ui.available_width())
                    .hint_text(OTHER_PORT_HINT),
            );
            if let Some(port) = typed_port(field.lost_focus(), chosen, &typed) {
                settings.port = port;
            }
            if chosen || (field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                ui.close();
            }
            if chosen || !field.has_focus() {
                typed.clear();
            }
        });
    ui.data_mut(|d| d.insert_temp(text_id, typed));
    // Listed again as the drop-down opens: a port may have come or gone.
    refresh_ports |= combo.response.on_hover(&PORT_HOVER).clicked();
    refresh_ports |= ui
        .small_button(REFRESH_GLYPH)
        .on_hover_text("Refresh port list")
        .clicked();

    baud_combo(ui, id, &mut settings.baud);
    option_combo(
        ui,
        ("Data bits", &DATA_BITS_HOVER),
        id.with("data_bits"),
        &mut settings.data_bits,
        DATA_BITS,
    );
    choice_combo(
        ui,
        ("Parity", &PARITY_HOVER),
        id.with("parity"),
        &mut settings.parity,
        PARITY,
    );
    choice_combo(
        ui,
        ("Stop bits", &STOP_BITS_HOVER),
        id.with("stop_bits"),
        &mut settings.stop_bits,
        STOP_BITS,
    );

    SettingsResponse {
        changed: *settings != before,
        refresh_ports,
    }
}

/// The second line: Flow, then the control lines. RTS and DTR are outputs this
/// computer sets, shown as buttons; under RTS/CTS flow control (`rts_by_flow`)
/// RTS has words instead of a button. CTS, DSR, DCD and RI are inputs read from
/// the port, each a circle filled when high and hollow when low, with its name
/// beside it in the ordinary text colour: the fill carries the level, so
/// colour is never the only sign. `live` is the last reading while the port is
/// open; with `None` the lines are greyed out and a sentence says why.
pub fn flow_and_lines(
    ui: &mut egui::Ui,
    id: egui::Id,
    flow: &mut FlowControl,
    live: Option<ControlLines>,
    rts_by_flow: Option<RtsByFlow>,
) -> LinesResponse {
    let before = *flow;
    option_combo(
        ui,
        ("Flow", &FLOW_HOVER),
        id.with("flow"),
        flow,
        FLOW_CONTROL,
    );
    ui.separator();

    let lines = live.unwrap_or_default();
    let mut click = None;
    ui.add_enabled_ui(live.is_some(), |ui| {
        match rts_by_flow {
            Some(by_flow) => {
                let (label, hover) = rts_by_flow_text(by_flow);
                keep_together(ui, text_width(ui, label, egui::TextStyle::Body));
                ui.label(label).on_hover_text(hover);
            }
            None => {
                if ui
                    .radio(lines.rts, "RTS")
                    .on_hover(&output_line_hover("RTS (Request To Send)", lines.rts))
                    .clicked()
                {
                    click = Some(LineClick::Rts(!lines.rts));
                }
            }
        }
        if ui
            .radio(lines.dtr, "DTR")
            .on_hover(&output_line_hover("DTR (Data Terminal Ready)", lines.dtr))
            .clicked()
        {
            click = Some(LineClick::Dtr(!lines.dtr));
        }
        ui.separator();
        for (name, long_name, high) in [
            ("CTS", "CTS (Clear To Send)", lines.cts),
            ("DSR", "DSR (Data Set Ready)", lines.dsr),
            ("DCD", "DCD (Data Carrier Detect)", lines.dcd),
            ("RI", "RI (Ring Indicator)", lines.ri),
        ] {
            input_line(ui, name, long_name, high);
        }
    });
    if live.is_none() {
        // Greyed out alone would not say why, disabled widgets show no hover,
        // and hollow circles would read as lines that are low.
        keep_together(ui, text_width(ui, LINES_CLOSED, egui::TextStyle::Body));
        ui.weak(LINES_CLOSED);
    }
    LinesResponse {
        changed: *flow != before,
        click,
    }
}

const PORT_HOVER: Hover = Hover::new(
    "The serial port to use.\n\n\
     The list shows the ports the system has now, and is read again each time it is \
     opened or the refresh button beside it is clicked. A port the system does not \
     list, such as a virtual one, can be typed by name at the bottom of the list.\n\n\
     A saved port that is not there now shows as not found.",
);
const BAUD_HOVER: Hover = Hover::new(
    "The speed of the line, in bits per second.\n\n\
     Both ends must use the same rate. The list holds the usual rates; any other can \
     be typed at the bottom of the list.",
);
const DATA_BITS_HOVER: Hover = Hover::new(
    "How many data bits each character has.\n\n\
     Both ends must use the same number. 8 is usual, and 7 is often used with Even \
     or Odd parity. Some ports cannot open with 5 or 6.",
);
const PARITY_HOVER: Hover = Hover::new(
    "An extra bit sent with each character, so the receiving end can detect some \
     errors.\n\n\
     Both ends must use the same parity. None is usual.",
);
const STOP_BITS_HOVER: Hover = Hover::new(
    "The idle time that ends each character, in bit lengths.\n\n\
     Both ends must use the same number. 1 is usual.",
);
const FLOW_HOVER: Hover = Hover::new(
    "How the two ends pause each other when one cannot keep up.\n\n\
     None: nothing pauses the sender.\n\n\
     XON/XOFF: the bytes 0x13 and 0x11 in the data pause and resume sending, so \
     those two bytes cannot also be data. A port receiving with XON/XOFF takes them \
     out of the data, so they are never shown or recorded.\n\n\
     RTS/CTS: the RTS and CTS wires pause and resume sending, which needs a cable \
     that connects them.",
);

/// What the second line says while the port is not open.
const LINES_CLOSED: &str = "Control lines show while the port is open.";

/// The refresh button's glyph, a circular arrow.
const REFRESH_GLYPH: &str = "\u{2B6E}";

/// What the port box's typed-name field shows when empty.
const OTHER_PORT_HINT: &str = "other port";

/// The port drop-down is at least as wide as this, so a usual port name fits
/// with room to type, and the box keeps its width when its port goes missing.
const PORT_WIDTH_SAMPLE: &str = "COM00 (not found)";

/// Windows port names ignore case, and Windows lists them in capitals. A port
/// elsewhere is a path such as /dev/ttyUSB0, where case matters.
const PORT_NAMES_IGNORE_CASE: bool = cfg!(windows);

/// The closed port drop-down's text. The configured port outlives the hardware
/// (it is saved), so say when it is not in the list; but until a list has been
/// read, nothing is known to be missing.
fn port_label(port: &str, ports: Option<&[String]>) -> String {
    if port.is_empty() {
        "select port\u{2026}".to_owned()
    } else if ports.is_none_or(|ports| ports.iter().any(|p| p == port)) {
        port.to_owned()
    } else {
        format!("{port} (not found)")
    }
}

/// A port name typed into the box, once the box is left with a name in it,
/// unless a listed port was chosen in the same frame.
fn typed_port(lost_focus: bool, chosen: bool, text: &str) -> Option<String> {
    let name = text.trim();
    (lost_focus && !chosen && !name.is_empty()).then(|| port_name(name, PORT_NAMES_IGNORE_CASE))
}

/// A typed port name as the system lists it: in capitals where port names
/// ignore case, so "com5" matches a listed COM5, and as typed elsewhere.
fn port_name(typed: &str, ignores_case: bool) -> String {
    if ignores_case {
        typed.to_uppercase()
    } else {
        typed.to_owned()
    }
}

/// A baud rate typed into the box, once the box is left with a positive whole
/// number in it, unless an offered rate was chosen in the same frame. A
/// half-typed value never applies.
fn custom_baud(lost_focus: bool, chosen: bool, text: &str) -> Option<u32> {
    if !lost_focus || chosen {
        return None;
    }
    text.trim().parse::<u32>().ok().filter(|&rate| rate > 0)
}

/// The most digits a typed baud rate can have. Eight reach 12,000,000, the
/// fastest rate of high-speed USB serial adapters.
const BAUD_DIGITS: usize = 8;

/// Baud and its drop-down: the offered rates, then a box for any other. The
/// list stays open while the box is typed in, and closes itself after a choice.
fn baud_combo(ui: &mut egui::Ui, id: egui::Id, baud: &mut u32) {
    let text_id = id.with("baud_text");
    let mut typed = ui
        .data(|d| d.get_temp::<String>(text_id))
        .unwrap_or_default();
    // As wide as the longest rate that can be typed, so a typed rate never
    // widens the box and moves the rest of the line.
    let widest = "9".repeat(BAUD_DIGITS);
    let width = combo_width(ui, [widest.as_str(), "custom"]);
    label_for_combo(ui, "Baud", &BAUD_HOVER, width);
    egui::ComboBox::from_id_salt(id.with("baud"))
        .selected_text(baud.to_string())
        .width(width)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show_ui(ui, |ui| {
            let mut chosen = false;
            for &rate in BAUD_RATES {
                chosen |= ui.selectable_value(baud, rate, rate.to_string()).clicked();
            }
            ui.separator();
            let field = ui.add(
                egui::TextEdit::singleline(&mut typed)
                    .id_salt(id.with("baud_typed"))
                    .desired_width(ui.available_width())
                    .char_limit(BAUD_DIGITS)
                    .hint_text("custom"),
            );
            if let Some(rate) = custom_baud(field.lost_focus(), chosen, &typed) {
                *baud = rate;
            }
            if chosen || (field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                ui.close();
            }
            if chosen || !field.has_focus() {
                typed.clear();
            }
        })
        .response
        .on_hover(&BAUD_HOVER);
    ui.data_mut(|d| d.insert_temp(text_id, typed));
}

/// A label and a drop-down over (value, text) options, as wide as its longest
/// option. The label and the drop-down share the hover.
fn option_combo<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    (label, hover): (&str, &Hover),
    id: egui::Id,
    value: &mut T,
    options: &[(T, &str)],
) {
    let shown = options
        .iter()
        .find(|(v, _)| v == value)
        .map_or("", |(_, text)| *text);
    let width = combo_width(ui, options.iter().map(|(_, text)| *text));
    label_for_combo(ui, label, hover, width);
    egui::ComboBox::from_id_salt(id)
        .selected_text(shown)
        .width(width)
        .show_ui(ui, |ui| {
            for (option, text) in options {
                ui.selectable_value(value, *option, *text);
            }
        })
        .response
        .on_hover(hover);
}

/// A label and a drop-down over offered options that may hold an unsupported
/// value, shown as itself until another is chosen. The label and the drop-down
/// share the hover.
fn choice_combo<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    (label, hover): (&str, &Hover),
    id: egui::Id,
    value: &mut Choice<T>,
    options: &[(T, &str)],
) {
    let shown = choice_text(*value, options);
    let width = combo_width(
        ui,
        options
            .iter()
            .map(|(_, text)| *text)
            .chain(std::iter::once(shown.as_str())),
    );
    label_for_combo(ui, label, hover, width);
    egui::ComboBox::from_id_salt(id)
        .selected_text(shown)
        .width(width)
        .show_ui(ui, |ui| {
            for (option, text) in options {
                ui.selectable_value(value, Choice::Offered(*option), *text);
            }
        })
        .response
        .on_hover(hover);
}

/// The text a [`Choice`] shows in its closed drop-down.
fn choice_text<T: PartialEq>(value: Choice<T>, options: &[(T, &str)]) -> String {
    match value {
        Choice::Offered(v) => options
            .iter()
            .find(|(o, _)| *o == v)
            .map_or_else(String::new, |(_, text)| (*text).to_owned()),
        Choice::Unsupported(what) => format!("{what} (not supported)"),
    }
}

/// The width a drop-down needs to show its widest text without clipping.
fn combo_width<'a>(ui: &egui::Ui, texts: impl IntoIterator<Item = &'a str>) -> f32 {
    let widest = texts
        .into_iter()
        .map(|text| text_width(ui, text, egui::TextStyle::Button))
        .fold(0.0, f32::max);
    let spacing = ui.spacing();
    widest + spacing.icon_width + spacing.icon_spacing + spacing.button_padding.x * 2.0
}

/// The width of `text` drawn in `style`.
fn text_width(ui: &egui::Ui, text: &str, style: egui::TextStyle) -> f32 {
    ui.painter()
        .layout_no_wrap(
            text.to_owned(),
            style.resolve(ui.style()),
            egui::Color32::PLACEHOLDER,
        )
        .size()
        .x
}

/// In a wrapping line, start a new row first when the pieces about to be
/// added, `width` wide together, would not fit on the rest of this one. A
/// drop-down does not move to a new row by itself: it would run past the edge,
/// or leave its label at the end of one row and itself at the start of the
/// next. At the start of a row, and in a line that does not wrap, this does
/// nothing.
fn keep_together(ui: &mut egui::Ui, width: f32) {
    let at_row_start = ui.available_rect_before_wrap().left() <= ui.max_rect().left() + 0.5;
    if !at_row_start && ui.available_size_before_wrap().x < width {
        ui.end_row();
    }
}

/// A label, kept on one row with the `combo`-wide drop-down that follows it.
fn label_for_combo(ui: &mut egui::Ui, label: &str, hover: &Hover, combo: f32) {
    let width = text_width(ui, label, egui::TextStyle::Body) + ui.spacing().item_spacing.x + combo;
    keep_together(ui, width);
    ui.label(label).on_hover(hover);
}

/// The words for RTS under RTS/CTS flow control, and their hover.
fn rts_by_flow_text(by_flow: RtsByFlow) -> (&'static str, &'static str) {
    match by_flow {
        RtsByFlow::High => (
            "RTS high, set by flow control",
            "With RTS/CTS flow control, the port opens with RTS high and keeps it high, \
             so it is not set from here.",
        ),
        RtsByFlow::Paced => (
            "RTS set by flow control",
            "With RTS/CTS flow control, the driver raises and lowers RTS to pace the \
             sender, so it is not set from here and its level is not shown.",
        ),
    }
}

/// The hover of an output line's button: what the line is, its level now, and
/// what a click does.
fn output_line_hover(name: &str, high: bool) -> Hover {
    let (now, then) = if high {
        ("high", "low")
    } else {
        ("low", "high")
    };
    Hover::from_paragraphs([
        format!("{name} is an output this computer sets."),
        format!("It is {now} now. Click to set it {then}."),
    ])
}

/// The hover of an input line: what the line is, and its level now.
fn input_line_hover(name: &str, high: bool) -> Hover {
    let now = if high { "high" } else { "low" };
    Hover::from_paragraphs([
        format!("{name} is an input read from the port."),
        format!("It is {now} now."),
    ])
}

/// How an input line's circle is drawn: its fill, if any, and its outline.
/// Only the circle shows the level; the name keeps the ordinary text colour.
fn circle_style(high: bool, ui: &egui::Ui) -> (Option<egui::Color32>, egui::Stroke) {
    let width: f32 = 1.5;
    if high {
        let accent = crate::palette::active(ui).running;
        (Some(accent), egui::Stroke::new(width, accent))
    } else {
        (None, egui::Stroke::new(width, ui.visuals().text_color()))
    }
}

/// The diameter of an input line's circle: about the height of the RTS and
/// DTR buttons beside it, larger than the text, so the level reads at a glance.
fn circle_diameter(ui: &egui::Ui) -> f32 {
    ui.spacing().interact_size.y
}

/// The gap between an input line's circle and its name: a quarter of the gap
/// between widgets, so the name reads as the circle's and not the next one's.
fn name_gap(ui: &egui::Ui) -> f32 {
    ui.spacing().item_spacing.x / 4.0
}

/// One input line: its circle, then its name, drawn as one widget so a
/// wrapping line never puts them on different rows.
fn input_line(ui: &mut egui::Ui, name: &str, long_name: &str, high: bool) {
    let hover = input_line_hover(long_name, high);
    let diameter = circle_diameter(ui);
    let gap = name_gap(ui);
    // The colour a label has, which a disabled Ui fades as it fades a label.
    let text_color = ui.visuals().text_color();
    let galley = ui.painter().layout_no_wrap(
        name.to_owned(),
        egui::TextStyle::Body.resolve(ui.style()),
        text_color,
    );
    let size = egui::vec2(
        diameter + gap + galley.size().x,
        diameter.max(galley.size().y),
    );
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let (fill, stroke) = circle_style(high, ui);
        let radius = diameter / 2.0 - stroke.width;
        let centre = egui::pos2(rect.left() + diameter / 2.0, rect.center().y);
        let painter = ui.painter();
        if let Some(fill) = fill {
            painter.circle_filled(centre, radius, fill);
        }
        painter.circle_stroke(centre, radius, stroke);
        let text_at = egui::pos2(
            rect.left() + diameter + gap,
            rect.center().y - galley.size().y / 2.0,
        );
        painter.galley(text_at, galley, text_color);
    }
    response.on_hover(&hover);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts<T>(options: &[(T, &str)]) -> Vec<String> {
        options.iter().map(|(_, text)| (*text).to_owned()).collect()
    }

    #[test]
    fn the_lists_offer_only_what_a_port_opens_with() {
        assert_eq!(texts(DATA_BITS), ["8", "7", "6", "5"]);
        assert_eq!(texts(PARITY), ["None", "Even", "Odd"]);
        assert_eq!(texts(STOP_BITS), ["1", "2"]);
        assert_eq!(texts(FLOW_CONTROL), ["None", "XON/XOFF", "RTS/CTS"]);
        assert_eq!(BAUD_RATES, [4800, 9600, 19200, 38400, 57600, 115200]);
    }

    #[test]
    fn an_unsupported_value_shows_as_itself() {
        assert_eq!(choice_text(Choice::Offered(Parity::Even), PARITY), "Even");
        assert_eq!(
            choice_text(Choice::<Parity>::Unsupported("Mark"), PARITY),
            "Mark (not supported)"
        );
        assert_eq!(
            choice_text(Choice::<StopBits>::Unsupported("1.5"), STOP_BITS),
            "1.5 (not supported)"
        );
    }

    #[test]
    fn the_port_box_says_when_a_port_is_not_listed() {
        let listed = ["COM3".to_owned()];
        assert_eq!(port_label("", Some(&listed)), "select port\u{2026}");
        assert_eq!(port_label("COM3", Some(&listed)), "COM3");
        assert_eq!(port_label("COM4", Some(&listed)), "COM4 (not found)");
        assert_eq!(
            port_label("COM4", None),
            "COM4",
            "nothing is known to be missing before a list is read"
        );
    }

    #[test]
    fn a_typed_port_applies_only_when_the_box_is_left() {
        assert_eq!(typed_port(false, false, "COM9"), None, "still typing");
        assert_eq!(typed_port(true, false, "  COM9 "), Some("COM9".to_owned()));
        assert_eq!(typed_port(true, false, "   "), None, "nothing typed");
        assert_eq!(
            typed_port(true, true, "COM9"),
            None,
            "a listed port chosen in the same frame wins"
        );
    }

    #[test]
    fn a_typed_port_is_put_in_capitals_only_where_case_is_ignored() {
        assert_eq!(port_name("com5", true), "COM5");
        assert_eq!(
            port_name("/dev/ttyUSB0", false),
            "/dev/ttyUSB0",
            "a path keeps its case"
        );
    }

    /// The port box keeps one width whether or not its port is listed, so
    /// the line does not shift when a device is unplugged.
    #[test]
    fn the_port_box_keeps_its_width_when_the_port_goes_missing() {
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let width = |label: &str| combo_width(ui, [label, OTHER_PORT_HINT, PORT_WIDTH_SAMPLE]);
            assert_eq!(width("COM3"), width("COM3 (not found)"));
        });
    }

    #[test]
    fn a_typed_baud_applies_only_when_whole_and_left() {
        assert_eq!(custom_baud(false, false, "250000"), None, "still typing");
        assert_eq!(custom_baud(true, false, "250000"), Some(250_000));
        assert_eq!(
            custom_baud(true, false, " 1200 "),
            Some(1200),
            "slower rates are typed"
        );
        assert_eq!(custom_baud(true, false, "0"), None);
        assert_eq!(custom_baud(true, false, "fast"), None);
        assert_eq!(
            custom_baud(true, true, "250000"),
            None,
            "an offered rate wins"
        );
    }

    #[test]
    fn hovers_name_the_line_and_its_level() {
        let input = input_line_hover("CTS (Clear To Send)", true);
        assert!(input.text().contains("CTS (Clear To Send) is an input"));
        assert!(input.text().contains("It is high now."));
        let output = output_line_hover("DTR (Data Terminal Ready)", false);
        assert!(output
            .text()
            .contains("It is low now. Click to set it high."));
    }

    /// A low line and a high one must differ with the colour taken away: the
    /// high circle is filled and the low one is not.
    #[test]
    fn a_line_level_is_shown_by_fill_not_colour_alone() {
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let (high_fill, _) = circle_style(true, ui);
            let (low_fill, _) = circle_style(false, ui);
            assert!(high_fill.is_some(), "a high line is filled");
            assert!(low_fill.is_none(), "a low line is hollow");
            assert!(
                circle_diameter(ui) > ui.text_style_height(&egui::TextStyle::Body),
                "the circle is larger than the text beside it"
            );
        });
    }

    fn settings() -> SerialSettings {
        SerialSettings {
            port: "COM3".to_owned(),
            baud: 115_200,
            data_bits: DataBits::Eight,
            parity: Choice::Offered(Parity::None),
            stop_bits: Choice::Offered(StopBits::One),
            flow: FlowControl::RtsCts,
        }
    }

    /// Draw both lines `wrapped` or not in a `width`-point `Ui`, and return
    /// each line's rect and the room it had.
    fn lay_out(width: f32, wrapped: bool) -> (egui::Rect, egui::Rect, egui::Rect) {
        let ctx = egui::Context::default();
        let mut rects = None;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.set_max_width(width);
            let room = ui.max_rect();
            let mut s = settings();
            let ports = ["COM3".to_owned(), "COM4".to_owned()];
            let live = ControlLines::default();
            let id = egui::Id::new("layout_test");
            let line = |ui: &mut egui::Ui, add: &mut dyn FnMut(&mut egui::Ui)| {
                if wrapped {
                    ui.horizontal_wrapped(|ui| add(ui)).response.rect
                } else {
                    ui.horizontal(|ui| add(ui)).response.rect
                }
            };
            let first = line(ui, &mut |ui| {
                settings_line(ui, id, &mut s, Some(&ports));
            });
            let mut flow = s.flow;
            let second = line(ui, &mut |ui| {
                flow_and_lines(ui, id, &mut flow, Some(live), Some(RtsByFlow::High));
            });
            rects = Some((first, second, room));
        });
        rects.expect("the frame ran")
    }

    /// In a channel's detail pane, about 320 points wide, the lines wrap
    /// rather than run off the edge.
    #[test]
    fn the_lines_fit_a_narrow_pane_when_wrapped() {
        let (first, second, room) = lay_out(320.0, true);
        for (name, line) in [("first", first), ("second", second)] {
            assert!(
                line.right() <= room.right() + 0.5,
                "the {name} line runs past the pane: {line:?} in {room:?}"
            );
        }
        // The same lines unwrapped do run past it, so the check above can fail.
        let (first, second, room) = lay_out(320.0, false);
        assert!(first.right() > room.right() && second.right() > room.right());
    }

    /// Choosing another port, losing it, or typing a long baud rate does not
    /// move the rest of the line.
    #[test]
    fn the_line_keeps_its_width_whatever_the_port_or_baud() {
        let listed = ["COM3".to_owned(), "COM4".to_owned()];
        let none: [String; 0] = [];
        let widths: Vec<(String, f32)> = [
            ("", Some(&listed[..]), 9600),
            ("COM3", Some(&listed[..]), 9600),
            ("COM4", Some(&listed[..]), 9600),
            ("COM9", Some(&listed[..]), 9600),
            ("COM3", None, 9600),
            ("COM3", Some(&none[..]), 9600),
            ("COM3", Some(&listed[..]), 1200),
            ("COM3", Some(&listed[..]), 99_999_999),
        ]
        .into_iter()
        .map(|(port, ports, baud)| {
            let ctx = egui::Context::default();
            let mut width = 0.0;
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                let mut s = SerialSettings {
                    port: port.to_owned(),
                    baud,
                    ..settings()
                };
                width = ui
                    .horizontal(|ui| settings_line(ui, egui::Id::new("w"), &mut s, ports))
                    .response
                    .rect
                    .width();
            });
            (format!("{port:?} with {ports:?} at {baud}"), width)
        })
        .collect();
        let first = widths[0].1;
        for (case, width) in &widths {
            assert!(
                (width - first).abs() < 0.5,
                "the line is {width} wide for {case}, {first} for the first: {widths:?}"
            );
        }
    }

    /// In Linemon's wide window each line stays one row.
    #[test]
    fn each_line_is_one_row_when_there_is_room() {
        let (first, second, _) = lay_out(1400.0, false);
        let ctx = egui::Context::default();
        let mut row = 0.0;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            row = ui.spacing().interact_size.y;
        });
        for (name, line) in [("first", first), ("second", second)] {
            assert!(
                line.height() < row * 1.5,
                "the {name} line is more than one row: {line:?}"
            );
        }
    }
}
