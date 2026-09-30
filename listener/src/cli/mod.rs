//! CLI presentation layer (spec §3).
//!
//! A thin layer over the runtime [`Listener`]: it
//! turns arguments (or a profile) into channel configs, starts them, prints the
//! `RuntimeEvent` stream, and shuts down gracefully on Ctrl-C. It contains no
//! business logic — channel construction lives in `runtime`/`config` (§3, §128).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Parser;

use crate::config::{templates, ChannelConfig, InterfaceConfig, Profile};
use crate::core::{ChannelId, RuntimeEvent};
use crate::diagnostics::EventLogStatus;
use crate::runtime::Listener;

/// Receive and inspect byte-oriented data from serial and network sources.
#[derive(Parser, Debug)]
#[command(name = "listener", version, about)]
pub struct Cli {
    /// Load a workspace profile (TOML) and start all its valid channels.
    #[arg(long, value_name = "PATH")]
    profile: Option<PathBuf>,

    /// Quick start: receive UDP datagrams on this port (binds 0.0.0.0).
    #[arg(long, value_name = "PORT")]
    udp: Option<u16>,

    /// Quick start: accept TCP clients on this port (binds 0.0.0.0).
    #[arg(long, value_name = "PORT")]
    tcp: Option<u16>,

    /// Quick start: open this serial port (e.g. COM3 or /dev/ttyUSB0).
    #[arg(long, value_name = "PORT")]
    serial: Option<String>,

    /// Serial baud rate (used with --serial).
    #[arg(long, default_value_t = 9600)]
    baud: u32,

    /// Launch the graphical interface. Also the default for a bare invocation
    /// (no source given) — e.g. double-clicking the executable.
    #[arg(short = 'g', long)]
    pub gui: bool,

    /// Force the headless CLI runner, overriding the bare-invocation GUI default.
    /// (A source flag already implies headless; this is for the no-source case.)
    #[arg(long, conflicts_with = "gui")]
    pub cli: bool,
}

impl Cli {
    /// Whether any data source was requested on the command line.
    fn has_source(&self) -> bool {
        self.profile.is_some() || self.udp.is_some() || self.tcp.is_some() || self.serial.is_some()
    }

    /// Whether to launch the GUI (§3). Explicit `--gui` always wins; explicit
    /// `--cli` or any source flag selects headless; a bare invocation (no source,
    /// no flag) defaults to the GUI so a double-clicked executable opens a window.
    pub fn wants_gui(&self) -> bool {
        if self.gui {
            true
        } else if self.cli {
            false
        } else {
            !self.has_source()
        }
    }

    /// A bare launch — no UI flag and no source. The double-click case, where a GUI
    /// failure (e.g. a headless box with no display) should hint at headless mode
    /// rather than surface a cryptic windowing error.
    pub fn is_bare_launch(&self) -> bool {
        !self.gui && !self.cli && !self.has_source()
    }
}

/// Parse the process arguments into a [`Cli`]. Kept separate from [`run`] so the
/// entry point can branch to the GUI before building the async runtime.
pub fn parse() -> Cli {
    Cli::parse()
}

/// Headless CLI entry point: build a Tokio runtime and run the event loop over the
/// already-parsed arguments (§3). The GUI path is dispatched in [`crate::run`].
pub fn run(cli: Cli) -> Result<()> {
    // §114, §118; non-fatal (§117). Declared first so it is dropped last: the
    // event log drains and flushes after the runtime has stopped every Channel.
    let event_log = crate::diagnostics::init_logging();
    let status = event_log.status();
    match (status.folder(), status.problem()) {
        (_, Some(problem)) => eprintln!("WARNING: the event log is not being saved: {problem}"),
        (Some(folder), None) => println!("event log: {}", folder.display()),
        (None, None) => {}
    }
    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
    runtime.block_on(run_cli(cli, status))
}

