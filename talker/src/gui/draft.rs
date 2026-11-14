use std::net::{Ipv4Addr, SocketAddr};
use std::sync::OnceLock;

use crate::core::{
    channel::{
        DataBits, FlowControl, InterfaceConfig, Parity, SerialConfig, StopBits, TcpClientConfig,
        UdpConfig, UdpMode,
    },
    message::{
        ByteOrder, ChecksumAlgorithm, ChecksumConfig, CodePage, MessageConfig, NmeaChecksumMode,
        PayloadConfig, TimestampConfig,
    },
    timing::CadenceAlignment,
};

// ── Channel interface ─────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ConnKind {
    Serial,
    Udp,
    Tcp,
}

impl ConnKind {
    /// Match listener's Add menu: the transport is selected when the channel
    /// is created and is not a routine field in the detail editor.
    pub const ADD_MENU: [Self; 3] = [Self::Udp, Self::Tcp, Self::Serial];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Serial => "Serial",
            Self::Udp => "UDP",
            Self::Tcp => "TCP",
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
pub enum UdpModeDraft {
    Unicast,
    Broadcast,
    Multicast,
}

/// Hold-to-repeat state for a single ± port control. Lives only at runtime;
/// never serialised.
///
/// Uses absolute `Instant` deadlines rather than accumulating per-frame `dt`,
/// so the repeat cadence is correct even when the framerate is jittery.
#[derive(Debug, Clone, Copy)]
pub struct PortHold {
    /// −1 for the minus button, +1 for the plus button.
    pub direction: i8,
    /// When the button was first pressed — used to accelerate the repeat
    /// interval (see `port_repeat_interval` in `gui/mod.rs`).
    pub started: std::time::Instant,
    /// Wall-clock instant of the next scheduled fire.
    pub next_fire_at: std::time::Instant,
}

/// One address + port pair, both held as user-editable text. Shared across
/// the three UDP modes so each can offer the same `[addr] Port: [-][port][+]`
/// layout without diverging copies.
#[derive(Default, Clone)]
pub struct AddrPortPair {
    pub addr: String,
    pub port: String,
    /// `true` once the user has explicitly committed this pair —
    /// either by pressing Enter on a field, starting the channel
    /// with this mode active, or loading from a saved profile.
    /// Switches the field-level red-box validation from *lenient*
    /// (no red on empty or partial typing) to *strict* (red on
    /// empty too, plus any malformed value). Not serialized.
    pub submitted: bool,
}

/// Detect the local machine's primary IPv4 and return its first three
/// octets followed by a dot — `"192.168.1."` etc. — so a fresh unicast
/// destination only needs the user to type the last octet.
///
/// Uses the classic "bind UDP, connect to a route-able address, read
/// local_addr" trick. No packet is actually sent; the OS just selects
/// the outbound interface. Cached behind a [`OnceLock`] so the routing
/// lookup only happens once per process.
///
/// Returns `None` on machines without a routable IPv4 (loopback-only
/// boxes, fully offline containers) so the field falls back to empty.
pub fn lan_addr_prefix() -> Option<String> {
    static CACHED: OnceLock<Option<String>> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
            // 8.8.8.8 is a stable public address — `connect` on UDP is
            // a pure routing lookup, no packet leaves the box.
            socket.connect("8.8.8.8:80").ok()?;
            let std::net::IpAddr::V4(v4) = socket.local_addr().ok()?.ip() else {
                return None;
            };
            // Skip loopback / unspecified — they're not what the user
            // wants pre-filled even if `connect` somehow yields them.
            if v4.is_loopback() || v4.is_unspecified() {
                return None;
            }
            let o = v4.octets();
            Some(format!("{}.{}.{}.", o[0], o[1], o[2]))
        })
        .clone()
}

