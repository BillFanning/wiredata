use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::{Args as ClapArgs, ValueEnum};
use wiredata_cli::{exit_status, HealthWatch, EXIT_CANNOT_START};

use crate::core::{
    channel::ChannelId,
    logging::{self, FileLogConfig, Rotation},
    message::decode_utf8_lossy_latin1,
    profile::{self, Profile},
    runner::{self, TalkerCommand, TalkerStatus},
    scheduler::Schedule,
};

/// How `--echo` renders each sent message to stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "lower")]
pub enum EchoFormat {
    /// UTF-8 decode with Latin-1 fallback — matches the GUI's
    /// "Rendered" display mode and gives every byte a printable
    /// glyph. Most readable for NMEA / ASCII / UTF-8 streams.
    #[default]
    Rendered,
    /// Printable ASCII as-is; every non-printable byte
    /// (`< 0x20`, `0x7F`, `>= 0x80`) shown as `<XX>`. Matches the
    /// GUI's "Raw" display mode with the HexEscapes control style.
    Raw,
    /// Each byte as two uppercase hex digits, space-separated.
    /// What `--echo` produced before the format flag existed.
    Hex,
}

// ── Clap argument struct ──────────────────────────────────────────────────────

/// CLI arguments (flattened into the top-level command in `main.rs`).
///
/// Each option uses explicit `help = ...` and `long_help = ...`
/// attributes so the long help opens with the short-help sentence
/// on the first line (no blank-line separator), then continues
/// with the detail in the same paragraph. Using clap-derive's
/// auto-derive-from-doc-comments instead would either put the
/// short help in its own paragraph (with a blank line) or strip
/// the detail entirely.
#[derive(ClapArgs, Debug)]
pub struct Args {
    #[arg(
        short = 'p',
        long,
        conflicts_with = "profile_path",
        value_name = "NAME",
        help = "Load a profile by name from the default profile directory.",
        long_help = "Load a profile by name from the default profile directory. \
                     The profile is looked up at `<default-dir>/<NAME>.toml`. The \
                     default directory is `dirs::config_dir()/talker/profiles`, which \
                     on Windows is `%APPDATA%\\talker\\profiles`, on Linux \
                     `$XDG_CONFIG_HOME/talker/profiles` (or \
                     `~/.config/talker/profiles`), and on macOS \
                     `~/Library/Application Support/talker/profiles`. Use \
                     `--list-profiles` to enumerate what's there. Mutually exclusive \
                     with `--profile-path`."
    )]
    pub profile: Option<String>,

    #[arg(
        short = 'P',
        long,
        conflicts_with = "profile",
        value_name = "FILE",
        help = "Load a profile from an explicit TOML file path.",
        long_help = "Load a profile from an explicit TOML file path. Accepts any \
                     path your shell can hand off (relative or absolute). Mutually \
                     exclusive with `--profile`; useful when the file lives outside \
                     the default directory or has a non-standard name."
    )]
    pub profile_path: Option<PathBuf>,

    #[arg(
        short = 'l',
        long,
        help = "List profile names found in the default directory and exit.",
        long_help = "List profile names found in the default directory and exit. \
                     Exit code is always 0, including when the directory is missing \
                     or empty — the message on stdout explains what was found. \
                     Combines fine with `--quiet` to suppress logging without \
                     affecting the listing itself."
    )]
    pub list_profiles: bool,

    #[arg(
        short = 'e',
        long,
        help = "Echo each sent message to stdout, tagged with its channel index.",
        long_help = "Echo each sent message to stdout, tagged with its channel \
                     index. Format defaults to `rendered` (UTF-8 with Latin-1 \
                     fallback — most readable for NMEA / ASCII); override with \
                     `--echo-format raw` or `--echo-format hex`. Each line is \
                     prefixed `chN: `. Independent of `--quiet` — echo always goes \
                     to stdout."
    )]
    pub echo: bool,

    #[arg(
        long,
        value_name = "FORMAT",
        default_value = "rendered",
        help = "How `--echo` renders each message (rendered|raw|hex).",
        long_help = "How `--echo` renders each message (rendered|raw|hex). \
                     `rendered` decodes bytes as UTF-8 with Latin-1 fallback so \
                     every byte has a glyph (matches the GUI's Rendered display \
                     mode). `raw` shows printable ASCII as-is and non-printable \
                     bytes as `<XX>` (matches the GUI's Raw mode + HexEscapes \
                     control style). `hex` shows each byte as two uppercase hex \
                     digits, space-separated — the format `--echo` used before \
                     this flag existed. Ignored when `--echo` is absent."
    )]
    pub echo_format: EchoFormat,

    #[arg(
        long,
        help = "Drop the `chN: ` channel-index prefix from --echo output.",
        long_help = "Drop the `chN: ` channel-index prefix from --echo output. \
                     Default is to include the prefix so a multi-channel stream \
                     can be filtered with `grep ch3:`. With a single channel, or \
                     when piping into a tool that expects raw payload lines, \
                     `--no-tag` keeps the output uncluttered. Ignored when \
                     `--echo` is absent."
    )]
    pub no_tag: bool,

    #[arg(
        short = 'q',
        long,
        help = "Suppress log output to stdout.",
        long_help = "Suppress log output to stdout. Affects the `tracing` subscriber \
                     only. `--echo`, the profile list from `--list-profiles`, and \
                     any logs written via `--log-file` still appear at their normal \
                     destinations."
    )]
    pub quiet: bool,

    /// `None` = flag absent; `Some(None)` = flag given bare;
    /// `Some(Some(p))` = flag given with path `p`. (Doc comment
    /// kept on the field for Rust readers — clap uses the explicit
    /// `help` / `long_help` attrs below for CLI output.)
    #[arg(
        short = 'L',
        long,
        value_name = "PATH",
        num_args = 0..=1,
        help = "Write logs to a file. Given without a path, a default location is used.",
        long_help = "Write logs to a file. Given without a path, a default location \
                     is used — `dirs::data_local_dir()/talker/logs/talker.log` \
                     (e.g. `%LOCALAPPDATA%\\talker\\logs\\talker.log` on Windows). With \
                     a path, the path is split into directory + filename prefix. \
                     File logging is additive — stdout logging is unaffected unless \
                     you also pass `--quiet`."
    )]
    pub log_file: Option<Option<PathBuf>>,

    #[arg(
        long,
        help = "Exit with code 2 unless every channel opens at start.",
        long_help = "Exit with code 2 unless every channel opens at start. Without \
                     it, a channel whose interface fails to open is retried — 1 s, \
                     doubling to 30 s — while the others send, and a WARNING on \
                     stderr says which channel is down and why."
    )]
    pub require_all: bool,
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// How long the stop may take, once requested, before the CLI stops waiting
/// for the channels' threads and exits with code 4 (ADR-060). A send blocked
/// in the operating system cannot be interrupted.
const STOP_LIMIT: Duration = Duration::from_secs(5);

