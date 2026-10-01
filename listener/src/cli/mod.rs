//! CLI presentation layer (spec §3).
//!
//! A thin layer over the runtime [`Listener`]: it
//! turns arguments (or a profile) into channel configs, starts what can start,
//! says loudly what is down (§3.1), prints the `RuntimeEvent` stream, and shuts
//! down gracefully on every OS stop request. It contains no business logic —
//! channel construction lives in `runtime`/`config` (§3, §128).

mod health;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::Parser;

use crate::config::{templates, ChannelConfig, InterfaceConfig, Profile};
use crate::core::{ChannelId, ChannelState, RuntimeEvent};
use crate::diagnostics::EventLogStatus;
use crate::runtime::{Listener, ShutdownOutcome, RUNTIME_SHUTDOWN_LIMIT};
use health::HealthWatch;
use wiredata_stop::StopRequests;

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

    /// Stop and exit with code 2 unless every channel starts. Without it, a
    /// channel that fails to start is retried under its reconnect policy
    /// while the others run.
    #[arg(long)]
    require_all: bool,

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

/// Exit codes (§3.1, talker ADR-060). 0 is healthy, or every outage
/// recovered; 1 is an internal error, which `main` reports.
const EXIT_CANNOT_START: u8 = 2;
const EXIT_DEGRADED: u8 = 3;
const EXIT_FINALIZATION_INCOMPLETE: u8 = 4;

/// The exit code for how the run ended (§3.1). Finalization incomplete wins
/// over degraded: it is the one that says recorded data may be missing.
fn exit_status(outcome: &ShutdownOutcome, degraded: bool) -> u8 {
    if !outcome.is_complete() {
        EXIT_FINALIZATION_INCOMPLETE
    } else if degraded {
        EXIT_DEGRADED
    } else {
        0
    }
}

/// Headless CLI entry point: build a Tokio runtime and run the event loop over the
/// already-parsed arguments (§3). The GUI path is dispatched in [`crate::run`].
pub fn run(cli: Cli) -> Result<ExitCode> {
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
    // Every OS stop request gets the graceful stop (§3.1). Listening starts
    // before any Channel, so a request during the starts is still graceful.
    let mut stop_requests = {
        let _runtime = runtime.enter();
        wiredata_stop::listen()
    };
    for problem in stop_requests.problems() {
        eprintln!("WARNING: {problem}");
    }
    let result = runtime.block_on(run_cli(cli, status, &mut stop_requests));
    // A file operation stuck in the blocking pool would otherwise keep the
    // process alive after every Channel has been stopped (§113).
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_LIMIT);
    // Only once the event log is flushed may a Windows logoff or shutdown that
    // is waiting on the stop go ahead: it may end the process straight away.
    drop(event_log);
    stop_requests.finish();
    result
}

