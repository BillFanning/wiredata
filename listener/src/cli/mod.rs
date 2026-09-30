//! CLI presentation layer (spec §3).
//!
//! A thin layer over the runtime [`Listener`]: it
//! turns arguments (or a profile) into channel configs, starts them, prints the
//! `RuntimeEvent` stream, and shuts down gracefully on Ctrl-C. It contains no
//! business logic — channel construction lives in `runtime`/`config` (§3, §128).

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Parser;

use crate::config::{templates, ChannelConfig, InterfaceConfig, Profile};
use crate::core::RuntimeEvent;
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
    crate::diagnostics::init_logging(); // §114; non-fatal if already installed (§117)
    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
    runtime.block_on(run_cli(cli))
}

async fn run_cli(cli: Cli) -> Result<()> {
    let configs = build_channel_configs(&cli)?;

    let mut listener = Listener::with_default_capacities();
    let mut events = listener
        .take_events()
        .expect("the event stream is available exactly once");

    let mut started = 0usize;
    for config in configs {
        let name = config.name.as_str().to_string();
        let id = listener.add_channel(config);
        match listener.start(id).await {
            Ok(()) => {
                println!("started \"{name}\" [{id}]");
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
    // app ticks it. Channels without reconnect enabled are unaffected.
    let mut reconnect = tokio::time::interval(std::time::Duration::from_millis(500));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = reconnect.tick() => listener.reconnect_tick().await,
            maybe = events.recv() => match maybe {
                Some(event) => println!("{}", format_event(&event)),
                None => break,
            },
        }
    }

    println!("stopping…");
    listener.shutdown().await;
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

fn format_event(event: &RuntimeEvent) -> String {
    match event {
        RuntimeEvent::ChannelStarted(id) => format!("[{id}] started"),
        RuntimeEvent::ChannelStopped(id) => format!("[{id}] stopped"),
        RuntimeEvent::ChannelFaulted(id) => format!("[{id}] FAULTED"),
        RuntimeEvent::RecordingFaulted(id, tap) => {
            format!("[{id}] {} recording faulted", tap.label())
        }
        RuntimeEvent::RecordingStarted(id, tap) => {
            format!("[{id}] {} recording started", tap.label())
        }
        RuntimeEvent::WarningRaised(id) => format!("[{id}] warning raised"),
        RuntimeEvent::ReceptionStalled(id, dur) => {
            format!("[{id}] reception stalled {} ms", dur.as_millis())
        }
        RuntimeEvent::TcpClientConnected(id) => format!("[{id}] TCP client connected"),
        RuntimeEvent::TcpClientDisconnected(id) => format!("[{id}] TCP client disconnected"),
        RuntimeEvent::ControlLinesChanged(id) => format!("[{id}] control lines changed"),
        RuntimeEvent::ChannelReconnecting(id, attempt) => {
            format!("[{id}] reconnecting (attempt {attempt})")
        }
        RuntimeEvent::ChannelReconnected(id) => format!("[{id}] reconnected"),
        RuntimeEvent::ChannelReconnectGaveUp(id) => format!("[{id}] reconnect gave up"),
        RuntimeEvent::DiskSpaceLow(id) => format!("[{id}] LOW DISK"),
        RuntimeEvent::RecordingStoppedLowDisk(id) => {
            format!("[{id}] recording stopped (low disk)")
        }
        RuntimeEvent::MatchTriggered(id, rule) => format!("[{id}] match rule {rule} fired"),
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