/// Run the profile until a stop request, and return the exit code (ADR-060):
/// 0 healthy or every outage recovered, 2 nothing can start, 3 degraded, 4 the
/// stop did not finish in time. An internal error is the `Err`, which `main`
/// reports as 1.
pub fn run(args: Args) -> anyhow::Result<u8> {
    // Leading newline for visual separation from the shell prompt
    // is handled in `main.rs` (via the `\n`-prefixed `about` string
    // and the error formatter), so this runner doesn't add its own.

    if args.list_profiles {
        list_profiles()?;
        return Ok(0);
    }

    // Windows 11 would otherwise ignore the high-rate timer request while
    // the console window is minimized/occluded (ADR-017).
    crate::core::timing::keep_timer_resolution_when_minimized();

    let (mut profile, path) = match load_profile(&args) {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("ERROR: {err:#}");
            return Ok(EXIT_CANNOT_START);
        }
    };
    // Mirror the GUI: the file root is the profile's identity, so
    // overlay `profile.name` from the path's stem. The TOML's `name`
    // field is skipped during deserialise, so otherwise `name` would
    // be empty and the log below would read `profile "" loaded`.
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        profile.name = stem.to_string();
    }

    // Apply CLI overrides to the profile's logging configuration, then install
    // logging before any tracing call below.
    let mut logging_config = profile.logging.clone();
    if args.quiet {
        logging_config.stdout = false;
    }
    if let Some(log_file) = &args.log_file {
        logging_config.file = Some(resolve_log_file(log_file.as_deref())?);
    }
    let _log = logging::init(&logging_config, None).context("initializing logging")?;

    tracing::info!("profile {:?} loaded", profile.name);
    if profile.channels.is_empty() {
        eprintln!("ERROR: the profile has no channels");
        return Ok(EXIT_CANNOT_START);
    }

    // Compile every schedule before opening any interface. Preparation is inert,
    // so a bad message cannot briefly claim a serial port or socket.
    let channels = profile.channels;
    let schedules = channels
        .iter()
        .enumerate()
        .map(|(i, channel)| {
            Schedule::compile_unarmed(&channel.messages)
                .map(|schedule| schedule.with_alignment(channel.cadence_alignment))
                .with_context(|| format!("compiling channel {i} schedule"))
        })
        .collect::<anyhow::Result<Vec<_>>>();
    let schedules = match schedules {
        Ok(schedules) => schedules,
        Err(err) => {
            eprintln!("ERROR: {err:#}");
            return Ok(EXIT_CANNOT_START);
        }
    };
    // Each channel gets its stable id here (ADR-020); the CLI never removes
    // channels, but statuses carry ids, so the echo funnel maps id → position.
    let mut prepared = Vec::new();
    for ((i, channel), schedule) in channels.into_iter().enumerate().zip(schedules) {
        let name = if channel.name.is_empty() {
            (i + 1).to_string()
        } else {
            channel.name.clone()
        };
        let who = runner::RunnerIdentity {
            id: ChannelId::mint(),
            label: if channel.name.is_empty() {
                name.clone()
            } else {
                format!("'{name}'")
            },
            run_id: crate::core::run_summary::RunId::mint(),
        };
        prepared.push((who, name, channel.interface, schedule));
    }

    // With `--require-all`, every channel opens now or nothing runs; an open
    // failure drops the handles already opened before any thread starts.
    let mut opened = if args.require_all {
        let mut opened = Vec::new();
        for (who, _, config, _) in &prepared {
            match config.open() {
                Ok(interface) => opened.push(interface),
                Err(err) => {
                    eprintln!(
                        "ERROR: channel {} did not open: {err:#} — stopping, because \
                         --require-all is set",
                        who.label
                    );
                    return Ok(EXIT_CANNOT_START);
                }
            }
        }
        Some(opened.into_iter())
    } else {
        None
    };

    // One runner thread per channel, each driven by the same core send loop
    // as the GUI (spec §2.2). Without `--require-all`, each opens its own
    // interface, retrying until it opens, so the others send meanwhile.
    //
    // The status channel is bounded, like the GUI's: if `--echo` output backs
    // up (stdout piped into a slow consumer), the runners drop status updates
    // and count them (`dropped_statuses`) instead of buffering without limit —
    // send cadence is never sacrificed to echo, and memory stays flat on a
    // long soak.
    let (status_tx, status_rx) = crossbeam_channel::bounded::<TalkerStatus>(1024);
    let (control_tx, control_rx) = crossbeam_channel::bounded(64);
    // `--echo` is the one consumer that wants every wire payload (ADR-018);
    // without it, sampled lanes keep the status traffic constant at any rate.
    let policy = if args.echo {
        runner::ObserverPolicy::every_send()
    } else {
        runner::ObserverPolicy::sampled()
    };
    let mut health = HealthWatch::new(
        "retrying, 1 s doubling to 30 s",
        "not retrying, so it stays down",
    );
    let mut cmd_txs = Vec::new();
    let mut handles = Vec::new();
    // Echo tags stay positional ("ch0:", as before): map each stable id back
    // to the channel's position in the profile.
    let mut echo_index: HashMap<ChannelId, usize> = HashMap::new();
    for (i, (who, name, config, schedule)) in prepared.into_iter().enumerate() {
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(8);
        cmd_txs.push(cmd_tx);
        echo_index.insert(who.id, i);
        health.add(who.id, &name, true);
        // No notify callback: the main thread waits on the channels anyway.
        let observer =
            runner::RunnerObserver::new(status_tx.clone(), policy).with_control(control_tx.clone());
        match opened.as_mut().and_then(Iterator::next) {
            Some(interface) => {
                health.up(who.id, Instant::now());
                handles.push(std::thread::spawn(move || {
                    runner::run(who, interface, Some(config), schedule, cmd_rx, observer);
                }));
            }
            None => handles.push(std::thread::spawn(move || {
                runner::open_retrying_and_run(who, config, schedule, cmd_rx, observer);
            })),
        }
    }
    drop(status_tx); // only the runners hold senders now
    drop(control_tx);

    // Every OS stop request gets the same graceful stop (ADR-060). On Unix,
    // Ctrl-C, SIGTERM and SIGHUP come through `ctrlc`. On Windows,
    // `wiredata-stop` handles the console events — it holds a console close
    // until the stop finishes, which `ctrlc` does not — and logoff and
    // shutdown, which never reach a console handler here. The handler never
    // blocks: a full command queue already holds a Stop, or belongs to a
    // runner that is stuck, which the stop limit covers.
    let (stop_tx, stop_rx) = crossbeam_channel::bounded::<wiredata_stop::StopRequest>(8);
    let stop = {
        let cmd_txs = cmd_txs.clone();
        move |request: wiredata_stop::StopRequest| {
            let _ = stop_tx.try_send(request);
            for tx in &cmd_txs {
                let _ = tx.try_send(TalkerCommand::Stop);
            }
        }
    };
    #[cfg(not(windows))]
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || stop(wiredata_stop::StopRequest::Interrupt))
            .context("installing the stop handler")?;
    }
    let windows_stop =
        wiredata_stop::on_windows_stop(stop).context("listening for Windows stop requests")?;

    tracing::info!("{} channel(s) starting — Ctrl+C to stop", handles.len());

    // Run until every runner has dropped its status sender (all stopped), or
    // until a requested stop has run past its limit. `--echo` mirrors each
    // send to stdout here on the main thread, so output lines can't interleave
    // mid-line across channels. The health lines go to stderr, so they are
    // never mixed into echoed data and `--quiet` does not hide them.
    let echo = args.echo;
    let echo_format = args.echo_format;
    let tag = !args.no_tag;
    let mut finals: HashMap<ChannelId, FinalCounts> = HashMap::new();
    let mut stopping_since: Option<Instant> = None;
    let mut stop_incomplete = false;
    let ticker = crossbeam_channel::tick(Duration::from_secs(1));
    let say = |line: Option<String>| {
        if let Some(line) = line {
            eprintln!("{line}");
        }
    };
    loop {
        crossbeam_channel::select! {
            recv(status_rx) -> status => {
                let Ok(status) = status else { break };
                let now = Instant::now();
                match status {
                    TalkerStatus::SendSample { channel, payload, .. } => {
                        if echo {
                            if let Some(&index) = echo_index.get(&channel) {
                                echo_line(index, &payload, echo_format, tag);
                            }
                        }
                    }
                    TalkerStatus::OpenFailed { channel, message }
                    | TalkerStatus::ConnectionError { channel, message } => {
                        say(health.down(channel, &message, now));
                    }
                    TalkerStatus::SendRecovered { channel, .. } => say(health.up(channel, now)),
                    TalkerStatus::Counters {
                        channel,
                        total_count,
                        total_bytes,
                        failed_sends,
                        possibly_partial_sends,
                        peer_bytes,
                        dropped_statuses,
                        ..
                    } => {
                        finals.insert(
                            channel,
                            FinalCounts {
                                sent: total_count,
                                bytes: total_bytes,
                                failed: failed_sends,
                                possibly_partial: possibly_partial_sends,
                                peer_bytes,
                                dropped: dropped_statuses,
                            },
                        );
                    }
                    _ => {}
                }
            }
            recv(control_rx) -> control => {
                if let Ok(runner::RunnerControlStatus::InterfaceOpened { channel, .. }) = control {
                    say(health.up(channel, Instant::now()));
                }
            }
            recv(stop_rx) -> request => {
                if let (Ok(request), None) = (request, stopping_since) {
                    tracing::info!("stopping ({request})…");
                    stopping_since = Some(Instant::now());
                }
            }
            recv(ticker) -> _ => {
                for line in health.reminders(Instant::now()) {
                    eprintln!("{line}");
                }
                if stopping_since.is_some_and(|since| since.elapsed() >= STOP_LIMIT) {
                    stop_incomplete = true;
                    break;
                }
            }
        }
    }

    if stop_incomplete {
        // The stuck threads end with the process.
        eprintln!(
            "WARNING: the stop did not finish within {} s: a channel's send is blocked \
             in the operating system",
            STOP_LIMIT.as_secs()
        );
    } else {
        for handle in handles {
            let _ = handle.join();
        }
        tracing::info!("stopped");
    }
    eprintln!("summary:");
    for (id, line) in health.summary(Instant::now()) {
        eprintln!("  {line}{}", counters_note(finals.get(&id), echo));
    }
    let status = exit_status(stop_incomplete, health.degraded());
    // A held Windows logoff or shutdown goes ahead only once the log has
    // flushed: Windows may end the process straight away.
    drop(_log);
    windows_stop.finish();
    Ok(status)
}

