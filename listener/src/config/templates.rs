//! Default channel templates (spec §81–§85).
//!
//! Templates provide safe, valid, *stopped* starting configurations (§81). They
//! never open interfaces. Resource-bearing fields (serial port name, UDP
//! port) are placeholders the user fills in before Start (§71).

use crate::core::{ChannelKind, ChannelName};
use crate::transport::udp::UdpMode;

use super::schema::*;

/// A bounded default retention so templates validate (§80). 64 KB — matches the GUI's
/// default scroll buffer (`gui::view_prefs::DEFAULT_SCROLL_BUFFER_BYTES`) and sits
/// within its 256 KB max, so a fresh channel's Scroll buffer reads 64 kB.
fn default_retention() -> RetentionConfig {
    RetentionConfig::with_byte_limit(64 * 1024)
}

/// Generic serial channel (§82): 9600 8N1, Raw display.
pub fn serial_template() -> ChannelConfig {
    ChannelConfig {
        id: None,
        name: ChannelName::new("Serial_Channel"),
        kind: ChannelKind::Serial,
        interface: InterfaceConfig::Serial(SerialConfig {
            port: String::new(),
            baud_rate: 9600,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow_control: FlowControl::None,
            rts: None,
            dtr: None,
        }),
        display: DisplayConfig::default(),
        raw_recording: RawRecordingConfig::default(),
        display_recording: DisplayRecordingConfig::default(),
        retention: default_retention(),
        reconnect: ReconnectPolicy::default(),
        match_rules: Vec::new(),
    }
}

/// UDP channel (§84): bind 0.0.0.0, unicast; datagrams append to the verbatim stream
/// (boundaries are a reception detail only, §18). Raw display.
pub fn udp_template() -> ChannelConfig {
    ChannelConfig {
        id: None,
        name: ChannelName::new("UDP_Channel"),
        kind: ChannelKind::Udp,
        interface: InterfaceConfig::Udp(UdpConfig {
            bind_address: "0.0.0.0".to_string(),
            port: 0,
            mode: UdpMode::Unicast,
            multicast_group: None,
            multicast_interface: None,
            recv_buffer_bytes: None,
            kernel_timestamps: false,
            shared_port: false,
        }),
        display: DisplayConfig::default(),
        raw_recording: RawRecordingConfig::default(),
        display_recording: DisplayRecordingConfig::default(),
        retention: default_retention(),
        reconnect: ReconnectPolicy::default(),
        match_rules: Vec::new(),
    }
}
