use anyhow::Context;

use super::config::{DataBits, FlowControl, InterfaceConfig, Parity, SerialConfig, StopBits};
use super::{Interface, MissingRetryConfiguration};

pub(super) struct SerialInterface {
    // `None` only between a failed handle being dropped and a later retry
    // successfully reopening the configured port.
    port: Option<Box<dyn serialport::SerialPort>>,
    /// The last write showed that this OS handle cannot be trusted again.
    /// Ordinary timeouts keep the handle: flow control can stall one write and
    /// then recover without a USB device having disappeared. The classifier
    /// still recognizes Windows' timeout-shaped aborted-write unplug error.
    reopen_required: bool,
}

impl SerialInterface {
    pub(super) fn open(config: &SerialConfig) -> anyhow::Result<Self> {
        Ok(Self {
            port: Some(open_port(config)?),
            reopen_required: false,
        })
    }
}

fn open_port(config: &SerialConfig) -> anyhow::Result<Box<dyn serialport::SerialPort>> {
    serialport::new(&config.port, config.baud_rate)
        .data_bits(to_sp_data_bits(config.data_bits))
        .parity(to_sp_parity(config.parity))
        .stop_bits(to_sp_stop_bits(config.stop_bits))
        .flow_control(to_sp_flow_control(config.flow_control))
        .timeout(std::time::Duration::from_secs(1))
        .open()
        .map_err(|error| describe_open_failure(&config.port, error))
        // Names the port. Why a port that appears in the list can still fail to
        // open is the UI's to explain (`serial_port_hint`) — only it knows what
        // is currently enumerated, and saying it here too put the same sentence
        // on screen twice.
        .with_context(|| format!("opening serial port {:?}", config.port))
}

/// Say what a failed open means for a serial port, not for a file.
///
/// The OS is reported verbatim for everything it describes in the reader's own
/// terms — permission denied, device busy — because it usually knows more than
/// we do. The one case it does not is absence: Windows opens a COM port through
/// the filesystem namespace, so a port that is not there can read "The system
/// cannot find the file specified." A technician looking at COM4 has no file in
/// mind and is left hunting for one.
///
/// `ErrorKind::NoDevice` is not narrow enough for this decision: serialport also
/// maps Windows access denied and POSIX busy/lock failures to it. Translate only
/// the known Windows file/path wording; an unfamiliar or localized message is
/// safer preserved than guessed at.
fn describe_open_failure(port: &str, error: serialport::Error) -> anyhow::Error {
    let description = error.to_string();
    let windows_not_found = [
        "The system cannot find the file specified",
        "The system cannot find the path specified",
    ];
    if error.kind() == serialport::ErrorKind::NoDevice
        && windows_not_found.iter().any(|known| {
            description
                .trim_end_matches('.')
                .eq_ignore_ascii_case(known)
        })
    {
        anyhow::anyhow!("no device is present at {port}")
    } else {
        anyhow::Error::new(error)
    }
}

impl Interface for SerialInterface {
    fn send(&mut self, data: &[u8]) -> anyhow::Result<()> {
        use std::io::Write;
        let result = self
            .port
            .as_mut()
            .context("serial port is not open")?
            .write_all(data);
        if let Err(error) = result {
            self.reopen_required = write_error_requires_reopen(&error);
            return Err(error).context("writing to serial port");
        }
        Ok(())
    }

    fn prepare_retry(&mut self, current: Option<&InterfaceConfig>) -> anyhow::Result<bool> {
        // A timeout-shaped flow-control stall retries this handle as-is and
        // needs no configuration. Check that before enforcing the stronger
        // replacement invariant below.
        if !self.reopen_required {
            return Ok(false);
        }
        let Some(InterfaceConfig::Serial(config)) = current else {
            return Err(MissingRetryConfiguration.into());
        };

        // A write classified as requiring reopen makes this handle untrusted.
        // Serial ports are exclusive, so release it before trying to open the
        // configured name again. This is essential when Windows re-enumerates
        // a returned USB serial device behind a new OS handle. Leave `None` on
        // failure so every later backoff retry performs another real open.
        drop(self.port.take());
        self.port = Some(open_port(config)?);
        self.reopen_required = false;
        Ok(true)
    }