pub struct ConnDraft {
    /// Channel display name (cosmetic; `ChannelConfig::name`). Empty means
    /// unnamed — the GUI falls back to "Channel N". Not part of
    /// `to_config()` (which builds only the interface); the profile writer
    /// carries it separately.
    pub name: String,
    /// Channel-level cadence wait policy, persisted with
    /// [`crate::core::channel::ChannelConfig`] by the GUI's all-or-none draft
    /// conversion.
    pub cadence_alignment: CadenceAlignment,
    // Creation-time identity of the interface draft. Kept private so the GUI
    // cannot accidentally reintroduce an in-place transport switch; Add or a
    // loaded profile chooses it, while Configure edits only its parameters.
    kind: ConnKind,
    // serial
    pub serial_port: String,
    pub baud_rate: u32,
    pub baud_custom: String, // text buffer for non-preset baud rates
    pub data_bits: u8,
    pub parity: u8,       // 0=None 1=Odd 2=Even
    pub stop_bits: u8,    // 1 or 2
    pub flow_control: u8, // 0=None 1=Software 2=Hardware
    // udp
    pub udp_mode: UdpModeDraft,
    /// Per-mode destination buffers, kept independently so switching
    /// modes doesn't trample what was typed for the others.
    pub udp_unicast: AddrPortPair,
    pub udp_broadcast: AddrPortPair,
    pub udp_multicast: AddrPortPair,
    /// Multicast outgoing interface / TTL. No editor widget yet (that comes
    /// with the GUI merge); carried through load → save so a profile that
    /// sets them via TOML is never silently stripped on the next save.
    pub udp_multicast_interface: Option<Ipv4Addr>,
    pub udp_multicast_ttl: Option<u32>,
    /// Transient hold-to-repeat state for the active mode's ± port
    /// buttons. Shared because only one mode is being edited at a time.
    pub udp_port_hold: Option<PortHold>,
    pub local_port: String, // optional local bind port
    // tcp
    pub tcp_addr: String,
}

impl Default for ConnDraft {
    fn default() -> Self {
        Self {
            name: String::new(),
            cadence_alignment: CadenceAlignment::default(),
            kind: ConnKind::Serial,
            serial_port: String::new(),
            baud_rate: 9600,
            baud_custom: String::new(),
            data_bits: 8,
            parity: 0,
            stop_bits: 1,
            flow_control: 0,
            udp_mode: UdpModeDraft::Unicast,
            udp_unicast: AddrPortPair {
                // Prefill the first three octets of the LAN address so
                // the user only types the last byte. Empty when no LAN
                // (offline / loopback-only).
                addr: lan_addr_prefix().unwrap_or_default(),
                ..Default::default()
            },
            udp_broadcast: AddrPortPair {
                addr: Ipv4Addr::BROADCAST.to_string(),
                ..Default::default()
            },
            udp_multicast: AddrPortPair::default(),
            udp_multicast_interface: None,
            udp_multicast_ttl: None,
            udp_port_hold: None,
            local_port: String::new(),
            tcp_addr: String::new(),
        }
    }
}

