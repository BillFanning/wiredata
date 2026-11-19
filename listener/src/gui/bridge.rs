//! The GUI↔runtime bridge (listener ADR-008).
//!
//! A background **driver** task owns the async [`Listener`] and stands between it
//! and the synchronous egui App. The App never touches the runtime directly
//! (AGENTS §5): it sends [`UiCommand`]s and receives [`UiUpdate`]s over channels,
//! and the driver translates commands into `Listener` method calls, forwards the
//! `RuntimeEvent` stream, and pushes periodic [`ChannelSnapshot`]s plus incremental
//! [`StreamDelta`]s (the scrollback bytes — kept out of the snapshot, ADR-011).
//!
//! This module is **egui-free** so it is unit-testable without a display: the only
//! coupling to the UI is an opaque `repaint` callback the driver invokes after
//! pushing an update (the App passes `egui::Context::request_repaint`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use tokio::sync::mpsc::{self, Receiver, Sender};

use crate::config::{
    describe_channel_errors, ChannelConfig, DataBits, DisplayConfig, DisplayRecordingConfig,
    FlowControl, InterfaceConfig, Parity, Profile, RawRecordingConfig, ReconnectPolicy,
    RetentionConfig, StopBits,
};
use crate::core::{ChannelId, ChannelName, ChannelState, RuntimeEvent};
use crate::runtime::{
    ChannelSnapshot, ChannelStats, DrainedStop, Listener, OpenedStart, OrchestratorError,
    PipelineCapacities, ReconnectAttempt, StreamDelta,
};
use crate::transport::udp::UdpMode;
use crate::transport::SerialControlLines;

use super::bind_scope::{local_addresses, BindScope, LocalAddress};

/// The longest one poll round may run before the Channels it has not reached
/// are left for the next round. Each request already waits at most
/// `STATUS_REPLY_WAIT`; this keeps several unresponsive Channels from
/// multiplying that wait. Checked between Channels, so a round can overrun by
/// the requests of the last Channel it started.
const POLL_ROUND_LIMIT: Duration = crate::runtime::channel::POLL_ROUND_LIMIT;

/// How often the driver polls running Channels for a fresh snapshot (the pull
/// surface, ADR-006). 5 Hz is responsive without busy-polling.
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(200);
/// Auto-reconnect cadence (§162): the orchestrator has no background loop, so the
/// driver ticks it, exactly as the CLI does.
const RECONNECT_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, Default)]
struct StreamPollCursor {
    generation: Option<u64>,
    offset: u64,
}

/// A command from the GUI to the runtime — the GUI's on-the-wire command form, which
/// the driver translates into [`Listener`] calls (the command surface; ADR-012). There
/// is no separate `core::RuntimeCommand` enum. The lifecycle variants map to `Listener`
/// methods; some variants are driver-owned *workflows* over several runtime calls
/// (`StartAll`/`StopAll` iterate, `SaveProfile`/`LoadProfile` gather/swap configs,
/// `Select` just steers polling). The dynamic in-pipeline actions
/// (`SetMatchRuleEnabled`, `MarkNow`, mid-run recording enable/disable) wait on the
/// command channel into `run_channel` (deferred, ADR-008).
#[derive(Debug)]
pub enum UiCommand {
    /// Register a channel from its config; the driver replies with `ChannelAdded`
    /// carrying the minted [`ChannelId`].
    AddChannel(Box<ChannelConfig>),
    Start(ChannelId),
    Stop(ChannelId),
    /// The unified lifecycle command (maps to `Listener::commit_and_start`): optionally
    /// commit `config`, optionally `start`, in one coordinated server-side sequence.
    /// This is how single-channel Start / Retry / "Apply & Restart" all reach the
    /// runtime — one command, no client-side Stop→Reconfigure→Start choreography, and
    /// the Faulted→Stopped→Starting recovery lives in the runtime, not the GUI. The
    /// driver echoes `ChannelReconfigured` when a config was committed.
    CommitAndStart {
        id: ChannelId,
        config: Option<Box<ChannelConfig>>,
        start: bool,
    },
    /// Start a batch of channels (the "Start all" button) in one command instead of a
    /// per-channel command burst that could overflow the bounded command channel. Each
    /// pair is a channel id and the config to start it with (the GUI has already
    /// validated them and filtered out unconfigured / already-running ones). The driver
    /// runs each through the same `commit_and_start` path as the single-channel command.
    StartAll(Vec<(ChannelId, Box<ChannelConfig>)>),
    /// Stop every channel the driver knows about (the "Stop all" button). One command
    /// per click instead of an N-command burst: a burst of per-channel `Stop`s could
    /// overflow the bounded command channel and silently drop some, leaving those
    /// channels' UI views stuck Running. The driver iterates server-side and stops each
    /// that is live (already-Stopped channels are skipped), off the UI thread.
    StopAll,
    /// Remove a channel from the runtime entirely (stops it first if live). Used to
    /// recover from a misconfigured channel (e.g. a bind conflict).
    RemoveChannel(ChannelId),
    /// Rename a channel in place — instant, no restart (the name is a label only).
    Rename(ChannelId, ChannelName),
    PauseDisplay(ChannelId),
    ResumeDisplay(ChannelId),
    SetRts(ChannelId, bool),
    SetDtr(ChannelId, bool),
    /// Begin (`true`) or stop (`false`) Raw recording on a running channel live,
    /// without a restart (§50.2, ADR-012). Carries the recording settings read from
    /// the editor **at the moment Record was pressed** — destination/overwrite/
    /// rotation/timestamps — so it records to exactly what's on screen, no separate
    /// Apply. The driver arms the running pipeline from these. The outcome shows up in
    /// the next snapshot's recording state.
    SetRecording(ChannelId, bool, Box<RawRecordingConfig>),
    /// Persist a channel's Raw recording **settings** (destination, rotation, overwrite,
    /// "record on start") into the stored config without a restart — Raw recording is a
    /// live field (ADR-012/-013). Keeps the runtime's config current so a profile save
    /// captures the settings; the live recorder is (re)armed separately by `SetRecording`.
    SetRawRecordingConfig(ChannelId, Box<RawRecordingConfig>),
    /// Begin (`true`) or stop (`false`) **Display** recording live (§54, ADR-012) —
    /// `SetRecording`'s sibling, carrying the display settings read from the editor
    /// at click time.
    SetDisplayRecording(ChannelId, bool, Box<DisplayRecordingConfig>),
    /// Persist a channel's Display recording settings into the stored config without
    /// a restart — `SetRawRecordingConfig`'s sibling (ADR-012/-013).
    SetDisplayRecordingConfig(ChannelId, Box<DisplayRecordingConfig>),
    /// Set a channel's reconnect policy live, without a restart (§9.1, ADR-045).
    SetReconnect(ChannelId, ReconnectPolicy),
    /// Update a channel's per-channel view settings in the stored config without a
    /// restart: the display config (mode, font, colors — §78) and the scroll-buffer
    /// `retention` (§87). The viewer renders these GUI-side and the GUI caps its own
    /// scrollback live, so this only keeps the runtime's stored config current — for a
    /// profile save, and so the runtime adopts the retention on the channel's next start.
    SetViewConfig(ChannelId, Box<DisplayConfig>, Box<RetentionConfig>),
    /// Tell the driver which channel is on screen (`None` = none). Only the selected
    /// channel gets a snapshot + incremental stream delta polled; the rest get cheap
    /// stats (ADR-006).
    Select(Option<ChannelId>),
    /// Tell the driver which channels have a pop-out window open (spec §50.4).
    /// Each of them gets the same incremental stream delta the selected channel
    /// gets, whether or not it is selected. The list replaces the previous one;
    /// ids of channels that do not exist are ignored.
    SetPopouts(Vec<ChannelId>),
    /// Save the current workspace (every registered channel's config) to a TOML
    /// profile at `path` (§67–§71). The driver replies with `ProfileSaved` or, on an
    /// I/O/serialization error, `ProfileError`.
    SaveProfile(std::path::PathBuf),
    /// Replace the current workspace with the profile loaded from `path` (§70): every
    /// existing channel is stopped and removed, then the profile's channels are
    /// registered Stopped (load never starts a channel). The driver emits the usual
    /// `ChannelRemoved`/`ChannelAdded` updates so the UI folds the change, then
    /// `ProfileLoaded`; a load/parse error yields `ProfileError` and leaves the
    /// workspace untouched.
    LoadProfile(std::path::PathBuf),
    /// Check that a registered profile can resume (ADR-045), opening nothing:
    /// see `resume_check`. Answered with `ResumeChecked`.
    CheckResume(std::path::PathBuf),
    /// Resume a registered profile (ADR-045): check it again, load it, and start
    /// every channel. Recordings set to start with their channel begin then.
    /// Answered with `Resumed`, or `ResumeChecked` with why it could not.
    Resume(std::path::PathBuf),
    /// List the host's local addresses for the UDP bind choice (§15). Answered
    /// with `LocalAddresses`.
    ListLocalAddresses,
    /// Discard the workspace and start empty: stop and remove every channel,
    /// emitting the usual `ChannelRemoved` updates so the UI folds the change.
    /// The teardown half of `LoadProfile` with nothing loaded after it.
    NewProfile,
    /// Open Linemon's hidden Channel with this config, or re-open it with a new
    /// one (spec §50.3). The newest config wins.
    LinemonOpen(Box<ChannelConfig>),
    /// Close Linemon's port. The hidden Channel stops; nothing else is touched.
    LinemonClose,
    /// Set RTS on Linemon's open serial port (§161). Ignored when no port is open.
    LinemonSetRts(bool),
    /// Set DTR on Linemon's open serial port (§161). Ignored when no port is open.
    LinemonSetDtr(bool),
    /// Stop all channels and end the driver (the App is closing).
    Shutdown,
}

/// An update from the runtime to the GUI. The App folds these into its view-model
/// ([`AppState`](super::state::AppState)); none of them borrow runtime state — each
/// carries owned data, so a slow UI can never stall reception.
#[derive(Debug)]
pub enum UiUpdate {
    /// A channel was registered: its runtime id, display name, a one-line
    /// connection description (interface + endpoint), and a copy of its config (so
    /// the UI can edit it, e.g. change the port).
    ChannelAdded(ChannelId, String, String, Box<ChannelConfig>),
    /// A channel's configuration changed (§13): its new connection description and
    /// config, so the UI refreshes its editor and details.
    ChannelReconfigured(ChannelId, String, Box<ChannelConfig>),
    /// A channel was renamed in place (§6) — its new display name. No restart and
    /// no connection change, so only the label updates.
    ChannelRenamed(ChannelId, String),
    /// A channel was removed from the runtime; the UI should drop it.
    ChannelRemoved(ChannelId),
    /// A command on a channel failed (e.g. a Start whose bind hit "address in
    /// use"): the channel id and the error text, so the UI can show the reason.
    ChannelError(ChannelId, String),
    /// A forwarded runtime event (the authoritative push surface, ADR-006).
    Event(RuntimeEvent),
    /// A periodic snapshot of the *selected* running channel's small observable
    /// state — diagnostics, matches, view pause, recording (the pull surface). The
    /// stream bytes ride the separate `StreamDelta` channel, not this.
    Snapshot(ChannelId, Box<ChannelSnapshot>),
    /// Periodic cheap liveness stats for a running channel, polled for *every*
    /// channel to keep per-tab health current (no scrollback cloning).
    Stats(ChannelId, Box<ChannelStats>),
    /// Current serial control/status lines for a running serial channel (§161),
    /// polled alongside snapshots.
    ControlLines(ChannelId, SerialControlLines),
    /// Incremental stream scrollback for the *selected* channel (§87, ADR-009):
    /// only the bytes new since the GUI's cursor, so the driver never re-ships the
    /// whole retained buffer (up to the 256 KB scroll cap) each poll. The App
    /// appends them to its live view.
    StreamDelta(ChannelId, Box<StreamDelta>),
    /// The workspace was saved to a profile file (the path, for a confirmation).
    ProfileSaved(std::path::PathBuf),
    /// A profile was loaded (its name); the channel set has been replaced via the
    /// preceding `ChannelRemoved`/`ChannelAdded` updates.
    ProfileLoaded(String),
    /// A profile save or load failed; carries a human-readable reason. The current
    /// workspace is unchanged.
    ProfileError(String),
    /// Whether a registered profile can resume: its name, or why not (ADR-045).
    ResumeChecked(std::path::PathBuf, Result<String, String>),
    /// A registered profile resumed, at this time (ADR-045).
    Resumed(String, std::time::SystemTime),
    /// The host's local addresses, or why the OS could not list them (§15).
    LocalAddresses(Result<Vec<LocalAddress>, String>),
    /// Something about Linemon's hidden Channel (spec §50.3).
    Linemon(LinemonUpdate),
}

/// An update about Linemon's hidden Channel. It never appears as an ordinary
/// Channel update, so the main window's model never sees it.
#[derive(Debug)]
pub enum LinemonUpdate {
    /// New bytes of the stream (the same incremental delta a Channel's view gets).
    Stream(Box<StreamDelta>),
    /// The hidden Channel's snapshot: its state, totals and diagnostics.
    Snapshot(Box<ChannelSnapshot>),
    /// Opening or re-opening failed; the reason, as a sentence for the window.
    Error(String),
    /// The open serial port's live control lines (§161).
    ControlLines(SerialControlLines),
}

impl UiUpdate {
    /// Whether this update rides the bounded best-effort lane. Telemetry and
    /// every runtime event are periodic; only structural results, which
    /// nothing re-sends, ride the reliable lane (see [`update_lanes`]).
    ///
    /// A runtime event is periodic so that it keeps its order relative to the
    /// snapshots and stats around it, and so that no autonomous source can
    /// fill the unbounded lane. Dropping one under a full queue costs only
    /// lateness: the next poll carries the Channel's state, totals, timing
    /// and recording state, and the stream delta's generation tells the GUI a
    /// new run began. What a dropped event cannot restore is the one-off
    /// notice `RecordingFaulted` raises, whose reason stays in the Channel's
    /// diagnostics.
    ///
    /// The match is exhaustive, so a new update must be classified to compile.
    pub fn is_periodic(&self) -> bool {
        match self {
            Self::Snapshot(..)
            | Self::Stats(..)
            | Self::ControlLines(..)
            | Self::StreamDelta(..)
            | Self::Linemon(
                LinemonUpdate::Stream(_)
                | LinemonUpdate::Snapshot(_)
                | LinemonUpdate::ControlLines(_),
            )
            | Self::Event(_) => true,
            Self::ChannelAdded(..)
            | Self::ChannelReconfigured(..)
            | Self::ChannelRenamed(..)
            | Self::ChannelRemoved(_)
            | Self::ChannelError(..)
            | Self::ProfileSaved(_)
            | Self::ProfileLoaded(_)
            | Self::ProfileError(_)
            | Self::ResumeChecked(..)
            | Self::Resumed(..)
            | Self::LocalAddresses(_)
            | Self::Linemon(LinemonUpdate::Error(_)) => false,
        }
    }
}

/// The driver's end of the two update lanes made by [`update_lanes`].
pub struct UpdateSender {
    periodic: Sender<UiUpdate>,
    reliable: mpsc::UnboundedSender<UiUpdate>,
}

impl UpdateSender {
    /// Queue one update on the lane its [`UiUpdate::is_periodic`] names. Never
    /// blocks. Returns whether the update was queued: a periodic update is
    /// dropped when its bounded lane is full, and a structural result is
    /// dropped only when the receiver has gone.
    fn send(&self, update: UiUpdate) -> bool {
        if update.is_periodic() {
            self.periodic.try_send(update).is_ok()
        } else {
            self.reliable.send(update).is_ok()
        }
    }
}

/// The GUI's end of the two update lanes made by [`update_lanes`].
pub struct UpdateReceiver {
    periodic: Receiver<UiUpdate>,
    reliable: mpsc::UnboundedReceiver<UiUpdate>,
}

impl UpdateReceiver {
    /// The next update without waiting. Every structural result queued is
    /// returned before any periodic update, each lane in the order it was sent,
    /// so telemetry never arrives ahead of the channel it describes.
    pub fn try_recv(&mut self) -> Result<UiUpdate, mpsc::error::TryRecvError> {
        if let Ok(update) = self.reliable.try_recv() {
            return Ok(update);
        }
        self.periodic.try_recv()
    }

