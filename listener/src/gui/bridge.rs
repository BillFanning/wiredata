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

use std::{collections::HashMap, time::Duration};

use tokio::sync::mpsc::{Receiver, Sender};

use crate::config::{
    ChannelConfig, DataBits, DisplayConfig, DisplayRecordingConfig, FlowControl, InterfaceConfig,
    Parity, Profile, RawRecordingConfig, RetentionConfig, StopBits,
};
use crate::core::{ChannelId, ChannelName, DisplayViewId, RuntimeEvent};
use crate::runtime::{ChannelSnapshot, ChannelStats, Listener, PipelineCapacities, StreamDelta};
use crate::transport::udp::UdpMode;
use crate::transport::SerialControlLines;

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
            format!("UDP {mode} · bind {}:{}", udp.bind_address, udp.port)
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
        Self {
            listener,
            events,
            commands,
            updates,
            repaint,
            channels: Vec::new(),
            selected: None,
            stream_cursors: HashMap::new(),
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
                _ = snapshot_tick.tick() => self.poll_snapshots().await,
                _ = reconnect_tick.tick() => self.listener.reconnect_tick().await,
            }
        }

        // The App is gone (or asked to shut down): stop every channel cleanly.
        self.listener.shutdown().await;
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

    /// The single-channel lifecycle path shared by `CommitAndStart` and each entry of
    /// `StartAll`: optionally commit `config`, optionally `start`, via the runtime's
    /// `commit_and_start` (which owns the Faulted→Stopped→Starting recovery). Echoes
    /// `ChannelReconfigured` when a config was committed so the UI refreshes its editor
    /// and connection details; reports any failure as a `ChannelError`.
    async fn commit_and_start_one(
        &mut self,
        id: ChannelId,
        config: Option<ChannelConfig>,
        start: bool,
    ) {
        let echo = config.as_ref().map(|c| (describe_interface(c), c.clone()));
        match self.listener.commit_and_start(id, config, start).await {
            Ok(()) => {
                if let Some((details, config)) = echo {
                    self.push(UiUpdate::ChannelReconfigured(id, details, Box::new(config)));
                }
            }
            Err(err) => {
                self.push_channel_error(id, err);
            }
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
            UiCommand::Start(id) => {
                // Surface the reason on failure (e.g. a bind "address in use"),
                // instead of leaving the channel Faulted with no explanation.
                if let Err(err) = self.listener.start(id).await {
                    self.push_channel_error(id, err);
                }
            }
            UiCommand::Stop(id) => {
                if let Err(err) = self.listener.stop(id).await {
                    self.push_channel_error(id, err);
                }
            }
            UiCommand::CommitAndStart { id, config, start } => {
                self.commit_and_start_one(id, config.map(|c| *c), start)
                    .await;
            }
            UiCommand::StartAll(batch) => {
                // Run each channel through the same single-channel commit_and_start
                // path, draining the runtime's lifecycle events after each so a large
                // batch can't pile them up undrained in the bounded event channel (the
                // `select!` arm that normally drains them doesn't run while we're here).
                for (id, config) in batch {
                    self.commit_and_start_one(id, Some(*config), true).await;
                    self.drain_events();
                }
            }
            UiCommand::StopAll => {
                // Stop each *live* channel server-side, off the UI thread. `stop_if_live`
                // skips an already-Stopped channel (no IllegalTransition to swallow), so
                // a genuine illegal transition still surfaces as an error instead of
                // disappearing. Drain events after each so the batch can't overflow the
                // bounded event channel and lose a ChannelStopped.
                for id in self.channels.clone() {
                    if let Err(err) = self.listener.stop_if_live(id).await {
                        self.push_channel_error(id, err);
                    }
                    self.drain_events();
                }
            }
            UiCommand::RemoveChannel(id) => {
                let _ = self.listener.remove_channel(id).await;
                self.channels.retain(|c| *c != id);
                self.stream_cursors.remove(&id);
                self.push(UiUpdate::ChannelRemoved(id));
            }
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
            UiCommand::LoadProfile(path) => self.load_profile(path).await,
            UiCommand::NewProfile => self.new_profile().await,
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

    /// Replace the workspace with a loaded profile (§70). Parse + schema-check
    /// first, so a bad file leaves the current channels untouched; only on success
    /// do we stop/remove every existing channel and register the loaded ones
    /// (Stopped — load never starts a channel). Each removal/addition emits the
    /// usual update so the App folds the swap with no special-casing.
    /// Tear the workspace down and leave it empty. Shares `load_profile`'s
    /// teardown so a new profile and a loaded one start from the same state —
    /// live channels stopped first (§8.5), selection and cursors cleared.
    async fn new_profile(&mut self) {
        for id in std::mem::take(&mut self.channels) {
            let _ = self.listener.remove_channel(id).await;
            self.push(UiUpdate::ChannelRemoved(id));
        }
        self.selected = None;
        self.stream_cursors.clear();
    }

    async fn load_profile(&mut self, path: std::path::PathBuf) {
        let profile = match Profile::load(&path) {
            Ok(p) => p,
            Err(err) => {
                self.push(UiUpdate::ProfileError(format!("load failed: {err}")));
                return;
            }
        };
        // Tear down the old workspace (stops live channels first, §8.5).
        for id in std::mem::take(&mut self.channels) {
            let _ = self.listener.remove_channel(id).await;
            self.push(UiUpdate::ChannelRemoved(id));
        }
        self.selected = None;
        self.stream_cursors.clear();
        // Validate before registering (§71, ADR-014): a hand-edited profile can carry an
        // invalid channel (e.g. a duplicate name) — skip those and load the rest, like
        // the CLI. Validation is workspace-level (`Profile::validate`), so duplicate
        // names flag every carrier.
        let results = profile.validate();
        let mut skipped: Vec<String> = Vec::new();
        // Register the loaded channels Stopped.
        for (config, (name, result)) in profile.channels.into_iter().zip(results) {
            if let Err(errors) = result {
                skipped.push(format!("\"{name}\": {errors:?}"));
                continue;
            }
            let name = config.name.as_str().to_string();
            let details = describe_interface(&config);
            let echo = config.clone();
            let id = self.listener.add_channel(config);
            self.channels.push(id);
            self.push(UiUpdate::ChannelAdded(id, name, details, Box::new(echo)));
        }
        if !skipped.is_empty() {
            self.push(UiUpdate::ProfileError(format!(
                "loaded with {} channel(s) skipped (invalid): {}",
                skipped.len(),
                skipped.join("; ")
            )));
        }
        self.push(UiUpdate::ProfileLoaded(profile.name));
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
