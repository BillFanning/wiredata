//! Map validated configuration to live runtime objects (spec §128 — the runtime
//! owns this mapping, not config).
//!
//! Pure and synchronous: it constructs *unopened*
//! transports from a [`ChannelConfig`](crate::config::ChannelConfig). Opening /
//! binding is async and happens at Start in the orchestrator (§8.2, §71). Serial
//! parameters that `serialport` cannot represent (Mark/Space parity, 1.5 stop
//! bits — §80.1) are rejected here rather than silently downgraded.

use std::net::SocketAddr;

use serialport::{DataBits, FlowControl, Parity, StopBits};

use crate::config::schema::{
    DataBits as CfgDataBits, DisplayViewConfig, FlowControl as CfgFlowControl, Parity as CfgParity,
    SerialConfig, StopBits as CfgStopBits, UdpConfig,
};
use crate::core::ChannelId;
use crate::display::DisplayView;
use crate::transport::serial::SerialTransport;
use crate::transport::udp::UdpTransport;

/// A configuration could not be realized as a runtime object.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("serial parity Mark/Space is not supported by the serial backend")]
    UnsupportedParity,
    #[error("1.5 stop bits is not supported by the serial backend")]
    UnsupportedStopBits,
    #[error("invalid socket address {address:?}:{port}")]
    InvalidSocketAddr { address: String, port: u16 },
    #[error("invalid multicast group address {0:?}")]
    InvalidMulticastGroup(String),
    #[error("invalid multicast interface address {0:?}")]
    InvalidMulticastInterface(String),
}

/// Build a Display View renderer from its config (§47, §78). Visual-only fields
/// (font, colors) don't affect produced text; the actual wrap width is set by
/// the UI at render time, so it starts unset here.
pub fn build_display_view(config: &DisplayViewConfig) -> DisplayView {
    DisplayView {
        mode: config.mode,
        encoding: config.encoding,
        character_rendering: config.character_rendering,
        wrapping: config.wrapping,
        wrap_width: None,
        hex_separator: " ".to_string(),
        hex_bytes_per_group: hex_bytes_per_group(config),
        // Never wrap. `HexGrouping` splits into rendering and layout, and only
        // rendering belongs in a recording: `bytes_per_group` changes what the
        // bytes look like and reaches the `.disp`, while `groups_per_line`
        // decides where lines end, which is the viewer's question (ADR-018 —
        // `.disp` is the exact rendered stream, never hard-wrapped).
        hex_bytes_per_line: 0,
    }
}

/// Bytes that run together between separators (§45). Zero would render every
/// byte of the stream as one unbroken run, which the config cannot mean, so it
/// resolves to the conventional single byte per group.
pub fn hex_bytes_per_group(config: &DisplayViewConfig) -> usize {
    usize::from(config.hex_grouping.bytes_per_group).max(1)
}

fn map_data_bits(bits: CfgDataBits) -> DataBits {
    match bits {
        CfgDataBits::Five => DataBits::Five,
        CfgDataBits::Six => DataBits::Six,
        CfgDataBits::Seven => DataBits::Seven,
        CfgDataBits::Eight => DataBits::Eight,
    }
}

fn map_parity(parity: CfgParity) -> Result<Parity, BuildError> {
    match parity {
        CfgParity::None => Ok(Parity::None),
        CfgParity::Even => Ok(Parity::Even),
        CfgParity::Odd => Ok(Parity::Odd),
        CfgParity::Mark | CfgParity::Space => Err(BuildError::UnsupportedParity),
    }
}

fn map_stop_bits(bits: CfgStopBits) -> Result<StopBits, BuildError> {
    match bits {
        CfgStopBits::One => Ok(StopBits::One),
        CfgStopBits::Two => Ok(StopBits::Two),
        CfgStopBits::OnePointFive => Err(BuildError::UnsupportedStopBits),
    }
}

fn map_flow_control(flow: CfgFlowControl) -> FlowControl {
    match flow {
        CfgFlowControl::None => FlowControl::None,
        CfgFlowControl::XonXoff => FlowControl::Software,
        CfgFlowControl::RtsCts => FlowControl::Hardware,
    }
}

/// Build an unopened serial transport (§14, §74).
pub fn build_serial(
    channel_id: ChannelId,
    config: &SerialConfig,
) -> Result<SerialTransport, BuildError> {
    let mut transport = SerialTransport::new(channel_id, config.port.clone(), config.baud_rate)
        .with_data_bits(map_data_bits(config.data_bits))
        .with_parity(map_parity(config.parity)?)
        .with_stop_bits(map_stop_bits(config.stop_bits)?)
        .with_flow_control(map_flow_control(config.flow_control));
    let (rts, dtr) = lines_at_open(config);
    if let Some(rts) = rts {
        transport = transport.with_rts(rts);
    }
    if let Some(dtr) = dtr {
        transport = transport.with_dtr(dtr);
    }
    Ok(transport)
}