impl From<&InterfaceConfig> for ConnDraft {
    fn from(cfg: &InterfaceConfig) -> Self {
        match cfg {
            InterfaceConfig::Serial(s) => {
                const PRESETS: &[u32] = &[4800, 9600, 19200, 38400, 57600, 115200];
                Self {
                    kind: ConnKind::Serial,
                    serial_port: s.port.clone(),
                    baud_rate: s.baud_rate,
                    baud_custom: if PRESETS.contains(&s.baud_rate) {
                        String::new()
                    } else {
                        s.baud_rate.to_string()
                    },
                    data_bits: match s.data_bits {
                        DataBits::Five => 5,
                        DataBits::Six => 6,
                        DataBits::Seven => 7,
                        _ => 8,
                    },
                    parity: match s.parity {
                        Parity::Odd => 1,
                        Parity::Even => 2,
                        _ => 0,
                    },
                    stop_bits: match s.stop_bits {
                        StopBits::Two => 2,
                        _ => 1,
                    },
                    flow_control: match s.flow_control {
                        FlowControl::Software => 1,
                        FlowControl::Hardware => 2,
                        _ => 0,
                    },
                    ..Default::default()
                }
            }
            InterfaceConfig::Udp(u) => {
                let mut draft = Self {
                    kind: ConnKind::Udp,
                    local_port: u.local_port.map(|p| p.to_string()).unwrap_or_default(),
                    ..Default::default()
                };
                // A loaded profile's config is treated as already
                // committed (`submitted = true`) — the user saved it
                // explicitly, so any empty / malformed fields surface
                // as red right away rather than waiting for a fresh
                // Enter press to switch the field into strict mode.
                match &u.mode {
                    UdpMode::Unicast { destination } => {
                        draft.udp_mode = UdpModeDraft::Unicast;
                        draft.udp_unicast = AddrPortPair {
                            addr: destination.ip().to_string(),
                            port: destination.port().to_string(),
                            submitted: true,
                        };
                    }
                    UdpMode::Broadcast { destination } => {
                        draft.udp_mode = UdpModeDraft::Broadcast;
                        draft.udp_broadcast = AddrPortPair {
                            addr: destination.ip().to_string(),
                            port: destination.port().to_string(),
                            submitted: true,
                        };
                    }
                    UdpMode::Multicast {
                        group,
                        port,
                        interface,
                        ttl,
                    } => {
                        draft.udp_mode = UdpModeDraft::Multicast;
                        draft.udp_multicast = AddrPortPair {
                            addr: group.to_string(),
                            port: port.to_string(),
                            submitted: true,
                        };
                        draft.udp_multicast_interface = *interface;
                        draft.udp_multicast_ttl = *ttl;
                    }
                }
                draft
            }
            InterfaceConfig::TcpClient(t) => Self {
                kind: ConnKind::Tcp,
                tcp_addr: t.address.to_string(),
                ..Default::default()
            },
        }
    }
}

impl ConnDraft {
    pub fn new(kind: ConnKind) -> Self {
        Self {
            kind,
            ..Self::default()
        }
    }

    pub const fn kind(&self) -> ConnKind {
        self.kind
    }

    pub fn to_config(&self) -> Option<InterfaceConfig> {
        // Empty local_port = "let the OS pick". Non-empty but unparseable
        // (e.g. "444444444" > u16::MAX) is a user mistake; fail the whole
        // build so the channel can't start and the disabled-Start tooltip
        // surfaces the error. (Previously we silently fell back to None.)
        let local_port = if self.local_port.is_empty() {
            None
        } else {
            Some(self.local_port.parse::<u16>().ok()?)
        };
        match self.kind {
            ConnKind::Serial => {
                if self.serial_port.is_empty() {
                    return None;
                }
                Some(InterfaceConfig::Serial(SerialConfig {
                    port: self.serial_port.clone(),
                    baud_rate: self.baud_rate,
                    data_bits: match self.data_bits {
                        5 => DataBits::Five,
                        6 => DataBits::Six,
                        7 => DataBits::Seven,
                        _ => DataBits::Eight,
                    },
                    parity: match self.parity {
                        1 => Parity::Odd,
                        2 => Parity::Even,
                        _ => Parity::None,
                    },
                    stop_bits: if self.stop_bits == 2 {
                        StopBits::Two
                    } else {
                        StopBits::One
                    },
                    flow_control: match self.flow_control {
                        1 => FlowControl::Software,
                        2 => FlowControl::Hardware,
                        _ => FlowControl::None,
                    },
                }))
            }
            ConnKind::Udp => {
                let pair = match self.udp_mode {
                    UdpModeDraft::Unicast => &self.udp_unicast,
                    UdpModeDraft::Broadcast => &self.udp_broadcast,
                    UdpModeDraft::Multicast => &self.udp_multicast,
                };
                let addr: Ipv4Addr = pair.addr.parse().ok()?;
                let port: u16 = pair.port.parse().ok()?;
                let mut udp = match self.udp_mode {
                    UdpModeDraft::Unicast => UdpConfig::unicast(SocketAddr::from((addr, port))),
                    UdpModeDraft::Broadcast => UdpConfig::broadcast(SocketAddr::from((addr, port))),
                    UdpModeDraft::Multicast => UdpConfig::multicast_with(
                        addr,
                        port,
                        self.udp_multicast_interface,
                        self.udp_multicast_ttl,
                    ),
                };
                udp.local_port = local_port;
                Some(InterfaceConfig::Udp(udp))
            }
            ConnKind::Tcp => {
                let addr: SocketAddr = self.tcp_addr.parse().ok()?;
                Some(InterfaceConfig::TcpClient(TcpClientConfig::new(addr)))
            }
        }
    }
}

