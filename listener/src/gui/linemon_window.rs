//! Linemon's window (listener ADR-070, spec §50.3): one serial or UDP port at a
//! time. The `linemon` program draws it as its own window.
//!
//! The window never owns a port. It asks the runtime's hidden Channel to open
//! one ([`UiCommand::LinemonOpen`]) and keeps what that Channel sends in a
//! [`LinemonStream`](super::linemon::LinemonStream). What the window wants
//! (started or not, which port, which settings) lives here; what the runtime
//! was last asked for is `sent`, and [`Linemon::sync`] sends the difference.
//!
//! The decisions that can be made without egui are plain functions at the top
//! of the file: the settings and their stored form, the marker line, the port
//! description, the status sentence, and stepping the UDP port number.

use serde::{Deserialize, Serialize};
use wiredata_ui::diagnostics::{tone_marker, SignalTone};
use wiredata_ui::format::{data_bits_hint, thousands, UDP_BIND_HINT};
use wiredata_ui::hover::{Hover, ShowHover};

use crate::config::{
    templates, ChannelConfig, DataBits, FlowControl, InterfaceConfig, Parity, SerialConfig,
    StopBits, UdpConfig,
};
use crate::core::ChannelState;
use crate::runtime::{transport_fault_prefix, ChannelSnapshot, BIND_FAILURE};
use crate::transport::serial::{open_error_prefix, RTS_HIGH_UNDER_RTS_CTS};
use crate::transport::udp::UdpMode;

use super::bind_scope::LocalAddresses;
use super::bridge::{LinemonUpdate, UiCommand};
use super::linemon::LinemonStream;
use super::port_list::PortList;
use super::view_prefs::ViewPrefs;
use super::widgets::{
    apply_serial_settings, control_lines, rts_by_flow, serial_settings, template_for,
    udp_bind_choice, udp_mode_and_sharing, AddKind, LineClick, Refresh, UDP_MODE_HOVER,
    UDP_PORT_HOVER,
};
use super::StreamRenderCache;
use crate::transport::SerialControlLines;

/// eframe storage key for Linemon's remembered settings, in the `linemon`
/// program's own storage.
pub(super) const LINEMON_KEY: &str = "linemon.settings";

/// The size the window opens at.
pub(super) const DEFAULT_SIZE: [f32; 2] = [960.0, 640.0];
/// The smallest size it can be dragged to: wide enough for the row of five
/// buttons at [`BUTTON_SIZE`].
pub(super) const MIN_SIZE: [f32; 2] = [600.0, 360.0];
/// How long a new source's settings take to fade in, in seconds.
const SOURCE_FADE_SECONDS: f64 = 0.15;

/// The size of Start, Stop, Pause, Clear and Copy scroll buffer: the size of
/// Listener's channel Start and Stop buttons.
const BUTTON_SIZE: egui::Vec2 = egui::vec2(96.0, 32.0);
const START_HOVER: Hover = Hover::new(
    "Opens the port shown above.\n\n\
     While Linemon is started, choosing another port or changing a setting takes \
     effect at once.",
);
const STOP_HOVER: Hover = Hover::new(
    "Closes the port.\n\n\
     What is on the display stays until you press Clear.",
);
const PAUSE_HOVER: Hover = Hover::new(
    "Stops the display so that you can read it.\n\n\
     The port stays open. Bytes that arrive while the display is paused are not shown.",
);
const CLEAR_HOVER: Hover = Hover::new("Empties the display.");
const COPY_ALL_HOVER: Hover = Hover::new(
    "Copies the whole scroll buffer to the clipboard, as it is displayed.\n\n\
     That is everything you can scroll back to, not only what is on screen. Its \
     size is set under Configure display.\n\n\
     To copy part of it, select the text and press Ctrl+C.",
);
const CLEAR_ON_CHANGE_HOVER: Hover = Hover::new(
    "Empties the display when you choose another port.\n\n\
     With this off, the data from every port you look at stays on the display, with a \
     line marking each change.",
);
const REOPEN_HOVER: Hover = Hover::new(
    "Opens the port Linemon last had as soon as its window opens.\n\n\
     With this off, Linemon waits for you to press Start.",
);
const UDP_PREVIOUS_HOVER: Hover = Hover::new("Moves to the port number one lower.");
const UDP_NEXT_HOVER: Hover = Hover::new("Moves to the port number one higher.");

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Which kind of port Linemon is looking at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum SourceKind {
    #[default]
    Serial,
    Udp,
}

/// What Linemon remembers between launches (spec §50.3): the kind of port, each
/// kind's settings, the display preferences and the two checkboxes. Whether the
/// port was open is not part of it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct LinemonSettings {
    pub kind: SourceKind,
    pub serial: SerialConfig,
    pub udp: UdpConfig,
    pub view: crate::config::schema::DisplayViewConfig,
    pub scroll_buffer_bytes: usize,
    pub clear_on_port_change: bool,
    pub reopen_on_start: bool,
}

impl Default for LinemonSettings {
    fn default() -> Self {
        let serial = match templates::serial_template().interface {
            InterfaceConfig::Serial(serial) => serial,
            InterfaceConfig::Udp(_) => unreachable!("the serial template is serial"),
        };
        let udp = match template_for(AddKind::Udp).interface {
            InterfaceConfig::Udp(udp) => udp,
            InterfaceConfig::Serial(_) => unreachable!("the UDP template is UDP"),
        };
        let mut settings = Self {
            kind: SourceKind::Serial,
            serial,
            udp,
            view: Default::default(),
            scroll_buffer_bytes: 0,
            clear_on_port_change: false,
            reopen_on_start: true,
        };
        settings.take_prefs(&ViewPrefs::default());
        settings
    }
}

impl LinemonSettings {
    /// The settings stored in `text`, or the defaults when it is missing or
    /// cannot be read.
    pub fn from_stored(text: Option<&str>) -> Self {
        text.and_then(|text| toml::from_str(text).ok())
            .unwrap_or_default()
    }

    /// The text to store.
    pub fn to_stored(&self) -> String {
        toml::to_string(self).unwrap_or_default()
    }

    /// The display preferences these settings hold.
    pub fn prefs(&self) -> ViewPrefs {
        let mut config = templates::serial_template();
        config.display.view = self.view.clone();
        config.retention.byte_limit = Some(self.scroll_buffer_bytes);
        ViewPrefs::from_config(&config)
    }

    /// Hold `prefs` as the display preferences.
    pub fn take_prefs(&mut self, prefs: &ViewPrefs) {
        let mut config = templates::serial_template();
        prefs.apply_to_config(&mut config);
        self.view = config.display.view;
        self.scroll_buffer_bytes = prefs.scroll_buffer_bytes;
    }

    /// The interface the bar describes.
    pub fn interface(&self) -> InterfaceConfig {
        match self.kind {
            SourceKind::Serial => InterfaceConfig::Serial(self.serial.clone()),
            SourceKind::Udp => InterfaceConfig::Udp(self.udp.clone()),
        }
    }
}

/// The hidden Channel's config: the template for the kind of port, the
/// interface from the bar, the display and retention from `prefs`, no match
/// rules and no reconnecting. The runtime names the Channel and turns
/// recording off.
fn build_config(interface: &InterfaceConfig, prefs: &ViewPrefs) -> ChannelConfig {
    let kind = match interface {
        InterfaceConfig::Serial(_) => AddKind::Serial,
        InterfaceConfig::Udp(_) => AddKind::Udp,
    };
    let mut config = template_for(kind);
    config.interface = interface.clone();
    prefs.apply_to_config(&mut config);
    config.match_rules.clear();
    config.reconnect.enabled = false;
    config
}

// ---------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------

/// A port by its identity: the name of a serial port, or a UDP port number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PortKey {
    Serial(String),
    Udp(u16),
}

/// The port an interface names, or `None` while none is chosen.
fn port_key(interface: &InterfaceConfig) -> Option<PortKey> {
    match interface {
        InterfaceConfig::Serial(serial) => {
            let name = serial.port.trim();
            (!name.is_empty()).then(|| PortKey::Serial(name.to_owned()))
        }
        InterfaceConfig::Udp(udp) => (udp.port != 0).then_some(PortKey::Udp(udp.port)),
    }
}

/// Whether moving from `previous` to `next` is a different port: another
/// serial port (names compared ignoring ASCII case), another UDP port number,
/// or a change between serial and UDP. A change of settings is not.
fn is_port_change(previous: &PortKey, next: &PortKey) -> bool {
    match (previous, next) {
        (PortKey::Serial(a), PortKey::Serial(b)) => !a.eq_ignore_ascii_case(b),
        (PortKey::Udp(a), PortKey::Udp(b)) => a != b,
        _ => true,
    }
}

/// The port as a sentence names it: `COM4` or `UDP port 5005`.
fn describe_port(key: &PortKey) -> String {
    match key {
        PortKey::Serial(name) => name.clone(),
        PortKey::Udp(port) => format!("UDP port {port}"),
    }
}

/// A serial line in the conventional compact form: `9600 8N1`. Flow control is
/// not part of it.
fn serial_line_text(serial: &SerialConfig) -> String {
    let data_bits = match serial.data_bits {
        DataBits::Five => "5",
        DataBits::Six => "6",
        DataBits::Seven => "7",
        DataBits::Eight => "8",
    };
    let parity = match serial.parity {
        Parity::None => "N",
        Parity::Even => "E",
        Parity::Odd => "O",
        Parity::Mark => "M",
        Parity::Space => "S",
    };
    let stop_bits = match serial.stop_bits {
        StopBits::One => "1",
        StopBits::OnePointFive => "1.5",
        StopBits::Two => "2",
    };
    format!("{} {data_bits}{parity}{stop_bits}", serial.baud_rate)
}