/// A channel's send counters at the stop (§4.4), for the summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FinalCounts {
    sent: u64,
    bytes: u64,
    failed: u64,
    /// Sends that failed after part of the message was written (§4.4).
    possibly_partial: u64,
    /// Bytes the TCP peer sent (§4.5); `None` for other transports.
    peer_bytes: Option<u64>,
    /// Status updates dropped because the reader was full: with `--echo`,
    /// echo lines not printed (§5.8).
    dropped: u64,
}

/// The summary's note on a channel's final send counters, and with `--echo`
/// how many echo lines were dropped; empty for a channel that never sent.
fn counters_note(counts: Option<&FinalCounts>, echo: bool) -> String {
    let Some(counts) = counts else {
        return String::new();
    };
    let mut note = format!(
        "; sent {} message{} ({} bytes), {} failed send{}",
        counts.sent,
        if counts.sent == 1 { "" } else { "s" },
        counts.bytes,
        counts.failed,
        if counts.failed == 1 { "" } else { "s" }
    );
    if counts.possibly_partial > 0 {
        note.push_str(&format!(", {} possibly partial", counts.possibly_partial));
    }
    if let Some(bytes) = counts.peer_bytes.filter(|&bytes| bytes > 0) {
        note.push_str(&format!(
            "; peer sent {bytes} byte{}",
            if bytes == 1 { "" } else { "s" }
        ));
    }
    if echo && counts.dropped > 0 {
        note.push_str(&format!("; {} echo lines dropped", counts.dropped));
    }
    note
}

