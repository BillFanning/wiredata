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

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use tokio::sync::mpsc::{self, Receiver, Sender};

use crate::config::{
    describe_channel_errors, ChannelConfig, DataBits, DisplayConfig, DisplayRecordingConfig,
    FlowControl, InterfaceConfig, Parity, Profile, RawRecordingConfig, ReconnectPolicy,
    RetentionConfig, StopBits,
};
use crate::core::{ChannelId, ChannelName, ChannelState, DisplayViewId, RuntimeEvent};
use crate::runtime::{
    ChannelSnapshot, ChannelStats, DrainedStop, Listener, OpenedStart, PipelineCapacities,
    ReconnectAttempt, StreamDelta,
};
use crate::transport::udp::UdpMode;
use crate::transport::SerialControlLines;

use super::bind_scope::{local_addresses, BindScope, LocalAddress};

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
    PauseDisplay(ChannelId, DisplayViewId),
    ResumeDisplay(ChannelId, DisplayViewId),
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
}

/// Whether a registered profile can resume (ADR-045). It must still be where it
/// was registered, load, and have only valid channels; and every folder a
/// channel records to when it starts must carry its destination marker
/// (ADR-043) — without it the folder may be on a different disk, such as an
/// empty mount point whose drive is not plugged in. Only reads files.
pub(crate) fn resume_check(path: &std::path::Path) -> Result<Profile, String> {
    if !path.is_file() {
        return Err(format!("{} is no longer there", path.display()));
    }
    let profile =
        Profile::load(path).map_err(|err| format!("{} does not load: {err}", path.display()))?;
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
    Ok(profile)
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
        InterfaceConfig::TcpListener(tcp) => {
            format!("TCP listener · {}:{}", tcp.bind_address, tcp.port)
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
}

/// A workspace change, made once no Channel has steps left (ADR-052).
enum WorkspaceStep {
    /// Forget the selection and the stream cursors.
    Clear,
    /// Register a profile's Channels, and start them for a resume.
    Load { profile: Box<Profile>, start: bool },
    /// Announce a resume, after the runtime events of its starts.
    Resumed(String),
}

pub struct Driver {
    listener: Listener,
    events: Receiver<RuntimeEvent>,
    commands: Receiver<UiCommand>,
    updates: Sender<UiUpdate>,
    repaint: Box<dyn Fn() + Send>,
    /// Channels registered so far, polled for stats.
    channels: Vec<ChannelId>,
    /// The channel currently on screen; only this one gets a snapshot + stream delta
    /// polled (the rest get cheap stats).
    selected: Option<ChannelId>,
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
}

impl Driver {
    /// Build a driver over an already-constructed `Listener` whose event stream has
    /// been taken. (`spawn` does this for the real GUI; tests build it directly.)
    pub fn new(
        listener: Listener,
        events: Receiver<RuntimeEvent>,
        commands: Receiver<UiCommand>,
        updates: Sender<UiUpdate>,
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
            stream_cursors: HashMap::new(),
            steps: HashMap::new(),
            workspace: VecDeque::new(),
            done_tx,
            done_rx,
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
        loop {
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
            if !self.steps.is_empty() {
                break;
            }
            match self.workspace.pop_front() {
                Some(change) => self.run_workspace(change),
                None => break,
            }
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
                self.push(UiUpdate::ChannelRemoved(id));
            }
            Step::Reconnected(attempt) => {
                let started = self.steps.get(&id).is_some_and(|steps| steps.started);
                self.listener.finish_reconnect(*attempt, started);
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
        }
    }

    /// Queue the reconnect attempts now due (§9.1): each a stop and a start,
    /// off the loop like any other (ADR-052). A Channel mid-step waits.
    fn queue_reconnects(&mut self) {
        let attempts = self
            .listener
            .reconnects_due(std::time::Instant::now(), |id| self.steps.contains_key(&id));
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
                self.stream_cursors.clear();
            }
            WorkspaceStep::Load { profile, start } => {
                let added = self.register_profile(*profile);
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
            UiCommand::PauseDisplay(id, view) => {
                let _ = self.listener.pause_display(id, view);
            }
            UiCommand::ResumeDisplay(id, view) => {
                let _ = self.listener.resume_display(id, view);
            }
            UiCommand::SetRts(id, on) => {
                let _ = self.listener.set_rts(id, on).await;
            }
            UiCommand::SetDtr(id, on) => {
                let _ = self.listener.set_dtr(id, on).await;
            }
            UiCommand::SetRecording(id, enabled, raw_config) => {
                // Arm from the settings read at click time (passed from the editor), so
                // recording goes exactly where the controls say — no Apply needed. A
                // `false` return means the command didn't reach a running data channel;
                // a begin that reaches the pipeline but can't open the file reports
                // separately via RecordingFaulted (§55).
                if !self.listener.set_recording(id, enabled, *raw_config).await {
                    self.push_channel_error(id, "can't change recording — channel isn't running");
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
                if !self
                    .listener
                    .set_display_recording(id, enabled, *display)
                    .await
                {
                    self.push_channel_error(
                        id,
                        "can't change display recording — channel isn't running",
                    );
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
            UiCommand::SaveProfile(path) => self.save_profile(path),
            UiCommand::LoadProfile(path) => self.load_profile(path),
            UiCommand::CheckResume(path) => {
                let checked = resume_check(&path).map(|profile| profile.name);
                self.push(UiUpdate::ResumeChecked(path, checked));
            }
            UiCommand::Resume(path) => self.resume(path),
            UiCommand::ListLocalAddresses => {
                self.push(UiUpdate::LocalAddresses(local_addresses()));
            }
            UiCommand::NewProfile => self.new_profile(),
            UiCommand::Shutdown => return false,
        }
        true
    }

    /// Save every registered channel's config to a TOML profile (§67). Gathers
    /// configs from the authoritative `Listener` in `self.channels` order; a missing
    /// config (a channel removed mid-flight) is skipped. Non-fatal: an I/O or
    /// serialization error is reported via `ProfileError`, not a panic.
    fn save_profile(&mut self, path: std::path::PathBuf) {
        let mut profile = Profile::new(profile_name_from_path(&path));
        profile.channels = self
            .channels
            .iter()
            .filter_map(|id| self.listener.config(*id).cloned())
            .collect();
        match profile.save(&path) {
            Ok(()) => {
                self.push(UiUpdate::ProfileSaved(path));
            }
            Err(err) => {
                self.push(UiUpdate::ProfileError(format!("save failed: {err}")));
            }
        }
    }

    /// Tear the workspace down and leave it empty: every Channel is stopped
    /// (§8.5) and removed, then the selection and cursors are cleared.
    fn new_profile(&mut self) {
        self.queue_removal_of_all();
        self.workspace.push_back(WorkspaceStep::Clear);
    }

    /// Resume a registered profile (ADR-045): check it again — the countdown
    /// gave a drive time to go — then load it and start every channel. A
    /// channel that cannot start is reported and left to its reconnect policy.
    fn resume(&mut self, path: std::path::PathBuf) {
        let profile = match resume_check(&path) {
            Ok(profile) => profile,
            Err(why) => {
                self.push(UiUpdate::ResumeChecked(path, Err(why)));
                return;
            }
        };
        let name = profile.name.clone();
        self.queue_removal_of_all();
        self.workspace.push_back(WorkspaceStep::Clear);
        self.workspace.push_back(WorkspaceStep::Load {
            profile: Box::new(profile),
            start: true,
        });
        self.workspace.push_back(WorkspaceStep::Resumed(name));
    }

    /// Replace the workspace with the profile at `path` (§70): every Channel
    /// is stopped and removed first, and the profile's Channels are registered
    /// Stopped (load never starts a channel).
    fn load_profile(&mut self, path: std::path::PathBuf) {
        let profile = match Profile::load(&path) {
            Ok(p) => p,
            Err(err) => {
                self.push(UiUpdate::ProfileError(format!("load failed: {err}")));
                return;
            }
        };
        self.queue_removal_of_all();
        self.workspace.push_back(WorkspaceStep::Clear);
        self.workspace.push_back(WorkspaceStep::Load {
            profile: Box::new(profile),
            start: false,
        });
    }

    /// Register a profile's Channels Stopped, and return their ids. Each
    /// addition emits the usual update, so the App folds the swap with no
    /// special-casing.
    fn register_profile(&mut self, profile: Profile) -> Vec<ChannelId> {
        // Validate before registering (§71, ADR-014): a hand-edited profile can carry an
        // invalid channel (e.g. a duplicate name) — skip those and load the rest, like
        // the CLI. Validation is workspace-level (`Profile::validate`), so duplicate
        // names flag every carrier.
        let results = profile.validate();
        let mut skipped: Vec<String> = Vec::new();
        let mut added = Vec::new();
        for (config, (name, result)) in profile.channels.into_iter().zip(results) {
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
        if !skipped.is_empty() {
            self.push(UiUpdate::ProfileError(format!(
                "loaded with {} channel(s) skipped (invalid):\n{}",
                skipped.len(),
                skipped.join("\n")
            )));
        }
        self.push(UiUpdate::ProfileLoaded(profile.name));
        added
    }

    /// Poll channels for the UI's pull surface. Every channel gets cheap **stats**
    /// (per-tab health); the **selected** channel additionally gets a snapshot (its
    /// bounded diagnostic/match detail) and an incremental **stream delta** (only the
    /// scrollback bytes new since our cursor — never the whole buffer, §87/ADR-009).
    /// Stopped/unknown channels yield `None`.
    ///
    /// Takes `&mut self` (not `&self`) so the `run` future stays `Send` — a shared
    /// `&Driver` held across the await would require `Driver: Sync`, which the mpsc
    /// `Receiver` is not. The real driver runs via `block_on` (no `Send` needed),
    /// but keeping it `Send` lets it run on a multi-thread runtime and be spawned.
    async fn poll_snapshots(&mut self) {
        for id in self.channels.clone() {
            if Some(id) == self.selected {
                // Two requests to the pipeline per poll, the snapshot and the
                // stream delta, where one would do. Not worth changing on its
                // own: fold them together when `PipelineRequest` next changes.
                if let Some(snap) = self.listener.snapshot(id).await {
                    self.push(UiUpdate::Snapshot(id, Box::new(snap)));
                }
                // Incremental live stream (§87, ADR-009): fetch only the bytes new
                // since our cursor, so we never re-ship the whole scrollback. A
                // "caught up" empty delta is a no-op (just advance past it). For a
                // non-empty delta, advance the cursor ONLY if the push was accepted —
                // if the bounded update channel was full and dropped it, keep the
                // cursor so the next poll re-fetches those bytes instead of skipping
                // them. (Previously the cursor advanced unconditionally, so a dropped
                // delta was lost forever and the view froze while bytes kept counting.)
                let cursor = self.stream_cursors.get(&id).copied().unwrap_or_default();
                if let Some(delta) = self.listener.stream_delta(id, cursor.offset).await {
                    let end = delta.end_offset;
                    let generation = delta.generation;
                    // Empty caught-up deltas normally stay off the UI channel. A new
                    // generation is different: forward even an empty window so a
                    // restart clears stale bytes when no new data has arrived yet.
                    let needs_update =
                        Some(generation) != cursor.generation || !delta.bytes.is_empty();
                    let advance =
                        !needs_update || self.push(UiUpdate::StreamDelta(id, Box::new(delta)));
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
            } else if let Some(stats) = self.listener.channel_stats(id).await {
                self.push(UiUpdate::Stats(id, Box::new(stats)));
            }
            // Live serial control/status lines (§161); `None` for non-serial or
            // stopped channels.
            if let Some(lines) = self.listener.serial_control_lines(id) {
                self.push(UiUpdate::ControlLines(id, lines));
            }
        }
    }

    /// Hand one update to the GUI and wake it. Advisory and non-blocking: a full
    /// update channel drops the item rather than stalling the driver (§99). Returns
    /// whether the update was actually enqueued — the stream-delta poll uses this to
    /// avoid advancing its cursor past bytes the UI never received (a dropped delta
    /// would otherwise be lost forever, freezing the view while bytes kept arriving).
    fn push(&self, update: UiUpdate) -> bool {
        let sent = self.updates.try_send(update).is_ok();
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
        self.push(UiUpdate::ChannelError(id, error.to_string()));
    }
}

/// A handle the egui App holds: the command sender, the update receiver, and the
/// runtime thread's join handle (kept alive for the App's lifetime).
pub struct BridgeHandle {
    pub commands: Sender<UiCommand>,
    pub updates: Receiver<UiUpdate>,
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

/// Channel depths for the bridge. Commands are rare (user clicks); updates are
/// higher-volume (events + 5 Hz snapshots) but advisory.
const COMMAND_CAPACITY: usize = 64;
const UPDATE_CAPACITY: usize = 256;

/// Start the runtime on its own thread and return the App's [`BridgeHandle`].
///
/// `repaint` is invoked whenever an update is pushed; the App passes
/// `egui::Context::request_repaint` so a streaming source wakes the UI.
pub fn spawn(repaint: impl Fn() + Send + 'static) -> anyhow::Result<BridgeHandle> {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(COMMAND_CAPACITY);
    let (upd_tx, upd_rx) = tokio::sync::mpsc::channel(UPDATE_CAPACITY);

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
        let (upd_tx, mut upd_rx) = tokio::sync::mpsc::channel(4096);
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
        Receiver<UiUpdate>,
        Vec<ChannelId>,
        tokio::task::JoinHandle<()>,
    ) {
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        listener.delay_opens_for_test(open_takes);
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (upd_tx, mut upd_rx) = tokio::sync::mpsc::channel(4096);
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

    #[tokio::test]
    async fn the_driver_lists_local_addresses() {
        async fn next(rx: &mut Receiver<UiUpdate>) -> UiUpdate {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("driver update timed out")
                .expect("update stream closed")
        }
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
        let (upd_tx, mut upd_rx) = tokio::sync::mpsc::channel(64);
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

    /// The driver translates commands into runtime calls, forwards lifecycle events,
    /// and pushes snapshots — end to end over a loopback UDP channel.
    #[tokio::test]
    async fn driver_adds_starts_reports_and_snapshots_a_channel() {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (upd_tx, mut upd_rx) = tokio::sync::mpsc::channel(64);

        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let driver = Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {}));
        let handle = tokio::spawn(driver.run());

        // Helper: await the next update of interest within a timeout.
        async fn next(rx: &mut Receiver<UiUpdate>) -> UiUpdate {
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
        let (upd_tx, mut upd_rx) = tokio::sync::mpsc::channel(64);
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let driver = Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {}));
        let handle = tokio::spawn(driver.run());

        async fn next(rx: &mut Receiver<UiUpdate>) -> UiUpdate {
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
        async fn next(rx: &mut Receiver<UiUpdate>) -> UiUpdate {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("driver update timed out")
                .expect("update stream closed")
        }

        let path = temp_profile_path();

        // First driver: add two channels, then save.
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (upd_tx, mut upd_rx) = tokio::sync::mpsc::channel(64);
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
        let (upd_tx, mut upd_rx) = tokio::sync::mpsc::channel(64);
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
        assert_eq!(resume_check(&path).unwrap().name, "Logging");

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
        async fn next(rx: &mut Receiver<UiUpdate>) -> UiUpdate {
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
        let (upd_tx, mut upd_rx) = tokio::sync::mpsc::channel(64);
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let handle =
            tokio::spawn(Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {})).run());
        cmd_tx.send(UiCommand::Resume(path.clone())).await.unwrap();

        let (mut added, mut started) = (false, false);
        loop {
            match next(&mut upd_rx).await {
                UiUpdate::ChannelAdded(..) => added = true,
                UiUpdate::Event(RuntimeEvent::ChannelStarted(_)) => started = true,
                UiUpdate::Resumed(name, _) => {
                    assert_eq!(name, "Logging");
                    break;
                }
                _ => {}
            }
        }
        assert!(added, "the profile was loaded");
        assert!(started, "its channel was started");

        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        handle.await.unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&recordings);
    }

    /// A LoadProfile of a missing/invalid file reports ProfileError and leaves the
    /// existing workspace untouched (no channels removed).
    #[tokio::test]
    async fn loading_a_missing_profile_errors_without_touching_the_workspace() {
        async fn next(rx: &mut Receiver<UiUpdate>) -> UiUpdate {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("driver update timed out")
                .expect("update stream closed")
        }

        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (upd_tx, mut upd_rx) = tokio::sync::mpsc::channel(64);
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
        async fn next(rx: &mut Receiver<UiUpdate>) -> UiUpdate {
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
        let (upd_tx, mut upd_rx) = tokio::sync::mpsc::channel(64);
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let handle =
            tokio::spawn(Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {})).run());

        cmd_tx
            .send(UiCommand::LoadProfile(path.clone()))
            .await
            .unwrap();

        let mut added = Vec::new();
        let mut saw_skip_error = false;
        loop {
            match next(&mut upd_rx).await {
                UiUpdate::ChannelAdded(_, name, _, _) => added.push(name),
                UiUpdate::ProfileError(msg) => {
                    assert!(msg.contains("skipped"), "skip notice expected, got: {msg}");
                    // Each skipped Channel on its own line, its reason in words
                    // rather than Rust's debug form.
                    let reasons: Vec<&str> = msg
                        .lines()
                        .filter(|line| line.starts_with("\"Dup\": "))
                        .collect();
                    assert_eq!(reasons.len(), 2, "{msg}");
                    for reason in reasons {
                        assert!(reason.contains("names must be unique"), "{msg}");
                        assert!(!reason.contains("DuplicateChannelName"), "{msg}");
                    }
                    saw_skip_error = true;
                }
                UiUpdate::ProfileLoaded(_) => break,
                _ => {}
            }
        }
        // Only the uniquely-named channel registered; both duplicates were skipped.
        assert_eq!(added, vec!["Unique".to_string()]);
        assert!(
            saw_skip_error,
            "a skip notice should report the dropped duplicates"
        );

        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        let _ = std::fs::remove_file(&path);
    }

    /// Dropping the command sender ends the driver (the App closed).
    #[tokio::test]
    async fn dropping_the_command_sender_stops_the_driver() {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<UiCommand>(4);
        let (upd_tx, _upd_rx) = tokio::sync::mpsc::channel(16);
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
        async fn next(rx: &mut Receiver<UiUpdate>) -> UiUpdate {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("driver update timed out")
                .expect("update stream closed")
        }

        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (upd_tx, mut upd_rx) = tokio::sync::mpsc::channel(64);
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
}