/// The words for a flow control, `none` included, so a marker always says.
fn flow_control_text(flow: FlowControl) -> &'static str {
    match flow {
        FlowControl::None => "flow control: none",
        FlowControl::RtsCts => "flow control: RTS/CTS",
        FlowControl::XonXoff => "flow control: XON/XOFF",
    }
}

/// The line that marks a change that left no port chosen: `— Serial, no port
/// chosen —` or `— UDP, no port chosen —`.
fn no_port_text(interface: &InterfaceConfig) -> String {
    let kind = match interface {
        InterfaceConfig::Serial(_) => "Serial",
        InterfaceConfig::Udp(_) => "UDP",
    };
    format!("\u{2014} {kind}, no port chosen \u{2014}")
}

/// The line that marks an open on the display: `— COM4, 9600 8N1, flow
/// control: none, RTS low, DTR low —` or `— UDP port 5005 —`, with broadcast
/// or the multicast group added when they apply. A serial port's outputs are
/// the levels the open sets.
fn marker_text(interface: &InterfaceConfig) -> String {
    marker_text_with(interface, None)
}

/// The marker line for `interface`, with a serial port's outputs at the
/// levels in `lines`, or at the levels an open sets when `lines` is `None`.
/// Every serial marker carries the whole settings line, so each one reads on
/// its own.
fn marker_text_with(interface: &InterfaceConfig, lines: Option<SerialControlLines>) -> String {
    let Some(key) = port_key(interface) else {
        return String::new();
    };
    let mut text = describe_port(&key);
    match interface {
        InterfaceConfig::Serial(serial) => {
            text.push_str(", ");
            text.push_str(&serial_line_text(serial));
            text.push_str(", ");
            text.push_str(flow_control_text(serial.flow_control));
            text.push_str(", ");
            text.push_str(&outputs_text(serial, lines));
        }
        InterfaceConfig::Udp(udp) => match udp.mode {
            UdpMode::Unicast => {}
            UdpMode::Broadcast => text.push_str(", broadcast"),
            UdpMode::Multicast => {
                text.push_str(", multicast");
                if let Some(group) = udp.multicast_group.as_deref().map(str::trim) {
                    if !group.is_empty() {
                        text.push_str(" group ");
                        text.push_str(group);
                    }
                }
            }
        },
    }
    format!("\u{2014} {text} \u{2014}")
}

/// The outputs as a marker gives them: `RTS low, DTR low`, from `lines` or,
/// when `None`, the levels an open sets (ADR-073). Every open sets them, so a
/// settings change that reopens the port puts them back, and the marker shows
/// it. Under RTS/CTS, RTS is given only where its level is known
/// ([`RTS_HIGH_UNDER_RTS_CTS`]).
fn outputs_text(serial: &SerialConfig, lines: Option<SerialControlLines>) -> String {
    let level = |high: bool| if high { "high" } else { "low" };
    let rts_cts = serial.flow_control == FlowControl::RtsCts;
    let rts_known = !rts_cts || RTS_HIGH_UNDER_RTS_CTS || serial.rts.is_some();
    let (rts, dtr) = match lines {
        Some(lines) => (lines.rts, lines.dtr),
        None => (serial.rts.unwrap_or(rts_cts), serial.dtr.unwrap_or(false)),
    };
    if rts_known {
        format!("RTS {}, DTR {}", level(rts), level(dtr))
    } else {
        format!("DTR {}", level(dtr))
    }
}

/// The UDP port number one lower or higher, staying within 1 to 65535. A port
/// of 0 (none chosen) steps to 1.
fn step_udp_port(port: u16, step: i32) -> u16 {
    let next = i32::from(port) + step;
    next.clamp(1, i32::from(u16::MAX)) as u16
}

// ---------------------------------------------------------------------------
// Status line
// ---------------------------------------------------------------------------

/// What the hidden Channel last reported.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct RunFacts {
    pub state: ChannelState,
    pub bytes_total: u64,
    pub bytes_per_sec: f64,
    /// The newest error diagnostic's message.
    pub fault: Option<String>,
}

impl RunFacts {
    fn from_snapshot(snapshot: &ChannelSnapshot) -> Self {
        let fault = snapshot
            .diagnostics
            .errors
            .iter()
            .max_by_key(|d| d.timestamp)
            .map(|d| d.message.clone());
        Self {
            state: snapshot.state,
            bytes_total: snapshot.activity.total_bytes,
            bytes_per_sec: snapshot.activity.bytes_per_sec,
            fault,
        }
    }
}

/// Everything the status sentence depends on.
pub(super) struct StatusFacts<'a> {
    pub interface: &'a InterfaceConfig,
    pub started: bool,
    pub paused: bool,
    /// Why the hidden Channel could not open the port.
    pub open_error: Option<&'a str>,
    pub run: Option<&'a RunFacts>,
    /// The serial port is not in the port list as last refreshed. The list
    /// is refreshed when the port fails, so this says the device has gone.
    pub port_unlisted: bool,
}

/// The status line: its text, and whether it reports a problem. A problem leads
/// with the warning glyph, so it is not told by colour alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StatusLine {
    pub text: String,
    pub problem: bool,
}

fn plural_bytes(count: u64) -> String {
    if count == 1 {
        "1 byte".to_owned()
    } else {
        format!("{} bytes", thousands(count))
    }
}

/// A reason as the end of a sentence: with its full stop.
fn with_full_stop(reason: &str) -> String {
    let reason = reason.trim();
    if reason.ends_with(['.', '!', '?']) {
        reason.to_owned()
    } else {
        format!("{reason}.")
    }
}

/// An open error without the words that say it was an open: the serial
/// transport's `opening serial port "COM3": `, or the runtime's `failed to
/// bind interface: `. The status line already says the port could not be
/// opened.
fn without_open_prefix<'a>(reason: &'a str, interface: &InterfaceConfig) -> &'a str {
    let prefix = match interface {
        InterfaceConfig::Serial(serial) => open_error_prefix(&serial.port),
        InterfaceConfig::Udp(_) => format!("{BIND_FAILURE}: "),
    };
    reason.strip_prefix(prefix.as_str()).unwrap_or(reason)
}

/// A serial setting's data bits as a number.
pub(super) fn data_bits_number(bits: crate::config::DataBits) -> u8 {
    use crate::config::DataBits;
    match bits {
        DataBits::Five => 5,
        DataBits::Six => 6,
        DataBits::Seven => 7,
        DataBits::Eight => 8,
    }
}

/// A stopped port's cause without the runtime's "transport fault on channel
/// Linemon: ", since the status line names the port already.
fn without_fault_prefix(reason: &str) -> &str {
    let prefix = transport_fault_prefix(super::bridge::LINEMON_CHANNEL_NAME);
    reason.strip_prefix(prefix.as_str()).unwrap_or(reason)
}

/// Whether an output differs between two readings of the control lines. A
/// reading follows the reader setting the line, so a marker written for it
/// records what the port did, not a click.
fn outputs_changed(before: SerialControlLines, now: SerialControlLines) -> bool {
    before.rts != now.rts || before.dtr != now.dtr
}

fn status_line(facts: &StatusFacts) -> StatusLine {
    let ok = |text: String| StatusLine {
        text,
        problem: false,
    };
    let problem = |text: String| StatusLine {
        text: format!("{}{text}", tone_marker(SignalTone::Warning)),
        problem: true,
    };
    let Some(key) = port_key(facts.interface) else {
        // Started, Start is disabled and a port chosen now opens at once, so
        // Start is only mentioned while Linemon is stopped.
        return ok(if facts.started {
            "Choose a port. Linemon opens it as soon as you choose one.".to_owned()
        } else {
            "Choose a port, then press Start.".to_owned()
        });
    };
    let port = describe_port(&key);
    if !facts.started {
        return ok(format!("Stopped. Press Start to open {port}."));
    }
    let gone = if facts.port_unlisted {
        " It is no longer in the port list, so the device may have been removed."
    } else {
        ""
    };
    if let Some(reason) = facts.open_error {
        // A port that refuses 5 or 6 data bits says only that a parameter is
        // wrong, so say which one it probably is; a refused bind seldom says
        // what holds the port, so say what may.
        let hint = match facts.interface {
            InterfaceConfig::Serial(serial) => data_bits_hint(data_bits_number(serial.data_bits)),
            InterfaceConfig::Udp(_) => reason.starts_with(BIND_FAILURE).then_some(UDP_BIND_HINT),
        }
        .map(|hint| format!(" {hint}"))
        .unwrap_or_default();
        let reason = without_open_prefix(reason, facts.interface);
        return problem(format!(
            "{port} could not be opened: {}{hint}{gone}",
            with_full_stop(reason)
        ));
    }
    let Some(run) = facts.run else {
        return ok(format!("Opening {port}\u{2026}"));
    };
    match run.state {
        ChannelState::Faulted => {
            let reason = run
                .fault
                .as_deref()
                .map(without_fault_prefix)
                .map_or_else(|| "no reason was reported.".to_owned(), with_full_stop);
            problem(format!(
                "{port} stopped: {reason}{gone} Press Start to try again."
            ))
        }
        ChannelState::Running => {
            let open = match facts.interface {
                InterfaceConfig::Serial(serial) => {
                    format!("{port} is open at {}.", serial_line_text(serial))
                }
                InterfaceConfig::Udp(_) => format!("{port} is open."),
            };
            let rate = thousands(run.bytes_per_sec.max(0.0).round() as u64);
            let mut text = format!(
                "{open} {} received, {rate} bytes per second.",
                plural_bytes(run.bytes_total)
            );
            if facts.paused {
                text.push_str(" The display is paused, so arriving bytes are not shown.");
            }
            ok(text)
        }
        ChannelState::Stopped | ChannelState::Starting | ChannelState::Stopping => {
            ok(format!("Opening {port}\u{2026}"))
        }
    }
}