/// The RTS and DTR levels to set at open (§14.2, ADR-073), as `(rts, dtr)`.
///
/// Listener cannot read an output line back, so what it shows is what it set.
/// A line the profile leaves unset is therefore set low, which makes the shown
/// level true whatever the operating system's default. The exception is RTS
/// under RTS/CTS flow control: that is the driver's to set, so it is left
/// alone (`None`) and shown as set by flow control.
fn lines_at_open(config: &SerialConfig) -> (Option<bool>, Option<bool>) {
    let rts = match config.flow_control {
        CfgFlowControl::RtsCts => config.rts,
        CfgFlowControl::None | CfgFlowControl::XonXoff => config.rts.or(Some(false)),
    };
    (rts, config.dtr.or(Some(false)))
}

fn socket_addr(address: &str, port: u16) -> Result<SocketAddr, BuildError> {
    format!("{address}:{port}")
        .parse()
        .map_err(|_| BuildError::InvalidSocketAddr {
            address: address.to_string(),
            port,
        })
}

/// Build an unbound UDP transport (§15, §75).
pub fn build_udp(channel_id: ChannelId, config: &UdpConfig) -> Result<UdpTransport, BuildError> {
    let addr = socket_addr(&config.bind_address, config.port)?;
    let mut transport = UdpTransport::new(channel_id, addr, config.mode);
    if let Some(group) = &config.multicast_group {
        let ip = group
            .parse()
            .map_err(|_| BuildError::InvalidMulticastGroup(group.clone()))?;
        transport = transport.with_multicast_group(ip);
    }
    if let Some(iface) = &config.multicast_interface {
        let ip = iface
            .parse()
            .map_err(|_| BuildError::InvalidMulticastInterface(iface.clone()))?;
        transport = transport.with_multicast_interface(ip);
    }
    transport = transport.with_recv_buffer(
        config
            .recv_buffer_bytes
            .unwrap_or(crate::config::DEFAULT_RECV_BUFFER_BYTES),
    );
    if config.kernel_timestamps {
        transport = transport.with_kernel_timestamps();
    }
    if config.shared_port {
        transport = transport.with_shared_port();
    }
    Ok(transport)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ChannelId;

    fn serial(flow: CfgFlowControl, rts: Option<bool>, dtr: Option<bool>) -> SerialConfig {
        let crate::config::InterfaceConfig::Serial(mut config) =
            crate::config::templates::serial_template().interface
        else {
            unreachable!("the serial template is serial");
        };
        config.flow_control = flow;
        config.rts = rts;
        config.dtr = dtr;
        config
    }

    #[test]
    fn an_output_line_left_unset_is_set_low_at_open() {
        // Listener cannot read an output line back, so it shows what it set.
        // Setting both at open makes that true on every operating system.
        for flow in [CfgFlowControl::None, CfgFlowControl::XonXoff] {
            assert_eq!(
                lines_at_open(&serial(flow, None, None)),
                (Some(false), Some(false)),
                "{flow:?}"
            );
        }
    }

    #[test]
    fn rts_is_left_to_the_driver_under_rts_cts_flow_control() {
        assert_eq!(
            lines_at_open(&serial(CfgFlowControl::RtsCts, None, None)),
            (None, Some(false))
        );
    }

    #[test]
    fn a_profile_that_sets_a_line_keeps_its_choice() {
        assert_eq!(
            lines_at_open(&serial(CfgFlowControl::None, Some(true), Some(true))),
            (Some(true), Some(true))
        );
        assert_eq!(
            lines_at_open(&serial(CfgFlowControl::RtsCts, Some(true), None)),
            (Some(true), Some(false))
        );
    }

    #[test]
    fn unsupported_serial_parameters_are_rejected() {
        assert!(matches!(
            map_parity(CfgParity::Mark),
            Err(BuildError::UnsupportedParity)
        ));
        assert!(matches!(
            map_parity(CfgParity::Space),
            Err(BuildError::UnsupportedParity)
        ));
        assert!(matches!(
            map_stop_bits(CfgStopBits::OnePointFive),
            Err(BuildError::UnsupportedStopBits)
        ));
        // Supported ones map cleanly.
        assert!(map_parity(CfgParity::Even).is_ok());
        assert!(map_stop_bits(CfgStopBits::Two).is_ok());
    }

    #[test]
    fn udp_address_is_validated() {
        let good = UdpConfig {
            bind_address: "127.0.0.1".to_string(),
            port: 9000,
            mode: crate::transport::udp::UdpMode::Unicast,
            multicast_group: None,
            multicast_interface: None,
            recv_buffer_bytes: None,
            kernel_timestamps: false,
            shared_port: false,
        };
        assert!(build_udp(ChannelId::new(), &good).is_ok());

        let bad = UdpConfig {
            bind_address: "not-an-address".to_string(),
            port: 9000,
            mode: crate::transport::udp::UdpMode::Unicast,
            multicast_group: None,
            multicast_interface: None,
            recv_buffer_bytes: None,
            kernel_timestamps: false,
            shared_port: false,
        };
        assert!(matches!(
            build_udp(ChannelId::new(), &bad),
            Err(BuildError::InvalidSocketAddr { .. })
        ));
    }
}