// ── Message ───────────────────────────────────────────────────────────────────

/// Which payload format a message draft is editing.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum PayloadKind {
    Hex,
    Utf8,
    Utf16,
    Ascii,
    Nmea,
}

/// Editable state for one message. A text buffer is kept per format so
/// switching the format selector does not discard what was typed.
pub struct ScheduleDraft {
    /// Monotonic content revision for cached compile/preview analysis. Only fields
    /// that can affect the wire message increment this; popup filters, insertion
    /// scratch text, and remove-confirmation chrome deliberately do not.
    pub(super) revision: u64,
    pub payload_kind: PayloadKind,
    // hex
    pub hex_data: String,
    // utf-8
    pub utf8_text: String,
    // utf-16
    pub utf16_text: String,
    pub utf16_big_endian: bool,
    pub utf16_bom: bool,
    /// Mirrors `PayloadConfig::Utf16::allow_raw_bytes`. When `false`
    /// (default), `‹XX›` in the text is literal; when `true` the
    /// editor recognises markers and an `Insert Byte` button appears.
    pub utf16_allow_raw_bytes: bool,
    // ascii
    pub ascii_text: String,
    pub ascii_code_page: CodePage,
    // nmea
    pub nmea_talker: String,
    pub nmea_sentence_type: String,
    pub nmea_fields: String, // comma-separated field values
    pub nmea_checksum_mode: NmeaChecksumMode,
    pub nmea_live_time: bool,
    pub nmea_live_millis: bool,
    /// True when `nmea_fields` was last set by the auto-prefill helper —
    /// not by the user typing. Lets the sentence-picker safely overwrite
    /// stale example fields when the user picks a new sentence type, but
    /// leave alone anything the user has actually edited.
    pub nmea_fields_autofilled: bool,
    /// Live filter text for the talker-picker popup (not serialized).
    pub nmea_talker_filter: String,
    /// Live filter text for the sentence-picker popup (not serialized).
    pub nmea_sentence_filter: String,
    // common
    pub interval_ms: String,
    // timestamp
    pub timestamp_enabled: bool,
    pub ts_date: bool,
    pub ts_millis: bool,
    pub ts_timezone: bool,
    pub ts_utc: bool,
    // checksum
    pub checksum_enabled: bool,
    pub checksum_algorithm: ChecksumAlgorithm,
    pub checksum_wrong: bool,
    /// Scratch buffer for the Insert Byte popup; not part of the message.
    pub insert_byte_hex: String,
    /// True while the user has clicked the message-remove (✕) button but
    /// hasn't yet confirmed. The header row swaps to "Remove? [✓] [✕]"
    /// while this is set. Cleared on Confirm, Cancel, or anything that
    /// makes the message disappear. Not serialised.
    pub pending_remove: bool,
}