// ---------------------------------------------------------------------------
// The window
// ---------------------------------------------------------------------------

/// Linemon: the window's state, its display, and what it has asked the runtime
/// for.
pub(super) struct Linemon {
    settings: LinemonSettings,
    prefs: ViewPrefs,
    stream: LinemonStream,
    cache: Option<StreamRenderCache>,
    /// The user has pressed Start (or Linemon started with Reopen port on
    /// start) and has not pressed Stop. The port opens whenever one is chosen.
    started: bool,
    paused: bool,
    /// The interface the runtime's hidden Channel was last asked to open and
    /// has not been asked to close since.
    sent: Option<InterfaceConfig>,
    /// The port last opened, for Clear display on port change. Kept through Stop.
    last_port: Option<PortKey>,
    /// The display was marked "no port chosen" while started. The next port
    /// opened is then a change of port, whatever port was open before.
    marked_no_port: bool,
    open_error: Option<String>,
    run: Option<RunFacts>,
    /// The open serial port's control lines, as last read. `None` while no
    /// serial port is open.
    control_lines: Option<SerialControlLines>,
    /// The serial ports last listed, `None` until the first list arrives.
    serial_ports: Option<Vec<String>>,
    /// Lists the serial ports on a helper thread, never on the window's.
    port_list: PortList,
    /// The read count of the list in `serial_ports`.
    port_list_seen: u64,
    /// The serial port failed, so the port list is read again at the next
    /// frame: a removed adapter then shows as not found.
    ports_stale: bool,
    /// Text typed into the UDP port field.
    udp_port_text: String,
    jump_to_bottom: bool,
    copy_all: bool,
    /// A text field in the window has the keyboard. A value still being typed,
    /// such as a multicast group, is not complete until the field is left, so
    /// `sync` holds back a changed interface meanwhile.
    editing_text: bool,
    /// The window's minimum width: [`MIN_SIZE`] at first, raised to fit the
    /// port settings line once it has been drawn, so its items never wrap.
    min_width: f32,
    /// The source whose settings were drawn last frame, and when the source
    /// last changed (egui's clock, in seconds), for the fade.
    drawn_kind: SourceKind,
    kind_changed_at: f64,
}

impl Linemon {
    /// Linemon with `stored` as its remembered settings (`None`, or text that
    /// cannot be read, gives the defaults). It starts stopped; see
    /// [`start_up`](Self::start_up).
    pub fn new(stored: Option<&str>) -> Self {
        let settings = LinemonSettings::from_stored(stored);
        let prefs = settings.prefs();
        // The source the window opens on is already drawn, so only a change of
        // source fades in.
        let drawn_kind = settings.kind;
        Self {
            stream: LinemonStream::new(prefs.scroll_buffer_bytes),
            settings,
            prefs,
            cache: None,
            started: false,
            paused: false,
            sent: None,
            last_port: None,
            marked_no_port: false,
            control_lines: None,
            open_error: None,
            run: None,
            serial_ports: None,
            port_list: PortList::default(),
            port_list_seen: 0,
            ports_stale: false,
            udp_port_text: String::new(),
            jump_to_bottom: false,
            copy_all: false,
            editing_text: false,
            min_width: MIN_SIZE[0],
            drawn_kind,
            kind_changed_at: f64::NEG_INFINITY,
        }
    }

    /// The settings to store.
    pub fn to_stored(&self) -> String {
        let mut settings = self.settings.clone();
        settings.take_prefs(&self.prefs);
        settings.to_stored()
    }

    /// Get ready as the program starts: list the serial ports, and open the
    /// port Linemon last had when Reopen port on start is ticked and a port is
    /// chosen.
    pub fn start_up(&mut self) {
        self.port_list.refresh();
        self.jump_to_bottom = true;
        self.started =
            self.settings.reopen_on_start && port_key(&self.settings.interface()).is_some();
    }

    /// The last request did not reach the runtime, which is busy or has
    /// stopped. Said as the port's open error, so Start is offered again and
    /// sends it again.
    pub fn request_not_delivered(&mut self) {
        if self.sent.is_some() {
            self.open_error = Some(
                "the request did not reach Linemon's runtime, which is busy or has stopped. \
                 Press Start to try again."
                    .to_owned(),
            );
        }
    }

    /// Fold an update about the hidden Channel. Stream bytes that arrive while
    /// the display is paused are not shown.
    pub fn handle(&mut self, update: LinemonUpdate) {
        match update {
            LinemonUpdate::Stream(delta) => {
                if !self.paused {
                    self.stream.append(&delta);
                }
            }
            LinemonUpdate::Snapshot(snapshot) => {
                if self.sent.is_some() {
                    self.take_run(RunFacts::from_snapshot(&snapshot));
                }
            }
            LinemonUpdate::Error(reason) => {
                if self.sent.is_some() {
                    self.serial_port_failed();
                    self.open_error = Some(reason);
                    // The port did not open, so nothing of a run is current:
                    // not a reading that landed late from the previous port.
                    self.run = None;
                    self.control_lines = None;
                }
            }
            LinemonUpdate::ControlLines(lines) => {
                if let Some(open @ InterfaceConfig::Serial(_)) = &self.sent {
                    // The first reading after an open is the open's own
                    // levels, which the open's marker covers.
                    if self
                        .control_lines
                        .is_some_and(|before| outputs_changed(before, lines))
                    {
                        let text = marker_text_with(open, Some(lines));
                        self.stream.mark_change(&text);
                    }
                    self.control_lines = Some(lines);
                }
            }
        }
    }

    /// Take the hidden Channel's latest run facts. The port list is refreshed
    /// when the port stops, so a removed adapter shows as not found, and when
    /// it opens while unlisted, since a port that opens is there.
    fn take_run(&mut self, run: RunFacts) {
        let was = self.run.as_ref().map(|r| r.state);
        if run.state == ChannelState::Faulted && was != Some(ChannelState::Faulted) {
            self.serial_port_failed();
        }
        // Once per open: a port that opens but is never listed is not looked
        // for again at every snapshot.
        if run.state == ChannelState::Running
            && was != Some(ChannelState::Running)
            && self.port_unlisted()
        {
            self.ports_stale = true;
        }
        self.run = Some(run);
    }

    /// The serial port in use failed to open or stopped: refresh the port
    /// list at the next frame, so the status can say whether it has gone.
    fn serial_port_failed(&mut self) {
        if matches!(self.sent, Some(InterfaceConfig::Serial(_))) {
            self.ports_stale = true;
        }
    }

    /// Wake the window when a new port list arrives.
    pub fn set_port_list_notify(&self, notify: std::sync::Arc<dyn Fn() + Send + Sync>) {
        self.port_list.set_notify(notify);
    }

    /// Whether the chosen serial port is missing from the port list. Never
    /// before the first list arrives.
    fn port_unlisted(&self) -> bool {
        match &self.settings.interface() {
            InterfaceConfig::Serial(serial) => {
                !serial.port.is_empty()
                    && self
                        .serial_ports
                        .as_ref()
                        .is_some_and(|ports| !ports.contains(&serial.port))
            }
            InterfaceConfig::Udp(_) => false,
        }
    }

    /// Ask the runtime for what the window wants now. Called every frame.
    ///
    /// Started with a port chosen: open it, and open it again whenever the
    /// interface differs from the last one asked for.
    /// Anything else: close what is open. An open marks the display, and
    /// empties it first when the port changed and Clear display on port change is
    /// ticked. A change that leaves no port chosen while started, such as
    /// switching to Serial before choosing a serial port, marks the display
    /// too, so the switch shows.
    fn sync(&mut self, out: &mut Vec<UiCommand>) {
        let interface = self.settings.interface();
        let key = port_key(&interface).filter(|_| self.started);
        let Some(key) = key else {
            if self.sent.take().is_some() {
                out.push(UiCommand::LinemonClose);
                self.control_lines = None;
                if self.started {
                    if self.settings.clear_on_port_change && self.last_port.is_some() {
                        self.stream.clear();
                    }
                    self.stream.mark_change(&no_port_text(&interface));
                    self.last_port = None;
                    self.marked_no_port = true;
                }
            }
            return;
        };
        if self.sent.as_ref() == Some(&interface) {
            return;
        }
        // A value still being typed is not complete until the field is left.
        // Hold the change back, so that a half-typed multicast group does not
        // reopen the port, with a marker line, at every keystroke. Enter, Tab or
        // a click elsewhere leaves the field and lets the change through.
        if self.editing_text {
            return;
        }
        self.open_error = None;
        self.run = None;
        self.control_lines = None;
        let changed = std::mem::take(&mut self.marked_no_port)
            || self
                .last_port
                .as_ref()
                .is_some_and(|last| is_port_change(last, &key));
        if changed && self.settings.clear_on_port_change {
            self.stream.clear();
        }
        self.stream.mark_change(&marker_text(&interface));
        self.last_port = Some(key);
        out.push(UiCommand::LinemonOpen(Box::new(build_config(
            &interface,
            &self.prefs,
        ))));
        self.sent = Some(interface);
    }