    fn reconfigure(
        &mut self,
        current: &InterfaceConfig,
        next: &InterfaceConfig,
    ) -> anyhow::Result<bool> {
        let (InterfaceConfig::Serial(current), InterfaceConfig::Serial(next)) = (current, next)
        else {
            return Ok(false);
        };

        if self.reopen_required {
            // A successful edit can heal the same failed handle immediately.
            // Drop first because serial ports are exclusive; if the new open
            // fails, ordinary recovery continues with the still-confirmed old
            // settings and no untrusted handle.
            drop(self.port.take());
            self.port = Some(open_port(next)?);
            self.reopen_required = false;
            return Ok(true);
        }
        if !can_reconfigure_in_place(current, next) {
            return Ok(false);
        }

        let Some(port) = self.port.as_mut() else {
            // The handle was released after a send failure. Let the runner's
            // ordinary replacement-open path apply this edit atomically.
            return Ok(false);
        };

        if let Err(change_err) = apply_config(port.as_mut(), next) {
            let rollback = apply_config(port.as_mut(), current);
            return match rollback {
                Ok(()) => {
                    Err(change_err
                        .context("applying serial settings; previous settings were restored"))
                }
                Err(rollback_err) => Err(change_err.context(format!(
                    "applying serial settings; restoring the previous settings also failed: \
                     {rollback_err:#}"
                ))),
            };
        }
        Ok(true)
    }
}

fn write_error_requires_reopen(error: &std::io::Error) -> bool {
    // Windows maps ERROR_OPERATION_ABORTED to `TimedOut`, but USB serial
    // drivers commonly return it when an unplug aborts `WriteFile`. Preserve
    // the raw code before exempting ordinary timeouts or a zero-byte write
    // caused by flow control.
    const WINDOWS_ERROR_OPERATION_ABORTED: i32 = 995;
    if error.raw_os_error() == Some(WINDOWS_ERROR_OPERATION_ABORTED) {
        return true;
    }
    // Removal-shaped errors vary by driver and OS, and many collapse into an
    // unclassified/Other kind. Keep only the observed transient cases below;
    // an unfamiliar error leaves a handle that just failed untrusted.
    !matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WriteZero
    )
}

fn apply_config(
    port: &mut dyn serialport::SerialPort,
    config: &SerialConfig,
) -> anyhow::Result<()> {
    port.set_baud_rate(config.baud_rate)
        .context("setting serial baud rate")?;
    port.set_data_bits(to_sp_data_bits(config.data_bits))
        .context("setting serial data bits")?;
    port.set_parity(to_sp_parity(config.parity))
        .context("setting serial parity")?;
    port.set_stop_bits(to_sp_stop_bits(config.stop_bits))
        .context("setting serial stop bits")?;
    port.set_flow_control(to_sp_flow_control(config.flow_control))
        .context("setting serial flow control")?;
    Ok(())
}

fn can_reconfigure_in_place(current: &SerialConfig, next: &SerialConfig) -> bool {
    current.port == next.port
}

fn to_sp_data_bits(d: DataBits) -> serialport::DataBits {
    match d {
        DataBits::Five => serialport::DataBits::Five,
        DataBits::Six => serialport::DataBits::Six,
        DataBits::Seven => serialport::DataBits::Seven,
        DataBits::Eight => serialport::DataBits::Eight,
    }
}

fn to_sp_parity(p: Parity) -> serialport::Parity {
    match p {
        Parity::None => serialport::Parity::None,
        Parity::Odd => serialport::Parity::Odd,
        Parity::Even => serialport::Parity::Even,
    }
}

fn to_sp_stop_bits(s: StopBits) -> serialport::StopBits {
    match s {
        StopBits::One => serialport::StopBits::One,
        StopBits::Two => serialport::StopBits::Two,
    }
}