async fn run_cli(cli: Cli, event_log: EventLogStatus) -> Result<()> {
    let configs = build_channel_configs(&cli)?;

    let mut listener = Listener::with_default_capacities();
    let mut events = listener
        .take_events()
        .expect("the event stream is available exactly once");

    let mut names = HashMap::new();
    let mut started = 0usize;
    for config in configs {
        let name = config.name.as_str().to_string();
        let id = listener.add_channel(config);
        names.insert(id, name.clone());
        match listener.start(id).await {
            Ok(()) => {
                println!("started \"{name}\"");
                started += 1;
            }
            Err(err) => eprintln!("could not start \"{name}\": {err}"),
        }
    }
    if started == 0 {
        bail!("no channels started");
    }

    println!("listening on {started} channel(s) — press Ctrl-C to stop");
    // Drive auto-reconnect (§162): the orchestrator has no background loop, so the
    // app ticks it. Channels without reconnect enabled are unaffected. The same
    // tick checks the event log, so a failure that starts mid-run is reported.
    let mut reconnect = tokio::time::interval(std::time::Duration::from_millis(500));
    let mut reported_problem = event_log.problem();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = reconnect.tick() => {
                listener.reconnect_tick().await;
                let problem = event_log.problem();
                if problem != reported_problem {
                    if let Some(problem) = &problem {
                        eprintln!("WARNING: the event log is not being saved: {problem}");
                    }
                    reported_problem = problem;
                }
            }
            maybe = events.recv() => match maybe {
                Some(event) => println!("{}", format_event(&event, &names)),
                None => break,
            },
        }
    }

    println!("stopping…");
    listener.shutdown().await;
    let lost = event_log.lost_entries();
    if lost > 0 {
        eprintln!("WARNING: {lost} event log lines were not saved; the log marks where");
    }
    Ok(())
}

/// Build the channels to run from the profile or the quick-start flags. Per §71,
/// invalid channels in a profile are reported and skipped, not fatal.
fn build_channel_configs(cli: &Cli) -> Result<Vec<ChannelConfig>> {
    if let Some(path) = &cli.profile {
        let profile =
            Profile::load(path).with_context(|| format!("loading profile {}", path.display()))?;
        let mut valid = Vec::new();
        for (config, (name, result)) in profile.channels.iter().zip(profile.validate()) {
            match result {
                Ok(()) => valid.push(config.clone()),
                Err(errors) => eprintln!("skipping invalid channel \"{name}\": {errors:?}"),
            }
        }
        return Ok(valid);
    }

    let config = if let Some(port) = cli.udp {
        let mut config = templates::udp_template();
        if let InterfaceConfig::Udp(udp) = &mut config.interface {
            udp.port = port;
        }
        config
    } else if let Some(port) = cli.tcp {
        let mut config = templates::tcp_listener_template();
        if let InterfaceConfig::TcpListener(tcp) = &mut config.interface {
            tcp.port = port;
        }
        config
    } else if let Some(port) = &cli.serial {
        let mut config = templates::serial_template();
        if let InterfaceConfig::Serial(serial) = &mut config.interface {
            serial.port = port.clone();
            serial.baud_rate = cli.baud;
        }
        config
    } else {
        bail!("specify --profile, --udp, --tcp, or --serial (see --help)");
    };

    Ok(vec![config])
}