    /// The next update, waiting for one; `None` once the driver is gone and
    /// both lanes are empty. Ordered as [`try_recv`](Self::try_recv).
    pub async fn recv(&mut self) -> Option<UiUpdate> {
        tokio::select! {
            biased;
            Some(update) = self.reliable.recv() => Some(update),
            update = self.periodic.recv() => update,
        }
    }
}

/// Make the two lanes between the driver and the GUI.
///
/// The periodic lane holds `periodic_capacity` updates and drops what does not
/// fit: the next poll sends fresh telemetry anyway. Runtime events ride this
/// lane too, so they stay in order with the telemetry they precede. The
/// reliable lane is unbounded and never drops, because structural results are
/// not re-sent and the GUI cannot rebuild a missing one. Only a user action or
/// a loaded profile produces one, so its volume follows what users do and the
/// size of a profile, never the polling rate or what the Channels do on their
/// own.
pub fn update_lanes(periodic_capacity: usize) -> (UpdateSender, UpdateReceiver) {
    let (periodic_tx, periodic_rx) = mpsc::channel(periodic_capacity);
    let (reliable_tx, reliable_rx) = mpsc::unbounded_channel();
    (
        UpdateSender {
            periodic: periodic_tx,
            reliable: reliable_tx,
        },
        UpdateReceiver {
            periodic: periodic_rx,
            reliable: reliable_rx,
        },
    )
}

/// Run file work on the blocking pool and await it, so a slow disk or network
/// share occupies a blocking thread and not an async worker. A panic in the
/// work comes back as an error message.
async fn run_blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| format!("the file work did not finish: {err}"))
}

/// Whether a registered profile can resume (ADR-045). It must still be where it
/// was registered, load, and have only valid channels; and every folder a
/// channel records to when it starts must carry its destination marker
/// (ADR-043) — without it the folder may be on a different disk, such as an
/// empty mount point whose drive is not plugged in. Only reads files.
pub(crate) fn resume_check(path: &std::path::Path) -> Result<(Profile, Vec<String>), String> {
    if !path.is_file() {
        return Err(format!("{} is no longer there", path.display()));
    }
    let (profile, warnings) = Profile::load_with_warnings(path)
        .map_err(|err| format!("{} does not load: {err}", path.display()))?;
    let invalid: Vec<String> = profile
        .validate()
        .into_iter()
        .filter_map(|(name, result)| {
            result
                .err()
                .map(|errors| format!("\"{name}\": {}", describe_channel_errors(&errors)))
        })
        .collect();
    if !invalid.is_empty() {
        return Err(format!(
            "{} has invalid channels:\n{}",
            path.display(),
            invalid.join("\n")
        ));
    }
    for channel in &profile.channels {
        let raw = &channel.raw_recording;
        let display = &channel.display_recording;
        for (destination, rotation) in [
            (
                raw.destination.as_ref().filter(|_| raw.enabled),
                raw.file_rotation,
            ),
            (
                display.destination.as_ref().filter(|_| display.enabled),
                display.file_rotation,
            ),
        ] {
            let Some(destination) = destination else {
                continue;
            };
            let folder = crate::record::recording_folder(destination, rotation);
            if !folder.join(crate::record::DESTINATION_MARKER).is_file() {
                return Err(format!(
                    "channel \"{}\" records to {}, which has no {} marker: it may be a \
                     different disk, or nothing has been recorded there yet",
                    channel.name.as_str(),
                    folder.display(),
                    crate::record::DESTINATION_MARKER
                ));
            }
        }
    }
    Ok((profile, warnings))
}

/// A one-line, human-readable description of a channel's interface and endpoint,
/// for the connection-details line in the UI.
fn describe_interface(config: &ChannelConfig) -> String {
    match &config.interface {
        InterfaceConfig::Udp(udp) => {
            let mode = match udp.mode {
                UdpMode::Unicast => "unicast",
                UdpMode::Broadcast => "broadcast",
                UdpMode::Multicast => "multicast",
            };
            let endpoint = format!("UDP {mode} · bind {}:{}", udp.bind_address, udp.port);
            // Who can reach it, in words (§15, ADR-047).
            match BindScope::of(&udp.bind_address).words() {
                Some(scope) => format!("{endpoint} · {scope}"),
                None => endpoint,
            }
        }
        InterfaceConfig::Serial(serial) => {
            // Conventional serial notation — `9600,8,N,1` — so the line reads
            // the way the settings are written down and spoken, rather than
            // spelling out one term and omitting the rest.
            let data = match serial.data_bits {
                DataBits::Five => 5,
                DataBits::Six => 6,
                DataBits::Seven => 7,
                DataBits::Eight => 8,
            };
            let parity = match serial.parity {
                Parity::None => "N",
                Parity::Even => "E",
                Parity::Odd => "O",
                Parity::Mark => "M",
                Parity::Space => "S",
            };
            let stop = match serial.stop_bits {
                StopBits::One => "1",
                StopBits::OnePointFive => "1.5",
                StopBits::Two => "2",
            };
            let flow = match serial.flow_control {
                FlowControl::None => "None",
                FlowControl::XonXoff => "XON/XOFF",
                FlowControl::RtsCts => "RTS/CTS",
            };
            format!(
                "Serial: {} {},{data},{parity},{stop} flow:{flow}",
                serial.port, serial.baud_rate,
            )
        }
    }
}

/// Derive a profile name from its file path: the file stem, or a fallback. The
/// schema requires a `name`; the filename is the natural default for a Save.
fn profile_name_from_path(path: &std::path::Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "workspace".to_string())
}

/// The background driver: owns the `Listener`, drains commands, forwards events,
/// and polls snapshots. Built by [`spawn`]; runs until `Shutdown` or the command
/// channel closes (the App dropped its sender).
/// One step of a Channel's lifecycle work in the driver (ADR-052). A Channel
/// runs its steps one at a time, in order; an interface open or a stop's drain
/// runs off the loop, so every other readout keeps updating meanwhile.
enum Step {
    /// Start; `report` says whether a failure is shown on the Channel. A
    /// reconnect attempt's is not: its reconnect status says it.
    Start {
        report: bool,
    },
    Stop,
    /// Stop a Running or Faulted Channel; nothing otherwise.
    StopIfLive,
    /// Bring the Channel up unless it is Running, stopping a Faulted one
    /// first (§8.5).
    StartIfDown,
    /// Adopt a configuration (§13) and echo it to the UI.
    Commit(Box<ChannelConfig>),
    /// Drop the Channel from the runtime and the UI.
    Remove,
    /// Record how a reconnect attempt went (§9.1).
    Reconnected(Box<ReconnectAttempt>),
}

/// What Linemon's window last asked of its hidden Channel (spec §50.3).
enum LinemonRequest {
    /// Run on this config.
    Open(Box<ChannelConfig>),
    /// Stop.
    Close,
}

/// Linemon's hidden Channel. It is registered with the runtime but never in
/// the driver's `channels`, so nothing that walks the user's Channels reaches
/// it, and everything about it goes out as [`UiUpdate::Linemon`].
struct LinemonChannel {
    id: ChannelId,
    /// The window wants the port open. Polling and error reports follow it.
    open: bool,
    /// The newest request that arrived while a step of the hidden Channel was
    /// in flight. It replaces any older one and runs when the Channel's steps
    /// are done, so a burst of changes ends on the last without queueing the
    /// ones between.
    waiting: Option<LinemonRequest>,
    /// Where the window's stream is up to.
    cursor: StreamPollCursor,
}

/// The name the hidden Channel has in the runtime. The runtime does not check
/// names for uniqueness, so a user Channel of the same name cannot collide.
const LINEMON_CHANNEL_NAME: &str = "Linemon";

/// `config` as Linemon's hidden Channel runs it: named for Linemon, and with
/// no recording, since Linemon is never a recording (spec §50.3).
fn linemon_config(mut config: ChannelConfig) -> ChannelConfig {
    config.name = ChannelName::new(LINEMON_CHANNEL_NAME);
    config.raw_recording.enabled = false;
    config.display_recording.enabled = false;
    config
}

/// The Channel a runtime event is about.
fn event_channel(event: &RuntimeEvent) -> ChannelId {
    match event {
        RuntimeEvent::ChannelStarted(id)
        | RuntimeEvent::ChannelStopped(id)
        | RuntimeEvent::ChannelFaulted(id)
        | RuntimeEvent::RecordingFaulted(id, _)
        | RuntimeEvent::RecordingStarted(id, _)
        | RuntimeEvent::WarningRaised(id)
        | RuntimeEvent::ReceptionStalled(id, _)
        | RuntimeEvent::ReceptionResumed(id, _)
        | RuntimeEvent::ControlLinesChanged(id)
        | RuntimeEvent::ChannelReconnecting(id, _)
        | RuntimeEvent::ChannelReconnected(id)
        | RuntimeEvent::ChannelReconnectGaveUp(id)
        | RuntimeEvent::DiskSpaceLow(id)
        | RuntimeEvent::RecordingStoppedLowDisk(id)
        | RuntimeEvent::MatchTriggered(id, _) => *id,
    }
}

/// A Channel's queued steps, and how its last step off the loop went.
#[derive(Default)]
struct ChannelSteps {
    queue: VecDeque<Step>,
    /// A step is running off the loop; the queue waits for it.
    running: bool,
    /// Whether the running start's failure is shown on the Channel.
    report: bool,
    /// Whether the last start reached Running.
    started: bool,
}

/// What a step running off the loop sends back when it finishes.
enum StepDone {
    Opened(Box<OpenedStart>),
    Drained(Box<DrainedStop>),
    /// Profile file work finished (see [`ProfileWork`]).
    Profile(Box<ProfileDone>),
}

/// Profile file work, run off the loop so a slow or hung disk or network share
/// cannot hold the driver. One piece runs at a time, in the order the commands
/// came; a command issued while one is outstanding waits behind it. The loop
/// keeps serving every other command meanwhile, so a Channel's state may change
/// before the work lands.
enum ProfileWork {
    /// Write the Channel configs gathered when the command was issued.
    Save {
        path: std::path::PathBuf,
        profile: Box<Profile>,
    },
    Load {
        path: std::path::PathBuf,
    },
    CheckResume {
        path: std::path::PathBuf,
    },
    Resume {
        path: std::path::PathBuf,
    },
}

type LoadedProfile = Result<(Profile, Vec<String>), String>;

/// What finished [`ProfileWork`] hands back to the loop.
enum ProfileDone {
    Saved {
        path: std::path::PathBuf,
        result: Result<(), String>,
    },
    Loaded(LoadedProfile),
    Checked {
        path: std::path::PathBuf,
        result: Result<String, String>,
    },
    ResumeRead {
        path: std::path::PathBuf,
        result: LoadedProfile,
    },
}

impl ProfileWork {
    /// Do the file work, on the blocking pool. `before_files` runs first on that
    /// thread (tests use it to hold the work).
    async fn run(self, before_files: FileHook) -> ProfileDone {
        let hold = move || {
            if let Some(hook) = before_files {
                hook();
            }
        };
        match self {
            Self::Save { path, profile } => {
                let saving = path.clone();
                let result = run_blocking(move || {
                    hold();
                    profile.save(&saving).map_err(|err| err.to_string())
                })
                .await
                .and_then(|saved| saved);
                ProfileDone::Saved { path, result }
            }
            Self::Load { path } => ProfileDone::Loaded(
                run_blocking(move || {
                    hold();
                    Profile::load_with_warnings(&path).map_err(|err| err.to_string())
                })
                .await
                .and_then(|loaded| loaded),
            ),
            Self::CheckResume { path } => {
                let checking = path.clone();
                let result = run_blocking(move || {
                    hold();
                    resume_check(&checking)
                })
                .await
                .and_then(|checked| checked)
                .map(|(profile, _)| profile.name);
                ProfileDone::Checked { path, result }
            }
            Self::Resume { path } => {
                let checking = path.clone();
                let result = run_blocking(move || {
                    hold();
                    resume_check(&checking)
                })
                .await
                .and_then(|checked| checked);
                ProfileDone::ResumeRead { path, result }
            }
        }
    }
}

/// Runs on the blocking thread before any profile file is touched. Always
/// `None` outside tests.
type FileHook = Option<std::sync::Arc<dyn Fn() + Send + Sync>>;

/// Where each poll round starts, so that Channels left unreached by one round
/// are the first of the next.
#[derive(Default)]
struct PollRotation {
    /// Where the next round starts.
    next: usize,
    /// Where this round started, and how many Channels it has visited.
    start: usize,
    visited: usize,
}

impl PollRotation {
    /// Begin a round over `len` Channels, from where the last round stopped.
    fn begin(&mut self, len: usize) {
        self.start = if len == 0 { 0 } else { self.next % len };
        self.visited = 0;
    }

    /// The next Channel to visit in this round, or `None` when the round is
    /// over: every Channel visited, or `limit` passed since `began`, in which
    /// case the next round starts with the first Channel not visited. At least
    /// one Channel is visited per round, so a round always makes progress.
    fn next_in_round(
        &mut self,
        ids: &[ChannelId],
        began: tokio::time::Instant,
        limit: Duration,
    ) -> Option<ChannelId> {
        if ids.is_empty() {
            return None;
        }
        if self.visited >= ids.len() {
            self.next = 0;
            return None;
        }
        let at = (self.start + self.visited) % ids.len();
        if self.visited > 0 && began.elapsed() >= limit {
            self.next = at;
            return None;
        }
        self.visited += 1;
        Some(ids[at])
    }
}

/// A workspace change, made once no Channel has steps left (ADR-052).
enum WorkspaceStep {
    /// Forget the selection and the stream cursors.
    Clear,
    /// Register a profile's Channels, and start them for a resume.
    Load {
        profile: Box<Profile>,
        /// What loading the file had to give up (see `Profile::load_with_warnings`).
        warnings: Vec<String>,
        start: bool,
    },
    /// Announce a resume, after the runtime events of its starts.
    Resumed(String),
}

pub struct Driver {
    listener: Listener,
    events: Receiver<RuntimeEvent>,
    commands: Receiver<UiCommand>,
    updates: UpdateSender,
    repaint: Box<dyn Fn() + Send>,
    /// Channels registered so far, polled for stats.
    channels: Vec<ChannelId>,
    /// The channel currently on screen; only this one gets a snapshot + stream delta
    /// polled (the rest get cheap stats).
    selected: Option<ChannelId>,
    /// Channels with a pop-out window open (spec §50.4). Like the selected
    /// channel they get a stream delta each poll, and they keep the cheap stats.
    popouts: HashSet<ChannelId>,
    /// Per-channel live stream cursors (§87, ADR-009). Preserving these across
    /// selection changes avoids re-shipping the retained window whenever the user
    /// revisits a tab. Generation changes still force a reliable run reset.
    stream_cursors: HashMap<ChannelId, StreamPollCursor>,
    /// Each Channel's queued lifecycle steps (ADR-052).
    steps: HashMap<ChannelId, ChannelSteps>,
    /// Workspace changes waiting for every Channel's steps to finish.
    workspace: VecDeque<WorkspaceStep>,
    /// Steps running off the loop report here. Unbounded, because each Channel
    /// has at most one step running.
    done_tx: mpsc::UnboundedSender<StepDone>,
    done_rx: mpsc::UnboundedReceiver<StepDone>,
    /// Profile file work waiting its turn behind the piece running now.
    profile_work: VecDeque<ProfileWork>,
    /// A piece of profile file work is running off the loop. It is not a
    /// Channel step: Shutdown does not wait for it.
    profile_running: bool,
    /// Where the next status poll round starts.
    poll_rotation: PollRotation,
    /// Linemon's hidden Channel, once the window has opened it.
    linemon: Option<LinemonChannel>,
    /// Held by the profile file work before it touches a file (tests only).
    #[cfg(test)]
    file_hook: FileHook,
}

impl Driver {
    /// Build a driver over an already-constructed `Listener` whose event stream has
    /// been taken. (`spawn` does this for the real GUI; tests build it directly.)
    pub fn new(
        listener: Listener,
        events: Receiver<RuntimeEvent>,
        commands: Receiver<UiCommand>,
        updates: UpdateSender,
        repaint: Box<dyn Fn() + Send>,
    ) -> Self {
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        Self {
            listener,
            events,
            commands,
            updates,
            repaint,
            channels: Vec::new(),
            selected: None,
            popouts: HashSet::new(),
            stream_cursors: HashMap::new(),
            steps: HashMap::new(),
            workspace: VecDeque::new(),
            done_tx,
            done_rx,
            profile_work: VecDeque::new(),
            profile_running: false,
            poll_rotation: PollRotation::default(),
            linemon: None,
            #[cfg(test)]
            file_hook: None,
        }
    }

