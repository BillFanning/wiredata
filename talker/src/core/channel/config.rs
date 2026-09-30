use std::net::{Ipv4Addr, SocketAddr};

use serde::{Deserialize, Serialize};

use crate::core::message::MessageConfig;
use crate::core::timing::CadenceAlignment;

/// One channel in a profile: a single interface and the messages sent on it.
///
/// A channel has exactly one interface port. Each message is configured
/// independently (payload, interval, and — from a later phase — timestamp and
/// checksum).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelConfig {
    /// Display name shown in the GUI channel list. Cosmetic only — channels
    /// are identified by position, so the name carries no uniqueness
    /// requirement. Empty means "unnamed"; the GUI falls back to
    /// "Channel N". Additive `#[serde(default)]` field: older profiles
    /// load with an empty name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Cadence-wait policy. Additive and omitted for the default Standard
    /// behavior, so existing schema-v2 profiles load unchanged.
    /// Optional UTC phase alignment for the first and re-based send deadlines.
    #[serde(default, skip_serializing_if = "CadenceAlignment::is_immediate")]
    pub cadence_alignment: CadenceAlignment,
    pub interface: InterfaceConfig,
    #[serde(default)]
    pub messages: Vec<MessageConfig>,
}

impl ChannelConfig {
    pub fn new(interface: InterfaceConfig, messages: Vec<MessageConfig>) -> Self {
        Self {
            name: String::new(),
            cadence_alignment: CadenceAlignment::default(),
            interface,
            messages,
        }
    }

    /// [`ChannelConfig::new`] with a display name.
    pub fn named(
        name: impl Into<String>,
        interface: InterfaceConfig,
        messages: Vec<MessageConfig>,
    ) -> Self {
        Self {
            name: name.into(),
            cadence_alignment: CadenceAlignment::default(),
            interface,
            messages,
        }
    }
}

/// The interface type and parameters for one channel.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InterfaceConfig {
    Serial(SerialConfig),
    Udp(UdpConfig),
    TcpClient(TcpClientConfig),
}

impl InterfaceConfig {
    /// Check that `message` compiles and that one send fits this interface.
    ///
    /// Only UDP limits size today, to what one datagram carries. Checked at
    /// validation, so an oversized message is refused before Start rather
    /// than failing every send.
    pub fn check_message(&self, message: &MessageConfig) -> anyhow::Result<()> {
        let compiled = message.compile()?;
        if let Self::Udp(udp) = self {
            let len = compiled.wire_len();
            let limit = udp.max_message_len();
            anyhow::ensure!(
                len <= limit,
                "a {len}-byte message is larger than one UDP datagram can carry ({limit} bytes)"
            );
        }
        Ok(())
    }
}

// ── Serial ────────────────────────────────────────────────────────────────────

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerialConfig {
    pub port: String,
    #[serde(default = "default_baud")]
    pub baud_rate: u32,
    #[serde(default)]
    pub data_bits: DataBits,
    #[serde(default)]
    pub parity: Parity,
    #[serde(default)]
    pub stop_bits: StopBits,
    #[serde(default)]
    pub flow_control: FlowControl,
}

fn default_baud() -> u32 {
    9600
}