/// One line for a runtime event, naming the Channel (§3.1). An id with no name
/// (a TCP connection) is shown as its UUID.
fn format_event(event: &RuntimeEvent, names: &HashMap<ChannelId, String>) -> String {
    let label = |id: &ChannelId| names.get(id).cloned().unwrap_or_else(|| id.to_string());
    match event {
        RuntimeEvent::ChannelStarted(id) => format!("[{}] started", label(id)),
        RuntimeEvent::ChannelStopped(id) => format!("[{}] stopped", label(id)),
        RuntimeEvent::ChannelFaulted(id) => format!("[{}] FAULTED", label(id)),
        RuntimeEvent::RecordingFaulted(id, tap) => {
            format!("[{}] {} recording faulted", label(id), tap.label())
        }
        RuntimeEvent::RecordingStarted(id, tap) => {
            format!("[{}] {} recording started", label(id), tap.label())
        }
        RuntimeEvent::WarningRaised(id) => format!("[{}] warning raised", label(id)),
        RuntimeEvent::ReceptionStalled(id, dur) => {
            format!("[{}] reception stalled {} ms", label(id), dur.as_millis())
        }
        RuntimeEvent::TcpClientConnected(id) => format!("[{}] TCP client connected", label(id)),
        RuntimeEvent::TcpClientDisconnected(id) => {
            format!("[{}] TCP client disconnected", label(id))
        }
        RuntimeEvent::ControlLinesChanged(id) => format!("[{}] control lines changed", label(id)),
        RuntimeEvent::ChannelReconnecting(id, attempt) => {
            format!("[{}] reconnecting (attempt {attempt})", label(id))
        }
        RuntimeEvent::ChannelReconnected(id) => format!("[{}] reconnected", label(id)),
        RuntimeEvent::ChannelReconnectGaveUp(id) => format!("[{}] reconnect gave up", label(id)),
        RuntimeEvent::DiskSpaceLow(id) => format!("[{}] LOW DISK", label(id)),
        RuntimeEvent::RecordingStoppedLowDisk(id) => {
            format!("[{}] recording stopped (low disk)", label(id))
        }
        RuntimeEvent::MatchTriggered(id, rule) => {
            format!("[{}] match rule {rule} fired", label(id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_cli() -> Cli {
        Cli {
            profile: None,
            udp: None,
            tcp: None,
            serial: None,
            baud: 9600,
            gui: false,
            cli: false,
        }
    }

    #[test]
    fn bare_invocation_defaults_to_the_gui() {
        let cli = base_cli(); // no source, no flag
        assert!(cli.wants_gui());
        assert!(cli.is_bare_launch());
    }

    #[test]
    fn a_source_flag_selects_headless() {
        let cli = Cli {
            udp: Some(9000),
            ..base_cli()
        };
        assert!(!cli.wants_gui());
        assert!(!cli.is_bare_launch());
    }

    #[test]
    fn explicit_flags_override_the_default() {
        // --gui with no source → GUI (and not a "bare" launch needing the hint).
        let gui = Cli {
            gui: true,
            ..base_cli()
        };
        assert!(gui.wants_gui());
        assert!(!gui.is_bare_launch());

        // --cli with no source → headless (the user asked for it).
        let headless = Cli {
            cli: true,
            ..base_cli()
        };
        assert!(!headless.wants_gui());
        assert!(!headless.is_bare_launch());

        // --gui still wins even with a source present.
        let gui_with_source = Cli {
            gui: true,
            udp: Some(9000),
            ..base_cli()
        };
        assert!(gui_with_source.wants_gui());
    }

    #[test]
    fn udp_quick_start_sets_the_port() {
        let cli = Cli {
            udp: Some(9000),
            ..base_cli()
        };
        let configs = build_channel_configs(&cli).unwrap();
        assert_eq!(configs.len(), 1);
        match &configs[0].interface {
            InterfaceConfig::Udp(udp) => assert_eq!(udp.port, 9000),
            other => panic!("expected UDP, got {other:?}"),
        }
    }

    #[test]
    fn serial_quick_start_sets_port_and_baud() {
        let cli = Cli {
            serial: Some("COM7".to_string()),
            baud: 4800,
            ..base_cli()
        };
        let configs = build_channel_configs(&cli).unwrap();
        match &configs[0].interface {
            InterfaceConfig::Serial(serial) => {
                assert_eq!(serial.port, "COM7");
                assert_eq!(serial.baud_rate, 4800);
            }
            other => panic!("expected serial, got {other:?}"),
        }
    }

    #[test]
    fn events_name_the_channel_rather_than_its_uuid() {
        // §3.1: output names Channels by name. An id with no name (a TCP
        // connection) falls back to the UUID.
        let id = crate::core::ChannelId::new();
        let names = std::collections::HashMap::from([(id, "GPS feed".to_owned())]);
        assert_eq!(
            format_event(&RuntimeEvent::ChannelFaulted(id), &names),
            "[GPS feed] FAULTED"
        );
        let unnamed = crate::core::ChannelId::new();
        assert_eq!(
            format_event(&RuntimeEvent::ChannelStarted(unnamed), &names),
            format!("[{unnamed}] started")
        );
    }

    #[test]
    fn no_source_is_an_error() {
        assert!(build_channel_configs(&base_cli()).is_err());
    }

    #[test]
    fn profile_loads_valid_channels_and_skips_invalid() {
        let mut path = std::env::temp_dir();
        path.push(format!("listener-cli-{}.toml", uuid::Uuid::new_v4()));

        let mut profile = Profile::new("test workspace");
        let mut good = templates::udp_template();
        good.name = crate::core::ChannelName::new("Good");
        let mut bad = templates::udp_template();
        bad.name = crate::core::ChannelName::new("Bad"); // unique names (§6) — isolate the
        bad.retention = crate::config::RetentionConfig::default(); // all-None → invalid (§80)
        profile.channels = vec![good, bad];
        profile.save(&path).unwrap();

        let cli = Cli {
            profile: Some(path.clone()),
            ..base_cli()
        };
        let configs = build_channel_configs(&cli).unwrap();
        // The valid channel loads; the unbounded-retention one is skipped.
        assert_eq!(configs.len(), 1);

        let _ = std::fs::remove_file(&path);
    }
}