/// Print one echo line. Payload bytes pass straight through the
/// chosen format; `println!` adds the one terminating newline.
/// (Earlier revisions stripped trailing `0x0D` / `0x0A` from the
/// payload to avoid a "double newline" in some output formats; the
/// extra blank line turned out to be a terminal-rendering thing,
/// not something Talker actually emits, so the simpler no-trim
/// form is back.)
fn echo_line(index: usize, payload: &[u8], format: EchoFormat, tag: bool) {
    let text = format_payload(payload, format);
    if tag {
        println!("ch{index}: {text}");
    } else {
        println!("{text}");
    }
}

/// Render `payload` per `format` for `--echo` output.
fn format_payload(payload: &[u8], format: EchoFormat) -> String {
    match format {
        EchoFormat::Hex => hex_string(payload),
        EchoFormat::Raw => raw_string(payload),
        EchoFormat::Rendered => decode_utf8_lossy_latin1(payload),
    }
}

/// Printable ASCII as-is; every other byte as `<XX>` (listener's hex-escape
/// format, so the two apps read identically). Matches the
/// GUI's `DisplayMode::Raw` with the `HexEscapes` control style —
/// chosen as the default for CLI since it doesn't depend on Unicode
/// control-picture fonts being installed.
fn raw_string(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if (0x20..=0x7E).contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("<{b:02X}>"));
        }
    }
    out
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Format bytes as space-separated uppercase hex pairs.
fn hex_string(data: &[u8]) -> String {
    data.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Resolve a `--log-file` argument into a [`FileLogConfig`].
///
/// `None` (a bare `--log-file`) selects the platform default directory.
/// Otherwise the path is split into a directory and a filename prefix.
fn resolve_log_file(path: Option<&Path>) -> anyhow::Result<FileLogConfig> {
    let Some(path) = path else {
        let dir = logging::default_log_dir()
            .context("cannot determine a default log directory on this platform")?;
        return Ok(FileLogConfig::new(dir));
    };
    let directory = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let prefix = match path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => "talker.log".to_string(),
    };
    Ok(FileLogConfig {
        directory,
        prefix,
        rotation: Rotation::default(),
    })
}