fn to_sp_flow_control(f: FlowControl) -> serialport::FlowControl {
    match f {
        FlowControl::None => serialport::FlowControl::None,
        FlowControl::Software => serialport::FlowControl::Software,
        FlowControl::Hardware => serialport::FlowControl::Hardware,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_serial_port_is_described_as_a_device() {
        let error = serialport::Error::new(
            serialport::ErrorKind::NoDevice,
            "The system cannot find the file specified.",
        );

        assert_eq!(
            describe_open_failure("COM4", error).to_string(),
            "no device is present at COM4"
        );
    }

    #[test]
    fn other_serial_open_errors_keep_their_system_detail() {
        // The crate maps access denied to NoDevice on Windows, so the kind
        // alone must never be treated as proof that the port is absent.
        let access_denied =
            serialport::Error::new(serialport::ErrorKind::NoDevice, "Access is denied.");
        assert_eq!(
            describe_open_failure("COM4", access_denied).to_string(),
            "Access is denied."
        );

        let error = serialport::Error::new(serialport::ErrorKind::Unknown, "device is busy");

        assert_eq!(
            describe_open_failure("COM4", error).to_string(),
            "device is busy"
        );
    }

    #[test]
    fn device_errors_reopen_but_flow_control_timeouts_keep_the_handle() {
        assert!(write_error_requires_reopen(&std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Access is denied"
        )));
        assert!(write_error_requires_reopen(&std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "device disconnected"
        )));
        assert!(write_error_requires_reopen(&std::io::Error::other(
            "unclassified driver error"
        )));
        assert!(!write_error_requires_reopen(&std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "CTS remained low"
        )));
        assert!(!write_error_requires_reopen(&std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "try again"
        )));
        assert!(!write_error_requires_reopen(&std::io::Error::new(
            std::io::ErrorKind::WriteZero,
            "flow-controlled write returned no bytes"
        )));

        let unplug_aborted_write = std::io::Error::from_raw_os_error(995);
        #[cfg(windows)]
        assert_eq!(
            unplug_aborted_write.kind(),
            std::io::ErrorKind::TimedOut,
            "Rust's Windows mapping is why raw error 995 needs precedence"
        );
        assert!(write_error_requires_reopen(&unplug_aborted_write));

        #[cfg(windows)]
        for code in [6, 31, 1167] {
            assert!(
                write_error_requires_reopen(&std::io::Error::from_raw_os_error(code)),
                "Windows device-removal-shaped error {code} must replace the handle"
            );
        }
    }

    #[test]
    fn a_transient_retry_does_not_require_reopen_configuration() {
        let mut interface = SerialInterface {
            port: None,
            reopen_required: false,
        };

        assert!(!interface
            .prepare_retry(None)
            .expect("the existing handle needs no configuration to be retried"));
    }

    #[test]
    fn replacing_a_handle_requires_confirmed_serial_configuration() {
        let mut interface = SerialInterface {
            port: None,
            reopen_required: true,
        };

        let error = interface.prepare_retry(None).unwrap_err();
        assert!(error.is::<MissingRetryConfiguration>());
    }

    #[test]
    fn open_nonexistent_port_returns_error() {
        let config = SerialConfig::new("/dev/does_not_exist_xyz");
        let result = SerialInterface::open(&config);
        let msg = result
            .err()
            .expect("expected error opening nonexistent port")
            .to_string();
        assert!(msg.contains("does_not_exist_xyz"));
    }

    #[test]
    fn only_the_same_serial_port_can_reconfigure_in_place() {
        let current = SerialConfig::new("COM1");
        let mut same_port = current.clone();
        same_port.baud_rate = 115_200;
        assert!(can_reconfigure_in_place(&current, &same_port));
        assert!(!can_reconfigure_in_place(
            &current,
            &SerialConfig::new("COM2")
        ));
    }

    #[test]
    fn data_bits_conversion_covers_all_variants() {
        assert!(matches!(
            to_sp_data_bits(DataBits::Five),
            serialport::DataBits::Five
        ));
        assert!(matches!(
            to_sp_data_bits(DataBits::Six),
            serialport::DataBits::Six
        ));
        assert!(matches!(
            to_sp_data_bits(DataBits::Seven),
            serialport::DataBits::Seven
        ));
        assert!(matches!(
            to_sp_data_bits(DataBits::Eight),
            serialport::DataBits::Eight
        ));
    }

    #[test]
    fn parity_conversion_covers_all_variants() {
        assert!(matches!(
            to_sp_parity(Parity::None),
            serialport::Parity::None
        ));
        assert!(matches!(to_sp_parity(Parity::Odd), serialport::Parity::Odd));
        assert!(matches!(
            to_sp_parity(Parity::Even),
            serialport::Parity::Even
        ));
    }

    #[test]
    fn stop_bits_conversion_covers_all_variants() {
        assert!(matches!(
            to_sp_stop_bits(StopBits::One),
            serialport::StopBits::One
        ));
        assert!(matches!(
            to_sp_stop_bits(StopBits::Two),
            serialport::StopBits::Two
        ));
    }

    #[test]
    fn flow_control_conversion_covers_all_variants() {
        assert!(matches!(
            to_sp_flow_control(FlowControl::None),
            serialport::FlowControl::None
        ));
        assert!(matches!(
            to_sp_flow_control(FlowControl::Software),
            serialport::FlowControl::Software
        ));
        assert!(matches!(
            to_sp_flow_control(FlowControl::Hardware),
            serialport::FlowControl::Hardware
        ));
    }
}