impl Default for ScheduleDraft {
    fn default() -> Self {
        Self {
            revision: 0,
            payload_kind: PayloadKind::Hex,
            hex_data: String::new(),
            utf8_text: String::new(),
            utf16_text: String::new(),
            utf16_big_endian: true,
            utf16_bom: false,
            utf16_allow_raw_bytes: false,
            ascii_text: String::new(),
            ascii_code_page: CodePage::default(),
            nmea_talker: String::new(),
            nmea_sentence_type: String::new(),
            nmea_fields: String::new(),
            nmea_checksum_mode: NmeaChecksumMode::Correct,
            nmea_live_time: false,
            nmea_live_millis: false,
            nmea_fields_autofilled: false,
            nmea_talker_filter: String::new(),
            nmea_sentence_filter: String::new(),
            interval_ms: "1000".to_string(),
            timestamp_enabled: false,
            // Minimal HH:MM:SS by default — Date / Milliseconds / UTC / Zone
            // are opt-in. New users overwhelmingly want a short timestamp;
            // power users can flip the extras on per-message.
            ts_date: false,
            ts_millis: false,
            ts_timezone: false,
            ts_utc: false,
            checksum_enabled: false,
            checksum_algorithm: ChecksumAlgorithm::default(),
            checksum_wrong: false,
            insert_byte_hex: String::new(),
            pending_remove: false,
        }
    }
}

impl From<&MessageConfig> for ScheduleDraft {
    fn from(m: &MessageConfig) -> Self {
        let mut d = ScheduleDraft::default();
        match &m.payload {
            PayloadConfig::RawHex { data } => {
                d.payload_kind = PayloadKind::Hex;
                d.hex_data = data.clone();
            }
            PayloadConfig::Utf8 { text } => {
                d.payload_kind = PayloadKind::Utf8;
                d.utf8_text = text.clone();
            }
            PayloadConfig::Utf16 {
                text,
                byte_order,
                bom,
                allow_raw_bytes,
            } => {
                d.payload_kind = PayloadKind::Utf16;
                d.utf16_text = text.clone();
                d.utf16_big_endian = matches!(byte_order, ByteOrder::BigEndian);
                d.utf16_bom = *bom;
                d.utf16_allow_raw_bytes = *allow_raw_bytes;
            }
            PayloadConfig::Ascii { text, code_page } => {
                d.payload_kind = PayloadKind::Ascii;
                d.ascii_text = text.clone();
                d.ascii_code_page = *code_page;
            }
            PayloadConfig::Nmea {
                talker,
                sentence_type,
                fields,
                nmea_checksum,
                live_time,
                live_time_millis,
            } => {
                d.payload_kind = PayloadKind::Nmea;
                d.nmea_talker = talker.clone();
                d.nmea_sentence_type = sentence_type.clone();
                d.nmea_fields = fields.join(",");
                d.nmea_checksum_mode = *nmea_checksum;
                d.nmea_live_time = *live_time;
                d.nmea_live_millis = *live_time_millis;
            }
        }
        d.interval_ms = m.interval_ms.to_string();
        if let Some(ts) = &m.timestamp {
            d.timestamp_enabled = true;
            d.ts_date = ts.include_date;
            d.ts_millis = ts.include_millis;
            d.ts_timezone = ts.include_timezone;
            d.ts_utc = ts.utc;
        }
        if let Some(cs) = &m.checksum {
            d.checksum_enabled = true;
            d.checksum_algorithm = cs.algorithm;
            d.checksum_wrong = cs.intentionally_wrong;
        }
        d
    }
}

impl ScheduleDraft {
    pub(super) fn revision(&self) -> u64 {
        self.revision
    }