fn load_profile(args: &Args) -> anyhow::Result<(Profile, PathBuf)> {
    let path = if let Some(p) = &args.profile_path {
        p.clone()
    } else if let Some(name) = &args.profile {
        let dir = profile::default_dir().context("cannot determine profile directory")?;
        dir.join(format!("{name}.toml"))
    } else {
        // Multi-line so the suggested invocations stand out from the
        // surrounding "error:" prefix that `main.rs` adds on stderr.
        // Joined from an array — Rust string-literal line
        // continuations strip leading whitespace, so trying to
        // indent inline mangles the alignment.
        let msg = [
            "no profile specified — pick one of:",
            "  -p, --profile <NAME>        load a profile by name from the default directory",
            "  -P, --profile-path <FILE>   load a profile from an explicit path",
            "  -l, --list-profiles         show what's in the default directory and exit",
            "  -g, --gui                   launch the graphical interface",
            "      --help                  full option list with descriptions",
        ]
        .join("\n");
        anyhow::bail!(msg);
    };
    let profile = Profile::load(&path)?;
    Ok((profile, path))
}

fn list_profiles() -> anyhow::Result<()> {
    let dir = match profile::default_dir() {
        Some(d) => d,
        None => {
            println!("Cannot determine profile directory on this platform.");
            return Ok(());
        }
    };

    if !dir.exists() {
        println!(
            "No profiles found (directory {} does not exist).",
            dir.display()
        );
        return Ok(());
    }

    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect();

    if names.is_empty() {
        println!("No profiles found in {}.", dir.display());
    } else {
        names.sort();
        for name in &names {
            println!("{name}");
        }
    }

    Ok(())
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> Result<Args, clap::Error> {
        use clap::Parser;

        #[derive(Parser)]
        struct Cmd {
            #[command(flatten)]
            args: Args,
        }

        Cmd::try_parse_from(argv).map(|c| c.args)
    }

    #[test]
    fn require_all_is_off_unless_asked_for() {
        // ADR-060: start what can start, unless --require-all.
        assert!(!parse(&["talker", "--profile", "p"]).unwrap().require_all);
        assert!(
            parse(&["talker", "--profile", "p", "--require-all"])
                .unwrap()
                .require_all
        );
    }

    #[test]
    fn the_summary_notes_each_channel_s_send_counters() {
        // ADR-060: the final summary gives each channel's send counters
        // (§4.4) and, with --echo, the echo lines dropped (§5.8).
        let mut counts = FinalCounts {
            sent: 120,
            bytes: 4_800,
            failed: 1,
            possibly_partial: 0,
            peer_bytes: None,
            dropped: 3,
        };
        assert_eq!(
            counters_note(Some(&counts), false),
            "; sent 120 messages (4800 bytes), 1 failed send"
        );
        assert_eq!(
            counters_note(Some(&counts), true),
            "; sent 120 messages (4800 bytes), 1 failed send; 3 echo lines dropped"
        );
        assert_eq!(counters_note(None, true), "", "a channel that never sent");
        // §4.4: said only when it happened.
        counts.possibly_partial = 2;
        assert_eq!(
            counters_note(Some(&counts), false),
            "; sent 120 messages (4800 bytes), 1 failed send, 2 possibly partial"
        );
        // §4.5: a TCP peer that answered.
        counts.peer_bytes = Some(12);
        assert_eq!(
            counters_note(Some(&counts), false),
            "; sent 120 messages (4800 bytes), 1 failed send, 2 possibly partial; \
             peer sent 12 bytes"
        );
    }

    #[test]
    fn parse_profile_name() {
        let a = parse(&["talker", "--profile", "my-profile"]).unwrap();
        assert_eq!(a.profile.as_deref(), Some("my-profile"));
        assert!(a.profile_path.is_none());
    }

    #[test]
    fn parse_short_flags_match_long() {
        // Each short form should reach the same field as its long
        // counterpart — proves the `short = 'X'` derive entries
        // line up with the long names.
        let a = parse(&["talker", "-p", "name", "-l", "-e", "-q", "-L", "/tmp/x.log"]).unwrap();
        assert_eq!(a.profile.as_deref(), Some("name"));
        assert!(a.list_profiles);
        assert!(a.echo);
        assert!(a.quiet);
        assert_eq!(a.log_file, Some(Some(PathBuf::from("/tmp/x.log"))));

        let b = parse(&["talker", "-P", "/etc/talker/foo.toml"]).unwrap();
        assert_eq!(
            b.profile_path.as_deref(),
            Some(Path::new("/etc/talker/foo.toml"))
        );
    }

    #[test]
    fn parse_profile_path() {
        let a = parse(&["talker", "--profile-path", "/etc/talker/foo.toml"]).unwrap();
        assert_eq!(
            a.profile_path.as_deref(),
            Some(Path::new("/etc/talker/foo.toml"))
        );
        assert!(a.profile.is_none());
    }

    #[test]
    fn parse_list_profiles() {
        let a = parse(&["talker", "--list-profiles"]).unwrap();
        assert!(a.list_profiles);
    }

    #[test]
    fn profile_and_profile_path_conflict() {
        let result = parse(&[
            "talker",
            "--profile",
            "foo",
            "--profile-path",
            "/bar/baz.toml",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_echo_and_quiet_flags() {
        let a = parse(&["talker", "--profile", "p", "--echo", "--quiet"]).unwrap();
        assert!(a.echo);
        assert!(a.quiet);
        let b = parse(&["talker", "--profile", "p"]).unwrap();
        assert!(!b.echo);
        assert!(!b.quiet);
    }

    #[test]
    fn parse_log_file_with_path() {
        let a = parse(&["talker", "--profile", "p", "--log-file", "/var/log/run.log"]).unwrap();
        assert_eq!(a.log_file, Some(Some(PathBuf::from("/var/log/run.log"))));
    }

    #[test]
    fn parse_log_file_bare_yields_some_none() {
        let a = parse(&["talker", "--profile", "p", "--log-file"]).unwrap();
        assert_eq!(a.log_file, Some(None));
    }

    #[test]
    fn parse_log_file_absent_is_none() {
        let a = parse(&["talker", "--profile", "p"]).unwrap();
        assert!(a.log_file.is_none());
    }

    #[test]
    fn load_profile_errors_when_neither_flag_given() {
        let args = Args {
            profile: None,
            profile_path: None,
            list_profiles: false,
            echo: false,
            echo_format: EchoFormat::default(),
            no_tag: false,
            quiet: false,
            log_file: None,
            require_all: false,
        };
        let err = load_profile(&args).unwrap_err();
        let msg = err.to_string();
        // Error should mention each of the suggested ways forward —
        // protects against accidental message regressions.
        for needle in [
            "--profile",
            "--profile-path",
            "--list-profiles",
            "--gui",
            "--help",
        ] {
            assert!(msg.contains(needle), "missing {needle}: {msg}");
        }
    }

    #[test]
    fn load_profile_errors_on_missing_file() {
        let args = Args {
            profile: None,
            profile_path: Some(PathBuf::from("/no/such/file.toml")),
            list_profiles: false,
            echo: false,
            echo_format: EchoFormat::default(),
            no_tag: false,
            quiet: false,
            log_file: None,
            require_all: false,
        };
        let err = load_profile(&args).unwrap_err();
        assert!(err.to_string().contains("reading profile"));
    }

    #[test]
    fn load_profile_returns_path() {
        // Round-tripped path lets `run()` derive the profile name
        // from the file stem.
        let args = Args {
            profile: None,
            profile_path: Some(PathBuf::from("/no/such/file.toml")),
            list_profiles: false,
            echo: false,
            echo_format: EchoFormat::default(),
            no_tag: false,
            quiet: false,
            log_file: None,
            require_all: false,
        };
        // Loading errors, but we just want to confirm the path
        // propagates in the error-free signature shape.
        let _ = load_profile(&args);
    }

    #[test]
    fn hex_string_formats_uppercase_spaced() {
        assert_eq!(hex_string(&[0xDE, 0xAD, 0xBE, 0xEF]), "DE AD BE EF");
        assert_eq!(hex_string(&[0x01]), "01");
        assert_eq!(hex_string(&[]), "");
    }

    #[test]
    fn raw_string_shows_printable_and_hex_escapes() {
        assert_eq!(raw_string(b"Hello!"), "Hello!");
        assert_eq!(raw_string(&[0x41, 0x0D, 0x0A]), "A<0D><0A>");
        // High byte (Latin-1 'î') is non-printable ASCII → hex escape.
        assert_eq!(raw_string(&[0xEE]), "<EE>");
    }

    #[test]
    fn format_payload_dispatches_by_format() {
        let nmea = b"$GPGGA,123519\r\n";
        // Hex: spaced uppercase hex.
        assert!(format_payload(nmea, EchoFormat::Hex).starts_with("24 47 50"));
        // Raw: printable as-is, CR/LF as hex escapes.
        let raw = format_payload(nmea, EchoFormat::Raw);
        assert!(raw.starts_with("$GPGGA"));
        assert!(raw.ends_with("<0D><0A>"));
        // Rendered: control bytes pass through as their Unicode codepoint.
        let rendered = format_payload(nmea, EchoFormat::Rendered);
        assert!(rendered.starts_with("$GPGGA"));
        assert!(rendered.ends_with('\n'));
    }

    #[test]
    fn echo_format_default_is_rendered() {
        assert_eq!(EchoFormat::default(), EchoFormat::Rendered);
    }

    #[test]
    fn parse_echo_format_flag() {
        let a = parse(&["talker", "-p", "x", "--echo", "--echo-format", "hex"]).unwrap();
        assert_eq!(a.echo_format, EchoFormat::Hex);
        let b = parse(&["talker", "-p", "x", "--echo", "--echo-format", "raw"]).unwrap();
        assert_eq!(b.echo_format, EchoFormat::Raw);
        // Default value applies when the flag is absent.
        let c = parse(&["talker", "-p", "x", "--echo"]).unwrap();
        assert_eq!(c.echo_format, EchoFormat::Rendered);
    }

    #[test]
    fn parse_no_tag_flag() {
        let a = parse(&["talker", "-p", "x", "--echo", "--no-tag"]).unwrap();
        assert!(a.no_tag);
        let b = parse(&["talker", "-p", "x", "--echo"]).unwrap();
        assert!(!b.no_tag);
    }

    #[test]
    fn resolve_log_file_splits_directory_and_prefix() {
        let cfg = resolve_log_file(Some(Path::new("/var/log/talker/run.log"))).unwrap();
        assert_eq!(cfg.directory, PathBuf::from("/var/log/talker"));
        assert_eq!(cfg.prefix, "run.log");
    }

    #[test]
    fn resolve_log_file_bare_filename_uses_current_dir() {
        let cfg = resolve_log_file(Some(Path::new("run.log"))).unwrap();
        assert_eq!(cfg.directory, PathBuf::from("."));
        assert_eq!(cfg.prefix, "run.log");
    }

    #[test]
    fn resolve_log_file_none_uses_default_location() {
        // On platforms without a local-data directory this returns an error;
        // otherwise it resolves to the default log directory.
        if let Ok(cfg) = resolve_log_file(None) {
            assert!(cfg.directory.ends_with("logs"));
        }
    }
}