    fn file_hook(&self) -> FileHook {
        #[cfg(test)]
        {
            self.file_hook.clone()
        }
        #[cfg(not(test))]
        {
            None
        }
    }

    /// Run until shutdown. Drains commands, forwards events, and on a timer polls
    /// each known channel for a snapshot and ticks auto-reconnect.
    pub async fn run(mut self) {
        let mut snapshot_tick = tokio::time::interval(SNAPSHOT_INTERVAL);
        snapshot_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut reconnect_tick = tokio::time::interval(RECONNECT_INTERVAL);
        reconnect_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut events_open = true;

        loop {
            tokio::select! {
                cmd = self.commands.recv() => match cmd {
                    Some(cmd) => {
                        if !self.handle(cmd).await {
                            break; // Shutdown
                        }
                    }
                    None => break, // the App dropped its command sender — close down
                },
                ev = self.events.recv(), if events_open => match ev {
                    Some(ev) => self.forward_event(ev),
                    None => events_open = false, // stream closed; keep serving commands
                },
                Some(done) = self.done_rx.recv() => self.on_done(done),
                _ = snapshot_tick.tick() => self.poll_snapshots().await,
                _ = reconnect_tick.tick() => self.queue_reconnects(),
            }
            self.pump().await;
        }

        // The App is gone (or asked to shut down): let what is opening or
        // draining land, then stop every channel cleanly.
        self.finish_in_flight().await;
        self.listener.shutdown().await;
    }

    /// Queue `steps` for a Channel, after any it already has (ADR-052).
    fn queue(&mut self, id: ChannelId, steps: impl IntoIterator<Item = Step>) {
        self.steps.entry(id).or_default().queue.extend(steps);
    }

    /// The steps of "optionally adopt `config`, optionally start" (§13): a live
    /// Channel stops first, so it comes back up on the new configuration.
    fn commit_steps(config: Option<Box<ChannelConfig>>, start: bool) -> Vec<Step> {
        let mut steps = Vec::new();
        if let Some(config) = config {
            steps.extend([Step::StopIfLive, Step::Commit(config)]);
        }
        if start {
            steps.push(Step::StartIfDown);
        }
        steps
    }

    /// Stop and remove every Channel.
    fn queue_removal_of_all(&mut self) {
        for id in self.channels.clone() {
            self.queue(id, [Step::StopIfLive, Step::Remove]);
        }
    }

    /// Run every step that can run now and, once no Channel has steps left,
    /// the next workspace change (ADR-052).
    async fn pump(&mut self) {
        self.start_profile_work();
        loop {
            self.dispatch_linemon_waiting();
            let ready: Vec<ChannelId> = self
                .steps
                .iter()
                .filter(|(_, steps)| !steps.running && !steps.queue.is_empty())
                .map(|(id, _)| *id)
                .collect();
            for id in ready {
                while let Some(step) = self.next_step(id) {
                    self.run_step(id, step).await;
                }
            }
            self.steps
                .retain(|_, steps| steps.running || !steps.queue.is_empty());
            if self.linemon_ready() {
                continue;
            }
            // Linemon's hidden Channel is not part of the workspace, so its
            // steps do not hold a workspace change back.
            if self.steps.keys().any(|id| !self.is_linemon(*id)) {
                break;
            }
            match self.workspace.pop_front() {
                Some(change) => self.run_workspace(change),
                None => break,
            }
        }
    }

    fn is_linemon(&self, id: ChannelId) -> bool {
        self.linemon
            .as_ref()
            .is_some_and(|linemon| linemon.id == id)
    }

    /// Whether a request is waiting for Linemon's hidden Channel and the
    /// Channel has no steps left to wait for.
    fn linemon_ready(&self) -> bool {
        self.linemon.as_ref().is_some_and(|linemon| {
            linemon.waiting.is_some() && !self.steps.contains_key(&linemon.id)
        })
    }

    /// Queue the steps of the request waiting for Linemon's hidden Channel,
    /// once it is free.
    fn dispatch_linemon_waiting(&mut self) {
        if !self.linemon_ready() {
            return;
        }
        let Some(linemon) = self.linemon.as_mut() else {
            return;
        };
        let id = linemon.id;
        if let Some(request) = linemon.waiting.take() {
            self.queue_linemon(id, request);
        }
    }

    /// Queue the steps that carry out one Linemon request: the same stop,
    /// commit and start "Apply & Restart" runs (§13).
    fn queue_linemon(&mut self, id: ChannelId, request: LinemonRequest) {
        let steps = match request {
            LinemonRequest::Open(config) => Self::commit_steps(Some(config), true),
            LinemonRequest::Close => vec![Step::StopIfLive],
        };
        self.queue(id, steps);
    }

    /// Take a request from Linemon's window. The hidden Channel is registered on
    /// the first open and reused after. A request that arrives while a step of
    /// the Channel is in flight waits as the one newest request.
    fn linemon_request(&mut self, request: LinemonRequest) {
        let id = match (&self.linemon, &request) {
            (Some(linemon), _) => linemon.id,
            (None, LinemonRequest::Open(config)) => {
                let id = self.listener.add_channel((**config).clone());
                self.linemon = Some(LinemonChannel {
                    id,
                    open: false,
                    waiting: None,
                    cursor: StreamPollCursor::default(),
                });
                id
            }
            (None, LinemonRequest::Close) => return,
        };
        let busy = self.steps.contains_key(&id);
        let Some(linemon) = self.linemon.as_mut() else {
            return;
        };
        linemon.open = matches!(request, LinemonRequest::Open(_));
        if busy {
            linemon.waiting = Some(request);
        } else {
            linemon.waiting = None;
            self.queue_linemon(id, request);
        }
    }

    /// A Channel's next step, unless one is still running off the loop.
    fn next_step(&mut self, id: ChannelId) -> Option<Step> {
        let steps = self.steps.get_mut(&id)?;
        if steps.running {
            None
        } else {
            steps.queue.pop_front()
        }
    }

    async fn run_step(&mut self, id: ChannelId, step: Step) {
        match step {
            Step::Start { report } => self.start_step(id, report),
            Step::Stop => self.stop_step(id),
            Step::StopIfLive => {
                if matches!(
                    self.listener.state(id),
                    Some(ChannelState::Running | ChannelState::Faulted)
                ) {
                    self.stop_step(id);
                }
            }
            Step::StartIfDown => {
                let state = self.listener.state(id);
                if state != Some(ChannelState::Running) {
                    self.steps
                        .entry(id)
                        .or_default()
                        .queue
                        .push_front(Step::Start { report: true });
                    if state == Some(ChannelState::Faulted) {
                        self.stop_step(id);
                    }
                }
            }
            Step::Commit(config) => {
                let details = describe_interface(&config);
                match self.listener.commit_config(id, (*config).clone()) {
                    // The window knows what it asked for.
                    Ok(()) if self.is_linemon(id) => {}
                    Ok(()) => {
                        self.push(UiUpdate::ChannelReconfigured(id, details, config));
                    }
                    Err(err) => self.push_channel_error(id, err),
                }
            }
            Step::Remove => {
                // Already stopped by the step before, so this does not wait.
                let _ = self.listener.remove_channel(id).await;
                self.channels.retain(|c| *c != id);
                self.stream_cursors.remove(&id);
                self.popouts.remove(&id);
                self.push(UiUpdate::ChannelRemoved(id));
            }
            Step::Reconnected(attempt) => {
                let started = self.steps.get(&id).is_some_and(|steps| steps.started);
                self.listener
                    .finish_reconnect(*attempt, started, std::time::Instant::now());
            }
        }
    }

    /// Begin a start, and open its interface off the loop (ADR-052).
    fn start_step(&mut self, id: ChannelId, report: bool) {
        match self.listener.begin_start(id) {
            Ok(ticket) => {
                let steps = self.steps.entry(id).or_default();
                steps.running = true;
                steps.report = report;
                let done = self.done_tx.clone();
                tokio::spawn(async move {
                    let _ = done.send(StepDone::Opened(Box::new(ticket.open().await)));
                });
            }
            Err(err) => {
                self.steps.entry(id).or_default().started = false;
                if report {
                    self.push_channel_error(id, err);
                }
            }
        }
    }

    /// Begin a stop, and drain it off the loop (ADR-052).
    fn stop_step(&mut self, id: ChannelId) {
        match self.listener.begin_stop(id) {
            Ok(ticket) => {
                self.steps.entry(id).or_default().running = true;
                let done = self.done_tx.clone();
                tokio::spawn(async move {
                    let _ = done.send(StepDone::Drained(Box::new(ticket.drain().await)));
                });
            }
            Err(err) => self.push_channel_error(id, err),
        }
    }

    /// Land a step that ran off the loop (ADR-052).
    fn on_done(&mut self, done: StepDone) {
        match done {
            StepDone::Opened(opened) => {
                let id = opened.id();
                let result = self.listener.finish_start(*opened);
                let steps = self.steps.entry(id).or_default();
                steps.running = false;
                steps.started = result.is_ok();
                let report = steps.report;
                if let Err(err) = result {
                    if report {
                        self.push_channel_error(id, err);
                    }
                }
            }
            StepDone::Drained(drained) => {
                let id = drained.id();
                self.listener.finish_stop(*drained);
                self.steps.entry(id).or_default().running = false;
            }
            StepDone::Profile(done) => {
                self.profile_running = false;
                self.land_profile(*done);
            }
        }
    }

    /// Start the next piece of profile file work unless one is running, off
    /// the loop.
    fn start_profile_work(&mut self) {
        if self.profile_running {
            return;
        }
        let Some(work) = self.profile_work.pop_front() else {
            return;
        };
        self.profile_running = true;
        let done = self.done_tx.clone();
        let hook = self.file_hook();
        tokio::spawn(async move {
            let _ = done.send(StepDone::Profile(Box::new(work.run(hook).await)));
        });
    }

    /// Land profile file work that finished: report it, or queue the
    /// workspace change it asked for.
    fn land_profile(&mut self, done: ProfileDone) {
        match done {
            ProfileDone::Saved { path, result } => match result {
                Ok(()) => {
                    self.push(UiUpdate::ProfileSaved(path));
                }
                Err(err) => {
                    self.push(UiUpdate::ProfileError(format!("save failed: {err}")));
                }
            },
            ProfileDone::Loaded(result) => match result {
                Ok((profile, warnings)) => self.replace_workspace(profile, warnings, false, None),
                Err(err) => {
                    self.push(UiUpdate::ProfileError(format!("load failed: {err}")));
                }
            },
            ProfileDone::Checked { path, result } => {
                self.push(UiUpdate::ResumeChecked(path, result));
            }
            ProfileDone::ResumeRead { path, result } => match result {
                Ok((profile, warnings)) => {
                    let name = profile.name.clone();
                    self.replace_workspace(profile, warnings, true, Some(name));
                }
                Err(why) => {
                    self.push(UiUpdate::ResumeChecked(path, Err(why)));
                }
            },
        }
    }

    /// Queue the teardown of every Channel, then registering `profile`'s: and
    /// for a resume, starting them and announcing it as `resumed`.
    fn replace_workspace(
        &mut self,
        profile: Profile,
        warnings: Vec<String>,
        start: bool,
        resumed: Option<String>,
    ) {
        self.queue_removal_of_all();
        self.workspace.push_back(WorkspaceStep::Clear);
        self.workspace.push_back(WorkspaceStep::Load {
            profile: Box::new(profile),
            warnings,
            start,
        });
        if let Some(name) = resumed {
            self.workspace.push_back(WorkspaceStep::Resumed(name));
        }
    }

    /// Queue the reconnect attempts now due (§9.1): each a stop and a start,
    /// off the loop like any other (ADR-052). A Channel mid-step waits.
    fn queue_reconnects(&mut self) {
        // Linemon's hidden Channel is never reconnected: the window opens it
        // again when it wants it.
        let linemon = self.linemon.as_ref().map(|linemon| linemon.id);
        let attempts = self
            .listener
            .reconnects_due(std::time::Instant::now(), |id| {
                self.steps.contains_key(&id) || Some(id) == linemon
            });
        for attempt in attempts {
            let id = attempt.id();
            self.queue(
                id,
                [
                    Step::StopIfLive,
                    Step::Start { report: false },
                    Step::Reconnected(Box::new(attempt)),
                ],
            );
        }
    }

    fn run_workspace(&mut self, change: WorkspaceStep) {
        match change {
            WorkspaceStep::Clear => {
                self.selected = None;
                self.popouts.clear();
                self.stream_cursors.clear();
            }
            WorkspaceStep::Load {
                profile,
                warnings,
                start,
            } => {
                let added = self.register_profile(*profile, &warnings);
                if start {
                    for id in added {
                        self.queue(id, [Step::Start { report: true }]);
                    }
                }
            }
            WorkspaceStep::Resumed(name) => {
                self.drain_events();
                self.push(UiUpdate::Resumed(name, std::time::SystemTime::now()));
            }
        }
    }

    /// Land every step still running off the loop, starting no more, within
    /// the stop grace: an open still hanging then is abandoned.
    async fn finish_in_flight(&mut self) {
        for steps in self.steps.values_mut() {
            steps.queue.clear();
        }
        let deadline = tokio::time::Instant::now() + Listener::STOP_GRACE;
        while self.steps.values().any(|steps| steps.running) {
            match tokio::time::timeout_at(deadline, self.done_rx.recv()).await {
                // Profile file work that lands now changes nothing: the app is
                // closing, and starting a teardown here would only delay it.
                Ok(Some(StepDone::Profile(_))) => {}
                Ok(Some(done)) => self.on_done(done),
                _ => break,
            }
        }
    }

    /// Forward one runtime event to the GUI, applying the stream-cursor reset a
    /// (re)start needs (§87). Shared by the `select!` loop and `drain_events`.
    fn forward_event(&mut self, ev: RuntimeEvent) {
        // A (re)start resets the channel's stream offset to 0. Forget the old run's
        // cursor even if this advisory event cannot be forwarded; StreamDelta's
        // generation remains the fallback when the runtime event itself is dropped.
        if let RuntimeEvent::ChannelStarted(id) | RuntimeEvent::ChannelReconnected(id) = ev {
            self.stream_cursors.remove(&id);
        }
        // Linemon's hidden Channel is not a Channel to the main window: its
        // state reaches Linemon through the snapshot.
        let about = event_channel(&ev);
        if let Some(linemon) = self.linemon.as_mut().filter(|l| l.id == about) {
            if matches!(
                ev,
                RuntimeEvent::ChannelStarted(_) | RuntimeEvent::ChannelReconnected(_)
            ) {
                linemon.cursor = StreamPollCursor::default();
            }
            return;
        }
        self.push(UiUpdate::Event(ev));
    }

    /// Forward every event currently queued in the runtime→driver channel, without
    /// blocking. The driver owns the `Listener` in this task, so while a long command
    /// handler (`StartAll`/`StopAll`) awaits the runtime, the `select!` arm that
    /// normally drains events isn't running — the runtime's `try_send` lifecycle
    /// events would just pile up in the bounded channel. Calling this after each
    /// per-channel await keeps it drained so a large batch can't overflow it and lose
    /// a `ChannelStarted`/`ChannelStopped` (the bug behind stale GUI status).
    fn drain_events(&mut self) {
        while let Ok(ev) = self.events.try_recv() {
            self.forward_event(ev);
        }
    }