    pub(super) fn mark_changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn to_message_config(&self) -> Option<MessageConfig> {
        let interval_ms: u64 = self.interval_ms.parse().ok()?;
        let payload = match self.payload_kind {
            PayloadKind::Hex => PayloadConfig::raw_hex(&self.hex_data),
            PayloadKind::Utf8 => PayloadConfig::Utf8 {
                text: self.utf8_text.clone(),
            },
            PayloadKind::Utf16 => PayloadConfig::Utf16 {
                text: self.utf16_text.clone(),
                byte_order: if self.utf16_big_endian {
                    ByteOrder::BigEndian
                } else {
                    ByteOrder::LittleEndian
                },
                bom: self.utf16_bom,
                allow_raw_bytes: self.utf16_allow_raw_bytes,
            },
            PayloadKind::Ascii => PayloadConfig::Ascii {
                text: self.ascii_text.clone(),
                code_page: self.ascii_code_page,
            },
            PayloadKind::Nmea => {
                if self.nmea_talker.is_empty() || self.nmea_sentence_type.is_empty() {
                    return None;
                }
                let fields: Vec<String> = if self.nmea_fields.is_empty() {
                    vec![]
                } else {
                    self.nmea_fields.split(',').map(str::to_string).collect()
                };
                PayloadConfig::Nmea {
                    talker: self.nmea_talker.clone(),
                    sentence_type: self.nmea_sentence_type.clone(),
                    fields,
                    nmea_checksum: self.nmea_checksum_mode,
                    live_time: self.nmea_live_time,
                    live_time_millis: self.nmea_live_millis,
                }
            }
        };
        let timestamp = self.timestamp_enabled.then_some(TimestampConfig {
            include_date: self.ts_date,
            include_millis: self.ts_millis,
            include_timezone: self.ts_timezone,
            utc: self.ts_utc,
        });
        let checksum = self.checksum_enabled.then_some(ChecksumConfig {
            algorithm: self.checksum_algorithm,
            intentionally_wrong: self.checksum_wrong,
        });
        Some(MessageConfig {
            payload,
            interval_ms,
            timestamp,
            checksum,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_menu_matches_listener_order_and_preserves_the_chosen_kind() {
        assert_eq!(
            ConnKind::ADD_MENU.map(ConnKind::label),
            ["UDP", "TCP", "Serial"]
        );
        for kind in ConnKind::ADD_MENU {
            let draft = ConnDraft::new(kind);
            assert_eq!(draft.kind(), kind);
            assert_eq!(draft.cadence_alignment, CadenceAlignment::Immediate);
        }
    }

    // ── ConnDraft round-trips ─────────────────────────────────────────────────
    //
    // A GUI-representable interface config must survive
    // InterfaceConfig -> ConnDraft -> to_config() unchanged.

    fn conn_round_trip(cfg: InterfaceConfig) {
        let draft = ConnDraft::from(&cfg);
        assert_eq!(draft.to_config(), Some(cfg));
    }

    #[test]
    fn serial_draft_round_trip() {
        conn_round_trip(InterfaceConfig::Serial(SerialConfig {
            port: "COM7".to_string(),
            baud_rate: 38400,
            data_bits: DataBits::Seven,
            parity: Parity::Even,
            stop_bits: StopBits::Two,
            flow_control: FlowControl::Hardware,
        }));
    }

    #[test]
    fn udp_unicast_draft_round_trip() {
        conn_round_trip(InterfaceConfig::Udp(UdpConfig::unicast(
            "192.168.1.50:4000".parse().unwrap(),
        )));
    }

    #[test]
    fn udp_broadcast_draft_round_trip() {
        conn_round_trip(InterfaceConfig::Udp(UdpConfig::broadcast(
            "255.255.255.255:9000".parse().unwrap(),
        )));
    }

    #[test]
    fn udp_multicast_draft_round_trip() {
        conn_round_trip(InterfaceConfig::Udp(UdpConfig::multicast(
            "239.0.0.7".parse().unwrap(),
            5500,
        )));
    }

    #[test]
    fn udp_multicast_draft_preserves_interface_and_ttl() {
        // No editor widget yet, but a load → save must not strip these.
        conn_round_trip(InterfaceConfig::Udp(UdpConfig::multicast_with(
            "239.0.0.7".parse().unwrap(),
            5500,
            Some("192.168.5.4".parse().unwrap()),
            Some(16),
        )));
    }

    #[test]
    fn udp_draft_preserves_local_port() {
        let mut udp = UdpConfig::unicast("127.0.0.1:5000".parse().unwrap());
        udp.local_port = Some(6000);
        conn_round_trip(InterfaceConfig::Udp(udp));
    }

    #[test]
    fn tcp_draft_round_trip() {
        conn_round_trip(InterfaceConfig::TcpClient(TcpClientConfig::new(
            "10.0.0.1:4001".parse().unwrap(),
        )));
    }

    // ── ScheduleDraft round-trips ─────────────────────────────────────────────
    //
    // A message must survive MessageConfig -> ScheduleDraft -> to_message_config()
    // unchanged, for every payload format and with/without timestamp + checksum.

    fn message_round_trip(m: MessageConfig) {
        let draft = ScheduleDraft::from(&m);
        assert_eq!(draft.to_message_config(), Some(m));
    }

    #[test]
    fn hex_message_round_trip() {
        message_round_trip(MessageConfig::new(PayloadConfig::raw_hex("DEADBEEF"), 500));
    }

    #[test]
    fn utf8_message_round_trip() {
        message_round_trip(MessageConfig::new(
            PayloadConfig::Utf8 {
                text: "héllo".to_string(),
            },
            1000,
        ));
    }

    #[test]
    fn utf16_message_round_trip() {
        message_round_trip(MessageConfig::new(
            PayloadConfig::Utf16 {
                text: "data".to_string(),
                byte_order: ByteOrder::LittleEndian,
                bom: true,
                allow_raw_bytes: false,
            },
            250,
        ));
    }

    #[test]
    fn ascii_message_round_trip() {
        message_round_trip(MessageConfig::new(
            PayloadConfig::Ascii {
                text: "café".to_string(),
                code_page: CodePage::Cp437,
            },
            750,
        ));
    }

    #[test]
    fn nmea_message_round_trip() {
        message_round_trip(MessageConfig::new(
            PayloadConfig::nmea("GP", "GGA", vec!["123519".to_string(), "N".to_string()]),
            1000,
        ));
    }

    #[test]
    fn nmea_message_with_no_fields_round_trip() {
        message_round_trip(MessageConfig::new(
            PayloadConfig::nmea("GN", "RMC", vec![]),
            2000,
        ));
    }

    #[test]
    fn live_nmea_message_round_trip() {
        message_round_trip(MessageConfig::new(
            PayloadConfig::nmea_live("GN", "RMC", vec!["typed".to_string()], true),
            250,
        ));
    }

    #[test]
    fn message_round_trip_with_timestamp_and_checksum() {
        message_round_trip(MessageConfig {
            payload: PayloadConfig::raw_hex("AABB"),
            interval_ms: 1000,
            timestamp: Some(TimestampConfig {
                include_date: true,
                include_millis: false,
                include_timezone: true,
                utc: true,
            }),
            checksum: Some(ChecksumConfig {
                algorithm: ChecksumAlgorithm::Crc16Modbus,
                intentionally_wrong: true,
            }),
        });
    }

    #[test]
    fn message_without_timestamp_or_checksum_stays_disabled() {
        let m = MessageConfig::new(PayloadConfig::raw_hex("00"), 100);
        let draft = ScheduleDraft::from(&m);
        assert!(!draft.timestamp_enabled);
        assert!(!draft.checksum_enabled);
        assert_eq!(draft.to_message_config(), Some(m));
    }

    #[test]
    fn switching_format_does_not_lose_other_buffers() {
        // A hex message loaded as a draft keeps usable defaults in the other
        // format buffers, so toggling the selector never panics or blanks out.
        // Talker / sentence_type start empty so the NMEA dropdowns open on
        // the explicit "(empty)" row instead of a presumed default.
        let draft = ScheduleDraft::from(&MessageConfig::new(PayloadConfig::raw_hex("FF"), 100));
        assert_eq!(draft.payload_kind, PayloadKind::Hex);
        assert_eq!(draft.nmea_talker, "");
        assert_eq!(draft.nmea_sentence_type, "");
    }
}