    /// Draw Linemon into `ui`, the program's whole window, and queue the
    /// commands it wants sent. `local_addresses` feeds the UDP binding choice.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        local_addresses: &LocalAddresses,
        out: &mut Vec<UiCommand>,
    ) {
        if let Some((seen, ports)) = self.port_list.newer_than(self.port_list_seen) {
            self.port_list_seen = seen;
            self.serial_ports = Some(ports);
        }
        // No line under the panel: the status box ends it, and the display
        // starts right below.
        egui::Panel::top("linemon_bar")
            .resizable(false)
            .show_separator_line(false)
            .show_inside(ui, |ui| {
                self.show_bar(ui, local_addresses, out);
            });
        self.editing_text = ui.ctx().text_edit_focused();
        self.sync(out);
        self.stream.set_cap(self.prefs.scroll_buffer_bytes);
        egui::CentralPanel::default().show_inside(ui, |ui| {
            let jump = std::mem::take(&mut self.jump_to_bottom);
            self.stream.show(ui, &self.prefs, &mut self.cache, jump);
            if std::mem::take(&mut self.copy_all) {
                let text = self.stream.copy_text(&self.cache);
                ui.ctx().copy_text(text);
            }
        });
    }

    fn show_bar(
        &mut self,
        ui: &mut egui::Ui,
        local_addresses: &LocalAddresses,
        out: &mut Vec<UiCommand>,
    ) {
        let mut refresh = None;
        // The same quarter of a row's height above the top line as below it.
        let row_gap = ui.spacing().interact_size.y * 0.25;
        ui.add_space(row_gap);
        ui.horizontal(|ui| {
            ui.label("Source");
            ui.radio_value(&mut self.settings.kind, SourceKind::Serial, "Serial");
            ui.radio_value(&mut self.settings.kind, SourceKind::Udp, "UDP");
            ui.separator();
            self.options(ui);
        });
        // A quarter of a row's height more between the source line and the
        // port settings, so the two read as separate lines.
        ui.add_space(row_gap);
        // Both sources' settings take the same height, so the buttons below
        // stay where they are when the source changes. The other source's
        // lines are laid out unseen, out of reach of the pointer, and the
        // shorter set is padded to the height of the taller.
        let kind = self.settings.kind;
        let other = match kind {
            SourceKind::Serial => SourceKind::Udp,
            SourceKind::Udp => SourceKind::Serial,
        };
        // The new source's lines fade in, so changing source is not an abrupt
        // swap of every control at once.
        let now = ui.input(|i| i.time);
        if kind != self.drawn_kind {
            self.drawn_kind = kind;
            self.kind_changed_at = now;
        }
        let fade = (((now - self.kind_changed_at) / SOURCE_FADE_SECONDS) as f32).clamp(0.0, 1.0);
        if fade < 1.0 {
            ui.ctx().request_repaint();
        }
        let top = ui.cursor().min;
        let shown = ui
            .vertical(|ui| {
                ui.multiply_opacity(fade);
                self.settings_lines(ui, kind, local_addresses, &mut refresh, out, row_gap)
            })
            .response
            .rect
            .height();
        let mut unseen = ui.new_child(egui::UiBuilder::new().invisible().max_rect(
            egui::Rect::from_min_size(top, egui::vec2(ui.available_width(), f32::INFINITY)),
        ));
        unseen.set_clip_rect(egui::Rect::NOTHING);
        self.settings_lines(
            &mut unseen,
            other,
            local_addresses,
            &mut None,
            &mut Vec::new(),
            row_gap,
        );
        ui.add_space((unseen.min_rect().height() - shown).max(0.0));
        if std::mem::take(&mut self.ports_stale) {
            self.port_list.refresh();
        }
        match refresh {
            Some(Refresh::SerialPorts) => self.port_list.refresh(),
            Some(Refresh::LocalAddresses) => out.push(UiCommand::ListLocalAddresses),
            None => {}
        }

        // The buttons, then the status, sit near the display, where the
        // eye is. The buttons are one fixed row, not one that wraps, so each
        // stays where the hand expects it whatever the window's width; the row
        // fits at MIN_SIZE.
        ui.add_space(4.0);
        ui.horizontal(|ui| self.buttons(ui));
        ui.add_space(4.0);
        self.show_status(ui);
        egui::CollapsingHeader::new("Configure display")
            .default_open(false)
            .show(ui, |ui| {
                super::detail::stream_view::show_view_controls(
                    ui,
                    &mut self.prefs,
                    Some(self.stream.len()),
                );
            });
    }

    /// The serial port's first line: the port with its refresh button, then the
    /// baud rate and the framing. The shared widget (listener ADR-074) draws it
    /// into the caller's row, and keeps what is being typed in the port and baud
    /// boxes itself.
    fn serial_controls(&mut self, ui: &mut egui::Ui, refresh: &mut Option<Refresh>) {
        let mut settings = serial_settings(&self.settings.serial);
        let response = wiredata_ui::serial::settings_line(
            ui,
            egui::Id::new("linemon_serial"),
            &mut settings,
            self.serial_ports.as_deref(),
        );
        if response.changed {
            apply_serial_settings(&mut self.settings.serial, &settings);
        }
        if response.refresh_ports {
            *refresh = Some(Refresh::SerialPorts);
        }
    }

    /// The serial port's second line: flow control, then the control lines
    /// (§161). RTS and DTR are outputs this computer sets: a click sets the
    /// line the other way. An output cannot be read back, so what is shown is
    /// what was set: low at open (ADR-073), then what a click set. Under RTS/CTS
    /// flow control RTS is the flow control's, so it has no button. CTS, DSR,
    /// DCD and RI are inputs read from the port, filled when high. The lines
    /// are live only while the port is running; otherwise they are greyed out
    /// and a sentence says why.
    fn serial_lines(&mut self, ui: &mut egui::Ui, out: &mut Vec<UiCommand>) {
        let mut settings = serial_settings(&self.settings.serial);
        let response = wiredata_ui::serial::flow_and_lines(
            ui,
            egui::Id::new("linemon_serial"),
            &mut settings.flow,
            self.live_control_lines().map(control_lines),
            rts_by_flow(&self.settings.interface()),
        );
        if response.changed {
            apply_serial_settings(&mut self.settings.serial, &settings);
            // What stands in for RTS follows the flow control just chosen, so
            // draw the line again with it.
            ui.ctx().request_repaint();
        }
        match response.click {
            Some(LineClick::Rts(on)) => out.push(UiCommand::LinemonSetRts(on)),
            Some(LineClick::Dtr(on)) => out.push(UiCommand::LinemonSetDtr(on)),
            None => {}
        }
    }

    fn udp_controls(
        &mut self,
        ui: &mut egui::Ui,
        local_addresses: &LocalAddresses,
        refresh: &mut Option<Refresh>,
    ) {
        ui.label("Port").on_hover(&UDP_PORT_HOVER);
        // − and + beside the number, as in the channel editor's port field. A
        // typed number is applied when it is valid and the field is left or
        // Enter is pressed. Otherwise the field shows the port in use.
        let port = self.settings.udp.port;
        if ui
            .small_button("\u{2212}")
            .on_hover(&UDP_PREVIOUS_HOVER)
            .clicked()
        {
            self.settings.udp.port = step_udp_port(port, -1);
        }
        let field = ui.add(
            egui::TextEdit::singleline(&mut self.udp_port_text)
                .id_salt("linemon_udp_port")
                .desired_width(56.0),
        );
        if field.lost_focus() {
            if let Ok(port) = self.udp_port_text.trim().parse::<u16>() {
                if port != 0 {
                    self.settings.udp.port = port;
                }
            }
        }
        if ui.small_button("+").on_hover(&UDP_NEXT_HOVER).clicked() {
            self.settings.udp.port = step_udp_port(port, 1);
        }
        if !field.has_focus() {
            self.udp_port_text = match self.settings.udp.port {
                0 => String::new(),
                port => port.to_string(),
            };
        }
        udp_bind_choice(ui, &mut self.settings.udp, local_addresses, refresh);
    }

    /// Whether Start would do anything now: a port is chosen, and the port is
    /// not already open or being opened.
    fn can_start(&self) -> bool {
        if port_key(&self.settings.interface()).is_none() {
            return false;
        }
        let faulted = self
            .run
            .as_ref()
            .is_some_and(|r| r.state == ChannelState::Faulted);
        !(self.started && self.sent.is_some() && self.open_error.is_none() && !faulted)
    }

    /// The control lines to show as live: the last reading, while the hidden
    /// Channel is known to be running. A faulted port's last reading stays
    /// behind, but the reader that kept it current has ended.
    fn live_control_lines(&self) -> Option<SerialControlLines> {
        let running = self
            .run
            .as_ref()
            .is_some_and(|r| r.state == ChannelState::Running);
        self.control_lines.filter(|_| running)
    }

    /// Start: open the port in the bar.
    fn press_start(&mut self) {
        self.started = true;
        self.open_error = None;
        self.run = None;
        self.control_lines = None;
        // Ask again even when the interface is the one last opened.
        self.sent = None;
    }

    fn buttons(&mut self, ui: &mut egui::Ui) {
        let button = |text: &str| egui::Button::new(text).min_size(BUTTON_SIZE);
        if ui
            .add_enabled(self.can_start(), button("Start"))
            .on_hover(&START_HOVER)
            .clicked()
        {
            self.press_start();
        }
        if ui
            .add_enabled(self.started, button("Stop"))
            .on_hover(&STOP_HOVER)
            .clicked()
        {
            self.started = false;
            self.open_error = None;
            self.run = None;
            self.control_lines = None;
        }
        let pause_label = if self.paused { "Resume" } else { "Pause" };
        if ui.add(button(pause_label)).on_hover(&PAUSE_HOVER).clicked() {
            self.paused = !self.paused;
            if !self.paused {
                self.jump_to_bottom = true;
            }
        }
        if ui.add(button("Clear")).on_hover(&CLEAR_HOVER).clicked() {
            self.stream.clear();
        }
        if ui
            .add(button("Copy scroll buffer"))
            .on_hover(&COPY_ALL_HOVER)
            .clicked()
        {
            self.copy_all = true;
        }
    }

    /// The two lines of `kind`'s port settings. Each line never wraps, and its
    /// drawn width sets the window's minimum width, measured rather than
    /// guessed, so it fits whatever the font size and screen scaling.
    fn settings_lines(
        &mut self,
        ui: &mut egui::Ui,
        kind: SourceKind,
        local_addresses: &LocalAddresses,
        refresh: &mut Option<Refresh>,
        out: &mut Vec<UiCommand>,
        row_gap: f32,
    ) {
        match kind {
            SourceKind::Serial => {
                // Two lines, as UDP has: the port and its line settings, then
                // flow control and the control lines.
                let line = ui
                    .horizontal(|ui| self.serial_controls(ui, refresh))
                    .response
                    .rect;
                self.fit_min_width(ui, line);
                ui.add_space(row_gap);
                let line = ui.horizontal(|ui| self.serial_lines(ui, out)).response.rect;
                self.fit_min_width(ui, line);
            }
            SourceKind::Udp => {
                // The same two lines as the channel editor in Listener.
                let line = ui
                    .horizontal(|ui| self.udp_controls(ui, local_addresses, refresh))
                    .response
                    .rect;
                self.fit_min_width(ui, line);
                ui.add_space(row_gap);
                let line = ui
                    .horizontal(|ui| {
                        ui.label("Mode").on_hover(&UDP_MODE_HOVER);
                        udp_mode_and_sharing(ui, &mut self.settings.udp);
                    })
                    .response
                    .rect;
                self.fit_min_width(ui, line);
            }
        }
    }

    /// Raise the window's minimum width to fit `line`, the port settings line
    /// as drawn, with the panel's margins either side. Only ever raised, so
    /// switching between Serial and UDP leaves room for the wider of the two.
    fn fit_min_width(&mut self, ui: &egui::Ui, line: egui::Rect) {
        let window = ui.ctx().content_rect();
        let margins = (line.left() - window.left()) * 2.0;
        let needed = (line.width() + margins).ceil();
        if needed > self.min_width {
            self.min_width = needed;
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(
                    needed,
                    MIN_SIZE[1],
                )));
        }
    }

    /// The two checkboxes, on the top row after the source choice.
    fn options(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(
            &mut self.settings.clear_on_port_change,
            "Clear display on port change",
        )
        .on_hover(&CLEAR_ON_CHANGE_HOVER);
        ui.checkbox(&mut self.settings.reopen_on_start, "Reopen port on start")
            .on_hover(&REOPEN_HOVER);
    }

    fn show_status(&self, ui: &mut egui::Ui) {
        let interface = self.settings.interface();
        let line = status_line(&StatusFacts {
            interface: &interface,
            started: self.started,
            paused: self.paused,
            open_error: self.open_error.as_deref(),
            run: self.run.as_ref(),
            // A list being read again may be the one about to say it is back.
            port_unlisted: self.port_unlisted() && !self.port_list.is_reading(),
        });
        let mut text = egui::RichText::new(line.text);
        if line.problem {
            // The colour Listener draws a channel's error in, so the same kind
            // of message looks the same in both programs. The ⚠ it leads with
            // says so without the colour.
            text = text.color(wiredata_ui::palette::active(ui).fault);
        }
        // A framed box the full width of the window, so the line that says what
        // Linemon is doing is easy to find.
        let visuals = ui.visuals();
        egui::Frame::new()
            .fill(visuals.faint_bg_color)
            .stroke(visuals.widgets.noninteractive.bg_stroke)
            .corner_radius(4.0)
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(text);
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::StreamDelta;

    fn serial(port: &str) -> SerialConfig {
        let mut s = LinemonSettings::default().serial;
        s.port = port.to_owned();
        s
    }

    fn udp(port: u16) -> UdpConfig {
        let mut u = LinemonSettings::default().udp;
        u.port = port;
        u.mode = UdpMode::Unicast;
        u
    }

    fn ser(port: &str) -> InterfaceConfig {
        InterfaceConfig::Serial(serial(port))
    }

    fn net(port: u16) -> InterfaceConfig {
        InterfaceConfig::Udp(udp(port))
    }

    // ---- settings ----

    #[test]
    fn defaults_are_9600_8n1_serial_with_reopen_on_and_clear_off() {
        let s = LinemonSettings::default();
        assert_eq!(s.kind, SourceKind::Serial);
        assert_eq!(serial_line_text(&s.serial), "9600 8N1");
        assert!(s.serial.port.is_empty());
        assert_eq!(s.udp.port, 0);
        assert!(s.reopen_on_start);
        assert!(!s.clear_on_port_change);
        assert!(s.prefs().eq_settings(&ViewPrefs::default()));
    }

    #[test]
    fn stored_settings_round_trip_every_remembered_field() {
        let mut s = LinemonSettings {
            kind: SourceKind::Udp,
            ..Default::default()
        };
        s.serial.port = "COM7".into();
        s.serial.baud_rate = 115_200;
        s.serial.parity = Parity::Even;
        s.udp.port = 5005;
        s.udp.mode = UdpMode::Multicast;
        s.udp.multicast_group = Some("239.0.0.1".into());
        s.clear_on_port_change = true;
        s.reopen_on_start = false;
        let mut prefs = s.prefs();
        prefs.font_size = 18.0;
        prefs.scroll_buffer_bytes = 128 * 1024;
        s.take_prefs(&prefs);
        let back = LinemonSettings::from_stored(Some(&s.to_stored()));
        assert_eq!(back, s);
        let prefs_back = back.prefs();
        assert_eq!(prefs_back.font_size, 18.0);
        assert_eq!(prefs_back.scroll_buffer_bytes, 128 * 1024);
    }

    #[test]
    fn missing_or_unreadable_stored_settings_give_the_defaults() {
        let defaults = LinemonSettings::default();
        assert_eq!(LinemonSettings::from_stored(None), defaults);
        assert_eq!(LinemonSettings::from_stored(Some("")), defaults);
        assert_eq!(LinemonSettings::from_stored(Some("not = [valid")), defaults);
        assert_eq!(
            LinemonSettings::from_stored(Some("kind = 7\n")),
            defaults,
            "a value of the wrong type is not trusted"
        );
    }

    #[test]
    fn stored_settings_missing_a_field_keep_the_default_for_it() {
        let s = LinemonSettings::from_stored(Some("reopen_on_start = false\n"));
        assert!(!s.reopen_on_start);
        assert_eq!(s.serial.baud_rate, 9600);
    }

    #[test]
    fn the_hidden_channel_config_follows_the_bar_and_the_prefs() {
        let prefs = ViewPrefs {
            scroll_buffer_bytes: 32 * 1024,
            ..Default::default()
        };
        let mut interface = serial("COM4");
        interface.baud_rate = 4800;
        let config = build_config(&InterfaceConfig::Serial(interface.clone()), &prefs);
        assert_eq!(config.interface, InterfaceConfig::Serial(interface));
        assert!(!config.reconnect.enabled);
        assert!(config.match_rules.is_empty());
        assert_eq!(config.retention.byte_limit, Some(32 * 1024));
        assert_eq!(config.display.view.mode, prefs.mode);
        let udp_config = build_config(&net(5005), &prefs);
        assert_eq!(udp_config.interface, net(5005));
        assert_eq!(udp_config.kind, crate::core::ChannelKind::Udp);
    }

    // ---- marker and description ----

    #[test]
    fn the_serial_marker_gives_port_baud_and_framing() {
        assert_eq!(
            marker_text(&ser("COM4")),
            "\u{2014} COM4, 9600 8N1, flow control: none, RTS low, DTR low \u{2014}"
        );
        let mut s = serial("COM4");
        s.baud_rate = 115_200;
        s.data_bits = DataBits::Seven;
        s.parity = Parity::Even;
        s.stop_bits = StopBits::Two;
        assert_eq!(
            marker_text(&InterfaceConfig::Serial(s)),
            "\u{2014} COM4, 115200 7E2, flow control: none, RTS low, DTR low \u{2014}"
        );
    }

    #[test]
    fn every_parity_data_bit_and_stop_bit_has_its_letter() {
        let line = |d, p, s| {
            let mut c = serial("COM1");
            c.data_bits = d;
            c.parity = p;
            c.stop_bits = s;
            serial_line_text(&c)
        };
        assert_eq!(
            line(DataBits::Five, Parity::None, StopBits::One),
            "9600 5N1"
        );
        assert_eq!(
            line(DataBits::Six, Parity::Odd, StopBits::OnePointFive),
            "9600 6O1.5"
        );
        assert_eq!(
            line(DataBits::Eight, Parity::Mark, StopBits::One),
            "9600 8M1"
        );
        assert_eq!(
            line(DataBits::Eight, Parity::Space, StopBits::Two),
            "9600 8S2"
        );
    }

    #[test]
    fn the_serial_marker_always_names_flow_control() {
        let with = |flow| {
            let mut c = serial("COM4");
            c.flow_control = flow;
            marker_text(&InterfaceConfig::Serial(c))
        };
        assert_eq!(
            with(FlowControl::None),
            "\u{2014} COM4, 9600 8N1, flow control: none, RTS low, DTR low \u{2014}"
        );
        // RTS under RTS/CTS is given only where its level is known.
        let rts_cts = if RTS_HIGH_UNDER_RTS_CTS {
            "\u{2014} COM4, 9600 8N1, flow control: RTS/CTS, RTS high, DTR low \u{2014}"
        } else {
            "\u{2014} COM4, 9600 8N1, flow control: RTS/CTS, DTR low \u{2014}"
        };
        assert_eq!(with(FlowControl::RtsCts), rts_cts);
        assert_eq!(
            with(FlowControl::XonXoff),
            "\u{2014} COM4, 9600 8N1, flow control: XON/XOFF, RTS low, DTR low \u{2014}"
        );
    }

    #[test]
    fn the_udp_marker_names_the_mode_when_it_is_not_unicast() {
        assert_eq!(marker_text(&net(5005)), "\u{2014} UDP port 5005 \u{2014}");
        let with = |mode, group: Option<&str>| {
            let mut c = udp(5005);
            c.mode = mode;
            c.multicast_group = group.map(str::to_owned);
            marker_text(&InterfaceConfig::Udp(c))
        };
        assert_eq!(
            with(UdpMode::Broadcast, None),
            "\u{2014} UDP port 5005, broadcast \u{2014}"
        );
        assert_eq!(
            with(UdpMode::Multicast, Some("239.0.0.1")),
            "\u{2014} UDP port 5005, multicast group 239.0.0.1 \u{2014}"
        );
        assert_eq!(
            with(UdpMode::Multicast, None),
            "\u{2014} UDP port 5005, multicast \u{2014}"
        );
    }

    #[test]
    fn no_port_has_no_key_and_no_marker() {
        assert_eq!(port_key(&ser("")), None);
        assert_eq!(port_key(&ser("  ")), None);
        assert_eq!(port_key(&net(0)), None);
        assert_eq!(marker_text(&ser("")), "");
        assert_eq!(
            port_key(&ser(" COM4 ")),
            Some(PortKey::Serial("COM4".into()))
        );
        assert_eq!(describe_port(&PortKey::Udp(5005)), "UDP port 5005");
    }

    // ---- port change ----

    #[test]
    fn a_port_change_is_another_port_or_another_kind() {
        let com4 = PortKey::Serial("COM4".into());
        assert!(is_port_change(&com4, &PortKey::Serial("COM5".into())));
        assert!(!is_port_change(&com4, &PortKey::Serial("com4".into())));
        assert!(is_port_change(&PortKey::Udp(5005), &PortKey::Udp(5006)));
        assert!(!is_port_change(&PortKey::Udp(5005), &PortKey::Udp(5005)));
        assert!(is_port_change(&com4, &PortKey::Udp(5005)));
        assert!(is_port_change(&PortKey::Udp(5005), &com4));
    }

    // ---- stepping ----

    #[test]
    fn stepping_a_udp_port_stays_within_1_and_65535() {
        assert_eq!(step_udp_port(5005, 1), 5006);
        assert_eq!(step_udp_port(5005, -1), 5004);
        assert_eq!(step_udp_port(1, -1), 1);
        assert_eq!(step_udp_port(65535, 1), 65535);
        assert_eq!(step_udp_port(0, 1), 1);
        assert_eq!(step_udp_port(0, -1), 1);
    }

    // ---- status ----

    fn run(state: ChannelState, total: u64, rate: f64) -> RunFacts {
        RunFacts {
            state,
            bytes_total: total,
            bytes_per_sec: rate,
            fault: None,
        }
    }

    fn facts<'a>(interface: &'a InterfaceConfig) -> StatusFacts<'a> {
        StatusFacts {
            interface,
            started: true,
            paused: false,
            open_error: None,
            run: None,
            port_unlisted: false,
        }
    }

    fn text(f: &StatusFacts) -> String {
        status_line(f).text
    }

    #[test]
    fn the_status_says_what_to_do_when_nothing_is_chosen_or_started() {
        let none = ser("");
        let mut f = facts(&none);
        // Started with no port: Start is disabled and choosing a port opens it,
        // so the line does not tell the user to press Start.
        assert_eq!(
            text(&f),
            "Choose a port. Linemon opens it as soon as you choose one."
        );
        f.started = false;
        assert_eq!(text(&f), "Choose a port, then press Start.");
        let none_udp = net(0);
        let mut f = facts(&none_udp);
        assert_eq!(
            text(&f),
            "Choose a port. Linemon opens it as soon as you choose one."
        );
        f.started = false;
        assert_eq!(text(&f), "Choose a port, then press Start.");
        let com4 = ser("COM4");
        let mut f = facts(&com4);
        f.started = false;
        assert_eq!(text(&f), "Stopped. Press Start to open COM4.");
        let u = net(5005);
        let mut f = facts(&u);
        f.started = false;
        assert_eq!(text(&f), "Stopped. Press Start to open UDP port 5005.");
    }

    #[test]
    fn the_status_reports_opening_and_running() {
        let com4 = ser("COM4");
        let mut f = facts(&com4);
        assert_eq!(text(&f), "Opening COM4\u{2026}");
        let starting = run(ChannelState::Starting, 0, 0.0);
        f.run = Some(&starting);
        assert_eq!(text(&f), "Opening COM4\u{2026}");
        let running = run(ChannelState::Running, 12_345, 480.2);
        f.run = Some(&running);
        assert_eq!(
            text(&f),
            "COM4 is open at 9600 8N1. 12,345 bytes received, 480 bytes per second."
        );
        let u = net(5005);
        let mut f = facts(&u);
        f.run = Some(&running);
        assert_eq!(
            text(&f),
            "UDP port 5005 is open. 12,345 bytes received, 480 bytes per second."
        );
        assert!(!status_line(&f).problem);
    }

    #[test]
    fn a_paused_display_is_said_after_the_running_sentence() {
        let com4 = ser("COM4");
        let running = run(ChannelState::Running, 1, 0.0);
        let mut f = facts(&com4);
        f.run = Some(&running);
        f.paused = true;
        assert_eq!(
            text(&f),
            "COM4 is open at 9600 8N1. 1 byte received, 0 bytes per second. \
             The display is paused, so arriving bytes are not shown."
        );
    }

    #[test]
    fn problems_lead_with_the_warning_glyph_and_say_why() {
        let com4 = ser("COM4");
        let glyph = tone_marker(SignalTone::Warning);
        assert!(!glyph.is_empty());

        let mut f = facts(&com4);
        f.open_error = Some("Access is denied.");
        let line = status_line(&f);
        assert!(line.problem);
        assert_eq!(
            line.text,
            format!("{glyph}COM4 could not be opened: Access is denied.")
        );

        let mut faulted = run(ChannelState::Faulted, 10, 0.0);
        faulted.fault = Some("The device was unplugged".into());
        let mut f = facts(&com4);
        f.run = Some(&faulted);
        let line = status_line(&f);
        assert!(line.problem);
        assert_eq!(
            line.text,
            format!("{glyph}COM4 stopped: The device was unplugged. Press Start to try again.")
        );
    }

    /// A port that stops gives its cause without the runtime's "transport
    /// fault on channel Linemon:" in front: Linemon has no channels to name.
    #[test]
    fn a_stopped_port_gives_its_cause_without_the_channel_wording() {
        let com3 = ser("COM3");
        let glyph = tone_marker(SignalTone::Warning);
        let mut faulted = run(ChannelState::Faulted, 10, 0.0);
        faulted.fault = Some(format!(
            "{}serial read failed: The device does not recognize the command.",
            transport_fault_prefix(super::super::bridge::LINEMON_CHANNEL_NAME)
        ));
        let mut f = facts(&com3);
        f.run = Some(&faulted);
        assert_eq!(
            text(&f),
            format!(
                "{glyph}COM3 stopped: serial read failed: The device does not recognize the \
                 command. Press Start to try again."
            )
        );
    }

    /// "The parameter is incorrect" is what an adapter that takes only 7 or
    /// 8 data bits says to 6; the line says which parameter it probably is.
    #[test]
    fn an_open_with_6_data_bits_says_many_adapters_take_only_7_or_8() {
        let mut com3 = ser("COM3");
        if let InterfaceConfig::Serial(serial) = &mut com3 {
            serial.data_bits = crate::config::DataBits::Six;
        }
        let glyph = tone_marker(SignalTone::Warning);
        let mut f = facts(&com3);
        f.open_error = Some("The parameter is incorrect.");
        assert_eq!(
            text(&f),
            format!(
                "{glyph}COM3 could not be opened: The parameter is incorrect. Many USB serial \
                 adapters take only 7 or 8 data bits."
            )
        );
        // 8 data bits, the usual, get no hint.
        let com4 = ser("COM4");
        let mut f = facts(&com4);
        f.open_error = Some("The parameter is incorrect.");
        assert_eq!(
            text(&f),
            format!("{glyph}COM4 could not be opened: The parameter is incorrect.")
        );
    }

    /// A refused bind is said once, as the port that could not be opened,
    /// and since the system's reason seldom says what holds the port, the
    /// line names the usual causes; any other open error of a UDP port gets
    /// no hint.
    #[test]
    fn a_refused_bind_says_what_may_hold_the_port() {
        let u = net(5005);
        let glyph = tone_marker(SignalTone::Warning);
        let refused = crate::runtime::OrchestratorError::Bind(std::io::Error::other(
            "Only one usage of each socket address is permitted.",
        ))
        .to_string();
        let mut f = facts(&u);
        f.open_error = Some(&refused);
        assert_eq!(
            text(&f),
            format!(
                "{glyph}UDP port 5005 could not be opened: Only one usage of each socket \
                 address is permitted. {}",
                wiredata_ui::format::UDP_BIND_HINT
            )
        );
        f.open_error = Some("the request did not reach Linemon's runtime.");
        assert_eq!(
            text(&f),
            format!(
                "{glyph}UDP port 5005 could not be opened: the request did not reach Linemon's \
                 runtime."
            )
        );
    }

    #[test]
    fn an_open_error_names_the_port_once_and_says_when_it_has_gone() {
        let com3 = ser("COM3");
        let glyph = tone_marker(SignalTone::Warning);
        let reason = format!(
            "{}The system cannot find the file specified.",
            open_error_prefix("COM3")
        );
        let mut f = facts(&com3);
        f.open_error = Some(&reason);
        assert_eq!(
            text(&f),
            format!("{glyph}COM3 could not be opened: The system cannot find the file specified.")
        );
        f.port_unlisted = true;
        assert_eq!(
            text(&f),
            format!(
                "{glyph}COM3 could not be opened: The system cannot find the file specified. \
                 It is no longer in the port list, so the device may have been removed."
            )
        );

        let mut faulted = run(ChannelState::Faulted, 10, 0.0);
        faulted.fault = Some("The device was unplugged".into());
        let mut f = facts(&com3);
        f.run = Some(&faulted);
        f.port_unlisted = true;
        assert_eq!(
            text(&f),
            format!(
                "{glyph}COM3 stopped: The device was unplugged. It is no longer in the port \
                 list, so the device may have been removed. Press Start to try again."
            )
        );
    }

    #[test]
    fn a_failed_serial_port_refreshes_the_port_list() {
        let mut l = started_on("COM3");
        let mut out = Vec::new();
        l.sync(&mut out);
        l.handle(LinemonUpdate::Error("gone".into()));
        assert!(l.ports_stale, "an open that failed");

        let mut l = started_on("COM3");
        l.sync(&mut out);
        let run = |state| RunFacts {
            state,
            bytes_total: 0,
            bytes_per_sec: 0.0,
            fault: None,
        };
        l.serial_ports = Some(vec!["COM3".into()]);
        l.take_run(run(ChannelState::Running));
        assert!(!l.ports_stale, "open and listed");
        l.take_run(run(ChannelState::Faulted));
        assert!(l.ports_stale, "the port stopped");

        // Plugged back in and opened before the list was refreshed: the list
        // still lacks it, so it is refreshed once, at the open.
        l.ports_stale = false;
        l.serial_ports = Some(Vec::new());
        assert!(l.port_unlisted());
        l.take_run(run(ChannelState::Running));
        assert!(l.ports_stale, "it opened, so it is there");
        l.ports_stale = false;
        l.take_run(run(ChannelState::Running));
        assert!(!l.ports_stale, "not again at every snapshot");
    }

    #[test]
    fn the_serial_marker_gives_the_levels_the_open_sets() {
        let mut c = serial("COM4");
        c.rts = Some(true);
        assert_eq!(
            marker_text(&InterfaceConfig::Serial(c)),
            "\u{2014} COM4, 9600 8N1, flow control: none, RTS high, DTR low \u{2014}"
        );
    }

    #[test]
    fn a_change_of_rts_or_dtr_is_marked_when_the_port_reports_it() {
        let mut l = started_on("COM4");
        let mut out = Vec::new();
        l.sync(&mut out);
        let opened = l.stream.marks().len();
        // The first reading is the open's levels: the open's marker covers it.
        l.handle(LinemonUpdate::ControlLines(SerialControlLines::default()));
        assert_eq!(l.stream.marks().len(), opened);
        // An input changing is not marked.
        let cts = SerialControlLines {
            cts: true,
            ..SerialControlLines::default()
        };
        l.handle(LinemonUpdate::ControlLines(cts));
        assert_eq!(l.stream.marks().len(), opened);
        let rts = SerialControlLines { rts: true, ..cts };
        let both = SerialControlLines { dtr: true, ..rts };
        l.handle(LinemonUpdate::ControlLines(rts));
        l.handle(LinemonUpdate::ControlLines(both));
        l.handle(LinemonUpdate::ControlLines(both));
        l.handle(LinemonUpdate::ControlLines(cts));
        let texts: Vec<&str> = l.stream.marks()[opened..]
            .iter()
            .map(|m| m.text.as_str())
            .collect();
        // Each marker is the whole settings line, one per reading that
        // changed an output, however many outputs it changed.
        assert_eq!(
            texts,
            [
                "\u{2014} COM4, 9600 8N1, flow control: none, RTS high, DTR low \u{2014}",
                "\u{2014} COM4, 9600 8N1, flow control: none, RTS high, DTR high \u{2014}",
                "\u{2014} COM4, 9600 8N1, flow control: none, RTS low, DTR low \u{2014}",
            ]
        );
    }

    #[test]
    fn a_stopped_linemon_reports_no_problem() {
        let com4 = ser("COM4");
        let mut f = facts(&com4);
        f.started = false;
        f.open_error = Some("x");
        assert_eq!(text(&f), "Stopped. Press Start to open COM4.");
    }

    // ---- the window's requests ----

    fn started_on(interface_port: &str) -> Linemon {
        let mut l = Linemon::new(None);
        l.settings.serial.port = interface_port.into();
        l.started = true;
        l
    }

    fn opened(out: &[UiCommand]) -> Vec<&ChannelConfig> {
        out.iter()
            .filter_map(|c| match c {
                UiCommand::LinemonOpen(config) => Some(&**config),
                _ => None,
            })
            .collect()
    }

    fn closes(out: &[UiCommand]) -> usize {
        out.iter()
            .filter(|c| matches!(c, UiCommand::LinemonClose))
            .count()
    }

    #[test]
    fn started_with_a_port_opens_it_once_and_marks_the_display() {
        let mut l = started_on("COM4");
        let mut out = Vec::new();
        l.sync(&mut out);
        assert_eq!(opened(&out).len(), 1);
        assert_eq!(l.stream.marks().len(), 1);
        assert_eq!(
            l.stream.marks()[0].text,
            "\u{2014} COM4, 9600 8N1, flow control: none, RTS low, DTR low \u{2014}"
        );
        // Nothing changed, so nothing more is sent.
        l.sync(&mut out);
        assert_eq!(opened(&out).len(), 1);
        assert_eq!(l.stream.marks().len(), 1);
    }

    #[test]
    fn a_changed_setting_reopens_at_once_and_does_not_clear() {
        let mut l = started_on("COM4");
        l.settings.clear_on_port_change = true;
        let mut out = Vec::new();
        l.sync(&mut out);
        l.stream.append(&StreamDelta {
            generation: 1,
            base_offset: 0,
            bytes: b"abc".to_vec().into(),
            end_offset: 3,
        });
        l.settings.serial.baud_rate = 115_200;
        l.sync(&mut out);
        assert_eq!(opened(&out).len(), 2);
        assert_eq!(l.stream.len(), 3, "a baud change never clears");
        assert_eq!(l.stream.marks().len(), 2);
    }

    #[test]
    fn a_value_still_being_typed_does_not_reopen_the_port() {
        let mut l = Linemon::new(None);
        l.settings.kind = SourceKind::Udp;
        l.settings.udp.port = 5005;
        l.settings.udp.mode = UdpMode::Multicast;
        l.started = true;
        let mut out = Vec::new();
        l.sync(&mut out);
        assert_eq!(opened(&out).len(), 1);

        // A group typed one keystroke at a time, with its field focused.
        l.editing_text = true;
        for typed in ["2", "23", "239", "239.", "239.0.0.1"] {
            l.settings.udp.multicast_group = Some(typed.into());
            l.sync(&mut out);
        }
        assert_eq!(opened(&out).len(), 1, "nothing opens while it is typed");
        assert_eq!(l.stream.marks().len(), 1, "and no marker line is added");
        assert_eq!(closes(&out), 0);

        // Leaving the field lets the finished value through, once.
        l.editing_text = false;
        l.sync(&mut out);
        assert_eq!(opened(&out).len(), 2);
        assert_eq!(l.stream.marks().len(), 2);
        assert_eq!(
            l.stream.marks()[1].text,
            "\u{2014} UDP port 5005, multicast group 239.0.0.1 \u{2014}"
        );
        l.sync(&mut out);
        assert_eq!(opened(&out).len(), 2, "nothing more is sent");
    }

    #[test]
    fn editing_a_field_does_not_hold_back_the_first_open() {
        // Start while a field has the keyboard: pressing Start moves focus away
        // in practice, but the first open is the same request as any other
        // change and waits only while the field is being edited.
        let mut l = started_on("COM4");
        l.editing_text = true;
        let mut out = Vec::new();
        l.sync(&mut out);
        assert!(opened(&out).is_empty());
        l.editing_text = false;
        l.sync(&mut out);
        assert_eq!(opened(&out).len(), 1);
    }

    #[test]
    fn a_changed_port_clears_only_when_the_checkbox_is_ticked() {
        for clear in [false, true] {
            let mut l = started_on("COM4");
            l.settings.clear_on_port_change = clear;
            let mut out = Vec::new();
            l.sync(&mut out);
            l.stream.append(&StreamDelta {
                generation: 1,
                base_offset: 0,
                bytes: b"abc".to_vec().into(),
                end_offset: 3,
            });
            l.settings.serial.port = "COM5".into();
            l.sync(&mut out);
            assert_eq!(l.stream.len(), if clear { 0 } else { 3 });
            assert_eq!(l.stream.marks().len(), if clear { 1 } else { 2 });
        }
    }

    #[test]
    fn the_first_open_never_clears() {
        let mut l = started_on("COM4");
        l.settings.clear_on_port_change = true;
        l.stream.append(&StreamDelta {
            generation: 1,
            base_offset: 0,
            bytes: b"old".to_vec().into(),
            end_offset: 3,
        });
        let mut out = Vec::new();
        l.sync(&mut out);
        assert_eq!(l.stream.len(), 3);
    }

    #[test]
    fn stopping_closes_and_pressing_start_again_reopens_the_same_port() {
        let mut l = started_on("COM4");
        let mut out = Vec::new();
        l.sync(&mut out);
        l.started = false;
        l.sync(&mut out);
        assert_eq!(closes(&out), 1);
        l.sync(&mut out);
        assert_eq!(closes(&out), 1, "closed once");
        // Changes while stopped send nothing.
        l.settings.serial.baud_rate = 4800;
        l.sync(&mut out);
        assert_eq!(opened(&out).len(), 1);
        // Start uses the settings as they are then.
        l.started = true;
        l.sync(&mut out);
        assert_eq!(opened(&out).len(), 2);
        assert_eq!(opened(&out)[1].interface, l.settings.interface());
    }

    #[test]
    fn no_port_closes_what_is_open_and_choosing_one_opens_it() {
        let mut l = started_on("COM4");
        let mut out = Vec::new();
        l.sync(&mut out);
        l.settings.serial.port.clear();
        l.sync(&mut out);
        assert_eq!(closes(&out), 1);
        l.settings.serial.port = "COM5".into();
        l.sync(&mut out);
        assert_eq!(opened(&out).len(), 2);
    }

    #[test]
    fn bytes_that_arrive_while_paused_are_not_shown() {
        let mut l = Linemon::new(None);
        let delta = |base: u64, bytes: &[u8]| {
            LinemonUpdate::Stream(Box::new(StreamDelta {
                generation: 1,
                base_offset: base,
                bytes: bytes.to_vec().into(),
                end_offset: base + bytes.len() as u64,
            }))
        };
        l.handle(delta(0, b"ab"));
        l.paused = true;
        l.handle(delta(2, b"cd"));
        assert_eq!(l.stream.len(), 2);
        l.paused = false;
        l.handle(delta(4, b"ef"));
        assert_eq!(l.stream.len(), 4);
    }

    #[test]
    fn starting_up_opens_only_when_the_checkbox_is_ticked_and_a_port_is_chosen() {
        for (reopen, port, expect) in [
            (true, "COM4", true),
            (false, "COM4", false),
            (true, "", false),
        ] {
            let mut l = Linemon::new(None);
            l.settings.reopen_on_start = reopen;
            l.settings.serial.port = port.into();
            l.start_up();
            assert_eq!(l.started, expect, "reopen {reopen}, port {port:?}");
        }
    }

    #[test]
    fn a_request_that_did_not_reach_the_runtime_is_said_and_start_is_offered() {
        let mut l = started_on("COM4");
        let mut out = Vec::new();
        l.sync(&mut out);
        assert!(!l.can_start(), "open and waiting");
        l.request_not_delivered();
        assert!(l.can_start(), "Start sends it again");
        let interface = l.settings.interface();
        let line = status_line(&StatusFacts {
            interface: &interface,
            started: true,
            paused: false,
            open_error: l.open_error.as_deref(),
            run: None,
            port_unlisted: false,
        });
        assert!(line.problem);
        assert!(
            line.text.ends_with("Press Start to try again."),
            "{}",
            line.text
        );
    }

    #[test]
    fn switching_to_a_kind_with_no_port_marks_the_display() {
        let mut l = Linemon::new(None);
        l.settings.kind = SourceKind::Udp;
        l.settings.udp.port = 5005;
        l.settings.serial.port.clear();
        l.started = true;
        let mut out = Vec::new();
        l.sync(&mut out);
        assert_eq!(opened(&out).len(), 1);

        l.settings.kind = SourceKind::Serial;
        l.sync(&mut out);
        assert_eq!(closes(&out), 1, "the UDP port is closed");
        let marks = l.stream.marks();
        assert_eq!(
            marks.last().map(|m| m.text.as_str()),
            Some("\u{2014} Serial, no port chosen \u{2014}")
        );
        // Nothing more on the next frames.
        let count = l.stream.marks().len();
        l.sync(&mut out);
        assert_eq!(l.stream.marks().len(), count);
    }

    #[test]
    fn opening_a_port_after_no_port_was_chosen_clears_the_display() {
        let mut l = Linemon::new(None);
        l.settings.kind = SourceKind::Udp;
        l.settings.udp.port = 5005;
        l.settings.serial.port.clear();
        l.settings.clear_on_port_change = true;
        l.started = true;
        let mut out = Vec::new();
        l.sync(&mut out);
        l.settings.kind = SourceKind::Serial;
        l.sync(&mut out);
        let before = l.stream.marks().len();
        assert!(before > 0, "the no-port marker is shown");

        l.settings.kind = SourceKind::Udp;
        l.sync(&mut out);
        let marks = l.stream.marks();
        assert_eq!(marks.len(), 1, "only the new port's marker is left");

        // Without the box ticked, nothing is cleared.
        let mut l = Linemon::new(None);
        l.settings.kind = SourceKind::Udp;
        l.settings.udp.port = 5005;
        l.settings.serial.port.clear();
        l.settings.clear_on_port_change = false;
        l.started = true;
        l.sync(&mut out);
        l.settings.kind = SourceKind::Serial;
        l.sync(&mut out);
        l.settings.kind = SourceKind::Udp;
        l.sync(&mut out);
        assert_eq!(l.stream.marks().len(), 3);
    }

    #[test]
    fn control_lines_show_only_while_a_serial_port_is_open() {
        let mut l = started_on("COM4");
        let mut out = Vec::new();
        l.sync(&mut out);
        let high = SerialControlLines {
            cts: true,
            ..SerialControlLines::default()
        };
        l.handle(LinemonUpdate::ControlLines(high));
        assert_eq!(l.control_lines, Some(high));

        // Stop forgets them: a closed port has no lines to show.
        l.started = false;
        l.sync(&mut out);
        assert_eq!(l.control_lines, None);
        // And a late reading after the close is not taken.
        l.handle(LinemonUpdate::ControlLines(high));
        assert_eq!(l.control_lines, None);
    }

    #[test]
    fn control_lines_are_live_only_while_the_port_runs() {
        // A port that faulted (an adapter unplugged) still has its last
        // reading, but the reader behind it has ended: the lines are not live.
        let run = |state| RunFacts {
            state,
            bytes_total: 0,
            bytes_per_sec: 0.0,
            fault: None,
        };
        let mut l = started_on("COM4");
        let mut out = Vec::new();
        l.sync(&mut out);
        let high = SerialControlLines {
            cts: true,
            ..SerialControlLines::default()
        };
        l.handle(LinemonUpdate::ControlLines(high));
        assert_eq!(l.live_control_lines(), None, "not known to be running yet");
        l.run = Some(run(ChannelState::Running));
        assert_eq!(l.live_control_lines(), Some(high));
        l.run = Some(run(ChannelState::Faulted));
        assert_eq!(l.live_control_lines(), None, "faulted");

        // A reading from the previous port can land after a switch. If the new
        // port then fails to open, nothing of the old run is left showing.
        l.run = Some(run(ChannelState::Running));
        l.handle(LinemonUpdate::Error("COM5 is in use".into()));
        assert_eq!(l.live_control_lines(), None, "the open failed");
        assert!(l.run.is_none());
    }

    #[test]
    fn stopping_adds_no_marker() {
        let mut l = started_on("COM4");
        let mut out = Vec::new();
        l.sync(&mut out);
        let count = l.stream.marks().len();
        l.started = false;
        l.sync(&mut out);
        assert_eq!(closes(&out), 1);
        assert_eq!(l.stream.marks().len(), count);
    }

    #[test]
    fn a_stale_error_or_snapshot_after_a_close_is_ignored() {
        let mut l = Linemon::new(None);
        l.handle(LinemonUpdate::Error("late".into()));
        assert_eq!(l.open_error, None);
    }

    #[test]
    fn only_a_change_of_source_fades_in_not_the_source_a_launch_opens_on() {
        assert_eq!(Linemon::new(None).drawn_kind, SourceKind::Serial);
        let stored = LinemonSettings {
            kind: SourceKind::Udp,
            ..Default::default()
        }
        .to_stored();
        let l = Linemon::new(Some(&stored));
        assert_eq!(l.settings.kind, SourceKind::Udp);
        assert_eq!(l.drawn_kind, SourceKind::Udp);
    }

    #[test]
    fn start_is_off_with_no_port_and_while_the_port_is_open() {
        let mut l = Linemon::new(None);
        assert!(!l.can_start());
        l.settings.serial.port = "COM4".into();
        assert!(l.can_start());
        l.started = true;
        l.sync(&mut Vec::new());
        assert!(!l.can_start(), "already open");
        l.open_error = Some("busy".into());
        assert!(l.can_start(), "try again after a failure");
    }

    /// Draw `kind`'s two settings lines in a `width`-point pane and return
    /// their height and how far they ran, from the pane's left edge.
    fn drawn_lines(kind: SourceKind, width: f32) -> (f32, f32) {
        let ctx = egui::Context::default();
        let mut l = Linemon::new(None);
        let mut drawn = (0.0, 0.0);
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.set_max_width(width);
            let left = ui.max_rect().left();
            let rect = ui
                .vertical(|ui| {
                    l.settings_lines(
                        ui,
                        kind,
                        &LocalAddresses::NotListed,
                        &mut None,
                        &mut Vec::new(),
                        4.0,
                    );
                })
                .response
                .rect;
            drawn = (rect.height(), rect.right() - left);
        });
        drawn
    }

    /// The serial lines are two rows whatever the width: in a pane too narrow
    /// for them they run past it, and the window's minimum width is what keeps
    /// them whole.
    #[test]
    fn the_serial_lines_never_wrap() {
        let (wide_height, wide_run) = drawn_lines(SourceKind::Serial, 1400.0);
        let (narrow_height, narrow_run) = drawn_lines(SourceKind::Serial, 320.0);
        assert_eq!(narrow_height, wide_height, "the lines wrapped");
        assert_eq!(narrow_run, wide_run);
        assert!(
            narrow_run > 320.0,
            "the lines are wider than the narrow pane"
        );
    }

    /// The same holds for the UDP lines, the port line with its binding
    /// address and the mode line with its group.
    #[test]
    fn the_udp_lines_never_wrap() {
        let (wide_height, wide_run) = drawn_lines(SourceKind::Udp, 1400.0);
        let (narrow_height, _) = drawn_lines(SourceKind::Udp, 320.0);
        assert_eq!(narrow_height, wide_height, "the lines wrapped");
        assert!(wide_run < 1400.0, "each line fits the wide pane");
    }
}