    /// Apply one command. Returns `false` only for `Shutdown` (end the loop).
    async fn handle(&mut self, cmd: UiCommand) -> bool {
        match cmd {
            UiCommand::AddChannel(config) => {
                let name = config.name.as_str().to_string();
                let details = describe_interface(&config);
                let echo = config.clone();
                let id = self.listener.add_channel(*config);
                self.channels.push(id);
                self.push(UiUpdate::ChannelAdded(id, name, details, echo));
            }
            // Lifecycle commands queue steps (ADR-052): each Channel runs its
            // own in order, and opens and drains run off this loop. A failure is
            // surfaced on the Channel (e.g. a bind "address in use"), rather than
            // leaving it Faulted with no explanation.
            UiCommand::Start(id) => self.queue(id, [Step::Start { report: true }]),
            UiCommand::Stop(id) => self.queue(id, [Step::Stop]),
            UiCommand::CommitAndStart { id, config, start } => {
                self.queue(id, Self::commit_steps(config, start));
            }
            UiCommand::StartAll(batch) => {
                for (id, config) in batch {
                    self.queue(id, Self::commit_steps(Some(config), true));
                }
            }
            // A Channel already Stopped is skipped, so only a genuine problem
            // surfaces as an error.
            UiCommand::StopAll => {
                for id in self.channels.clone() {
                    self.queue(id, [Step::StopIfLive]);
                }
            }
            UiCommand::RemoveChannel(id) => self.queue(id, [Step::StopIfLive, Step::Remove]),
            UiCommand::Rename(id, name) => {
                if self.listener.rename(id, name.clone()).is_ok() {
                    self.push(UiUpdate::ChannelRenamed(id, name.as_str().to_string()));
                }
            }
            UiCommand::PauseDisplay(id) => {
                let _ = self.listener.pause_display(id);
            }
            UiCommand::ResumeDisplay(id) => {
                let _ = self.listener.resume_display(id);
            }
            // A line command for a Channel that is not a running serial Channel
            // is ignored; one its serial reader did not take in time is said so.
            UiCommand::SetRts(id, on) => {
                let result = self.listener.set_rts(id, on).await;
                self.report_untaken(id, result);
            }
            UiCommand::SetDtr(id, on) => {
                let result = self.listener.set_dtr(id, on).await;
                self.report_untaken(id, result);
            }
            UiCommand::SetRecording(id, enabled, raw_config) => {
                // Arm from the settings read at click time (passed from the editor), so
                // recording goes exactly where the controls say — no Apply needed. A
                // `false` return means the command didn't reach a running data channel;
                // a begin that reaches the pipeline but can't open the file reports
                // separately via RecordingFaulted (§55).
                match self.listener.set_recording(id, enabled, *raw_config).await {
                    Ok(true) => {}
                    Ok(false) => self
                        .push_channel_error(id, "can't change recording — channel isn't running"),
                    Err(err) => self.push_channel_error(id, err),
                }
            }
            UiCommand::SetRawRecordingConfig(id, raw) => {
                // Persist the Raw recording settings into the stored config (no restart),
                // so a profile save captures them. Arming the live recorder is separate
                // (SetRecording).
                self.listener.set_raw_recording_config(id, *raw);
            }
            UiCommand::SetDisplayRecording(id, enabled, display) => {
                // The Raw toggle's sibling (§54): arm from click-time settings; a begin
                // that can't open the file reports via RecordingFaulted (§55).
                match self
                    .listener
                    .set_display_recording(id, enabled, *display)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => self.push_channel_error(
                        id,
                        "can't change display recording — channel isn't running",
                    ),
                    Err(err) => self.push_channel_error(id, err),
                }
            }
            UiCommand::SetDisplayRecordingConfig(id, display) => {
                self.listener.set_display_recording_config(id, *display);
            }
            UiCommand::SetReconnect(id, policy) => {
                if let Err(err) = self.listener.set_reconnect_policy(id, policy) {
                    self.push_channel_error(id, err);
                }
            }
            UiCommand::SetViewConfig(id, display, retention) => {
                // View settings render GUI-side and the scroll buffer is capped GUI-side
                // live, so this just keeps the stored config current (no restart): for a
                // profile save, and so the runtime adopts the retention on next start.
                self.listener.set_view_config(id, *display, *retention);
            }
            UiCommand::Select(id) => {
                // Each channel retains its own cursor, so revisiting a tab fetches only
                // bytes that arrived while it was off-screen.
                self.selected = id;
            }
            UiCommand::SetPopouts(ids) => {
                self.popouts = ids
                    .into_iter()
                    .filter(|id| self.channels.contains(id))
                    .collect();
            }
            // Profile file work runs off the loop, one piece at a time (see
            // `ProfileWork`).
            UiCommand::SaveProfile(path) => self.save_profile(path),
            UiCommand::LoadProfile(path) => self.profile_work.push_back(ProfileWork::Load { path }),
            UiCommand::CheckResume(path) => {
                self.profile_work
                    .push_back(ProfileWork::CheckResume { path });
            }
            UiCommand::Resume(path) => self.profile_work.push_back(ProfileWork::Resume { path }),
            UiCommand::ListLocalAddresses => {
                self.push(UiUpdate::LocalAddresses(local_addresses()));
            }
            UiCommand::NewProfile => self.new_profile(),
            UiCommand::LinemonOpen(config) => {
                self.linemon_request(LinemonRequest::Open(Box::new(linemon_config(*config))));
            }
            UiCommand::LinemonClose => self.linemon_request(LinemonRequest::Close),
            UiCommand::LinemonSetRts(on) => self.linemon_set_line(true, on).await,
            UiCommand::LinemonSetDtr(on) => self.linemon_set_line(false, on).await,
            UiCommand::Shutdown => return false,
        }
        true
    }

    /// Queue saving every registered channel's config to a TOML profile (§67).
    /// Gathers configs from the authoritative `Listener` in `self.channels`
    /// order when the command arrives; a missing config (a channel removed
    /// mid-flight) is skipped. Non-fatal: an I/O or serialization error is
    /// reported via `ProfileError`, not a panic.
    fn save_profile(&mut self, path: std::path::PathBuf) {
        let mut profile = Profile::new(profile_name_from_path(&path));
        profile.channels = self
            .channels
            .iter()
            .filter_map(|id| self.listener.config(*id).cloned())
            .collect();
        self.profile_work.push_back(ProfileWork::Save {
            path,
            profile: Box::new(profile),
        });
    }

    /// Tear the workspace down and leave it empty: every Channel is stopped
    /// (§8.5) and removed, then the selection and cursors are cleared.
    fn new_profile(&mut self) {
        self.queue_removal_of_all();
        self.workspace.push_back(WorkspaceStep::Clear);
    }

    /// Say so when a line command was not taken in time. Any other refusal
    /// (not a running serial Channel) is ignored, as the lines are not shown
    /// then.
    fn report_untaken(&self, id: ChannelId, result: Result<(), OrchestratorError>) {
        if let Err(err @ OrchestratorError::NotTakenInTime(_)) = result {
            self.push_channel_error(id, err);
        }
    }

    /// Register a profile's Channels Stopped, and return their ids. Each
    /// addition emits the usual update, so the App folds the swap with no
    /// special-casing.
    fn register_profile(&mut self, profile: Profile, warnings: &[String]) -> Vec<ChannelId> {
        // Validate before registering (§71, ADR-014): a hand-edited profile can carry an
        // invalid channel (e.g. a duplicate name) — skip those and load the rest, like
        // the CLI. Validation is workspace-level (`Profile::validate`), so duplicate
        // names flag every carrier.
        let results = profile.validate();
        let mut skipped: Vec<String> = Vec::new();
        let mut added = Vec::new();
        let profile_name = profile.name.clone();
        let channels = profile.into_effective_channels();
        for (config, (name, result)) in channels.into_iter().zip(results) {
            if let Err(errors) = result {
                skipped.push(format!("\"{name}\": {}", describe_channel_errors(&errors)));
                continue;
            }
            let name = config.name.as_str().to_string();
            let details = describe_interface(&config);
            let echo = config.clone();
            let id = self.listener.add_channel(config);
            self.channels.push(id);
            added.push(id);
            self.push(UiUpdate::ChannelAdded(id, name, details, Box::new(echo)));
        }
        // The skip notice goes after ProfileLoaded: both set the one status
        // line, so the other way round "Loaded" replaced the reasons at once.
        self.push(UiUpdate::ProfileLoaded(profile_name.clone()));
        if !warnings.is_empty() {
            self.push(UiUpdate::ProfileError(format!(
                "Loaded “{}” with changes to its old settings:\n{}",
                profile_name,
                warnings.join("\n")
            )));
        }
        if !skipped.is_empty() {
            self.push(UiUpdate::ProfileError(format!(
                "Loaded “{}” with {} channel(s) skipped (invalid):\n{}",
                profile_name,
                skipped.len(),
                skipped.join("\n")
            )));
        }
        added
    }

    /// Poll channels for the UI's pull surface. Every unselected channel gets cheap
    /// **stats** (per-tab health); the **selected** channel gets a snapshot (its
    /// bounded diagnostic/match detail) instead. The selected channel and every
    /// channel with a pop-out open also get an incremental **stream delta** (only the
    /// scrollback bytes new since our cursor — never the whole buffer, §87/ADR-009).
    /// Stopped/unknown channels yield `None`.
    ///
    /// Takes `&mut self` (not `&self`) so the `run` future stays `Send` — a shared
    /// `&Driver` held across the await would require `Driver: Sync`, which the mpsc
    /// `Receiver` is not. The real driver runs via `block_on` (no `Send` needed),
    /// but keeping it `Send` lets it run on a multi-thread runtime and be spawned.
    async fn poll_snapshots(&mut self) {
        let ids = self.channels.clone();
        let began = tokio::time::Instant::now();
        self.poll_rotation.begin(ids.len());
        // Linemon's Channel goes first, so a round that runs out of time
        // never starves it; the limit still ends the round after it.
        self.poll_linemon().await;
        while let Some(id) = self
            .poll_rotation
            .next_in_round(&ids, began, POLL_ROUND_LIMIT)
        {
            self.poll_channel(id).await;
        }
    }

    /// Set RTS (`rts`) or DTR on Linemon's hidden Channel, while the window
    /// has its port open and no newer request waits (§161). A line its serial
    /// reader did not take in time is not reported: the window shows the lines
    /// as the next poll reads them, so it shows what the port did.
    async fn linemon_set_line(&mut self, rts: bool, on: bool) {
        let Some(id) = self
            .linemon
            .as_ref()
            .filter(|l| l.open && l.waiting.is_none())
            .map(|l| l.id)
        else {
            return;
        };
        let _ = if rts {
            self.listener.set_rts(id, on).await
        } else {
            self.listener.set_dtr(id, on).await
        };
    }

    /// Linemon's share of a poll round: a snapshot and the stream's new bytes,
    /// with the cursor rules of [`poll_channel`](Self::poll_channel). Nothing is
    /// sent once the window has closed the port, or while a newer request waits
    /// for the Channel: what the Channel shows then is about to change.
    async fn poll_linemon(&mut self) {
        let Some(linemon) = self.linemon.as_ref() else {
            return;
        };
        if !linemon.open || linemon.waiting.is_some() {
            return;
        }
        let (id, cursor) = (linemon.id, linemon.cursor);
        if let Some(snap) = self.listener.snapshot(id).await {
            self.push(UiUpdate::Linemon(LinemonUpdate::Snapshot(Box::new(snap))));
        }
        // `None` for UDP, or a serial port not open yet.
        if let Some(lines) = self.listener.serial_control_lines(id) {
            self.push(UiUpdate::Linemon(LinemonUpdate::ControlLines(lines)));
        }
        if let Some(delta) = self.listener.stream_delta(id, cursor.offset).await {
            let end = delta.end_offset;
            let generation = delta.generation;
            let needs_update = Some(generation) != cursor.generation || !delta.bytes.is_empty();
            let advance = !needs_update
                || self.push(UiUpdate::Linemon(LinemonUpdate::Stream(Box::new(delta))));
            if advance {
                if let Some(linemon) = self.linemon.as_mut() {
                    linemon.cursor = StreamPollCursor {
                        generation: Some(generation),
                        offset: end,
                    };
                }
            }
        }
    }

    /// One Channel's share of a poll round: stats for every unselected Channel, a
    /// snapshot for the selected one, and the stream's new bytes for the selected
    /// Channel and for each Channel with a pop-out open. A Channel that is both
    /// is polled once.
    async fn poll_channel(&mut self, id: ChannelId) {
        let selected = Some(id) == self.selected;
        if selected {
            // Two requests to the pipeline per poll, the snapshot and the
            // stream delta, where one would do. Not worth changing on its
            // own: fold them together when `PipelineRequest` next changes.
            if let Some(snap) = self.listener.snapshot(id).await {
                self.push(UiUpdate::Snapshot(id, Box::new(snap)));
            }
        } else if let Some(stats) = self.listener.channel_stats(id).await {
            self.push(UiUpdate::Stats(id, Box::new(stats)));
        }
        if selected || self.popouts.contains(&id) {
            self.poll_stream(id).await;
        }
        // Live serial control/status lines (§161); `None` for non-serial or
        // stopped channels.
        if let Some(lines) = self.listener.serial_control_lines(id) {
            self.push(UiUpdate::ControlLines(id, lines));
        }
    }

    /// Incremental live stream (§87, ADR-009): fetch only the bytes new since
    /// the Channel's cursor, so we never re-ship the whole scrollback. A
    /// "caught up" empty delta is a no-op (just advance past it). For a
    /// non-empty delta, advance the cursor ONLY if the push was accepted — if
    /// the bounded update channel was full and dropped it, keep the cursor so
    /// the next poll re-fetches those bytes instead of skipping them.
    async fn poll_stream(&mut self, id: ChannelId) {
        let cursor = self.stream_cursors.get(&id).copied().unwrap_or_default();
        if let Some(delta) = self.listener.stream_delta(id, cursor.offset).await {
            let end = delta.end_offset;
            let generation = delta.generation;
            // Empty caught-up deltas normally stay off the UI channel. A new
            // generation is different: forward even an empty window so a
            // restart clears stale bytes when no new data has arrived yet.
            let needs_update = Some(generation) != cursor.generation || !delta.bytes.is_empty();
            let advance = !needs_update || self.push(UiUpdate::StreamDelta(id, Box::new(delta)));
            if advance {
                self.stream_cursors.insert(
                    id,
                    StreamPollCursor {
                        generation: Some(generation),
                        offset: end,
                    },
                );
            }
        }
    }

    /// Hand one update to the GUI and wake it. Non-blocking (§99): a periodic
    /// update is dropped when its bounded lane is full rather than stalling the
    /// driver, while a structural result always has room (see [`update_lanes`]).
    /// Returns whether the update was actually enqueued — the stream-delta poll
    /// uses this to avoid advancing its cursor past bytes the UI never received.
    fn push(&self, update: UiUpdate) -> bool {
        let sent = self.updates.send(update);
        (self.repaint)();
        sent
    }

    /// Surface a channel error. The id routes it to the right channel, and the
    /// text is the error alone.
    ///
    /// The channel name used to be prefixed, but every surface that shows this
    /// already identifies the channel — the detail pane sits under that
    /// channel's Name field, and the list row is that channel's own tab — so the
    /// prefix repeated what was next to it. It also made these messages differ
    /// from talker's for the same failure, and from Listener's own
    /// `complain_unconfigured`, which never prefixed.
    fn push_channel_error(&self, id: ChannelId, error: impl std::fmt::Display) {
        // Linemon's hidden Channel reports to its window, and only for the
        // newest request: a failure of one already replaced is not news.
        if let Some(linemon) = self.linemon.as_ref().filter(|l| l.id == id) {
            if linemon.open && linemon.waiting.is_none() {
                self.push(UiUpdate::Linemon(LinemonUpdate::Error(error.to_string())));
            }
            return;
        }
        self.push(UiUpdate::ChannelError(id, error.to_string()));
    }
}

/// A handle the egui App holds: the command sender, the update receiver, and the
/// runtime thread's join handle (kept alive for the App's lifetime).
pub struct BridgeHandle {
    pub commands: Sender<UiCommand>,
    pub updates: UpdateReceiver,
    /// The runtime thread, taken and joined by [`shutdown_and_join`](Self::shutdown_and_join)
    /// on app exit. `Option` so it can be moved out of `&mut self` (in `on_exit`).
    runtime_thread: Option<std::thread::JoinHandle<()>>,
}

impl BridgeHandle {
    /// Orderly shutdown on app exit: tell the driver to stop, then **block** until its
    /// thread finishes. The driver's loop runs `Listener::shutdown()` on its way out,
    /// which finalizes (flushes + closes) every open recording. Without this join the
    /// process could exit while a recording's last bytes were still buffered, leaving a
    /// `.raw`/`.disp` file unflushed — this is the X-button close path. Idempotent.
    pub fn shutdown_and_join(&mut self) {
        // Best-effort signal; if the channel is already closed the driver is stopping
        // anyway. `blocking_send` is correct from this (non-async) UI thread.
        let _ = self.commands.blocking_send(UiCommand::Shutdown);
        if let Some(handle) = self.runtime_thread.take() {
            let _ = handle.join();
        }
    }
}