async fn run_cli(
    cli: Cli,
    event_log: EventLogStatus,
    stop_requests: &mut StopRequests,
) -> Result<ExitCode> {
    let configs = match build_channel_configs(&cli) {
        Ok(configs) if configs.is_empty() => {
            eprintln!("ERROR: the profile has no valid channels");
            return Ok(ExitCode::from(EXIT_CANNOT_START));
        }
        Ok(configs) => configs,
        Err(err) => {
            eprintln!("ERROR: {}", describe_error(&err));
            return Ok(ExitCode::from(EXIT_CANNOT_START));
        }
    };

    let mut listener = Listener::with_default_capacities();
    let mut events = listener
        .take_events()
        .expect("the event stream is available exactly once");

    let mut health = HealthWatch::default();
    let (run, stop_early) =
        start_channels(&mut listener, configs, cli.require_all, &mut health).await;
    if let Some(why) = stop_early {
        eprintln!("ERROR: {why} — stopping");
        listener.shutdown().await;
        return Ok(ExitCode::from(EXIT_CANNOT_START));
    }
    let StartedRun {
        names,
        ids,
        running,
    } = run;
    println!(
        "listening on {running} of {} channel(s) — press Ctrl-C to stop",
        ids.len()
    );
    // Drive auto-reconnect (§162): the orchestrator has no background loop, so the
    // app ticks it. Channels without reconnect enabled are unaffected. The same
    // tick checks the event log, so a failure that starts mid-run is reported.
    let mut reconnect = tokio::time::interval(std::time::Duration::from_millis(500));
    let mut reported_problem = event_log.problem();
    let mut stop_reason = "the event stream closed".to_owned();
    loop {
        tokio::select! {
            request = stop_requests.recv() => {
                stop_reason = request
                    .map_or_else(|| "the stop listeners ended".to_owned(), |r| r.to_string());
                break;
            }
            _ = reconnect.tick() => {
                listener.reconnect_tick().await;
                observe_health(&listener, &mut health, &ids, &names).await;
                let problem = event_log.problem();
                if problem != reported_problem {
                    if let Some(problem) = &problem {
                        eprintln!("WARNING: the event log is not being saved: {problem}");
                    }
                    reported_problem = problem;
                }
            }
            maybe = events.recv() => match maybe {
                Some(event) => {
                    if let RuntimeEvent::RecordingFaulted(id, tap) = event {
                        health.recording_fault(id, tap);
                    }
                    if let Some(line) = format_event(&event, &names) {
                        println!("{line}");
                    }
                }
                None => break,
            },
        }
    }

    println!("stopping ({stop_reason})…");
    let outcome = listener.shutdown().await;
    if !outcome.is_complete() {
        eprintln!(
            "WARNING: finalization incomplete for {}: the end of their recordings may be \
             missing (see the event log)",
            outcome
                .incomplete
                .iter()
                .map(|name| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!("stopped");
    println!("summary:");
    for line in health.summary(Instant::now()) {
        println!("  {line}");
    }
    let lost = event_log.lost_entries();
    if lost > 0 {
        eprintln!("WARNING: {lost} event log lines were not saved; the log marks where");
    }
    Ok(ExitCode::from(exit_status(&outcome, health.degraded())))
}

/// The Channels of a run once their first start has been tried.
struct StartedRun {
    names: HashMap<ChannelId, String>,
    /// In start order, for the health lines and the summary.
    ids: Vec<ChannelId>,
    /// How many started on the first try.
    running: usize,
}

/// Start what can start (§3.1): a Channel that fails to start is reported and
/// left to its reconnect policy while the others run. Also returns why the run
/// cannot go on, when it cannot: `--require-all` with a Channel down, or
/// nothing running and nothing that will retry (exit code 2).
async fn start_channels(
    listener: &mut Listener,
    configs: Vec<ChannelConfig>,
    require_all: bool,
    health: &mut HealthWatch,
) -> (StartedRun, Option<&'static str>) {
    let mut run = StartedRun {
        names: HashMap::new(),
        ids: Vec::new(),
        running: 0,
    };
    for config in configs {
        if let Some(warning) = reconnect_off_warning(&config) {
            eprintln!("{warning}");
        }
        let name = config.name.as_str().to_string();
        let retries = config.reconnect.enabled;
        let id = listener.add_channel(config);
        run.names.insert(id, name.clone());
        run.ids.push(id);
        health.add(id, &name, retries);
        match listener.start(id).await {
            Ok(()) => {
                println!("started \"{name}\"");
                health.up(id, Instant::now());
                run.running += 1;
            }
            Err(err) => {
                if let Some(line) = health.down(id, &err.to_string(), Instant::now()) {
                    eprintln!("{line}");
                }
            }
        }
    }
    let stop = if require_all && run.running < run.ids.len() {
        Some("not every channel started, and --require-all is set")
    } else if health.nothing_can_run() {
        Some("no channel started, and none will retry")
    } else {
        None
    };
    (run, stop)
}

/// The warning for a recording Channel whose reconnect is off (§3.1): if its
/// device drops, the recording stops for the rest of the run.
fn reconnect_off_warning(config: &ChannelConfig) -> Option<String> {
    let records = config.raw_recording.enabled || config.display_recording.enabled;
    (records && !config.reconnect.enabled).then(|| {
        format!(
            "WARNING: [{}] records with reconnect off: if its device drops, nothing is \
             recorded until the run is restarted",
            config.name.as_str()
        )
    })
}

/// An error with its root cause, each stated once: an I/O error, for one,
/// often repeats its cause in its own message.
fn describe_error(err: &anyhow::Error) -> String {
    let top = err.to_string();
    let root = err.root_cause().to_string();
    if top == root || top.ends_with(&root) {
        top
    } else {
        format!("{top}: {root}")
    }
}

/// Bring the health watch up to date with the runtime (§3.1), printing what
/// changed: a Channel going down or recovering, retries running out, and the
/// five-minute reminder while any is down. Polled rather than driven by
/// events, so a dropped event cannot leave a Channel's health wrong.
async fn observe_health(
    listener: &Listener,
    health: &mut HealthWatch,
    ids: &[ChannelId],
    names: &HashMap<ChannelId, String>,
) {
    let now = Instant::now();
    for &id in ids {
        let line = match listener.state(id) {
            Some(ChannelState::Running) => health.up(id, now),
            Some(ChannelState::Faulted) => {
                let reason = latest_error(listener, id, &names[&id]).await;
                let down = health.down(id, &reason, now);
                if down.is_some() || !listener.reconnect_exhausted(id) {
                    down
                } else {
                    health.gave_up(id)
                }
            }
            // Starting and Stopping are passing through.
            _ => None,
        };
        if let Some(line) = line {
            eprintln!("{line}");
        }
    }
    for line in health.reminders(now) {
        eprintln!("{line}");
    }
}

/// Why a Channel is down: the newest error in its diagnostics, without the
/// Channel-name prefix a start fault carries.
async fn latest_error(listener: &Listener, id: ChannelId, name: &str) -> String {
    let message = listener
        .snapshot(id)
        .await
        .and_then(|snap| snap.diagnostics.errors.last().map(|d| d.message.clone()));
    match message {
        Some(message) => message
            .strip_prefix(&format!("{name}: "))
            .map_or_else(|| message.clone(), str::to_owned),
        None => "see the event log".to_owned(),
    }
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
/// (a TCP connection) is shown as its UUID. Lifecycle events give no line: the
/// health watch reports a Channel going down, its reminders and its recovery,
/// instead of a line for every retry.
fn format_event(event: &RuntimeEvent, names: &HashMap<ChannelId, String>) -> Option<String> {
    let label = |id: &ChannelId| names.get(id).cloned().unwrap_or_else(|| id.to_string());
    Some(match event {
        RuntimeEvent::ChannelStarted(_)
        | RuntimeEvent::ChannelStopped(_)
        | RuntimeEvent::ChannelFaulted(_)
        | RuntimeEvent::ChannelReconnecting(..)
        | RuntimeEvent::ChannelReconnected(_)
        | RuntimeEvent::ChannelReconnectGaveUp(_) => return None,
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
        RuntimeEvent::DiskSpaceLow(id) => format!("[{}] LOW DISK", label(id)),
        RuntimeEvent::RecordingStoppedLowDisk(id) => {
            format!("[{}] recording stopped (low disk)", label(id))
        }
        RuntimeEvent::MatchTriggered(id, rule) => {
            format!("[{}] match rule {rule} fired", label(id))
        }
    })
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
            require_all: false,
            gui: false,
            cli: false,
        }
    }

    /// A UDP channel on a loopback port held by another socket, so its start
    /// fails — the stand-in for an adapter that has not enumerated yet.
    fn blocked_udp_channel(name: &str, retries: bool) -> (ChannelConfig, std::net::UdpSocket) {
        let port = crate::test_ports::reserve_udp_port();
        let blocker = std::net::UdpSocket::bind(("127.0.0.1", port)).unwrap();
        let mut config = templates::udp_template();
        config.name = crate::core::ChannelName::new(name);
        config.reconnect.enabled = retries;
        if let InterfaceConfig::Udp(udp) = &mut config.interface {
            udp.bind_address = "127.0.0.1".to_string();
            udp.port = port;
        }
        (config, blocker)
    }

    fn free_udp_channel(name: &str) -> ChannelConfig {
        let mut config = templates::udp_template();
        config.name = crate::core::ChannelName::new(name);
        if let InterfaceConfig::Udp(udp) = &mut config.interface {
            udp.bind_address = "127.0.0.1".to_string();
            udp.port = crate::test_ports::reserve_udp_port();
        }
        config
    }

    #[tokio::test]
    async fn a_channel_that_will_retry_keeps_the_run_going() {
        // §3.1: start what can start. One channel down but retrying is no
        // reason to stop; the health lines say what is down.
        let mut listener = Listener::with_default_capacities();
        let _events = listener.take_events();
        let (late, _blocker) = blocked_udp_channel("GPS", true);
        let mut health = HealthWatch::default();
        let (run, stop) = start_channels(
            &mut listener,
            vec![late, free_udp_channel("AIS")],
            false,
            &mut health,
        )
        .await;
        assert_eq!(stop, None);
        assert_eq!(run.running, 1);
        assert!(health.degraded(), "until GPS starts");
        listener.shutdown().await;
    }

    #[tokio::test]
    async fn nothing_running_and_nothing_retrying_stops_the_run() {
        let mut listener = Listener::with_default_capacities();
        let _events = listener.take_events();
        let (down, _blocker) = blocked_udp_channel("GPS", false);
        let mut health = HealthWatch::default();
        let (_, stop) = start_channels(&mut listener, vec![down], false, &mut health).await;
        assert_eq!(stop, Some("no channel started, and none will retry"));
        listener.shutdown().await;
    }

    #[tokio::test]
    async fn require_all_stops_the_run_when_any_channel_is_down() {
        let mut listener = Listener::with_default_capacities();
        let _events = listener.take_events();
        let (late, _blocker) = blocked_udp_channel("GPS", true);
        let mut health = HealthWatch::default();
        let (_, stop) = start_channels(
            &mut listener,
            vec![late, free_udp_channel("AIS")],
            true,
            &mut health,
        )
        .await;
        assert_eq!(
            stop,
            Some("not every channel started, and --require-all is set")
        );
        listener.shutdown().await;
    }

    #[test]
    fn a_recording_channel_with_reconnect_off_is_warned_about_at_start() {
        let mut config = templates::udp_template();
        config.name = crate::core::ChannelName::new("GPS");
        config.reconnect.enabled = false;
        assert_eq!(reconnect_off_warning(&config), None, "it does not record");
        config.raw_recording.enabled = true;
        assert_eq!(
            reconnect_off_warning(&config).as_deref(),
            Some(
                "WARNING: [GPS] records with reconnect off: if its device drops, nothing \
                 is recorded until the run is restarted"
            )
        );
        config.reconnect.enabled = true;
        assert_eq!(reconnect_off_warning(&config), None);
    }

    #[test]
    fn an_error_and_its_root_cause_are_each_stated_once() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err = anyhow::Error::new(io).context("loading profile p.toml");
        assert_eq!(describe_error(&err), "loading profile p.toml: file missing");
        let plain = anyhow::anyhow!("specify a source");
        assert_eq!(describe_error(&plain), "specify a source");
    }

    #[test]
    fn the_exit_code_says_how_the_run_ended() {
        // §3.1: healthy 0, degraded 3, finalization incomplete 4 — and 4 wins,
        // since it is the one that says recorded data may be missing.
        let complete = ShutdownOutcome::default();
        let incomplete = ShutdownOutcome {
            incomplete: vec!["GPS".to_owned()],
        };
        assert_eq!(exit_status(&complete, false), 0);
        assert_eq!(exit_status(&complete, true), 3);
        assert_eq!(exit_status(&incomplete, false), 4);
        assert_eq!(exit_status(&incomplete, true), 4);
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
            format_event(&RuntimeEvent::DiskSpaceLow(id), &names).as_deref(),
            Some("[GPS feed] LOW DISK")
        );
        let unnamed = crate::core::ChannelId::new();
        assert_eq!(
            format_event(&RuntimeEvent::DiskSpaceLow(unnamed), &names),
            Some(format!("[{unnamed}] LOW DISK"))
        );
    }

    #[test]
    fn lifecycle_events_are_left_to_the_health_lines() {
        // A retry loop would otherwise print reconnecting, stopped and FAULTED
        // for every attempt; the health watch says it once, then reminds.
        let id = crate::core::ChannelId::new();
        let names = std::collections::HashMap::new();
        for event in [
            RuntimeEvent::ChannelStarted(id),
            RuntimeEvent::ChannelStopped(id),
            RuntimeEvent::ChannelFaulted(id),
            RuntimeEvent::ChannelReconnecting(id, 3),
            RuntimeEvent::ChannelReconnected(id),
            RuntimeEvent::ChannelReconnectGaveUp(id),
        ] {
            assert_eq!(format_event(&event, &names), None, "{event:?}");
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