impl SerialConfig {
    pub fn new(port: impl Into<String>) -> Self {
        Self {
            port: port.into(),
            baud_rate: default_baud(),
            data_bits: DataBits::default(),
            parity: Parity::default(),
            stop_bits: StopBits::default(),
            flow_control: FlowControl::default(),
        }
    }
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataBits {
    Five,
    Six,
    Seven,
    #[default]
    Eight,
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Parity {
    #[default]
    None,
    Odd,
    Even,
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopBits {
    #[default]
    One,
    Two,
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowControl {
    #[default]
    None,
    Software,
    Hardware,
}

// ── UDP ───────────────────────────────────────────────────────────────────────

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UdpConfig {
    pub mode: UdpMode,
    /// Local port to bind; `None` lets the OS choose an ephemeral port.
    #[serde(default)]
    pub local_port: Option<u16>,
}

impl UdpConfig {
    pub fn unicast(destination: SocketAddr) -> Self {
        Self {
            mode: UdpMode::Unicast { destination },
            local_port: None,
        }
    }

    pub fn broadcast(destination: SocketAddr) -> Self {
        Self {
            mode: UdpMode::Broadcast { destination },
            local_port: None,
        }
    }

    pub fn multicast(group: Ipv4Addr, port: u16) -> Self {
        Self {
            mode: UdpMode::Multicast {
                group,
                port,
                interface: None,
                ttl: None,
            },
            local_port: None,
        }
    }

    /// Where datagrams go: the unicast or broadcast destination, or the
    /// multicast group and port.
    pub fn destination(&self) -> SocketAddr {
        match &self.mode {
            UdpMode::Unicast { destination } | UdpMode::Broadcast { destination } => *destination,
            UdpMode::Multicast { group, port, .. } => SocketAddr::from((*group, *port)),
        }
    }

    /// The largest message one datagram to [`Self::destination`] can carry:
    /// 65,535 bytes less the 8-byte UDP header and, for IPv4, the 20-byte IP
    /// header (IPv6's length field excludes its own header).
    pub fn max_message_len(&self) -> usize {
        if self.destination().is_ipv6() {
            65_527
        } else {
            65_507
        }
    }

    /// Multicast with an explicit outgoing `interface` and/or `ttl`
    /// (either `None` = OS default). See [`UdpMode::Multicast`].
    pub fn multicast_with(
        group: Ipv4Addr,
        port: u16,
        interface: Option<Ipv4Addr>,
        ttl: Option<u32>,
    ) -> Self {
        Self {
            mode: UdpMode::Multicast {
                group,
                port,
                interface,
                ttl,
            },
            local_port: None,
        }
    }
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UdpMode {
    Unicast {
        destination: SocketAddr,
    },
    Broadcast {
        destination: SocketAddr,
    },
    Multicast {
        group: Ipv4Addr,
        port: u16,
        /// Outgoing interface, by its local IPv4 address; `None` means the
        /// OS default interface (`IP_MULTICAST_IF`).
        #[serde(default)]
        interface: Option<Ipv4Addr>,
        /// Multicast hop limit (`IP_MULTICAST_TTL`); `None` leaves the OS
        /// default, which is `1` — datagrams stay on the local subnet.
        /// Raise it to route multicast across routers.
        #[serde(default)]
        ttl: Option<u32>,
    },
}

// ── TCP client ────────────────────────────────────────────────────────────────

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TcpClientConfig {
    pub address: SocketAddr,
}

impl TcpClientConfig {
    pub fn new(address: SocketAddr) -> Self {
        Self { address }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::message::PayloadConfig;

    fn round_trip<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(
        value: &T,
    ) {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(*value, back);
    }

    fn raw_message(len: usize) -> MessageConfig {
        MessageConfig::new(PayloadConfig::raw_hex("AA".repeat(len)), 100)
    }

    /// One send must fit one datagram: 65,535 bytes less the UDP header and,
    /// for IPv4, the IP header. Validation refuses anything larger, so it
    /// never reaches the send path.
    #[test]
    fn a_udp_message_must_fit_one_datagram() {
        let v4 = InterfaceConfig::Udp(UdpConfig::unicast("127.0.0.1:9".parse().unwrap()));
        assert!(v4.check_message(&raw_message(65_507)).is_ok());
        let err = v4.check_message(&raw_message(65_508)).unwrap_err();
        assert!(format!("{err:#}").contains("65507"), "{err:#}");

        let v6 = InterfaceConfig::Udp(UdpConfig::unicast("[::1]:9".parse().unwrap()));
        assert!(v6.check_message(&raw_message(65_527)).is_ok());
        assert!(v6.check_message(&raw_message(65_528)).is_err());

        let serial = InterfaceConfig::Serial(SerialConfig::new("COM1"));
        assert!(serial.check_message(&raw_message(70_000)).is_ok());
    }

    #[test]
    fn serial_config_defaults() {
        let c = SerialConfig::new("COM1");
        assert_eq!(c.baud_rate, 9600);
        assert_eq!(c.data_bits, DataBits::Eight);
        assert_eq!(c.parity, Parity::None);
        assert_eq!(c.stop_bits, StopBits::One);
        assert_eq!(c.flow_control, FlowControl::None);
    }

    #[test]
    fn serial_config_round_trip() {
        let c = SerialConfig::new("/dev/ttyUSB0");
        round_trip(&c);
    }

    #[test]
    fn interface_config_serial_tag() {
        let c = InterfaceConfig::Serial(SerialConfig::new("COM3"));
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("\"type\":\"serial\""));
        round_trip(&c);
    }

    #[test]
    fn udp_unicast_round_trip() {
        let c = InterfaceConfig::Udp(UdpConfig::unicast("127.0.0.1:5000".parse().unwrap()));
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("\"type\":\"udp\""));
        assert!(json.contains("\"type\":\"unicast\""));
        round_trip(&c);
    }

    #[test]
    fn udp_broadcast_round_trip() {
        let c = InterfaceConfig::Udp(UdpConfig::broadcast(
            "255.255.255.255:9999".parse().unwrap(),
        ));
        round_trip(&c);
    }

    #[test]
    fn udp_multicast_round_trip() {
        let c = InterfaceConfig::Udp(UdpConfig::multicast("239.1.2.3".parse().unwrap(), 5000));
        round_trip(&c);
    }

    #[test]
    fn udp_multicast_with_interface_and_ttl_round_trip() {
        let c = InterfaceConfig::Udp(UdpConfig::multicast_with(
            "239.1.2.3".parse().unwrap(),
            5000,
            Some("192.168.1.10".parse().unwrap()),
            Some(8),
        ));
        round_trip(&c);
    }

    #[test]
    fn udp_multicast_defaults_interface_and_ttl_to_none() {
        // Old profiles predate the interface / ttl keys; #[serde(default)]
        // means they deserialize to None, not an error.
        let json =
            r#"{"mode":{"type":"multicast","group":"239.1.2.3","port":5000},"local_port":null}"#;
        let cfg: UdpConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.mode,
            UdpMode::Multicast {
                group: "239.1.2.3".parse().unwrap(),
                port: 5000,
                interface: None,
                ttl: None,
            }
        );
    }

    #[test]
    fn tcp_client_round_trip() {
        let c = InterfaceConfig::TcpClient(TcpClientConfig::new("10.0.0.1:4001".parse().unwrap()));
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("\"type\":\"tcp_client\""));
        round_trip(&c);
    }

    #[test]
    fn channel_config_round_trip() {
        let c = ChannelConfig::new(
            InterfaceConfig::Udp(UdpConfig::unicast("127.0.0.1:6000".parse().unwrap())),
            vec![MessageConfig::new(PayloadConfig::raw_hex("AABB"), 500)],
        );
        round_trip(&c);
    }

    #[test]
    fn channel_config_defaults_messages_to_empty() {
        let json = r#"{"interface":{"type":"tcp_client","address":"10.0.0.1:1"}}"#;
        let c: ChannelConfig = serde_json::from_str(json).unwrap();
        assert!(c.messages.is_empty());
        // Older profiles predate the name key — defaults to unnamed.
        assert!(c.name.is_empty());
    }

    #[test]
    fn channel_config_name_round_trips_and_empty_is_omitted() {
        let iface = InterfaceConfig::TcpClient(TcpClientConfig::new("10.0.0.1:1".parse().unwrap()));
        let named = ChannelConfig::named("GPS feed", iface.clone(), vec![]);
        round_trip(&named);
        // Unnamed channels serialize without a name key at all, keeping
        // old-style profiles byte-identical.
        let json = serde_json::to_string(&ChannelConfig::new(iface, vec![])).unwrap();
        assert!(!json.contains("\"name\""), "{json}");
    }

    #[test]
    fn data_bits_default_is_eight() {
        assert_eq!(DataBits::default(), DataBits::Eight);
    }

    #[test]
    fn parity_default_is_none() {
        assert_eq!(Parity::default(), Parity::None);
    }

    #[test]
    fn stop_bits_default_is_one() {
        assert_eq!(StopBits::default(), StopBits::One);
    }

    #[test]
    fn flow_control_default_is_none() {
        assert_eq!(FlowControl::default(), FlowControl::None);
    }
}