/// Channel depths for the bridge. Commands are rare (user clicks); periodic
/// updates are higher-volume (5 Hz snapshots) but advisory.
const COMMAND_CAPACITY: usize = 64;
const UPDATE_CAPACITY: usize = 256;

/// Start the runtime on its own thread and return the App's [`BridgeHandle`].
///
/// `repaint` is invoked whenever an update is pushed; the App passes
/// `egui::Context::request_repaint` so a streaming source wakes the UI.
pub fn spawn(repaint: impl Fn() + Send + 'static) -> anyhow::Result<BridgeHandle> {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(COMMAND_CAPACITY);
    let (upd_tx, upd_rx) = update_lanes(UPDATE_CAPACITY);

    let runtime_thread = std::thread::Builder::new()
        .name("listener-runtime".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(err) => {
                    tracing::error!("failed to start the async runtime: {err}");
                    return;
                }
            };
            runtime.block_on(async move {
                let mut listener = Listener::new(PipelineCapacities::default());
                let events = listener
                    .take_events()
                    .expect("the event stream is available exactly once");
                Driver::new(listener, events, cmd_rx, upd_tx, Box::new(repaint))
                    .run()
                    .await;
            });
            // The window waits on this thread to exit; a file operation stuck
            // in the blocking pool must not keep it, or the process, alive
            // (§113).
            runtime.shutdown_timeout(crate::runtime::RUNTIME_SHUTDOWN_LIMIT);
        })?;

    Ok(BridgeHandle {
        commands: cmd_tx,
        updates: upd_rx,
        runtime_thread: Some(runtime_thread),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{templates, InterfaceConfig};

    // Shared with the other lib-test modules so concurrent callers in this one
    // test process never pick the same port (the AddrInUse-flake fix).
    use crate::test_ports::reserve_udp_port as free_udp_port;

    fn udp_config(port: u16) -> ChannelConfig {
        let mut config = templates::udp_template();
        if let InterfaceConfig::Udp(udp) = &mut config.interface {
            udp.bind_address = "127.0.0.1".to_string();
            udp.port = port;
        }
        config
    }

    #[test]
    fn a_udp_header_states_who_can_reach_it() {
        let mut config = templates::udp_template();
        let InterfaceConfig::Udp(udp) = &mut config.interface else {
            unreachable!("the UDP template is UDP")
        };
        udp.bind_address = "0.0.0.0".to_string();
        udp.port = 10110;
        assert_eq!(
            describe_interface(&config),
            "UDP unicast · bind 0.0.0.0:10110 · all interfaces (reachable from the network)"
        );
        assert_eq!(
            describe_interface(&udp_config(10110)),
            "UDP unicast · bind 127.0.0.1:10110 · this computer only"
        );
    }

    /// PLAN 7.6: the GUI's status keeps updating at least once a second while
    /// one Channel's start blocks for 10 s — a serial port slow to open must
    /// not freeze every other readout.
    #[tokio::test(start_paused = true)]
    async fn status_keeps_updating_while_a_start_blocks() {
        const OPEN_TAKES: Duration = Duration::from_secs(10);
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        listener.delay_opens_for_test(OPEN_TAKES);
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (upd_tx, mut upd_rx) = update_lanes(4096);
        let handle =
            tokio::spawn(Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {})).run());

        cmd_tx
            .send(UiCommand::AddChannel(Box::new(udp_config(free_udp_port()))))
            .await
            .unwrap();
        let id = loop {
            if let Some(UiUpdate::ChannelAdded(id, ..)) = upd_rx.recv().await {
                break id;
            }
        };
        let asked = tokio::time::Instant::now();
        cmd_tx.send(UiCommand::Start(id)).await.unwrap();

        // Seconds of the start in which a snapshot reached the UI.
        let mut seconds_with_an_update = std::collections::BTreeSet::new();
        loop {
            match upd_rx.recv().await.expect("the driver keeps running") {
                // An unselected Channel's readouts come as stats, a selected
                // one's as a snapshot: either is the status updating.
                UiUpdate::Snapshot(..) | UiUpdate::Stats(..) => {
                    let at = asked.elapsed();
                    if at < OPEN_TAKES {
                        seconds_with_an_update.insert(at.as_secs());
                    }
                }
                UiUpdate::Event(RuntimeEvent::ChannelStarted(_)) => break,
                _ => {}
            }
        }
        assert!(asked.elapsed() >= OPEN_TAKES, "the start did block");
        assert_eq!(
            seconds_with_an_update.len(),
            OPEN_TAKES.as_secs() as usize,
            "status updated in only these seconds of the start: {seconds_with_an_update:?}"
        );

        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        handle.await.unwrap();
    }

    /// A driver whose every interface open takes `open_takes`, with `n` UDP
    /// Channels added. Returns the command sender, the update receiver, the
    /// Channel ids and the driver task.
    async fn slow_driver(
        open_takes: Duration,
        n: usize,
    ) -> (
        Sender<UiCommand>,
        UpdateReceiver,
        Vec<ChannelId>,
        tokio::task::JoinHandle<()>,
    ) {
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        listener.delay_opens_for_test(open_takes);
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (upd_tx, mut upd_rx) = update_lanes(4096);
        let handle =
            tokio::spawn(Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {})).run());
        let mut ids = Vec::new();
        for _ in 0..n {
            cmd_tx
                .send(UiCommand::AddChannel(Box::new(udp_config(free_udp_port()))))
                .await
                .unwrap();
            ids.push(loop {
                if let Some(UiUpdate::ChannelAdded(id, ..)) = upd_rx.recv().await {
                    break id;
                }
            });
        }
        (cmd_tx, upd_rx, ids, handle)
    }

    /// ADR-052: a Stop sent while the Channel is still starting waits its
    /// turn, then runs, so the Channel ends Stopped rather than refusing it.
    #[tokio::test(start_paused = true)]
    async fn a_stop_during_a_slow_start_runs_once_the_start_lands() {
        let (cmd_tx, mut upd_rx, ids, handle) = slow_driver(Duration::from_secs(10), 1).await;
        let id = ids[0];
        cmd_tx.send(UiCommand::Start(id)).await.unwrap();
        cmd_tx.send(UiCommand::Stop(id)).await.unwrap();
        let mut seen = Vec::new();
        while seen.len() < 2 {
            match upd_rx.recv().await.expect("the driver keeps running") {
                UiUpdate::Event(RuntimeEvent::ChannelStarted(_)) => seen.push("started"),
                UiUpdate::Event(RuntimeEvent::ChannelStopped(_)) => seen.push("stopped"),
                UiUpdate::ChannelError(_, why) => panic!("the stop was refused: {why}"),
                _ => {}
            }
        }
        assert_eq!(seen, ["started", "stopped"]);
        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        handle.await.unwrap();
    }

    /// ADR-052: Channels do not wait on each other. Two starts that each take
    /// 10 s both land at 10 s, not one at 10 s and the other at 20 s.
    #[tokio::test(start_paused = true)]
    async fn slow_starts_on_two_channels_overlap() {
        const OPEN_TAKES: Duration = Duration::from_secs(10);
        let (cmd_tx, mut upd_rx, ids, handle) = slow_driver(OPEN_TAKES, 2).await;
        let asked = tokio::time::Instant::now();
        cmd_tx
            .send(UiCommand::StartAll(
                ids.iter()
                    .map(|id| (*id, Box::new(udp_config(free_udp_port()))))
                    .collect(),
            ))
            .await
            .unwrap();
        let mut started = 0;
        while started < 2 {
            if let UiUpdate::Event(RuntimeEvent::ChannelStarted(_)) =
                upd_rx.recv().await.expect("the driver keeps running")
            {
                started += 1;
            }
        }
        assert!(
            asked.elapsed() < OPEN_TAKES * 2,
            "the second start waited for the first: {:?}",
            asked.elapsed()
        );
        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        handle.await.unwrap();
    }

    /// File work goes to the blocking pool: on a one-thread runtime another
    /// task keeps running while the work waits. Run on the loop itself, the
    /// work would hold the only thread and the ticker would not advance.
    #[tokio::test(flavor = "current_thread")]
    async fn file_work_runs_off_the_loop() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;
        let ticks = Arc::new(AtomicU32::new(0));
        let ticking = ticks.clone();
        let ticker = tokio::spawn(async move {
            loop {
                ticking.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
            }
        });
        let seen = ticks.clone();
        let (start, end) = (ticks.clone(), ticks.clone());
        let before = run_blocking(move || {
            let first = start.load(Ordering::SeqCst);
            while end.load(Ordering::SeqCst) < first + 3 {
                std::thread::yield_now();
            }
            first
        })
        .await
        .unwrap();
        assert!(seen.load(Ordering::SeqCst) >= before + 3);
        ticker.abort();
    }

    #[tokio::test]
    async fn the_driver_lists_local_addresses() {
        async fn next(rx: &mut UpdateReceiver) -> UiUpdate {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("driver update timed out")
                .expect("update stream closed")
        }
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
        let (upd_tx, mut upd_rx) = update_lanes(64);
        let handle =
            tokio::spawn(Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {})).run());
        cmd_tx.send(UiCommand::ListLocalAddresses).await.unwrap();
        loop {
            if let UiUpdate::LocalAddresses(listed) = next(&mut upd_rx).await {
                let listed = listed.expect("the OS lists its addresses");
                assert!(listed.iter().any(|a| a.ip.is_loopback()), "{listed:?}");
                break;
            }
        }
        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        handle.await.unwrap();
    }

    /// A running driver with one UDP Channel added: the command sender, the
    /// update receiver, the Channel's id and port, and the driver task.
    async fn popout_driver() -> (
        Sender<UiCommand>,
        UpdateReceiver,
        ChannelId,
        u16,
        tokio::task::JoinHandle<()>,
    ) {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (upd_tx, mut upd_rx) = update_lanes(256);
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let handle =
            tokio::spawn(Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {})).run());
        let port = free_udp_port();
        cmd_tx
            .send(UiCommand::AddChannel(Box::new(udp_config(port))))
            .await
            .unwrap();
        let id = loop {
            let update = tokio::time::timeout(Duration::from_secs(5), upd_rx.recv())
                .await
                .expect("driver update timed out")
                .expect("update stream closed");
            if let UiUpdate::ChannelAdded(id, ..) = update {
                break id;
            }
        };
        (cmd_tx, upd_rx, id, port, handle)
    }

    /// Wait until the Channel's start is reported.
    async fn wait_started(rx: &mut UpdateReceiver, id: ChannelId) {
        loop {
            let update = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("driver update timed out")
                .expect("update stream closed");
            if matches!(update, UiUpdate::Event(RuntimeEvent::ChannelStarted(c)) if c == id) {
                return;
            }
        }
    }

    /// Spec §50.4: a Channel with a pop-out open gets its stream whether or not
    /// it is selected, and stops getting it once it leaves the set.
    #[tokio::test]
    async fn a_popped_out_unselected_channel_streams_until_removed_from_the_set() {
        let (cmd_tx, mut upd_rx, id, port, handle) = popout_driver().await;
        cmd_tx.send(UiCommand::SetPopouts(vec![id])).await.unwrap();
        cmd_tx.send(UiCommand::Start(id)).await.unwrap();
        wait_started(&mut upd_rx, id).await;

        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut got_stream = false;
        while !got_stream {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no stream delta arrived for the popped-out channel"
            );
            let _ = client.send_to(b"hello", ("127.0.0.1", port)).await;
            match tokio::time::timeout(Duration::from_millis(250), upd_rx.recv()).await {
                Ok(Some(UiUpdate::StreamDelta(sid, delta))) if sid == id => {
                    got_stream = !delta.bytes.is_empty();
                }
                Ok(Some(UiUpdate::Snapshot(sid, _))) if sid == id => {
                    panic!("an unselected channel got a snapshot")
                }
                Ok(Some(_)) | Err(_) => {}
                Ok(None) => panic!("update stream closed early"),
            }
        }

        // Out of the set: let anything already queued land, then no new bytes
        // may arrive however much the channel receives.
        cmd_tx
            .send(UiCommand::SetPopouts(Vec::new()))
            .await
            .unwrap();
        let settle = tokio::time::Instant::now() + Duration::from_millis(700);
        while tokio::time::Instant::now() < settle {
            let _ = tokio::time::timeout(Duration::from_millis(50), upd_rx.recv()).await;
        }
        let quiet = tokio::time::Instant::now() + Duration::from_millis(900);
        while tokio::time::Instant::now() < quiet {
            let _ = client.send_to(b"more", ("127.0.0.1", port)).await;
            if let Ok(Some(UiUpdate::StreamDelta(sid, delta))) =
                tokio::time::timeout(Duration::from_millis(50), upd_rx.recv()).await
            {
                assert!(
                    sid != id || delta.bytes.is_empty(),
                    "a stream delta arrived after the pop-out left the set"
                );
            }
        }

        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("driver did not shut down")
            .unwrap();
    }

    /// A Channel that is both selected and popped out is polled once: its bytes
    /// reach the GUI exactly once.
    #[tokio::test]
    async fn a_selected_and_popped_out_channel_gets_each_byte_once() {
        let (cmd_tx, mut upd_rx, id, port, handle) = popout_driver().await;
        cmd_tx.send(UiCommand::Select(Some(id))).await.unwrap();
        cmd_tx.send(UiCommand::SetPopouts(vec![id])).await.unwrap();
        cmd_tx.send(UiCommand::Start(id)).await.unwrap();
        wait_started(&mut upd_rx, id).await;

        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client
            .send_to(b"0123456789", ("127.0.0.1", port))
            .await
            .unwrap();
        let mut received: Vec<u8> = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
        while tokio::time::Instant::now() < deadline {
            if let Ok(Some(UiUpdate::StreamDelta(sid, delta))) =
                tokio::time::timeout(Duration::from_millis(100), upd_rx.recv()).await
            {
                if sid == id {
                    received.extend_from_slice(&delta.bytes);
                }
            }
        }
        assert_eq!(received, b"0123456789");

        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        handle.await.unwrap();
    }

    /// The set holds only Channels that exist, and a removed Channel leaves it.
    #[tokio::test]
    async fn the_popout_set_drops_unknown_and_removed_channels() {
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let (_cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(4);
        let (upd_tx, _upd_rx) = update_lanes(64);
        let mut driver = Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {}));
        driver
            .handle(UiCommand::AddChannel(Box::new(udp_config(free_udp_port()))))
            .await;
        let id = driver.channels[0];
        let unknown = ChannelId::new();
        driver
            .handle(UiCommand::SetPopouts(vec![id, unknown]))
            .await;
        assert_eq!(driver.popouts, HashSet::from([id]));
        driver.handle(UiCommand::RemoveChannel(id)).await;
        driver.pump().await;
        assert!(driver.popouts.is_empty());
        driver.handle(UiCommand::SetPopouts(vec![id])).await;
        assert!(driver.popouts.is_empty());
    }

    /// The driver translates commands into runtime calls, forwards lifecycle events,
    /// and pushes snapshots — end to end over a loopback UDP channel.
    #[tokio::test]
    async fn driver_adds_starts_reports_and_snapshots_a_channel() {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (upd_tx, mut upd_rx) = update_lanes(64);

        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let driver = Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {}));
        let handle = tokio::spawn(driver.run());

        // Helper: await the next update of interest within a timeout.
        async fn next(rx: &mut UpdateReceiver) -> UiUpdate {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("driver update timed out")
                .expect("update stream closed")
        }

        let port = free_udp_port();
        cmd_tx
            .send(UiCommand::AddChannel(Box::new(udp_config(port))))
            .await
            .unwrap();

        // The driver mints the id and reports it, with connection details.
        let id = loop {
            if let UiUpdate::ChannelAdded(id, name, details, _) = next(&mut upd_rx).await {
                assert_eq!(name, "UDP_Channel");
                assert!(
                    details.contains("UDP"),
                    "details name the interface: {details}"
                );
                break id;
            }
        };

        cmd_tx.send(UiCommand::Start(id)).await.unwrap();
        // Select the channel so the driver polls a *full* snapshot for it (others
        // get cheap stats only).
        cmd_tx.send(UiCommand::Select(Some(id))).await.unwrap();

        // Lifecycle event is forwarded; the small snapshot and the incremental
        // stream delta both arrive for the selected channel.
        let mut started = false;
        let mut got_snapshot = false;
        let mut got_stream = false;
        // Send a datagram so the stream delta has content.
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !(started && got_snapshot && got_stream) {
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "did not observe start + snapshot + stream \
                     (started={started}, snap={got_snapshot}, stream={got_stream})"
                );
            }
            // Keep poking the channel so a datagram arrives after Start.
            let _ = client.send_to(b"hi", ("127.0.0.1", port)).await;
            match tokio::time::timeout(Duration::from_millis(250), upd_rx.recv()).await {
                Ok(Some(UiUpdate::Event(RuntimeEvent::ChannelStarted(eid)))) if eid == id => {
                    started = true;
                }
                Ok(Some(UiUpdate::Snapshot(sid, _))) if sid == id => got_snapshot = true,
                // The scrollback bytes ride the incremental delta, not the snapshot.
                Ok(Some(UiUpdate::StreamDelta(sid, delta))) if sid == id => {
                    // A new generation is forwarded even before its first byte so
                    // the GUI can clear stale data from a previous run promptly.
                    got_stream |= !delta.bytes.is_empty();
                }
                Ok(Some(_)) => {}
                Ok(None) => panic!("update stream closed early"),
                Err(_) => {} // tick again
            }
        }

        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("driver did not shut down")
            .unwrap();
    }

    /// A Start that can't bind (two channels on one UDP port) reports the reason
    /// to the UI instead of leaving the channel Faulted with no explanation.
    #[tokio::test]
    async fn starting_two_channels_on_one_udp_port_reports_the_conflict() {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (upd_tx, mut upd_rx) = update_lanes(64);
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let driver = Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {}));
        let handle = tokio::spawn(driver.run());

        async fn next(rx: &mut UpdateReceiver) -> UiUpdate {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("driver update timed out")
                .expect("update stream closed")
        }

        let port = free_udp_port();

        // First channel binds the port and starts.
        cmd_tx
            .send(UiCommand::AddChannel(Box::new(udp_config(port))))
            .await
            .unwrap();
        let id1 = loop {
            if let UiUpdate::ChannelAdded(id, ..) = next(&mut upd_rx).await {
                break id;
            }
        };
        cmd_tx.send(UiCommand::Start(id1)).await.unwrap();
        loop {
            if let UiUpdate::Event(RuntimeEvent::ChannelStarted(e)) = next(&mut upd_rx).await {
                if e == id1 {
                    break;
                }
            }
        }

        // Second channel on the same port can't bind — the driver says why.
        cmd_tx
            .send(UiCommand::AddChannel(Box::new(udp_config(port))))
            .await
            .unwrap();
        let id2 = loop {
            if let UiUpdate::ChannelAdded(id, ..) = next(&mut upd_rx).await {
                if id != id1 {
                    break id;
                }
            }
        };
        cmd_tx.send(UiCommand::Start(id2)).await.unwrap();
        let reason = loop {
            if let UiUpdate::ChannelError(e, msg) = next(&mut upd_rx).await {
                if e == id2 {
                    break msg;
                }
            }
        };
        assert!(!reason.is_empty(), "the bind conflict reason is reported");

        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// A unique temp profile path per call (parallel tests must not collide).
    fn temp_profile_path() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "listener-bridge-profile-{}-{n}.toml",
            std::process::id()
        ))
    }

    /// Save the workspace through the driver, then load it into a fresh driver: the
    /// channels reappear (ChannelAdded), and the load reports the profile name. The
    /// round-trip proves SaveProfile/LoadProfile are wired end to end.
    #[tokio::test]
    async fn save_then_load_round_trips_the_workspace_through_the_driver() {
        async fn next(rx: &mut UpdateReceiver) -> UiUpdate {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("driver update timed out")
                .expect("update stream closed")
        }

        let path = temp_profile_path();

        // First driver: add two channels, then save.
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (upd_tx, mut upd_rx) = update_lanes(64);
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let handle =
            tokio::spawn(Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {})).run());

        // Distinct names (§6, ADR-014) — the GUI auto-suffixes on add, but this test
        // sends raw AddChannel, so name them so neither is skipped as a duplicate on load.
        let mut c1 = udp_config(free_udp_port());
        c1.name = crate::core::ChannelName::new("Chan_A");
        let mut c2 = udp_config(free_udp_port());
        c2.name = crate::core::ChannelName::new("Chan_B");
        cmd_tx
            .send(UiCommand::AddChannel(Box::new(c1)))
            .await
            .unwrap();
        cmd_tx
            .send(UiCommand::AddChannel(Box::new(c2)))
            .await
            .unwrap();
        // Drain the two ChannelAdded acks, keeping the first channel's id + config.
        let mut added = 0;
        let mut first: Option<(ChannelId, ChannelConfig)> = None;
        while added < 2 {
            if let UiUpdate::ChannelAdded(cid, _, _, config) = next(&mut upd_rx).await {
                first.get_or_insert((cid, *config));
                added += 1;
            }
        }
        // Set a non-default scroll buffer + Raw recording settings on the first channel
        // (the live-config commands) so we can prove they survive save→load.
        let (first_id, first_config) = first.unwrap();
        let mut retention = first_config.retention.clone();
        retention.byte_limit = Some(32 * 1024);
        cmd_tx
            .send(UiCommand::SetViewConfig(
                first_id,
                Box::new(first_config.display.clone()),
                Box::new(retention),
            ))
            .await
            .unwrap();
        let mut raw = first_config.raw_recording.clone();
        raw.destination = Some(std::path::PathBuf::from("rec.raw"));
        raw.enabled = true; // "Record on start"
        cmd_tx
            .send(UiCommand::SetRawRecordingConfig(first_id, Box::new(raw)))
            .await
            .unwrap();
        cmd_tx
            .send(UiCommand::SaveProfile(path.clone()))
            .await
            .unwrap();
        let saved = loop {
            match next(&mut upd_rx).await {
                UiUpdate::ProfileSaved(p) => break p,
                UiUpdate::ProfileError(e) => panic!("save errored: {e}"),
                _ => {}
            }
        };
        assert_eq!(saved, path);
        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;

        // Second, fresh driver: load the saved profile; the channels come back.
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (upd_tx, mut upd_rx) = update_lanes(64);
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let handle =
            tokio::spawn(Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {})).run());

        cmd_tx
            .send(UiCommand::LoadProfile(path.clone()))
            .await
            .unwrap();
        let mut loaded_channels = 0;
        let mut saw_scroll_buffer = false;
        let mut saw_raw_recording = false;
        let name = loop {
            match next(&mut upd_rx).await {
                UiUpdate::ChannelAdded(_, _, _, config) => {
                    loaded_channels += 1;
                    if config.retention.byte_limit == Some(32 * 1024) {
                        saw_scroll_buffer = true;
                    }
                    if config.raw_recording.enabled
                        && config.raw_recording.destination
                            == Some(std::path::PathBuf::from("rec.raw"))
                    {
                        saw_raw_recording = true;
                    }
                }
                UiUpdate::ProfileLoaded(name) => break name,
                UiUpdate::ProfileError(e) => panic!("load errored: {e}"),
                _ => {}
            }
        };
        assert_eq!(loaded_channels, 2, "both saved channels were re-registered");
        assert!(
            saw_scroll_buffer,
            "the per-channel scroll buffer (retention.byte_limit) survived save→load"
        );
        assert!(
            saw_raw_recording,
            "Raw recording settings (incl. \"record on start\") survived save→load"
        );
        assert_eq!(name, path.file_stem().unwrap().to_str().unwrap());

        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        let _ = std::fs::remove_file(&path);
    }

    /// A profile with one UDP channel that records on start to `recordings`.
    fn resume_profile(recordings: &std::path::Path) -> std::path::PathBuf {
        let mut config = udp_config(free_udp_port());
        config.raw_recording.enabled = true;
        config.raw_recording.destination = Some(recordings.to_path_buf());
        config.raw_recording.file_rotation = crate::record::FileRotationPolicy::Daily;
        let mut profile = Profile::new("Logging");
        profile.channels.push(config);
        let path = temp_profile_path();
        profile.save(&path).unwrap();
        path
    }

    #[test]
    fn a_profile_resumes_only_where_its_recordings_went_before() {
        // ADR-045: a missing profile, or a recording folder without its marker —
        // perhaps a mount point whose drive is not there — means nothing starts.
        let recordings = temp_profile_path().with_extension("rec");
        let path = resume_profile(&recordings);
        let refused = resume_check(&path).unwrap_err();
        assert!(
            refused.contains("has no .wiredata-destination marker"),
            "{refused}"
        );

        std::fs::create_dir_all(&recordings).unwrap();
        std::fs::write(recordings.join(crate::record::DESTINATION_MARKER), "").unwrap();
        assert_eq!(resume_check(&path).unwrap().0.name, "Logging");

        let _ = std::fs::remove_file(&path);
        assert!(resume_check(&path)
            .unwrap_err()
            .contains("is no longer there"));
        let _ = std::fs::remove_dir_all(&recordings);
    }

    #[test]
    fn a_profile_that_cannot_resume_says_why_in_words() {
        let mut profile = Profile::new("Dupes");
        for _ in 0..2 {
            let mut config = udp_config(free_udp_port());
            config.name = crate::core::ChannelName::new("Dup");
            profile.channels.push(config);
        }
        let path = temp_profile_path();
        profile.save(&path).unwrap();

        let refused = resume_check(&path).unwrap_err();
        let reasons: Vec<&str> = refused
            .lines()
            .filter(|line| line.starts_with("\"Dup\": "))
            .collect();
        assert_eq!(reasons.len(), 2, "one line per Channel: {refused}");
        for reason in reasons {
            assert!(reason.contains("names must be unique"), "{refused}");
            assert!(!reason.contains("DuplicateChannelName"), "{refused}");
        }

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn resume_loads_the_profile_and_starts_its_channels() {
        async fn next(rx: &mut UpdateReceiver) -> UiUpdate {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("driver update timed out")
                .expect("update stream closed")
        }
        let recordings = temp_profile_path().with_extension("rec");
        std::fs::create_dir_all(&recordings).unwrap();
        std::fs::write(recordings.join(crate::record::DESTINATION_MARKER), "").unwrap();
        let path = resume_profile(&recordings);

        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (upd_tx, mut upd_rx) = update_lanes(64);
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let handle =
            tokio::spawn(Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {})).run());
        cmd_tx.send(UiCommand::Resume(path.clone())).await.unwrap();

        // The started event rides the periodic lane, so it may arrive after
        // the structural `Resumed`.
        let (mut added, mut started, mut resumed) = (false, false, false);
        while !(added && started && resumed) {
            match next(&mut upd_rx).await {
                UiUpdate::ChannelAdded(..) => added = true,
                UiUpdate::Event(RuntimeEvent::ChannelStarted(_)) => started = true,
                UiUpdate::Resumed(name, _) => {
                    assert_eq!(name, "Logging");
                    resumed = true;
                }
                _ => {}
            }
        }

        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        handle.await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&recordings);
    }

    /// A LoadProfile of a missing/invalid file reports ProfileError and leaves the
    /// existing workspace untouched (no channels removed).
    #[tokio::test]
    async fn loading_a_missing_profile_errors_without_touching_the_workspace() {
        async fn next(rx: &mut UpdateReceiver) -> UiUpdate {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("driver update timed out")
                .expect("update stream closed")
        }

        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (upd_tx, mut upd_rx) = update_lanes(64);
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let handle =
            tokio::spawn(Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {})).run());

        cmd_tx
            .send(UiCommand::AddChannel(Box::new(udp_config(free_udp_port()))))
            .await
            .unwrap();
        loop {
            if let UiUpdate::ChannelAdded(..) = next(&mut upd_rx).await {
                break;
            }
        }

        let missing = std::env::temp_dir().join("listener-no-such-profile.toml");
        let _ = std::fs::remove_file(&missing);
        cmd_tx.send(UiCommand::LoadProfile(missing)).await.unwrap();

        // We get a ProfileError, and crucially no ChannelRemoved beforehand.
        loop {
            match next(&mut upd_rx).await {
                UiUpdate::ProfileError(_) => break,
                UiUpdate::ChannelRemoved(_) => {
                    panic!("a failed load must not tear down the workspace")
                }
                _ => {}
            }
        }

        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// A hand-edited profile with duplicate channel names loads the valid channels and
    /// skips the duplicates (§71, ADR-014) — the GUI load path validates like the CLI,
    /// rather than registering every channel blindly.
    #[tokio::test]
    async fn loading_a_profile_with_duplicate_names_skips_them_and_loads_the_rest() {
        async fn next(rx: &mut UpdateReceiver) -> UiUpdate {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("driver update timed out")
                .expect("update stream closed")
        }

        // Build a profile: two channels share a name (both invalid), one is unique.
        let mut profile = Profile::new("dupes");
        let mut a = udp_config(free_udp_port());
        a.name = crate::core::ChannelName::new("Dup");
        let mut b = udp_config(free_udp_port());
        b.name = crate::core::ChannelName::new("Dup");
        let mut c = udp_config(free_udp_port());
        c.name = crate::core::ChannelName::new("Unique");
        profile.channels = vec![a, b, c];
        let path = std::env::temp_dir().join(format!("listener-dup-{}.toml", uuid::Uuid::new_v4()));
        profile.save(&path).unwrap();

        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (upd_tx, mut upd_rx) = update_lanes(64);
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let handle =
            tokio::spawn(Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {})).run());

        cmd_tx
            .send(UiCommand::LoadProfile(path.clone()))
            .await
            .unwrap();

        // Fold every update into the GUI state, as the app does: what the user
        // reads is the status line the updates leave behind, not any one update.
        let mut state = crate::gui::state::AppState::default();
        let mut added = Vec::new();
        loop {
            let update = next(&mut upd_rx).await;
            let loaded = matches!(update, UiUpdate::ProfileLoaded(_));
            if let UiUpdate::ChannelAdded(_, name, _, _) = &update {
                added.push(name.clone());
            }
            state.apply(update);
            if loaded {
                break;
            }
        }
        // The load's updates are queued together, so any that followed
        // ProfileLoaded are already here.
        while let Ok(update) = upd_rx.try_recv() {
            state.apply(update);
        }
        // Only the uniquely-named channel registered; both duplicates were skipped.
        assert_eq!(added, vec!["Unique".to_string()]);
        let status = state.workspace_status().unwrap_or_default();
        assert!(status.contains("“dupes”"), "names the profile: {status}");
        // Each skipped Channel on its own line, its reason in words rather than
        // Rust's debug form.
        let reasons: Vec<&str> = status
            .lines()
            .filter(|line| line.starts_with("\"Dup\": "))
            .collect();
        assert_eq!(reasons.len(), 2, "{status}");
        for reason in reasons {
            assert!(reason.contains("names must be unique"), "{status}");
            assert!(!reason.contains("DuplicateChannelName"), "{status}");
        }

        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        let _ = std::fs::remove_file(&path);
    }

    /// Holds profile file work on its blocking thread until released, as a
    /// hung network share would.
    struct FileGate {
        open: std::sync::Mutex<bool>,
        changed: std::sync::Condvar,
    }

    impl FileGate {
        /// A closed gate, and a guard that opens it when dropped, so a failing
        /// test leaves no thread behind to hold up the runtime's shutdown.
        fn closed() -> (std::sync::Arc<Self>, ReleaseOnDrop) {
            let gate = std::sync::Arc::new(Self {
                open: std::sync::Mutex::new(false),
                changed: std::sync::Condvar::new(),
            });
            let guard = ReleaseOnDrop(gate.clone());
            (gate, guard)
        }

        fn hook(self: &std::sync::Arc<Self>) -> FileHook {
            let gate = self.clone();
            Some(std::sync::Arc::new(move || {
                let mut open = gate.open.lock().unwrap();
                while !*open {
                    open = gate.changed.wait(open).unwrap();
                }
            }))
        }

        fn release(&self) {
            *self.open.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }

    struct ReleaseOnDrop(std::sync::Arc<FileGate>);

    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    /// Wait for three status updates, failing if they stop coming or if any
    /// profile result arrives: the file work is being held.
    async fn three_polls_while_held(upd_rx: &mut UpdateReceiver) {
        let waiting = async {
            let mut polls = 0;
            while polls < 3 {
                match upd_rx.recv().await.expect("the driver keeps running") {
                    UiUpdate::Stats(..) | UiUpdate::Snapshot(..) => polls += 1,
                    UiUpdate::ProfileSaved(_)
                    | UiUpdate::ProfileLoaded(_)
                    | UiUpdate::ProfileError(_) => {
                        panic!("the file work finished while held")
                    }
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("status stopped updating while the file work was held");
    }

    /// A driver with `file_hook` installed and one Channel added.
    async fn driver_with_file_hook(
        file_hook: FileHook,
    ) -> (
        Sender<UiCommand>,
        UpdateReceiver,
        ChannelId,
        tokio::task::JoinHandle<()>,
    ) {
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (upd_tx, mut upd_rx) = update_lanes(4096);
        let mut driver = Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {}));
        driver.file_hook = file_hook;
        let handle = tokio::spawn(driver.run());
        cmd_tx
            .send(UiCommand::AddChannel(Box::new(udp_config(free_udp_port()))))
            .await
            .unwrap();
        let id = loop {
            if let Some(UiUpdate::ChannelAdded(id, ..)) = upd_rx.recv().await {
                break id;
            }
        };
        (cmd_tx, upd_rx, id, handle)
    }

    /// Profile file work that never returns leaves the loop free: status keeps
    /// updating and Shutdown is served and completes. These run in real time:
    /// Tokio's paused clock does not advance while a blocking task is running.
    #[tokio::test]
    async fn a_profile_save_that_never_completes_does_not_stop_status_or_shutdown() {
        let (gate, _release) = FileGate::closed();
        let (cmd_tx, mut upd_rx, _id, handle) = driver_with_file_hook(gate.hook()).await;
        cmd_tx
            .send(UiCommand::SaveProfile(temp_profile_path()))
            .await
            .unwrap();

        three_polls_while_held(&mut upd_rx).await;
        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("Shutdown waited for the file work")
            .unwrap();
    }

    /// A profile command issued while one is outstanding waits behind it: a
    /// load queued behind a save of the same file finds the file the save wrote.
    #[tokio::test]
    async fn a_second_profile_command_waits_behind_the_first() {
        let (gate, _release) = FileGate::closed();
        let (cmd_tx, mut upd_rx, _id, handle) = driver_with_file_hook(gate.hook()).await;
        let path = temp_profile_path();
        cmd_tx
            .send(UiCommand::SaveProfile(path.clone()))
            .await
            .unwrap();
        cmd_tx
            .send(UiCommand::LoadProfile(path.clone()))
            .await
            .unwrap();
        // Neither has finished while the first is held.
        three_polls_while_held(&mut upd_rx).await;
        gate.release();
        let mut order = Vec::new();
        while order.len() < 2 {
            match upd_rx.recv().await.expect("the driver keeps running") {
                UiUpdate::ProfileSaved(_) => order.push("saved"),
                UiUpdate::ProfileLoaded(_) => order.push("loaded"),
                UiUpdate::ProfileError(why) => panic!("profile work failed: {why}"),
                _ => {}
            }
        }
        assert_eq!(order, ["saved", "loaded"]);
        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        handle.await.unwrap();
        let _ = std::fs::remove_file(path);
    }

    /// The fake time a poll visit takes in the rotation tests.
    const VISIT: Duration = Duration::from_millis(300);

    /// Run one round over `ids`, each visit taking [`VISIT`] of paused time;
    /// returns the Channels visited.
    async fn run_round(
        rotation: &mut PollRotation,
        ids: &[ChannelId],
        limit: Duration,
    ) -> Vec<ChannelId> {
        let began = tokio::time::Instant::now();
        rotation.begin(ids.len());
        let mut visited = Vec::new();
        while let Some(id) = rotation.next_in_round(ids, began, limit) {
            tokio::time::sleep(VISIT).await;
            visited.push(id);
        }
        visited
    }

    /// With Channels that each take a full request wait, a round ends at the
    /// round limit and the next one starts with the Channels the first left.
    #[tokio::test(start_paused = true)]
    async fn a_poll_round_ends_at_its_limit_and_the_next_starts_where_it_stopped() {
        let ids: Vec<ChannelId> = (0..5).map(|_| ChannelId::new()).collect();
        let mut rotation = PollRotation::default();
        let began = tokio::time::Instant::now();

        let first = run_round(&mut rotation, &ids, POLL_ROUND_LIMIT).await;
        assert_eq!(first, ids[0..2], "two waits fit in the round limit");
        assert!(began.elapsed() <= POLL_ROUND_LIMIT + VISIT);

        let second = run_round(&mut rotation, &ids, POLL_ROUND_LIMIT).await;
        assert_eq!(
            second,
            ids[2..4],
            "the next round starts where this one stopped"
        );
        let third = run_round(&mut rotation, &ids, POLL_ROUND_LIMIT).await;
        assert_eq!(third, [ids[4], ids[0]], "and wraps around the list");
    }

    /// A round over Channels that all answer promptly visits every one, and the
    /// next starts again at the head of the list.
    #[tokio::test(start_paused = true)]
    async fn a_poll_round_over_prompt_channels_visits_them_all() {
        let ids: Vec<ChannelId> = (0..3).map(|_| ChannelId::new()).collect();
        let mut rotation = PollRotation::default();
        // The limit is never reached when nothing takes time.
        let began = tokio::time::Instant::now();
        rotation.begin(ids.len());
        let mut visited = Vec::new();
        while let Some(id) = rotation.next_in_round(&ids, began, POLL_ROUND_LIMIT) {
            visited.push(id);
        }
        assert_eq!(visited, ids);
        rotation.begin(ids.len());
        assert_eq!(
            rotation.next_in_round(&ids, began, POLL_ROUND_LIMIT),
            Some(ids[0])
        );
    }

    /// A round visits at least one Channel, whatever the limit, and no round
    /// is run for an empty list.
    #[tokio::test(start_paused = true)]
    async fn a_poll_round_always_makes_progress() {
        let ids: Vec<ChannelId> = (0..3).map(|_| ChannelId::new()).collect();
        let mut rotation = PollRotation::default();
        let visited = run_round(&mut rotation, &ids, Duration::ZERO).await;
        assert_eq!(visited, ids[0..1]);
        let visited = run_round(&mut rotation, &ids, Duration::ZERO).await;
        assert_eq!(visited, ids[1..2]);
        assert!(run_round(&mut rotation, &[], POLL_ROUND_LIMIT)
            .await
            .is_empty());
    }

    /// Dropping the command sender ends the driver (the App closed).
    #[tokio::test]
    async fn dropping_the_command_sender_stops_the_driver() {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<UiCommand>(4);
        let (upd_tx, _upd_rx) = update_lanes(16);
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let driver = Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {}));
        let handle = tokio::spawn(driver.run());

        drop(cmd_tx); // the App is gone
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("driver did not stop when the command channel closed")
            .unwrap();
    }

    /// `CommitAndStart` recovers a Faulted channel through the driver in one command:
    /// no client-side Stop choreography, and the GUI sees the channel come up. Drives
    /// the channel to Faulted with a bad bind, then commits a good config + starts.
    #[tokio::test]
    async fn commit_and_start_recovers_a_faulted_channel_through_the_driver() {
        async fn next(rx: &mut UpdateReceiver) -> UiUpdate {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("driver update timed out")
                .expect("update stream closed")
        }

        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (upd_tx, mut upd_rx) = update_lanes(64);
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let handle =
            tokio::spawn(Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {})).run());

        // A channel whose bind address is invalid → Start faults it.
        let mut bad = udp_config(free_udp_port());
        if let InterfaceConfig::Udp(udp) = &mut bad.interface {
            udp.bind_address = "not-an-ip-address".to_string();
        }
        cmd_tx
            .send(UiCommand::AddChannel(Box::new(bad)))
            .await
            .unwrap();
        let id = loop {
            if let UiUpdate::ChannelAdded(id, ..) = next(&mut upd_rx).await {
                break id;
            }
        };
        cmd_tx.send(UiCommand::Start(id)).await.unwrap();
        // Observe the fault (forwarded as an Event).
        loop {
            if let UiUpdate::Event(RuntimeEvent::ChannelFaulted(fid)) = next(&mut upd_rx).await {
                assert_eq!(fid, id);
                break;
            }
        }

        // One CommitAndStart with a good config recovers and starts — no manual Stop.
        cmd_tx
            .send(UiCommand::CommitAndStart {
                id,
                config: Some(Box::new(udp_config(free_udp_port()))),
                start: true,
            })
            .await
            .unwrap();
        // We see the reconfigure echo and a ChannelStarted (the recovery worked).
        let mut reconfigured = false;
        let mut started = false;
        while !(reconfigured && started) {
            match next(&mut upd_rx).await {
                UiUpdate::ChannelReconfigured(rid, ..) if rid == id => reconfigured = true,
                UiUpdate::Event(RuntimeEvent::ChannelStarted(sid)) if sid == id => started = true,
                UiUpdate::ChannelError(eid, msg) if eid == id => {
                    panic!("commit_and_start should have recovered the channel, got: {msg}")
                }
                _ => {}
            }
        }

        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    #[test]
    fn one_time_results_survive_a_full_periodic_lane() {
        let (tx, mut rx) = update_lanes(1);
        let id = ChannelId::new();
        let telemetry = || UiUpdate::ControlLines(id, SerialControlLines::default());
        assert!(tx.send(telemetry()), "the first periodic update fits");
        assert!(
            !tx.send(telemetry()),
            "a periodic update past capacity is dropped"
        );

        let config = udp_config(10110);
        assert!(tx.send(UiUpdate::ChannelAdded(
            id,
            "A".into(),
            "details".into(),
            Box::new(config.clone())
        )));
        assert!(tx.send(UiUpdate::ChannelReconfigured(
            id,
            "details".into(),
            Box::new(config)
        )));
        assert!(tx.send(UiUpdate::ChannelRenamed(id, "B".into())));
        assert!(tx.send(UiUpdate::ChannelRemoved(id)));
        assert!(tx.send(UiUpdate::ProfileError("x".into())));

        let mut kinds = Vec::new();
        while let Ok(update) = rx.try_recv() {
            kinds.push(match update {
                UiUpdate::ChannelAdded(..) => "added",
                UiUpdate::ChannelReconfigured(..) => "reconfigured",
                UiUpdate::ChannelRenamed(..) => "renamed",
                UiUpdate::ChannelRemoved(_) => "removed",
                UiUpdate::ProfileError(_) => "error",
                UiUpdate::ControlLines(..) => "telemetry",
                other => panic!("unexpected {other:?}"),
            });
        }
        // Every one-time result arrives, in the order sent, before the one
        // periodic update that fit.
        assert_eq!(
            kinds,
            [
                "added",
                "reconfigured",
                "renamed",
                "removed",
                "error",
                "telemetry"
            ]
        );
    }

    #[test]
    fn the_reliable_lane_is_applied_before_the_periodic_lane() {
        let (tx, mut rx) = update_lanes(8);
        let id = ChannelId::new();
        // Telemetry is queued first, yet must not be applied ahead of the
        // ChannelAdded it refers to.
        assert!(tx.send(UiUpdate::ControlLines(id, SerialControlLines::default())));
        assert!(tx.send(UiUpdate::ChannelAdded(
            id,
            "A".into(),
            "details".into(),
            Box::new(udp_config(10110))
        )));
        assert!(matches!(rx.try_recv(), Ok(UiUpdate::ChannelAdded(..))));
        assert!(matches!(rx.try_recv(), Ok(UiUpdate::ControlLines(..))));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn waiting_for_an_update_also_prefers_the_reliable_lane() {
        let (tx, mut rx) = update_lanes(8);
        let id = ChannelId::new();
        assert!(tx.send(UiUpdate::ControlLines(id, SerialControlLines::default())));
        assert!(tx.send(UiUpdate::ChannelRemoved(id)));
        assert!(matches!(rx.recv().await, Some(UiUpdate::ChannelRemoved(_))));
        assert!(matches!(rx.recv().await, Some(UiUpdate::ControlLines(..))));
        drop(tx);
        assert!(rx.recv().await.is_none());
    }

    fn running_stats(id: ChannelId) -> UiUpdate {
        use crate::runtime::{ChannelActivity, ChannelStats, ChunkShape, DurationHistogram};
        use crate::runtime::{Provenance, QueueDepth, TransportHealth};
        UiUpdate::Stats(
            id,
            Box::new(ChannelStats {
                provenance: Provenance::Fresh,
                state: ChannelState::Running,
                reconnect_pending: false,
                reconnect: None,
                activity: ChannelActivity {
                    last_data_at: None,
                    bytes_per_sec: 0.0,
                    total_bytes: 0,
                },
                event_count: 0,
                warning_count: 0,
                error_count: 0,
                raw_recording: None,
                display_recording: None,
                display_paused: false,
                match_boundary_saves: 0,
                ingest_delay: DurationHistogram::default(),
                recent_ingest_delay: DurationHistogram::default(),
                ingest_processing: DurationHistogram::default(),
                recent_ingest_processing: DurationHistogram::default(),
                chunk_shape: ChunkShape::default(),
                transport_health: TransportHealth::default(),
                ingest_queue: QueueDepth::default(),
                raw_recording_queue: None,
                raw_recording_status: None,
                display_recording_status: None,
            }),
        )
    }

    #[test]
    fn running_stats_queued_before_a_stop_leave_the_model_stopped() {
        use crate::gui::state::{AppState, ChannelStatus};
        let (tx, mut rx) = update_lanes(8);
        let mut state = AppState::default();
        let id = ChannelId::new();
        assert!(tx.send(UiUpdate::ChannelAdded(
            id,
            "A".into(),
            "details".into(),
            Box::new(udp_config(10110))
        )));
        assert!(tx.send(running_stats(id)));
        assert!(tx.send(UiUpdate::Event(RuntimeEvent::ChannelStopped(id))));
        while let Ok(update) = rx.try_recv() {
            state.apply(update);
        }
        assert_eq!(state.channel(id).unwrap().status, ChannelStatus::Stopped);
    }

    #[test]
    fn the_reliable_lane_receives_no_runtime_events() {
        let (tx, mut rx) = update_lanes(1);
        let id = ChannelId::new();
        let events = [
            RuntimeEvent::ChannelStarted(id),
            RuntimeEvent::ChannelStopped(id),
            RuntimeEvent::ChannelFaulted(id),
            RuntimeEvent::RecordingFaulted(id, crate::core::RecordingTap::Raw),
            RuntimeEvent::RecordingStarted(id, crate::core::RecordingTap::Raw),
            RuntimeEvent::ChannelReconnecting(id, 1),
            RuntimeEvent::ChannelReconnected(id),
            RuntimeEvent::ChannelReconnectGaveUp(id),
            RuntimeEvent::DiskSpaceLow(id),
        ];
        for event in events {
            assert!(UiUpdate::Event(event.clone()).is_periodic(), "{event:?}");
            // Only the first fits the one-slot periodic lane; none may reach
            // the unbounded lane, so the rest are dropped.
            tx.send(UiUpdate::Event(event));
        }
        let mut received = 0;
        while rx.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, 1, "events never ride the reliable lane");
    }

    #[test]
    fn a_full_periodic_queue_drops_events_and_telemetry_but_not_structural_results() {
        let (tx, mut rx) = update_lanes(1);
        let id = ChannelId::new();
        assert!(tx.send(running_stats(id)));
        assert!(!tx.send(running_stats(id)), "telemetry is dropped");
        assert!(
            !tx.send(UiUpdate::Event(RuntimeEvent::ChannelStopped(id))),
            "an event is dropped"
        );
        assert!(tx.send(UiUpdate::ChannelRemoved(id)));
        assert!(tx.send(UiUpdate::ChannelError(id, "x".into())));
        assert!(matches!(rx.try_recv(), Ok(UiUpdate::ChannelRemoved(_))));
        assert!(matches!(rx.try_recv(), Ok(UiUpdate::ChannelError(..))));
        assert!(matches!(rx.try_recv(), Ok(UiUpdate::Stats(..))));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn stale_telemetry_for_a_removed_channel_is_ignored_and_ids_are_not_reused() {
        use crate::gui::state::AppState;
        let mut state = AppState::default();
        let id = ChannelId::new();
        let added =
            |id| UiUpdate::ChannelAdded(id, "A".into(), "d".into(), Box::new(udp_config(10110)));
        state.apply(added(id));
        state.apply(UiUpdate::ChannelRemoved(id));
        // Telemetry queued before the removal is applied after it.
        state.apply(UiUpdate::ControlLines(id, SerialControlLines::default()));
        assert!(state.channel_ids().is_empty());
        // A channel added again is a different id, so the stale update above
        // can never land on it.
        let again = ChannelId::new();
        assert_ne!(id, again);
        state.apply(added(again));
        assert_eq!(state.channel_ids(), vec![again]);
    }

    /// A channel that leaves retention unset is registered with the profile's
    /// default, and a channel with its own value keeps it.
    #[tokio::test]
    async fn registering_a_profile_applies_its_defaults_to_the_channels() {
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let (_cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (upd_tx, mut upd_rx) = update_lanes(64);
        let mut driver = Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {}));

        let mut inherits = udp_config(free_udp_port());
        inherits.name = ChannelName::new("Inherits");
        inherits.retention = crate::config::RetentionConfig::default();
        let mut own = udp_config(free_udp_port());
        own.name = ChannelName::new("Own");
        own.retention = crate::config::RetentionConfig::with_byte_limit(1_000);
        let mut profile = Profile::new("defaults");
        profile.channels = vec![inherits, own];
        profile.defaults.retention = Some(crate::config::RetentionConfig::with_byte_limit(4_096));

        let ids = driver.register_profile(profile, &[]);
        let limit = |id| driver.listener.config(id).unwrap().retention.byte_limit;
        assert_eq!(limit(ids[0]), Some(4_096));
        assert_eq!(limit(ids[1]), Some(1_000));
        // The GUI's copy of the config says the same, so a Save writes it.
        let mut echoed = Vec::new();
        while let Ok(update) = upd_rx.try_recv() {
            if let UiUpdate::ChannelAdded(_, _, _, config) = update {
                echoed.push(config.retention.byte_limit);
            }
        }
        assert_eq!(echoed, vec![Some(4_096), Some(1_000)]);
    }

    // ---- Linemon's hidden Channel (spec §50.3) ----

    fn udp_port_of(config: &ChannelConfig) -> u16 {
        match &config.interface {
            InterfaceConfig::Udp(udp) => udp.port,
            InterfaceConfig::Serial(_) => unreachable!("these tests use UDP"),
        }
    }

    /// A driver that is stepped by hand, not run, so a test can look between
    /// steps. Every interface open takes `open_takes`.
    fn stepped_driver(open_takes: Duration) -> Driver {
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        listener.delay_opens_for_test(open_takes);
        let (_cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
        let (upd_tx, _upd_rx) = update_lanes(256);
        Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {}))
    }

    /// Land the step running off the loop, then run what that unblocks.
    async fn land_one_step(driver: &mut Driver) {
        let done = driver.done_rx.recv().await.expect("a step is running");
        driver.on_done(done);
        driver.pump().await;
    }

    async fn open_linemon(driver: &mut Driver, port: u16) {
        driver
            .handle(UiCommand::LinemonOpen(Box::new(udp_config(port))))
            .await;
        driver.pump().await;
    }

    /// Three opens while the first is in flight leave one waiting request, the
    /// newest, and the earlier two are never committed.
    #[tokio::test(start_paused = true)]
    async fn rapid_linemon_opens_keep_only_the_newest_waiting() {
        let mut driver = stepped_driver(Duration::from_secs(10));
        let ports: Vec<u16> = (0..4).map(|_| free_udp_port()).collect();
        open_linemon(&mut driver, ports[0]).await;
        let id = driver.linemon.as_ref().expect("registered").id;
        assert!(driver.steps[&id].running, "the first open is in flight");

        for port in &ports[1..] {
            open_linemon(&mut driver, *port).await;
            assert!(
                driver.steps[&id].queue.is_empty(),
                "a request waits beside the queue, never in it"
            );
            let waiting = driver.linemon.as_ref().and_then(|l| l.waiting.as_ref());
            let Some(LinemonRequest::Open(config)) = waiting else {
                panic!("the newest open waits");
            };
            assert_eq!(udp_port_of(config), *port);
        }
        assert_eq!(udp_port_of(driver.listener.config(id).unwrap()), ports[0]);

        // The first open lands; the waiting request stops it, then commits.
        land_one_step(&mut driver).await;
        assert_eq!(driver.listener.state(id), Some(ChannelState::Stopping));
        land_one_step(&mut driver).await;
        assert_eq!(
            udp_port_of(driver.listener.config(id).unwrap()),
            ports[3],
            "only the newest config was applied"
        );
        land_one_step(&mut driver).await;
        assert_eq!(driver.listener.state(id), Some(ChannelState::Running));
        assert!(driver.linemon.as_ref().unwrap().waiting.is_none());
        assert!(driver.steps.is_empty());
        driver.listener.shutdown().await;
    }

    /// A close that arrives during an open replaces the waiting open.
    #[tokio::test(start_paused = true)]
    async fn a_linemon_close_wins_over_an_older_waiting_open() {
        let mut driver = stepped_driver(Duration::from_secs(10));
        open_linemon(&mut driver, free_udp_port()).await;
        let id = driver.linemon.as_ref().unwrap().id;
        open_linemon(&mut driver, free_udp_port()).await;
        driver.handle(UiCommand::LinemonClose).await;
        driver.pump().await;
        assert!(matches!(
            driver.linemon.as_ref().unwrap().waiting,
            Some(LinemonRequest::Close)
        ));
        land_one_step(&mut driver).await;
        land_one_step(&mut driver).await;
        assert_eq!(driver.listener.state(id), Some(ChannelState::Stopped));
        assert!(driver.steps.is_empty());
    }

    /// The hidden Channel is registered under its own name, whatever the
    /// config carried, and with no recording.
    #[tokio::test]
    async fn the_hidden_channel_is_named_linemon_and_never_records() {
        let mut driver = stepped_driver(Duration::ZERO);
        let mut config = udp_config(free_udp_port());
        config.name = ChannelName::new("UDP_Channel");
        config.raw_recording.enabled = true;
        config.display_recording.enabled = true;
        driver
            .handle(UiCommand::LinemonOpen(Box::new(config)))
            .await;
        let id = driver.linemon.as_ref().unwrap().id;
        let stored = driver.listener.config(id).unwrap();
        assert_eq!(stored.name.as_str(), "Linemon");
        assert!(!stored.raw_recording.enabled && !stored.display_recording.enabled);
        assert!(driver.channels.is_empty(), "not in the user's Channels");
    }

    /// A driver run as the GUI runs it.
    fn run_driver() -> (
        Sender<UiCommand>,
        UpdateReceiver,
        tokio::task::JoinHandle<()>,
    ) {
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (upd_tx, upd_rx) = update_lanes(4096);
        let handle =
            tokio::spawn(Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {})).run());
        (cmd_tx, upd_rx, handle)
    }

    async fn next_update(rx: &mut UpdateReceiver, wait: Duration) -> Option<UiUpdate> {
        tokio::time::timeout(wait, rx.recv()).await.ok().flatten()
    }

    /// The Channel an ordinary (non-Linemon) update is about, if it is about one.
    fn ordinary_channel(update: &UiUpdate) -> Option<ChannelId> {
        match update {
            UiUpdate::ChannelAdded(id, ..)
            | UiUpdate::ChannelReconfigured(id, ..)
            | UiUpdate::ChannelRenamed(id, _)
            | UiUpdate::ChannelRemoved(id)
            | UiUpdate::ChannelError(id, _)
            | UiUpdate::Snapshot(id, _)
            | UiUpdate::Stats(id, _)
            | UiUpdate::ControlLines(id, _)
            | UiUpdate::StreamDelta(id, _) => Some(*id),
            UiUpdate::Event(event) => Some(event_channel(event)),
            _ => None,
        }
    }

    /// Wait until a Linemon snapshot says Running, and return the datagram
    /// target's port for convenience. Panics after five seconds, or on a
    /// Linemon error.
    async fn until_linemon_running(rx: &mut UpdateReceiver) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match next_update(rx, Duration::from_millis(250)).await {
                Some(UiUpdate::Linemon(LinemonUpdate::Snapshot(snap)))
                    if snap.state == ChannelState::Running =>
                {
                    return;
                }
                Some(UiUpdate::Linemon(LinemonUpdate::Error(why))) => {
                    panic!("Linemon did not open: {why}")
                }
                _ => {}
            }
        }
        panic!("Linemon did not reach Running");
    }

    /// Send datagrams to `port` until a Linemon stream update carries bytes.
    async fn until_linemon_bytes(rx: &mut UpdateReceiver, port: u16) -> Vec<u8> {
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            let _ = client.send_to(b"linemon", ("127.0.0.1", port)).await;
            if let Some(UiUpdate::Linemon(LinemonUpdate::Stream(delta))) =
                next_update(rx, Duration::from_millis(250)).await
            {
                if !delta.bytes.is_empty() {
                    return delta.bytes.to_vec();
                }
            }
        }
        panic!("no Linemon stream bytes arrived on port {port}");
    }

    fn port_is_free(port: u16) -> bool {
        std::net::UdpSocket::bind(("127.0.0.1", port)).is_ok()
    }

    /// Linemon's stream and snapshot reach the GUI as Linemon updates; no
    /// ordinary update is about its Channel; and a saved profile, Stop all and
    /// New leave it alone.
    #[tokio::test]
    async fn linemon_updates_stay_apart_from_the_workspace() {
        let (cmd_tx, mut rx, handle) = run_driver();
        cmd_tx
            .send(UiCommand::AddChannel(Box::new(udp_config(free_udp_port()))))
            .await
            .unwrap();
        let user = loop {
            if let Some(UiUpdate::ChannelAdded(id, ..)) =
                next_update(&mut rx, Duration::from_secs(5)).await
            {
                break id;
            }
        };

        let port = free_udp_port();
        cmd_tx
            .send(UiCommand::LinemonOpen(Box::new(udp_config(port))))
            .await
            .unwrap();
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (mut snapshot, mut stream) = (false, false);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !(snapshot && stream) {
            assert!(tokio::time::Instant::now() < deadline, "no Linemon updates");
            let _ = client.send_to(b"hello", ("127.0.0.1", port)).await;
            let Some(update) = next_update(&mut rx, Duration::from_millis(250)).await else {
                continue;
            };
            match &update {
                UiUpdate::Linemon(LinemonUpdate::Snapshot(_)) => snapshot = true,
                UiUpdate::Linemon(LinemonUpdate::Stream(delta)) => {
                    stream |= !delta.bytes.is_empty();
                }
                UiUpdate::Linemon(LinemonUpdate::Error(why)) => panic!("{why}"),
                other => assert!(
                    ordinary_channel(other).is_none_or(|id| id == user),
                    "an ordinary update is about the hidden Channel: {other:?}"
                ),
            }
        }

        // Save: the profile holds the user's Channel and not Linemon's.
        let path = temp_profile_path();
        cmd_tx
            .send(UiCommand::SaveProfile(path.clone()))
            .await
            .unwrap();
        loop {
            match next_update(&mut rx, Duration::from_secs(5)).await {
                Some(UiUpdate::ProfileSaved(_)) => break,
                Some(UiUpdate::ProfileError(why)) => panic!("{why}"),
                Some(other) => assert!(ordinary_channel(&other).is_none_or(|id| id == user)),
                None => panic!("no save result"),
            }
        }
        let (saved, _) = Profile::load_with_warnings(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(saved.channels.len(), 1);
        assert_eq!(saved.channels[0].name.as_str(), "UDP_Channel");

        // Stop all and New tear down the workspace; Linemon keeps running.
        cmd_tx.send(UiCommand::StopAll).await.unwrap();
        cmd_tx.send(UiCommand::NewProfile).await.unwrap();
        loop {
            match next_update(&mut rx, Duration::from_secs(5)).await {
                Some(UiUpdate::ChannelRemoved(id)) => {
                    assert_eq!(id, user);
                    break;
                }
                Some(UiUpdate::Linemon(LinemonUpdate::Snapshot(snap))) => {
                    assert_eq!(snap.state, ChannelState::Running);
                }
                Some(_) => {}
                None => panic!("no removal"),
            }
        }
        until_linemon_running(&mut rx).await;
        assert!(!port_is_free(port), "Linemon still holds its port");

        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        handle.await.unwrap();
    }

    /// A second open with another config restarts the hidden Channel on it.
    #[tokio::test]
    async fn a_second_linemon_open_restarts_on_the_new_config() {
        let (cmd_tx, mut rx, handle) = run_driver();
        let (first, second) = (free_udp_port(), free_udp_port());
        cmd_tx
            .send(UiCommand::LinemonOpen(Box::new(udp_config(first))))
            .await
            .unwrap();
        until_linemon_running(&mut rx).await;
        assert!(!port_is_free(first));

        cmd_tx
            .send(UiCommand::LinemonOpen(Box::new(udp_config(second))))
            .await
            .unwrap();
        let bytes = until_linemon_bytes(&mut rx, second).await;
        assert!(bytes.ends_with(b"linemon"));
        assert!(port_is_free(first), "the old port was released");
        assert!(!port_is_free(second));

        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        handle.await.unwrap();
    }

    /// Close stops the hidden Channel and its updates, and keeps it registered
    /// so the next open reuses it.
    #[tokio::test]
    async fn closing_linemon_stops_the_channel_and_its_updates() {
        let (cmd_tx, mut rx, handle) = run_driver();
        let port = free_udp_port();
        cmd_tx
            .send(UiCommand::LinemonOpen(Box::new(udp_config(port))))
            .await
            .unwrap();
        until_linemon_running(&mut rx).await;

        cmd_tx.send(UiCommand::LinemonClose).await.unwrap();
        // A reliable result is sent after the close was handled; what was
        // queued before it is old.
        cmd_tx.send(UiCommand::ListLocalAddresses).await.unwrap();
        loop {
            if let Some(UiUpdate::LocalAddresses(_)) =
                next_update(&mut rx, Duration::from_secs(5)).await
            {
                break;
            }
        }
        while rx.try_recv().is_ok() {}
        // Several poll rounds pass.
        tokio::time::sleep(Duration::from_millis(900)).await;
        if let Ok(update) = rx.try_recv() {
            panic!("an update arrived after the close: {update:?}");
        }
        assert!(port_is_free(port), "the port was released");

        // The same hidden Channel opens again.
        cmd_tx
            .send(UiCommand::LinemonOpen(Box::new(udp_config(port))))
            .await
            .unwrap();
        until_linemon_running(&mut rx).await;

        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        handle.await.unwrap();
    }

    /// A port that cannot be opened comes back as a Linemon error, in the
    /// start path's own words and with no ordinary Channel error.
    #[tokio::test]
    async fn a_failed_linemon_open_is_a_linemon_error() {
        let (cmd_tx, mut rx, handle) = run_driver();
        let port = free_udp_port();
        let _held = std::net::UdpSocket::bind(("127.0.0.1", port)).unwrap();
        cmd_tx
            .send(UiCommand::LinemonOpen(Box::new(udp_config(port))))
            .await
            .unwrap();
        let why = loop {
            match next_update(&mut rx, Duration::from_secs(5)).await {
                Some(UiUpdate::Linemon(LinemonUpdate::Error(why))) => break why,
                Some(UiUpdate::ChannelError(_, why)) => panic!("an ordinary error: {why}"),
                Some(_) => {}
                None => panic!("no error arrived"),
            }
        };
        assert!(why.contains("failed to bind interface"), "{why}");
        assert!(why.starts_with("failed"), "no Channel-name prefix: {why}");

        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        handle.await.unwrap();
    }

    /// Shutdown stops the hidden Channel along with the rest.
    #[tokio::test]
    async fn shutdown_stops_linemons_channel() {
        let (cmd_tx, mut rx, handle) = run_driver();
        let port = free_udp_port();
        cmd_tx
            .send(UiCommand::LinemonOpen(Box::new(udp_config(port))))
            .await
            .unwrap();
        until_linemon_running(&mut rx).await;
        assert!(!port_is_free(port));
        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("the driver stopped")
            .unwrap();
        assert!(port_is_free(port), "Shutdown released the port");
    }

    #[test]
    fn linemon_stream_is_periodic_and_its_error_is_not() {
        let stream = UiUpdate::Linemon(LinemonUpdate::Stream(Box::new(StreamDelta {
            generation: 1,
            base_offset: 0,
            bytes: std::sync::Arc::from(&[][..]),
            end_offset: 0,
        })));
        assert!(stream.is_periodic());
        let error = UiUpdate::Linemon(LinemonUpdate::Error("x".to_string()));
        assert!(!error.is_periodic());
    }
}
