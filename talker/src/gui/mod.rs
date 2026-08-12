mod channels;
mod detail;
mod diagnostics;
mod display;
mod draft;
mod notice;
mod view;
mod widgets;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Context as _;
use egui::{Align, Layout, ScrollArea};
use wiredata_ui::selection;

use crate::core::{
    channel::{ChannelConfig, ChannelId, InterfaceConfig},
    logging::{
        FileLogConfig, FileLogState, FileLogToggle, LogEvent, LogLevel, LogLevelHandle,
        LoggingConfig,
    },
    message::{code_page_replacements, CodePage, CodePageReplacementSummary, MessageConfig},
    profile::Profile,
    runner,
    scheduler::Schedule,
    supervisor::TalkerSupervisor,
};

use draft::{ConnDraft, ConnKind, ScheduleDraft, UdpModeDraft};
use view::ChannelView;

// ── Entry point ───────────────────────────────────────────────────────────────

/// Detach the inherited console when going graphical. The `talker` binary is a
/// console-subsystem app so the headless CLI works when launched from a
/// terminal; the cost is that a double-click into the GUI leaves a console
/// window behind. Freeing it here removes that window. `talker-gui.exe` never
/// allocates one in the first place. No-op off Windows.
#[cfg(windows)]
fn detach_console() {
    // SAFETY: `FreeConsole` takes no arguments and is always safe to call; it
    // simply detaches the process from its console if it has one.
    unsafe {
        let _ = windows_sys::Win32::System::Console::FreeConsole();
    }
}

#[cfg(not(windows))]
fn detach_console() {}

/// Launch the graphical interface.
///
/// **Shared GUI-startup funnel (two-binary invariant — see
/// `src/bin/talker-gui.rs`).** Both GUI entry points route through here: the
/// `talker-gui` (windows-subsystem) binary and `talker.exe`'s `--gui` path.
/// Put ALL GUI startup (console detach, logging, window options) in this
/// function so the two binaries stay identical — never in either `main`.
pub fn run(initial_profile: Option<PathBuf>) -> anyhow::Result<()> {
    detach_console(); // drop the double-click console before the window opens

    // Windows 11 would otherwise ignore the high-rate timer request while
    // the window is minimized — the usual state of a long soak (ADR-017).
    crate::core::timing::keep_timer_resolution_when_minimized();
    let (log_tx, log_rx) = crossbeam_channel::bounded::<LogEvent>(512);
    // `logging` stays in scope until `run_native` returns so the
    // file-appender worker guards aren't dropped early. The reload
    // handle is cloned out for the GUI's log-level ComboBox.
    let logging = crate::core::logging::init(&LoggingConfig::default(), Some(log_tx))
        .context("initializing logging")?;
    let level_handle = logging.level_handle();
    let file_log = logging
        .file_log_toggle()
        .context("GUI file-log control was not installed")?;

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 740.0])
            .with_min_inner_size([640.0, 480.0]),
        // Don't persist/restore window geometry (listener's lesson, ADR-016):
        // eframe restores the saved window state *after* the window is shown,
        // which produced a double frame/title-bar flash on launch — and a bad
        // tiny geometry could be restored too. The window always opens at the
        // default size. Zoom is egui's own `zoom_factor`, persisted with egui
        // memory; the last profile is persisted separately.
        persist_window: false,
        ..Default::default()
    };
    eframe::run_native(
        "Talker",
        options,
        Box::new(move |cc| {
            Ok(Box::new(TalkerApp::new(
                log_rx,
                initial_profile,
                &cc.egui_ctx,
                cc.storage,
                level_handle,
                file_log,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

// ── App ───────────────────────────────────────────────────────────────────────

// The status-queue bound moved into core with the supervisor (ADR-019); the
// detail header's performance readouts still reference it via this path.
pub(crate) use crate::core::supervisor::STATUS_QUEUE_CAP;

/// eframe-storage key for the newline-joined recent-profiles list.
const RECENT_PROFILES_KEY: &str = "recent_profiles";
/// How many entries the Profile menu's Recent section keeps.
const MAX_RECENT_PROFILES: usize = 8;

/// All-or-none draft → profile conversion, **index-preserving**: any channel
/// or message that cannot convert aborts the whole flush with human-readable
/// reasons, so `profile.channels[i]` always corresponds to `conn_drafts[i]`.
/// Free function (not a method) so it is unit-testable without a `TalkerApp`.
fn drafts_to_channels(
    conn_drafts: &[ConnDraft],
    sched_drafts: &[Vec<ScheduleDraft>],
) -> Result<Vec<ChannelConfig>, Vec<String>> {
    let mut channels = Vec::with_capacity(conn_drafts.len());
    let mut problems = Vec::new();
    for (i, draft) in conn_drafts.iter().enumerate() {
        let label = if draft.name.is_empty() {
            format!("Channel {}", i + 1)
        } else {
            format!("Channel {} (\u{201C}{}\u{201D})", i + 1, draft.name)
        };
        let Some(interface) = draft.to_config() else {
            problems.push(format!(
                "{label}: interface configuration is incomplete or invalid"
            ));
            continue;
        };
        let mut messages = Vec::new();
        for (m, d) in sched_drafts
            .get(i)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .enumerate()
        {
            match d.to_message_config() {
                Some(mc) => match mc.validate() {
                    Ok(()) => messages.push(mc),
                    Err(err) => problems.push(format!("{label}, message {}: {err:#}", m + 1)),
                },
                None => problems.push(format!(
                    "{label}, message {}: invalid interval — fix or remove it",
                    m + 1
                )),
            }
        }
        let mut cfg = ChannelConfig::new(interface, messages);
        cfg.name = draft.name.clone();
        cfg.cadence_alignment = draft.cadence_alignment;
        channels.push(cfg);
    }
    if problems.is_empty() {
        Ok(channels)
    } else {
        Err(problems)
    }
}

/// A loaded profile after every fallible read, parse, conversion, and payload
/// validation has completed. Building this is side-effect free with respect to
/// the active workspace, so the GUI can replace runners only after it exists.
struct PreparedProfileLoad {
    profile: Profile,
    conn_drafts: Vec<ConnDraft>,
    sched_drafts: Vec<Vec<ScheduleDraft>>,
}

enum MessagePreview {
    Incomplete,
    Invalid(String),
    Text(String),
    Hex(String),
    Ascii {
        bytes: Vec<u8>,
        code_page: CodePage,
        replacement_wire_offsets: Vec<usize>,
    },
}

/// Everything derived from one message draft that was previously rebuilt in
/// several widgets every repaint. One content revision performs one conversion,
/// one compile, and one fixed-time preview render; all consumers borrow it.
struct MessageDraftAnalysis {
    config: Option<MessageConfig>,
    validation_error: Option<String>,
    /// Exact wire length from the one fixed-time preview render. Dynamic
    /// fields are fixed-width, so capacity preflight can reuse it without
    /// rendering or cloning the message on every frame.
    wire_len: Option<usize>,
    replacements: Option<CodePageReplacementSummary>,
    preview: MessagePreview,
}

impl MessageDraftAnalysis {
    fn build(draft: &ScheduleDraft) -> Self {
        use std::fmt::Write as _;

        let replacements = (draft.payload_kind == draft::PayloadKind::Ascii)
            .then(|| code_page_replacements(&draft.ascii_text, draft.ascii_code_page))
            .flatten();
        let Some(config) = draft.to_message_config() else {
            return Self {
                config: None,
                validation_error: None,
                wire_len: None,
                replacements,
                preview: MessagePreview::Incomplete,
            };
        };

        let reference = chrono::DateTime::<chrono::Utc>::from_timestamp(1_704_110_400, 0)
            .unwrap_or(chrono::DateTime::<chrono::Utc>::UNIX_EPOCH);
        match config.compile() {
            Ok(compiled) => {
                let timestamp_len = config
                    .timestamp
                    .as_ref()
                    .map(|timestamp| timestamp.format(reference).len())
                    .unwrap_or(0);
                let bytes = compiled.render_at(reference);
                let wire_len = bytes.len();
                let preview = match draft.payload_kind {
                    draft::PayloadKind::Ascii => MessagePreview::Ascii {
                        replacement_wire_offsets: replacements
                            .as_ref()
                            .map(|summary| {
                                summary
                                    .payload_offsets
                                    .iter()
                                    .map(|offset| timestamp_len + offset)
                                    .collect()
                            })
                            .unwrap_or_default(),
                        bytes,
                        code_page: draft.ascii_code_page,
                    },
                    draft::PayloadKind::Utf8 | draft::PayloadKind::Nmea => {
                        MessagePreview::Text(widgets::preview_text(&bytes))
                    }
                    draft::PayloadKind::Hex | draft::PayloadKind::Utf16 => {
                        let mut text = String::with_capacity(bytes.len().saturating_mul(3));
                        for (index, byte) in bytes.iter().enumerate() {
                            if index > 0 {
                                text.push(' ');
                            }
                            let _ = write!(text, "{byte:02X}");
                        }
                        MessagePreview::Hex(text)
                    }
                };
                Self {
                    config: Some(config),
                    validation_error: None,
                    wire_len: Some(wire_len),
                    replacements,
                    preview,
                }
            }
            Err(error) => {
                let error = format!("{error:#}");
                Self {
                    config: Some(config),
                    validation_error: Some(error.clone()),
                    wire_len: None,
                    replacements,
                    preview: MessagePreview::Invalid(error),
                }
            }
        }
    }
}

#[derive(Default)]
struct MessageAnalysisCache {
    revision: Option<u64>,
    analysis: Option<MessageDraftAnalysis>,
    #[cfg(test)]
    rebuilds: usize,
}

impl MessageAnalysisCache {
    fn refresh(&mut self, draft: &ScheduleDraft) -> &MessageDraftAnalysis {
        if self.revision != Some(draft.revision()) {
            self.analysis = Some(MessageDraftAnalysis::build(draft));
            self.revision = Some(draft.revision());
            #[cfg(test)]
            {
                self.rebuilds += 1;
            }
        }
        self.analysis
            .as_ref()
            .expect("assigned above whenever the revision was recorded")
    }
}

fn analyzed_messages_match(
    analyses: Option<&[MessageAnalysisCache]>,
    target: &[MessageConfig],
) -> bool {
    let Some(analyses) = analyses else {
        return false;
    };
    analyses.len() == target.len()
        && analyses.iter().zip(target).all(|(cached, target)| {
            cached
                .analysis
                .as_ref()
                .and_then(|a| a.config.as_ref())
                .is_some_and(|config| config == target)
        })
}

fn prepare_profile_load(path: &Path) -> anyhow::Result<PreparedProfileLoad> {
    let mut profile = Profile::load(path)?;
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        profile.name = stem.to_string();
    }
    profile
        .validate()
        .with_context(|| format!("validating profile '{}'", profile.name))?;

    let conn_drafts: Vec<_> = profile
        .channels
        .iter()
        .map(|channel| {
            let mut draft = ConnDraft::from(&channel.interface);
            draft.name = channel.name.clone();
            draft.cadence_alignment = channel.cadence_alignment;
            draft
        })
        .collect();
    let sched_drafts: Vec<Vec<_>> = profile
        .channels
        .iter()
        .map(|channel| channel.messages.iter().map(ScheduleDraft::from).collect())
        .collect();

    let rebuilt = drafts_to_channels(&conn_drafts, &sched_drafts).map_err(|problems| {
        anyhow::anyhow!(
            "loaded profile cannot be represented by the GUI:\n{}",
            problems.join("\n")
        )
    })?;
    anyhow::ensure!(
        rebuilt == profile.channels,
        "loaded profile cannot be represented by the GUI without changing it"
    );

    Ok(PreparedProfileLoad {
        profile,
        conn_drafts,
        sched_drafts,
    })
}

/// A fully built replacement run. Constructing this value is pure draft
/// preflight: it performs no I/O and does not mutate supervisor state, so a
/// failure cannot disturb an already-running channel.
#[derive(Debug)]
struct PreparedChannelRun {
    interface: InterfaceConfig,
    messages: Vec<MessageConfig>,
    schedule: Schedule,
}

fn prepare_channel_run(
    conn: &ConnDraft,
    drafts: &[ScheduleDraft],
) -> anyhow::Result<PreparedChannelRun> {
    let interface = conn
        .to_config()
        .context("interface configuration is incomplete or invalid")?;
    anyhow::ensure!(!drafts.is_empty(), "channel has no messages");
    let messages = drafts
        .iter()
        .enumerate()
        .map(|(i, draft)| {
            draft.to_message_config().with_context(|| {
                format!("message {} is incomplete or has an invalid interval", i + 1)
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let schedule = Schedule::compile_unarmed(&messages)?.with_alignment(conn.cadence_alignment);
    Ok(PreparedChannelRun {
        interface,
        messages,
        schedule,
    })
}

/// Replace one supervisor slot only after its complete candidate run has
/// compiled. This ordering is the safety boundary: every error return occurs
/// before [`TalkerSupervisor::start`] can stop or reset the existing runner.
fn replace_channel_run(
    supervisor: &mut TalkerSupervisor,
    index: usize,
    label: String,
    conn: &ConnDraft,
    drafts: &[ScheduleDraft],
) -> anyhow::Result<()> {
    let prepared = prepare_channel_run(conn, drafts)?;
    supervisor.start(
        index,
        label,
        prepared.interface,
        prepared.messages,
        prepared.schedule,
    );
    Ok(())
}

struct TalkerApp {
    /// Repaint-on-status coalescer shared with every runner thread: a status
    /// wakes the UI instantly, but N statuses between frames cost **one**
    /// winit wake (see `wiredata_ui::repaint`). Re-armed at the top of each
    /// frame, before the status drain.
    repaint: std::sync::Arc<wiredata_ui::repaint::RepaintCoalescer>,
    profile: Profile,
    profile_path: Option<PathBuf>,
    dirty: bool,
    conn_drafts: Vec<ConnDraft>,
    sched_drafts: Vec<Vec<ScheduleDraft>>,
    /// Revision-keyed compile/validation/preview results, shape-matched to
    /// `sched_drafts`. This keeps long message text off unchanged repaint paths.
    message_analysis: Vec<Vec<MessageAnalysisCache>>,
    /// The channel collection (ADR-019): runner threads, command/status
    /// channels, draining buckets, and per-channel telemetry all live in
    /// core's supervisor — the GUI keeps only view-state and reads
    /// [`TalkerSupervisor::telemetry`] when rendering.
    sup: TalkerSupervisor,
    log_rx: crossbeam_channel::Receiver<LogEvent>,
    /// Buffered log lines paired with their level. The level (not a
    /// baked colour) is stored so the log re-colours live when the
    /// theme is toggled — see [`level_color`].
    log_lines: Vec<(String, tracing::Level)>,
    log_level: LogLevel,
    log_level_handle: LogLevelHandle,
    /// Per-channel GUI view state — Output buffer, acknowledged warnings, and
    /// rate estimator — indexed by slot position. One vector rather than one
    /// per field, so no lifecycle path can update some and forget the rest;
    /// see [`view`] for the bug that argued for it.
    views: Vec<ChannelView>,
    last_title: String,
    serial_ports: Vec<String>,
    /// `true` = dark theme, `false` = light. Persisted; toggled from
    /// the top-bar sun/moon button next to the zoom control.
    dark_mode: bool,
    /// Index of the channel shown in the detail pane. `None` only when
    /// there are no channels.
    selected: Option<usize>,
    /// Whether the channel list is collapsed to the thin status strip.
    channels_collapsed: bool,
    /// Per-channel log-event tallies for the channel-list rows, keyed by the
    /// slot's stable [`ChannelId`] (ADR-020) — a positional Vec misrouted a
    /// running runner's events after a channel above it was removed.
    log_counts: HashMap<ChannelId, LogCounts>,
    /// Per-severity display filters for the retained log panel. These never
    /// change what the recording threshold admits to the pane or file.
    log_visibility: LogVisibility,
    /// Runtime GUI file destination. The worker behind this handle owns every
    /// open/write/flush/close operation.
    file_log: FileLogToggle,
    file_log_state: FileLogState,
    file_log_config: Option<FileLogConfig>,
    /// Recently loaded/saved profile paths, most recent first (max
    /// [`MAX_RECENT_PROFILES`]); persisted via eframe storage.
    recent_profiles: Vec<PathBuf>,
    /// A pending "remove this channel?" confirmation; `Some(index)` while
    /// the modal is up.
    confirm_remove: Option<usize>,
    /// Mutations that the channel-card render loop has requested. Drained at
    /// the END of each frame (after egui's layout passes complete) — never
    /// mid-frame — so the state changes can't cause widgets to appear,
    /// disappear, or change identity between egui's first and second layout
    /// passes (which trips the "Widget rect changed id between passes" warn).
    deferred: DeferredActions,
}

#[derive(Default)]
struct DeferredActions {
    apply: Vec<usize>,
    /// Switch the detail pane to this channel (from a list-row click).
    select: Option<usize>,
    start: Option<usize>,
    stop: Option<usize>,
    start_all: bool,
    stop_all: bool,
    remove: Option<usize>,
    /// Add a channel of this kind (from the list header's `+ Add` menu).
    add_channel: Option<ConnKind>,
    refresh_ports: bool,
}

/// Per-channel tallies of log events attributed via the structured
/// `channel` tracing field (see `LogEvent::channel`), shown on the
/// channel-list rows. Reset when the channel starts, like the send counts.
#[derive(Clone, Copy, Default)]
struct LogCounts {
    info: u32,
    warn: u32,
    error: u32,
}

impl LogCounts {
    fn record(&mut self, level: tracing::Level) {
        match level {
            tracing::Level::ERROR => self.error = self.error.saturating_add(1),
            tracing::Level::WARN => self.warn = self.warn.saturating_add(1),
            tracing::Level::INFO => self.info = self.info.saturating_add(1),
            // Channel cards report technician-facing signal. Diagnostic detail
            // remains in the global log and must not inflate the INFO badge.
            tracing::Level::DEBUG | tracing::Level::TRACE => {}
        }
    }
}

#[derive(Clone, Copy)]
struct LogVisibility {
    trace: bool,
    debug: bool,
    info: bool,
    warn: bool,
    error: bool,
}

impl Default for LogVisibility {
    fn default() -> Self {
        // This preserves the old panel behavior: every admitted event was
        // visible until the reader hid its severity group.
        Self {
            trace: true,
            debug: true,
            info: true,
            warn: true,
            error: true,
        }
    }
}

impl LogVisibility {
    fn includes(self, level: tracing::Level) -> bool {
        match level {
            tracing::Level::TRACE => self.trace,
            tracing::Level::DEBUG => self.debug,
            tracing::Level::INFO => self.info,
            tracing::Level::WARN => self.warn,
            tracing::Level::ERROR => self.error,
        }
    }
}

/// Rolling-throughput window, matching Listener's technician-facing rate.
const RATE_WINDOW_SECS: u64 = 5;
const RATE_BUCKETS: usize = RATE_WINDOW_SECS as usize;

/// Lightweight bounded throughput estimator for a channel. Deltas from the
/// cumulative locally accepted message/byte totals are placed in five fixed
/// one-second buckets, so the displayed rate is a true rolling five-second
/// average that decays to zero during a quiet period.
#[derive(Clone, Copy)]
struct RateTracker {
    epoch: Instant,
    last_total: u64,
    last_bytes: u64,
    messages: [u64; RATE_BUCKETS],
    bytes: [u64; RATE_BUCKETS],
    newest_sec: u64,
    per_sec: f32,
    bytes_per_sec: f32,
}

impl RateTracker {
    fn new() -> Self {
        Self::with_epoch(Instant::now())
    }

    fn with_epoch(epoch: Instant) -> Self {
        Self {
            epoch,
            last_total: 0,
            last_bytes: 0,
            messages: [0; RATE_BUCKETS],
            bytes: [0; RATE_BUCKETS],
            newest_sec: 0,
            per_sec: 0.0,
            bytes_per_sec: 0.0,
        }
    }

    fn second_at(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.epoch).as_secs()
    }

    fn advance_to(&mut self, second: u64) {
        if second <= self.newest_sec {
            return;
        }
        let gap = (second - self.newest_sec).min(RATE_BUCKETS as u64);
        for offset in 1..=gap {
            let index = ((self.newest_sec + offset) % RATE_BUCKETS as u64) as usize;
            self.messages[index] = 0;
            self.bytes[index] = 0;
        }
        self.newest_sec = second;
    }

    fn refresh_rates(&mut self, second: u64) {
        let mut messages = 0u64;
        let mut bytes = 0u64;
        for candidate in second.saturating_sub(RATE_BUCKETS as u64 - 1)..=second {
            if candidate > self.newest_sec
                || self.newest_sec.saturating_sub(candidate) >= RATE_BUCKETS as u64
            {
                continue;
            }
            let index = (candidate % RATE_BUCKETS as u64) as usize;
            messages = messages.saturating_add(self.messages[index]);
            bytes = bytes.saturating_add(self.bytes[index]);
        }
        self.per_sec = messages as f32 / RATE_WINDOW_SECS as f32;
        self.bytes_per_sec = bytes as f32 / RATE_WINDOW_SECS as f32;
    }

    fn reset_window(&mut self, now: Instant) {
        self.epoch = now;
        self.messages.fill(0);
        self.bytes.fill(0);
        self.newest_sec = 0;
        self.per_sec = 0.0;
        self.bytes_per_sec = 0.0;
    }

    fn sample(&mut self, now: Instant, total: u64, bytes: u64, running: bool) {
        if !running {
            self.reset_window(now);
            self.last_total = total;
            self.last_bytes = bytes;
            return;
        }

        // A new run resets cumulative telemetry before its first UI sample.
        // If that first update already includes accepted messages, count them
        // from zero instead of losing them to saturating subtraction against
        // the preceding run's larger totals.
        if total < self.last_total || bytes < self.last_bytes {
            self.reset_window(now);
            self.last_total = 0;
            self.last_bytes = 0;
        }

        let second = self.second_at(now);
        self.advance_to(second);
        let index = (second % RATE_BUCKETS as u64) as usize;
        self.messages[index] =
            self.messages[index].saturating_add(total.saturating_sub(self.last_total));
        self.bytes[index] = self.bytes[index].saturating_add(bytes.saturating_sub(self.last_bytes));
        self.last_total = total;
        self.last_bytes = bytes;
        self.refresh_rates(second);
    }
}

impl TalkerApp {
    fn new(
        log_rx: crossbeam_channel::Receiver<LogEvent>,
        initial_profile: Option<PathBuf>,
        ctx: &egui::Context,
        storage: Option<&dyn eframe::Storage>,
        log_level_handle: LogLevelHandle,
        file_log: FileLogToggle,
    ) -> Self {
        // Default to dark; persisted across runs. Stored as the string
        // "false" only when the user has switched to light.
        let dark_mode = storage
            .and_then(|s| s.get_string("dark_mode"))
            .map(|s| s != "false")
            .unwrap_or(true);
        // The shared wiredata look (ADR-016): font stack, both themes'
        // visuals, and the style tweaks come from `wiredata-ui`, so talker
        // and listener read as one product. Talker keeps its dark/light
        // toggle; `apply_theme` just picks which installed theme is active.
        wiredata_ui::install_chrome(ctx);
        apply_theme(ctx, dark_mode);
        let repaint = wiredata_ui::repaint::RepaintCoalescer::for_ctx(ctx.clone());
        {
            let repaint = std::sync::Arc::clone(&repaint);
            file_log.set_notify(std::sync::Arc::new(move || repaint.notify()));
        }
        // Sampled lanes (ADR-018): the GUI's display cost stays constant
        // regardless of send rate; statuses wake the UI via the coalescer.
        let mut sup = TalkerSupervisor::new(runner::ObserverPolicy::sampled());
        {
            let r = std::sync::Arc::clone(&repaint);
            sup.set_notify(std::sync::Arc::new(move || r.notify()));
        }
        let file_log_state = file_log.state();
        let mut app = Self {
            repaint,
            profile: Profile::default(),
            profile_path: None,
            dirty: false,
            conn_drafts: Vec::new(),
            sched_drafts: Vec::new(),
            message_analysis: Vec::new(),
            sup,
            log_rx,
            log_lines: Vec::new(),
            log_level: LogLevel::default(),
            log_level_handle,
            views: Vec::new(),
            last_title: String::new(),
            serial_ports: Vec::new(),
            dark_mode,
            selected: None,
            channels_collapsed: false,
            log_counts: HashMap::new(),
            log_visibility: LogVisibility::default(),
            file_log,
            file_log_state,
            file_log_config: crate::core::logging::default_log_dir().map(FileLogConfig::new),
            recent_profiles: storage
                .and_then(|s| s.get_string(RECENT_PROFILES_KEY))
                .map(|joined| {
                    joined
                        .lines()
                        .filter(|l| !l.is_empty())
                        .map(PathBuf::from)
                        .collect()
                })
                .unwrap_or_default(),
            confirm_remove: None,
            deferred: DeferredActions::default(),
        };
        app.refresh_serial_ports();

        // CLI arg takes precedence; fall back to last path saved in
        // storage. The storage path is also filtered through
        // `Path::exists()` — when the file is gone (renamed, on a
        // disconnected drive, etc.) we skip the load and start
        // empty rather than logging the same "file not found"
        // error on every launch. `profile_path` stays `None`, so
        // the next `save()` overwrites the stale storage entry
        // with an empty string and the loop self-clears.
        let path = initial_profile.or_else(|| {
            storage
                .and_then(|s| s.get_string("last_profile_path"))
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .and_then(|p| {
                    if p.exists() {
                        Some(p)
                    } else {
                        tracing::warn!("last-used profile {p:?} is gone — opening empty");
                        None
                    }
                })
        });

        if let Some(p) = path {
            app.load_profile_from_path(&p);
        }

        app
    }

    fn is_connection_running(&self, i: usize) -> bool {
        self.sup.is_running(i)
    }

    fn can_start_connection(&self, i: usize) -> bool {
        !self.is_connection_running(i) && self.drafts_are_startable(i)
    }

    fn refresh_message_analysis(&mut self) {
        self.message_analysis
            .resize_with(self.sched_drafts.len(), Vec::new);
        for (drafts, caches) in self
            .sched_drafts
            .iter()
            .zip(self.message_analysis.iter_mut())
        {
            caches.resize_with(drafts.len(), MessageAnalysisCache::default);
            for (draft, cache) in drafts.iter().zip(caches.iter_mut()) {
                cache.refresh(draft);
            }
        }
    }

    /// Whether the current drafts form a complete, compilable candidate run.
    /// Unlike [`Self::can_start_connection`], this deliberately ignores the
    /// current lifecycle so it can also gate a running channel's replacement.
    fn drafts_are_startable(&self, i: usize) -> bool {
        let (Some(conn), Some(messages), Some(analyses)) = (
            self.conn_drafts.get(i),
            self.sched_drafts.get(i),
            self.message_analysis.get(i),
        ) else {
            return false;
        };
        // The lazy predicate: stops at the first blocker, formats nothing —
        // this runs for every channel on every frame via `can_start_any`.
        !widgets::any_start_blocker_analyzed(conn, messages, analyses)
    }

    fn can_start_any(&self) -> bool {
        (0..self.conn_drafts.len()).any(|i| self.can_start_connection(i))
    }

    /// Compare the draft for channel `i` against the applied config (what
    /// the talker thread is actually using). Returns `(interface_drift,
    /// run_drift)`:
    ///
    /// - `interface_drift`: the draft's interface params don't match the
    ///   applied interface. Can be applied live by pressing Enter (sends
    ///   `UpdateInterface` to the talker thread).
    /// - `run_drift`: the message list or Timing mode differs from the
    ///   applied run. Both require a stop+start; the scheduler and its wait
    ///   policy are fixed at channel start.
    fn detect_drift(&self, i: usize, draft_interface: Option<&InterfaceConfig>) -> (bool, bool) {
        let draft_cadence_alignment = self
            .conn_drafts
            .get(i)
            .map(|draft| draft.cadence_alignment)
            .unwrap_or_default();
        let analyses = self.message_analysis.get(i).map(Vec::as_slice);
        let messages_complete = analyses.is_some_and(|analyses| {
            analyses.iter().all(|cached| {
                cached
                    .analysis
                    .as_ref()
                    .is_some_and(|analysis| analysis.config.is_some())
            })
        });

        if self.sup.is_running(i) {
            // Interface and schedule are one runner-confirmed fact. Until open
            // succeeds there is no applied baseline, so valid drafts remain
            // visibly pending instead of being inferred from spawn intent.
            let Some(applied) = self.sup.applied_run_config(i) else {
                return (draft_interface.is_some(), messages_complete);
            };
            return (
                draft_interface != Some(&applied.interface),
                !analyzed_messages_match(analyses, &applied.messages)
                    || draft_cadence_alignment != applied.cadence_alignment,
            );
        }

        let Some(profile) = self.profile.channels.get(i) else {
            return (false, false);
        };
        (
            draft_interface != Some(&profile.interface),
            !analyzed_messages_match(analyses, &profile.messages)
                || draft_cadence_alignment != profile.cadence_alignment,
        )
    }

    fn refresh_serial_ports(&mut self) {
        self.serial_ports = serialport::available_ports()
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.port_name)
            .collect();
        self.serial_ports.sort();
    }

    fn window_title(&self) -> String {
        // Profile name + dirty marker live next to the `Profile:`
        // text field in the top row — see `show_top_bar`. The title
        // bar is just the app identity.
        format!("Talker v{}", env!("CARGO_PKG_VERSION"))
    }

    // ── Profile actions ───────────────────────────────────────────────────────

    fn load_profile_from_path(&mut self, path: &Path) {
        // Complete every fallible operation before touching the active workspace.
        // A missing, malformed, or unrepresentable profile leaves healthy runners
        // and all current drafts exactly as they were.
        match prepare_profile_load(path) {
            Ok(prepared) => {
                let PreparedProfileLoad {
                    profile,
                    conn_drafts,
                    sched_drafts,
                } = prepared;
                let n = profile.channels.len();

                self.stop_all();
                self.conn_drafts = conn_drafts;
                self.sched_drafts = sched_drafts;
                self.message_analysis.clear();
                self.refresh_message_analysis();
                self.sup.resize_slots(0); // orphan any old runners, then size fresh
                self.sup.resize_slots(n);
                self.views = (0..n).map(|_| ChannelView::default()).collect();
                // Fresh slots minted fresh ids, so old entries are unreachable
                // — clear rather than leak them.
                self.log_counts.clear();
                self.selected = if n > 0 { Some(0) } else { None };
                self.profile = profile;
                self.profile_path = Some(path.to_path_buf());
                self.push_recent(path);
                self.dirty = false;
                tracing::info!("profile '{}' loaded", self.profile.name);
            }
            Err(e) => tracing::error!("load failed: {e:#}"),
        }
    }

    fn confirm_discard(&self) -> bool {
        !self.dirty
            || rfd::MessageDialog::new()
                .set_title("Unsaved Changes")
                .set_description("Discard unsaved changes?")
                .set_buttons(rfd::MessageButtons::OkCancel)
                .show()
                == rfd::MessageDialogResult::Ok
    }

    /// Every per-channel collection is indexed by the same channel position, so
    /// they must all be the length the supervisor reports.
    ///
    /// The three profile-owned vectors are maintained by five lifecycle paths
    /// (new, load, add, remove, start) that each have to touch all of them, and
    /// one that falls out of step hands out another channel's drafts rather
    /// than panicking. Checked once a frame in debug builds, so a mismatch is
    /// loud where it happens rather than silent wherever it is read.
    #[cfg(debug_assertions)]
    fn debug_assert_channel_state_aligned(&self) {
        let n = self.sup.len();
        for (what, len) in [
            ("conn_drafts", self.conn_drafts.len()),
            ("sched_drafts", self.sched_drafts.len()),
            ("message_analysis", self.message_analysis.len()),
            ("views", self.views.len()),
        ] {
            debug_assert_eq!(len, n, "{what} is out of step with the supervisor's slots");
        }
    }

    #[cfg(not(debug_assertions))]
    fn debug_assert_channel_state_aligned(&self) {}

    fn new_profile(&mut self) {
        if !self.confirm_discard() {
            return;
        }
        self.stop_all();
        self.profile = Profile::default();
        self.profile_path = None;
        self.dirty = true;
        self.conn_drafts.clear();
        self.sched_drafts.clear();
        self.message_analysis.clear();
        self.sup.resize_slots(0);
        self.views.clear();
        self.log_counts.clear();
        self.selected = None;
        tracing::info!("new profile");
    }

    /// Move `path` to the front of the recent-profiles list (deduplicated,
    /// capped at [`MAX_RECENT_PROFILES`]).
    fn push_recent(&mut self, path: &Path) {
        self.recent_profiles.retain(|p| p != path);
        self.recent_profiles.insert(0, path.to_path_buf());
        self.recent_profiles.truncate(MAX_RECENT_PROFILES);
    }

    fn load_profile_dialog(&mut self) {
        if !self.confirm_discard() {
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .add_filter("TOML Profile", &["toml"])
            .pick_file()
        else {
            return;
        };
        self.load_profile_from_path(&path);
    }

    fn save_profile(&mut self) {
        if !self.flush_or_report() {
            return;
        }
        let path = match &self.profile_path {
            Some(p) => p.clone(),
            None => match self.pick_save_path() {
                Some(p) => p,
                None => return,
            },
        };
        self.write_profile_to(&path);
    }

    /// Always opens the native save dialog, so the user can fork the
    /// current profile to a new file. On success the new path becomes
    /// the bound `profile_path`, so subsequent plain Save writes there.
    fn save_profile_as(&mut self) {
        if !self.flush_or_report() {
            return;
        }
        let Some(path) = self.pick_save_path() else {
            return;
        };
        self.write_profile_to(&path);
    }

    fn pick_save_path(&self) -> Option<PathBuf> {
        let stem = if self.profile.name.is_empty() {
            "profile"
        } else {
            &self.profile.name
        };
        let name = format!("{stem}.toml");
        let mut dialog = rfd::FileDialog::new()
            .add_filter("TOML Profile", &["toml"])
            .set_file_name(&name);
        // Seed the dialog at the current profile's directory so
        // Save As lands next to the original by default.
        if let Some(parent) = self.profile_path.as_deref().and_then(Path::parent) {
            dialog = dialog.set_directory(parent);
        }
        dialog.save_file()
    }

    fn write_profile_to(&mut self, path: &Path) {
        match self.profile.save(path) {
            Ok(()) => {
                self.profile_path = Some(path.to_path_buf());
                self.push_recent(path);
                // Keep the in-memory display name in sync with the
                // file root — see [`Profile::name`]. Especially
                // matters after Save As to a new path.
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    self.profile.name = stem.to_string();
                }
                self.dirty = false;
                tracing::info!("profile '{}' saved", self.profile.name);
            }
            Err(e) => tracing::error!("save failed: {e:#}"),
        }
    }

    // ── Talker thread lifecycle ────────────────────────────────────────────────

    fn start_connection(&mut self, i: usize) {
        // Starting (or attempting to start) is an explicit commit —
        // flip the active UDP destination into strict validation so
        // missing / malformed fields surface as red immediately.
        if let Some(draft) = self.conn_drafts.get_mut(i) {
            if matches!(draft.kind(), ConnKind::Udp) {
                let pair = match draft.udp_mode {
                    UdpModeDraft::Unicast => &mut draft.udp_unicast,
                    UdpModeDraft::Broadcast => &mut draft.udp_broadcast,
                    UdpModeDraft::Multicast => &mut draft.udp_multicast,
                };
                pair.submitted = true;
            }
        }

        // Log text names the channel by its start-time label; the structured
        // field carries the slot's stable id (ADR-020) so the tally lands on
        // this row whatever happens above it later.
        let label = self.channel_label(i);
        let cid = self.sup.channel_id(i).map_or(0, |id| id.as_u64());

        // These three fire on the most ordinary failure in the application —
        // pressing Start with something unfilled — so they say what is missing
        // in the words the screen uses. "Preflight" and "draft" are this
        // module's own vocabulary and appear nowhere the reader was looking.
        let Some(conn) = self.conn_drafts.get(i) else {
            tracing::error!(
                channel = cid,
                "channel {label} could not start: no interface settings"
            );
            return;
        };
        let Some(drafts) = self.sched_drafts.get(i) else {
            tracing::error!(
                channel = cid,
                "channel {label} could not start: no messages configured"
            );
            return;
        };
        // Build the complete candidate before touching the supervisor, then
        // replace through the one ordering boundary shared with the regression
        // test. A malformed edit leaves the healthy run untouched.
        if let Err(e) = replace_channel_run(&mut self.sup, i, label.clone(), conn, drafts) {
            tracing::error!(
                channel = cid,
                "channel {label} could not start; nothing was changed: {e:#}"
            );
            return;
        }

        // Lifecycle, telemetry reset, predecessor joining, and the runner
        // spawn all live in the supervisor (ADR-019); the GUI resets only
        // its own view-state (log tallies reset per run, like send counts).
        if let Some(id) = self.sup.channel_id(i) {
            self.log_counts.remove(&id);
        }
        if let Some(view) = self.views.get_mut(i) {
            view.start_run();
        }
    }

    /// Stop channel `i` without blocking the UI (the supervisor parks the
    /// runner to drain in the background; enqueue failures surface in the
    /// channel's telemetry).
    fn stop_connection(&mut self, i: usize) {
        let _ = self.sup.stop(i);
    }

    fn start_all(&mut self) {
        let n = self.conn_drafts.len();
        for i in 0..n {
            if self.can_start_connection(i) {
                self.start_connection(i);
            }
        }
    }

    fn stop_all(&mut self) {
        self.sup.stop_all();
    }

    /// Commit every draft into `profile.channels`, **all or none**. On any
    /// invalid draft the profile is left untouched and the reasons are
    /// returned (and logged) — never silently drop or reindex a user's
    /// channels. (The previous `filter_map` dropped invalid entries: Save
    /// lost drafts permanently, and the compressed indices made Start read
    /// another channel's messages.)
    fn flush_drafts_to_profile(&mut self) -> Result<(), Vec<String>> {
        match drafts_to_channels(&self.conn_drafts, &self.sched_drafts) {
            Ok(channels) => {
                self.profile.channels = channels;
                Ok(())
            }
            Err(problems) => {
                for p in &problems {
                    tracing::error!("{p}");
                }
                Err(problems)
            }
        }
    }

    /// Flush for a save; on invalid drafts, block the save with a dialog
    /// listing exactly what to fix (nothing is written).
    fn flush_or_report(&mut self) -> bool {
        if let Err(problems) = self.flush_drafts_to_profile() {
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Error)
                .set_title("Profile not saved")
                .set_description(format!(
                    "Fix or remove these first — nothing was written:\n\n{}",
                    problems.join("\n")
                ))
                .show();
            return false;
        }
        true
    }

    fn apply_connection(&mut self, i: usize) {
        let Some(cfg) = self.conn_drafts[i].to_config() else {
            return;
        };
        if self.sup.is_running(i) {
            // Enqueue is not application. The supervisor retains `cfg`; only the
            // runner's reliable success result updates the applied baseline below.
            let _ = self.sup.update_interface(i, cfg);
        } else if i < self.profile.channels.len() {
            self.profile.channels[i].interface = cfg;
        } else {
            let mut channel =
                ChannelConfig::named(self.conn_drafts[i].name.clone(), cfg, Vec::new());
            channel.cadence_alignment = self.conn_drafts[i].cadence_alignment;
            self.profile.channels.push(channel);
        }
        self.dirty = true;
    }

    // ── Channel polling ───────────────────────────────────────────────────────

    fn poll_channels(&mut self, ctx: &egui::Context) {
        // Keyboard shortcuts
        let (new, load, save, save_as) = ctx.input(|inp| {
            let ctrl = inp.modifiers.ctrl || inp.modifiers.mac_cmd;
            let shift = inp.modifiers.shift;
            (
                ctrl && !shift && inp.key_pressed(egui::Key::N),
                ctrl && !shift && inp.key_pressed(egui::Key::O),
                ctrl && !shift && inp.key_pressed(egui::Key::S),
                ctrl && shift && inp.key_pressed(egui::Key::S),
            )
        });
        if new {
            self.new_profile();
        }
        if load {
            self.load_profile_dialog();
        }
        if save {
            self.save_profile();
        }
        if save_as {
            self.save_profile_as();
        }

        for event in self.log_rx.try_iter() {
            // Tally channel-attributed events (structured `channel` field — a
            // stable ChannelId, ADR-020) for the channel-list rows. Keyed by
            // id, so a runner below a removed channel keeps counting into its
            // own row instead of the one that slid into its old position.
            if let Some(id) = event.channel {
                let c = self.log_counts.entry(id).or_default();
                c.record(event.level);
            }
            let ts = event.timestamp.format("%H:%M:%S%.3f");
            let line = format!("[{ts}] [{:<5}] {}", event.level, event.message);
            self.log_lines.push((line, event.level));
        }
        const LOG_CAP: usize = 2000;
        if self.log_lines.len() > LOG_CAP {
            self.log_lines.drain(..self.log_lines.len() - LOG_CAP);
        }

        // Drain runner telemetry (ADR-019: the supervisor owns the statuses;
        // the GUI gets back only the display samples) and route the sampled
        // payloads into the Output panes.
        for sample in self.sup.poll() {
            if let Some(view) = self.views.get_mut(sample.slot) {
                view.display
                    .push(sample.payload, sample.replacement_wire_offsets);
            }
        }
        // The supervisor has already reconciled applied runtime state and
        // telemetry. The GUI currently needs no per-completion animation, but
        // drains the public completion feed so it stays bounded.
        let _ = self.sup.take_command_completions();

        // Refresh the per-channel rolling five-second acceptance rates.
        let now = Instant::now();
        for i in 0..self.views.len() {
            let (count, bytes) = self
                .sup
                .telemetry_ref(i)
                .map(|t| (t.total_count, t.total_bytes))
                .unwrap_or_default();
            self.views[i]
                .rate
                .sample(now, count, bytes, self.sup.is_running(i));
        }

        if self.sup.any_running() || self.sup.any_draining() {
            // Sends wake the UI instantly via the runners' notify callbacks
            // (ADR-016); this slower heartbeat only covers what has no
            // callback — log lines arriving over `log_rx`, the window title,
            // and reaping drained threads.
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }

        // Update window title when it changes.
        let title = self.window_title();
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }
    }
}

// ── eframe::App ───────────────────────────────────────────────────────────────

impl eframe::App for TalkerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Re-arm the repaint coalescer BEFORE draining statuses, so a status
        // arriving mid-drain either lands in this frame's batch or triggers a
        // fresh wake — never lost.
        self.repaint.frame_started();
        self.debug_assert_channel_state_aligned();
        self.sync_file_log_state(ui.ctx());
        self.poll_channels(ui.ctx());
        self.refresh_message_analysis();
        self.handle_tab_keys(ui.ctx());
        // Trial 3 px window frame: white / blue / white, one pixel each. Three
        // nested frames rather than one thick stroke, because egui paints a
        // frame's stroke with `StrokeKind::Inside` — a 1 px stroke sits wholly
        // within its own rect, so a 1 px inner margin per level lands the next
        // line exactly against it with no gap. The outermost carries the panel
        // fill so no band is left unpainted (an unfilled margin was the earlier
        // "black outline").
        let fill = ui.visuals().panel_fill;
        let line = |colour: egui::Color32| {
            egui::Frame::new()
                .stroke(egui::Stroke::new(1.0_f32, colour))
                .inner_margin(1.0)
        };
        let blue = egui::Color32::from_rgb(60, 110, 200);
        egui::Frame::new().fill(fill).show(ui, |ui| {
            line(egui::Color32::WHITE).show(ui, |ui| {
                line(blue).show(ui, |ui| {
                    line(egui::Color32::WHITE).show(ui, |ui| {
                        self.show_top_bar(ui);
                        self.show_status_bar(ui);
                        self.show_log_panel(ui);
                        // Master–detail (spec §3.2): the channel list on the left
                        // (or its collapsed status strip), the selected channel's
                        // detail pane in the centre.
                        let channels_collapsed = self.channels_collapsed;
                        let channel_panel = if channels_collapsed {
                            egui::Panel::left("channel_strip")
                                .resizable(false)
                                .show_separator_line(false)
                                .show_inside(ui, |ui| self.show_channel_strip(ui))
                        } else {
                            egui::Panel::left("channel_list")
                                .resizable(true)
                                .default_size(280.0)
                                .show_separator_line(false)
                                .show_inside(ui, |ui| self.show_channel_list(ui))
                        };
                        egui::CentralPanel::default().show_inside(ui, |ui| self.show_detail(ui));
                        selection::connect_tab_to_page(
                            ui,
                            channel_panel.response.rect,
                            channel_panel.inner,
                            (!channels_collapsed).then(|| egui::Id::new("channel_list")),
                        );
                    });
                });
            });
        });
        self.show_remove_confirm(ui.ctx());
        // Apply user-requested mutations AFTER the layout closes — never
        // inside it — so egui's two-pass layout sees one consistent state.
        self.process_deferred();
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let path_str = self
            .profile_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        storage.set_string("last_profile_path", path_str);
        storage.set_string("dark_mode", self.dark_mode.to_string());
        let recents = self
            .recent_profiles
            .iter()
            .filter_map(|p| p.to_str())
            .collect::<Vec<_>>()
            .join("\n");
        storage.set_string(RECENT_PROFILES_KEY, recents);
    }

    /// On window close (the X button) or any app exit: orderly shutdown.
    /// Every runner gets Stop and is then joined (bounded by the interface
    /// send timeouts), so serial ports and sockets close cleanly before the
    /// process dies instead of being killed mid-write.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_all();
        self.sup.join_all();
    }
}

// ── Panel renderers ───────────────────────────────────────────────────────────

impl TalkerApp {
    fn show_top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top_bar").show_inside(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                // Profile UI (menu + name/dirty status) lives in the channel-
                // list header next to "+ Add", as in listener — see
                // `show_profile_menu` in `channels.rs`. The top bar keeps the
                // app-wide controls: zoom and theme.
                //
                // egui's own zoom, not a hand-rolled one. It scales *relative*
                // to the OS DPI rather than replacing it, which is what the old
                // control did — a 1.15 base multiplied whatever the user had
                // already set for their display. It also brings Ctrl +/−/0 and
                // Ctrl+scroll for free, and egui persists the factor itself.
                ui.menu_button(
                    format!("Zoom {:.0}%", ui.ctx().zoom_factor() * 100.0),
                    |ui| {
                        egui::gui_zoom::zoom_menu_buttons(ui);
                    },
                );

                ui.separator();
                // Theme toggle — the shared button (same storage key as
                // listener, so the two apps read and behave identically).
                wiredata_ui::style::theme_toggle_button(ui, &mut self.dark_mode);
            });
            ui.add_space(4.0);
        });
    }

    fn show_status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status_bar").show_inside(ui, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let total = self.sup.len();
                let running = (0..total).filter(|&i| self.sup.is_running(i)).count();
                let palette = wiredata_ui::palette::active(ui);
                let (color, label) = if running > 0 {
                    (
                        palette.running,
                        if running == total && total > 0 {
                            "\u{2022} All running".to_string()
                        } else {
                            format!("\u{2022} {running}/{total} running")
                        },
                    )
                } else {
                    (palette.idle, "\u{2022} Stopped".to_string())
                };
                ui.colored_label(color, label);
                ui.separator();
                let (total_sent, errors) = (0..self.sup.len())
                    .filter_map(|i| self.sup.telemetry_ref(i))
                    .fold((0u64, 0u64), |(s, e), t| {
                        (s + t.total_count, e + t.errors_total)
                    });
                ui.label(format!("Sent: {total_sent}")).on_hover_text(
                    "Configured-interface writes that returned success across all channels. \
                         This does not confirm physical-wire or peer delivery.",
                );
                ui.separator();
                // Per-run errors: each channel's tally resets when it starts,
                // like the send counts and log tallies.
                ui.label(format!("Errors: {errors}"));
                if let Some(path) = &self.profile_path {
                    ui.separator();
                    let display = path.display().to_string();
                    ui.label(&display).on_hover_text(&display);
                }
            });
            ui.add_space(2.0);
        });
    }

    fn sync_file_log_state(&mut self, ctx: &egui::Context) {
        let next = self.file_log.state();
        if next != self.file_log_state {
            let previous = std::mem::replace(&mut self.file_log_state, next.clone());
            match &next {
                FileLogState::Enabled { directory, prefix } => tracing::info!(
                    "file logging enabled for files beginning with {:?} in {:?}",
                    prefix,
                    directory
                ),
                FileLogState::Disabled
                    if matches!(
                        previous,
                        FileLogState::Disabling | FileLogState::Enabled { .. }
                    ) =>
                {
                    tracing::info!("file logging disabled");
                }
                FileLogState::Failed(error) => {
                    tracing::error!("file logging is unavailable: {error}");
                }
                FileLogState::Disabled | FileLogState::Enabling | FileLogState::Disabling => {}
            }
        }
        if matches!(
            self.file_log_state,
            FileLogState::Enabling | FileLogState::Disabling
        ) {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    fn show_log_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("log_panel")
            .resizable(true)
            .default_size(190.0)
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("Log");
                    ui.separator();
                    ui.label("Detail:").on_hover_text(
                        "Choose how much new logging to collect. Debug is collected at Debug or \
                         Trace and is saved when Log file is on. Trace is collected only at Trace. \
                         This setting lasts until Talker closes; profiles do not change it.",
                    );
                    let before = self.log_level;
                    egui::ComboBox::from_id_salt("log_level")
                        .selected_text(log_level_label(self.log_level))
                        .show_ui(ui, |ui| {
                            for lvl in [
                                LogLevel::Trace,
                                LogLevel::Debug,
                                LogLevel::Info,
                                LogLevel::Warn,
                                LogLevel::Error,
                            ] {
                                ui.selectable_value(&mut self.log_level, lvl, log_level_label(lvl));
                            }
                        });
                    if self.log_level != before {
                        // Don't log on success — the new filter may hide
                        // an info-level confirmation, and the ComboBox
                        // itself shows the active level.
                        if let Err(e) = self.log_level_handle.set(self.log_level) {
                            tracing::error!("log level change failed: {e:#}");
                        }
                    }
                    ui.separator();

                    let pending = matches!(
                        self.file_log_state,
                        FileLogState::Enabling | FileLogState::Disabling
                    );
                    let mut file_enabled = matches!(
                        self.file_log_state,
                        FileLogState::Enabling | FileLogState::Enabled { .. }
                    );
                    let file_available = self.file_log_config.is_some();
                    let response = ui.add_enabled(
                        file_available && !pending,
                        egui::Checkbox::new(&mut file_enabled, "Log file"),
                    );
                    let mut file_hint = match &self.file_log_state {
                        FileLogState::Enabled { directory, prefix } => format!(
                            "Saving logs in {}. Files begin with {prefix}; a new file is started \
                             each day. Show in pane does not change file content. File logging \
                             stays on only until Talker closes.",
                            directory.display()
                        ),
                        FileLogState::Enabling => "Starting file logging…".to_owned(),
                        FileLogState::Disabling => {
                            "Finishing the current log file…".to_owned()
                        }
                        FileLogState::Failed(error) => {
                            format!("File logging is off: {error}. Select Log file to retry.")
                        }
                        FileLogState::Disabled => match &self.file_log_config {
                            Some(config) => format!(
                                "Save logs in {}. Files begin with {}; a new file is started each \
                                 day. This starts off each time Talker opens, and profiles do not \
                                 turn it on.",
                                config.directory.display(),
                                config.prefix
                            ),
                            None => "File logging is unavailable because no local data directory \
                                     was found."
                                .to_owned(),
                        },
                    };
                    let dropped = self.file_log.dropped_events();
                    if dropped > 0 {
                        file_hint.push_str(&format!(
                            " {dropped} log entries were not written to a file this session because \
                             the file writer could not keep up."
                        ));
                    }
                    response.clone().on_hover_text(file_hint);
                    if let FileLogState::Failed(error) = &self.file_log_state {
                        ui.colored_label(
                            wiredata_ui::palette::active(ui).fault,
                            "File unavailable",
                        )
                        .on_hover_text(format!(
                            "The log file is off: {error}. Select Log file to retry."
                        ));
                    }
                    if dropped > 0 {
                        ui.colored_label(
                            wiredata_ui::palette::active(ui).warning,
                            format!("{dropped} log entries not saved"),
                        )
                        .on_hover_text(
                            "Log entries not written to a file this session because the file writer \
                             could not keep up. Sending does not wait for file logging; the Log pane \
                             is handled separately.",
                        );
                    }
                    if response.changed() {
                        let result = if file_enabled {
                            self.file_log_config
                                .clone()
                                .context("no default file-log directory is available")
                                .and_then(|config| self.file_log.enable(config))
                        } else {
                            self.file_log.disable()
                        };
                        if let Err(error) = result {
                            tracing::error!("file log change failed: {error:#}");
                        }
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .small_button("Clear")
                            .on_hover_text(
                                "Clear the on-screen log. Channel counts and saved log files are \
                                 unchanged.",
                            )
                            .clicked()
                        {
                            self.log_lines.clear();
                        }
                    });
                });
                ui.horizontal(|ui| {
                    ui.label("Show in pane:").on_hover_text(
                        "Display only. These boxes hide or show entries already in this pane; they \
                         do not change which new entries Detail collects or what the file contains. \
                         These choices last until Talker closes.",
                    );
                    ui.checkbox(&mut self.log_visibility.trace, "Trace");
                    ui.checkbox(&mut self.log_visibility.debug, "Debug");
                    ui.checkbox(&mut self.log_visibility.info, "Info");
                    ui.checkbox(&mut self.log_visibility.warn, "Warn");
                    ui.checkbox(&mut self.log_visibility.error, "Error");
                });
                ui.separator();
                // Filter first, then virtualize: `show_rows` lays out only the
                // visible rows instead of all (up to 2,000) lines every
                // repaint. Rows must be uniform height for virtualization, so
                // long lines truncate (hover shows the full text) rather than
                // wrap.
                let visible: Vec<(&str, tracing::Level)> = self
                    .log_lines
                    .iter()
                    .filter(|(_, level)| self.log_visibility.includes(*level))
                    .map(|(line, level)| (line.as_str(), *level))
                    .collect();
                let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
                ui.spacing_mut().item_spacing.y = 0.0;
                ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show_rows(ui, row_h, visible.len(), |ui, range| {
                        for &(line, level) in &visible[range] {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(line)
                                        .monospace()
                                        .color(level_color(ui, level)),
                                )
                                .truncate(),
                            )
                            .on_hover_text(line);
                        }
                    });
            });
    }

    /// Ctrl+Tab / Ctrl+Shift+Tab cycles the channel selection (same keys as
    /// listener). Plain Tab is left to egui's widget-focus traversal.
    fn handle_tab_keys(&mut self, ctx: &egui::Context) {
        let (tab, shift, ctrl) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Tab),
                i.modifiers.shift,
                i.modifiers.ctrl,
            )
        });
        if tab && ctrl {
            let n = self.conn_drafts.len();
            if n > 0 {
                self.selected = Some(match self.selected {
                    Some(s) if !shift => (s + 1) % n,
                    Some(s) => (s + n - 1) % n,
                    None => 0,
                });
            }
        }
    }

    /// The "are you sure?" dialog for channel Remove. A modal so it can't be
    /// ignored; confirming queues the removal (selection then falls to the
    /// neighbour). Clicking the dimmed backdrop or pressing Escape cancels.
    fn show_remove_confirm(&mut self, ctx: &egui::Context) {
        let Some(i) = self.confirm_remove else {
            return;
        };
        match wiredata_ui::dialog::confirm_remove_channel(ctx, &self.channel_name(i)) {
            wiredata_ui::dialog::Confirm::Pending => {}
            wiredata_ui::dialog::Confirm::Cancelled => self.confirm_remove = None,
            wiredata_ui::dialog::Confirm::Confirmed => {
                self.deferred.remove = Some(i);
                self.confirm_remove = None;
            }
        }
    }

    /// Apply every mutation queued on `self.deferred` during the just-
    /// completed egui layout. Called from the end of `ui()`, OUTSIDE any
    /// egui `show()` closure, so the state changes can't interleave with
    /// egui's two-pass layout.
    fn process_deferred(&mut self) {
        let mut d = std::mem::take(&mut self.deferred);
        // Dedup applies: multiple committed field edits in one frame on the
        // same channel are pointless to apply twice.
        d.apply.sort_unstable();
        d.apply.dedup();
        for i in d.apply {
            self.apply_connection(i);
        }
        if let Some(i) = d.select {
            if i < self.conn_drafts.len() {
                self.selected = Some(i);
            }
        }
        if d.start_all {
            self.start_all();
        }
        if d.stop_all {
            self.stop_all();
        }
        if let Some(i) = d.start {
            self.start_connection(i);
        }
        if let Some(i) = d.stop {
            self.stop_connection(i);
        }
        if let Some(i) = d.remove {
            // Drop the removed channel's tally by its id — the other rows'
            // tallies stay keyed to their own ids, untouched by the shift.
            if let Some(id) = self.sup.channel_id(i) {
                self.log_counts.remove(&id);
            }
            // The supervisor stops the runner and parks it in its orphan
            // bucket (reaped by poll — never joined on the UI thread).
            self.sup.remove_slot(i);
            self.conn_drafts.remove(i);
            self.sched_drafts.remove(i);
            self.message_analysis.remove(i);
            self.views.remove(i);
            if i < self.profile.channels.len() {
                self.profile.channels.remove(i);
            }
            // Keep the selection on the same visual position: the row that
            // slid into the removed slot, else the new last row, else none.
            self.selected = match self.selected {
                Some(s) if s == i => {
                    let n = self.conn_drafts.len();
                    if n == 0 {
                        None
                    } else {
                        Some(i.min(n - 1))
                    }
                }
                Some(s) if s > i => Some(s - 1),
                other => other,
            };
            self.dirty = true;
        }
        if let Some(kind) = d.add_channel {
            self.conn_drafts.push(ConnDraft::new(kind));
            self.sched_drafts.push(Vec::new());
            self.message_analysis.push(Vec::new());
            self.sup.push_slot();
            self.views.push(ChannelView::default());
            // log_counts: entries appear on demand, keyed by the new slot's id.
            // Jump straight to the new channel for editing.
            self.selected = Some(self.conn_drafts.len() - 1);
            self.dirty = true;
        }
        if d.refresh_ports {
            self.refresh_serial_ports();
        }
    }
}

// ── Theme ─────────────────────────────────────────────────────────────────────

/// Apply `dark`/light to `ctx` via [`egui::ThemePreference`]. The visuals for
/// both themes are installed by `wiredata_ui::style::install_visuals` (ADR-016).
fn apply_theme(ctx: &egui::Context, dark: bool) {
    ctx.set_theme(if dark {
        egui::ThemePreference::Dark
    } else {
        egui::ThemePreference::Light
    });
}

/// Stable title-case names for the GUI's logging-detail control.
fn log_level_label(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Trace => "Trace",
        LogLevel::Debug => "Debug",
        LogLevel::Info => "Info",
        LogLevel::Warn => "Warn",
        LogLevel::Error => "Error",
    }
}

/// Log-line colour for `level`, adapted to the active theme.
///
/// ERROR / WARN keep saturated reds/ambers that read on either
/// background. INFO follows the theme's body text. DEBUG / TRACE are
/// muted greys, lightened on dark and darkened on light so they
/// stay legible and still read as "less important than INFO".
fn level_color(ui: &egui::Ui, level: tracing::Level) -> egui::Color32 {
    let palette = wiredata_ui::palette::active(ui);
    match level {
        // The same red as every other "something is wrong" in both apps: a log
        // line reporting a fault should not be a second shade of it.
        tracing::Level::ERROR => palette.fault,
        tracing::Level::WARN => palette.warning,
        // Below INFO the level is context, not signal, so it recedes rather
        // than taking an accent of its own. DEBUG and TRACE share one faded
        // colour deliberately: every line already prints its level (the format
        // is `[time] [LEVEL] message`), so the word tells them apart and two
        // near-identical greys bought nothing but a pair to keep distinct.
        tracing::Level::DEBUG | tracing::Level::TRACE => ui.visuals().weak_text_color(),
        // INFO: the theme's body text colour — this line is the baseline the
        // others are read against, so it takes no accent at all.
        _ => ui.visuals().text_color(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_detail_choices_use_technician_facing_title_case() {
        assert_eq!(log_level_label(LogLevel::Trace), "Trace");
        assert_eq!(log_level_label(LogLevel::Debug), "Debug");
        assert_eq!(log_level_label(LogLevel::Info), "Info");
        assert_eq!(log_level_label(LogLevel::Warn), "Warn");
        assert_eq!(log_level_label(LogLevel::Error), "Error");
    }

    #[test]
    fn each_log_level_has_an_independent_display_switch() {
        let levels = [
            tracing::Level::TRACE,
            tracing::Level::DEBUG,
            tracing::Level::INFO,
            tracing::Level::WARN,
            tracing::Level::ERROR,
        ];
        let cases = [
            LogVisibility {
                trace: true,
                debug: false,
                info: false,
                warn: false,
                error: false,
            },
            LogVisibility {
                trace: false,
                debug: true,
                info: false,
                warn: false,
                error: false,
            },
            LogVisibility {
                trace: false,
                debug: false,
                info: true,
                warn: false,
                error: false,
            },
            LogVisibility {
                trace: false,
                debug: false,
                info: false,
                warn: true,
                error: false,
            },
            LogVisibility {
                trace: false,
                debug: false,
                info: false,
                warn: false,
                error: true,
            },
        ];

        for (wanted, visibility) in levels.into_iter().zip(cases) {
            for level in levels {
                assert_eq!(
                    visibility.includes(level),
                    level == wanted,
                    "{wanted} must not share a display switch with {level}"
                );
            }
        }
        for level in levels {
            assert!(
                LogVisibility::default().includes(level),
                "defaults preserve the old all-admitted-lines-visible behavior"
            );
        }
    }

    #[test]
    fn channel_card_info_excludes_debug_and_trace_detail() {
        let mut counts = LogCounts::default();
        for level in [
            tracing::Level::TRACE,
            tracing::Level::DEBUG,
            tracing::Level::INFO,
            tracing::Level::WARN,
            tracing::Level::ERROR,
        ] {
            counts.record(level);
        }

        assert_eq!(counts.info, 1);
        assert_eq!(counts.warn, 1);
        assert_eq!(counts.error, 1);
    }

    /// Log severities come from the shared palette, in both themes.
    ///
    /// They used to be six hardcoded literals, so "error" in the log panel was a
    /// different red from "faulted" everywhere else. INFO is the exception by
    /// design: it is the baseline the other levels are read against, so it takes
    /// the theme's body colour and no accent at all.
    #[test]
    fn log_severity_colors_come_from_the_shared_palette() {
        for dark in [false, true] {
            let ctx = egui::Context::default();
            ctx.set_theme(if dark {
                egui::ThemePreference::Dark
            } else {
                egui::ThemePreference::Light
            });
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                let palette = wiredata_ui::palette::active(ui);
                assert_eq!(level_color(ui, tracing::Level::ERROR), palette.fault);
                assert_eq!(level_color(ui, tracing::Level::WARN), palette.warning);
                assert_eq!(
                    level_color(ui, tracing::Level::INFO),
                    ui.visuals().text_color(),
                    "INFO is the baseline, not an accent"
                );
                // Below INFO the theme supplies the emphasis, not the palette.
                for below in [tracing::Level::DEBUG, tracing::Level::TRACE] {
                    assert_eq!(level_color(ui, below), ui.visuals().weak_text_color());
                }
                // Only the two that mean something take an accent.
                for accented in [tracing::Level::ERROR, tracing::Level::WARN] {
                    assert_ne!(level_color(ui, accented), ui.visuals().text_color());
                    assert_ne!(level_color(ui, accented), ui.visuals().weak_text_color());
                }
            });
        }
    }

    /// A tinted surface is derived from the panel behind it, so one accent
    /// covers both themes — the reason the palette holds no background pairs.
    #[test]
    fn a_tinted_status_strip_follows_the_theme_from_one_accent() {
        let mut fills = Vec::new();
        for dark in [false, true] {
            let ctx = egui::Context::default();
            ctx.set_theme(if dark {
                egui::ThemePreference::Dark
            } else {
                egui::ThemePreference::Light
            });
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                let accent = wiredata_ui::palette::active(ui).running;
                let fill = wiredata_ui::palette::tint(ui, accent, detail::STATUS_STRIP_TINT_ALPHA);
                assert_ne!(fill, accent, "a strip is a tint, not the accent itself");
                assert_ne!(fill, ui.visuals().panel_fill, "the tint must be visible");
                fills.push(fill);
            });
        }
        assert_ne!(fills[0], fills[1], "light and dark strips must differ");
    }

    #[test]
    fn throughput_uses_a_five_second_rolling_window_and_decays() {
        let base = Instant::now();
        let mut rate = RateTracker::with_epoch(base);

        rate.sample(base + std::time::Duration::from_millis(100), 10, 100, true);
        assert_eq!(rate.per_sec, 2.0);
        assert_eq!(rate.bytes_per_sec, 20.0);

        rate.sample(
            base + std::time::Duration::from_millis(1_100),
            20,
            300,
            true,
        );
        assert_eq!(rate.per_sec, 4.0);
        assert_eq!(rate.bytes_per_sec, 60.0);

        rate.sample(base + std::time::Duration::from_secs(6), 20, 300, true);
        assert_eq!(rate.per_sec, 0.0);
        assert_eq!(rate.bytes_per_sec, 0.0);
    }

    #[test]
    fn stopped_throughput_resets_without_recounting_old_totals() {
        let base = Instant::now();
        let mut rate = RateTracker::with_epoch(base);
        rate.sample(base + std::time::Duration::from_millis(100), 10, 100, true);

        rate.sample(base + std::time::Duration::from_secs(1), 10, 100, false);
        assert_eq!(rate.per_sec, 0.0);
        assert_eq!(rate.bytes_per_sec, 0.0);

        rate.sample(base + std::time::Duration::from_secs(2), 10, 100, true);
        assert_eq!(rate.per_sec, 0.0);
        assert_eq!(rate.bytes_per_sec, 0.0);
    }

    #[test]
    fn throughput_counts_the_first_update_after_cumulative_totals_reset() {
        let base = Instant::now();
        let mut rate = RateTracker::with_epoch(base);
        rate.sample(base + std::time::Duration::from_millis(100), 10, 100, true);
        rate.sample(base + std::time::Duration::from_secs(1), 10, 100, false);

        let first_new_run = base + std::time::Duration::from_secs(2);
        rate.sample(first_new_run, 1, 10, true);
        assert!((rate.per_sec - 0.2).abs() < f32::EPSILON);
        assert_eq!(rate.bytes_per_sec, 2.0);

        // Repainting the same cumulative snapshot must not count it twice.
        rate.sample(
            first_new_run + std::time::Duration::from_millis(100),
            1,
            10,
            true,
        );
        assert!((rate.per_sec - 0.2).abs() < f32::EPSILON);
        assert_eq!(rate.bytes_per_sec, 2.0);
    }

    fn serial_draft(name: &str) -> ConnDraft {
        let mut draft = ConnDraft::new(ConnKind::Serial);
        draft.name = name.to_string();
        draft.serial_port = "COM9".to_string();
        draft
    }

    fn message_draft(interval: &str) -> ScheduleDraft {
        use crate::core::message::{MessageConfig, PayloadConfig};
        let mut d = ScheduleDraft::from(&MessageConfig::new(PayloadConfig::raw_hex("AB"), 100));
        d.interval_ms = interval.to_string();
        d
    }

    #[test]
    fn message_analysis_reuses_unchanged_revisions_and_rebuilds_after_an_edit() {
        let mut draft = message_draft("100");
        let mut cache = MessageAnalysisCache::default();

        cache.refresh(&draft);
        cache.refresh(&draft);
        assert_eq!(cache.rebuilds, 1, "unchanged repaint reused analysis");

        draft.hex_data.push_str("CD");
        draft.mark_changed();
        cache.refresh(&draft);
        assert_eq!(cache.rebuilds, 2, "wire edit invalidated analysis");
    }

    fn temp_profile_path(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "talker_gui_{label}_{}_{nonce}.toml",
            std::process::id()
        ))
    }

    #[test]
    fn profile_load_preparation_rejects_bad_payloads_before_workspace_replacement() {
        use crate::core::message::{MessageConfig, PayloadConfig};

        let path = temp_profile_path("bad_payload");
        let mut profile = Profile::new("bad_payload");
        profile.channels.push(ChannelConfig::new(
            serial_draft("active").to_config().unwrap(),
            vec![MessageConfig::new(
                PayloadConfig::Ascii {
                    text: "broken‹marker".to_string(),
                    code_page: Default::default(),
                },
                100,
            )],
        ));
        profile.save(&path).unwrap();

        let error = match prepare_profile_load(&path) {
            Ok(_) => panic!("bad payload must not be prepared"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("complete ‹XX› byte marker"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn profile_load_preparation_is_complete_before_it_returns_success() {
        let path = temp_profile_path("valid");
        let mut profile = Profile::new("valid");
        let mut channel = ChannelConfig::new(
            serial_draft("A").to_config().unwrap(),
            vec![message_draft("250").to_message_config().unwrap()],
        );
        channel.name = "A".to_string();
        channel.cadence_alignment = crate::core::timing::CadenceAlignment::UtcPhase;
        profile.channels.push(channel);
        profile.save(&path).unwrap();

        let prepared = prepare_profile_load(&path).expect("valid candidate");
        assert_eq!(prepared.profile.channels.len(), 1);
        assert_eq!(prepared.conn_drafts[0].name, "A");
        assert_eq!(
            prepared.conn_drafts[0].cadence_alignment,
            crate::core::timing::CadenceAlignment::UtcPhase
        );
        assert_eq!(prepared.sched_drafts[0][0].interval_ms, "250");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn replacement_preflight_reports_the_exact_one_based_message_error() {
        let conn = serial_draft("active");
        let message = ScheduleDraft {
            payload_kind: draft::PayloadKind::Ascii,
            ascii_text: "broken‹marker".to_string(),
            ..ScheduleDraft::default()
        };

        let err = prepare_channel_run(&conn, &[message])
            .expect_err("malformed marker must fail preflight");
        let text = format!("{err:#}");
        assert!(text.contains("compiling message 1"), "error was: {text}");
        assert!(
            text.contains("complete ‹XX› byte marker"),
            "error was: {text}"
        );
    }

    #[test]
    fn replacement_preflight_builds_the_complete_candidate_run() {
        let mut conn = serial_draft("active");
        conn.cadence_alignment = crate::core::timing::CadenceAlignment::UtcPhase;
        let prepared =
            prepare_channel_run(&conn, &[message_draft("100")]).expect("valid candidate");

        assert_eq!(prepared.messages.len(), 1);
        assert_eq!(prepared.schedule.len(), 1);
        assert_eq!(prepared.interface, conn.to_config().unwrap());
        assert_eq!(
            prepared.schedule.cadence_alignment(),
            crate::core::timing::CadenceAlignment::UtcPhase
        );
    }

    #[test]
    fn malformed_replacement_leaves_the_active_run_unchanged() {
        use std::time::Duration;

        use crate::core::channel::UdpConfig;

        let interface =
            InterfaceConfig::Udp(UdpConfig::unicast("127.0.0.1:49152".parse().unwrap()));
        let conn = ConnDraft::from(&interface);
        let valid = vec![message_draft("1000")];
        let mut supervisor = TalkerSupervisor::new(runner::ObserverPolicy::sampled());
        supervisor.push_slot();
        replace_channel_run(&mut supervisor, 0, "active".to_string(), &conn, &valid)
            .expect("initial run starts");

        let deadline = Instant::now() + Duration::from_secs(1);
        while supervisor.applied_run_config(0).is_none() && Instant::now() < deadline {
            supervisor.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        let applied = supervisor
            .applied_run_config(0)
            .cloned()
            .expect("initial interface opens");

        let malformed = ScheduleDraft {
            payload_kind: draft::PayloadKind::Ascii,
            ascii_text: "broken‹marker".to_string(),
            ..ScheduleDraft::default()
        };
        let error = replace_channel_run(
            &mut supervisor,
            0,
            "replacement".to_string(),
            &conn,
            &[malformed],
        )
        .expect_err("malformed replacement must fail before restart");

        assert!(format!("{error:#}").contains("complete ‹XX› byte marker"));
        assert!(supervisor.is_running(0));
        assert_eq!(supervisor.applied_run_config(0), Some(&applied));

        supervisor.stop_all();
        supervisor.join_all();
    }

    /// The flush is all-or-none and index-preserving: one bad entry aborts
    /// everything with a reason naming it — nothing is silently dropped or
    /// shifted (a filter_map here once lost drafts on Save and made Start
    /// read another channel's messages through compressed indices).
    #[test]
    fn drafts_to_channels_is_all_or_none_with_reasons() {
        let conn = vec![serial_draft("A"), serial_draft("B")];
        let sched = vec![
            vec![message_draft("100")],
            vec![message_draft("100"), message_draft("not-a-number")],
        ];
        let problems = drafts_to_channels(&conn, &sched).unwrap_err();
        assert_eq!(problems.len(), 1);
        assert!(
            problems[0].contains("Channel 2") && problems[0].contains("message 2"),
            "reason names the exact entry: {}",
            problems[0]
        );

        // An invalid interface reports too, without dropping the channel.
        let mut broken = serial_draft("C");
        broken.serial_port.clear();
        let conn = vec![serial_draft("A"), broken];
        let sched = vec![vec![message_draft("100")], vec![message_draft("100")]];
        let problems = drafts_to_channels(&conn, &sched).unwrap_err();
        assert!(problems[0].contains("Channel 2"));

        // Conversion alone is not enough: Save and profile replacement must also
        // reject payloads that core compilation cannot represent safely.
        let malformed = ScheduleDraft {
            payload_kind: draft::PayloadKind::Ascii,
            ascii_text: "broken‹marker".to_string(),
            ..ScheduleDraft::default()
        };
        let problems = drafts_to_channels(&[serial_draft("A")], &[vec![malformed]])
            .expect_err("malformed payload must block the whole conversion");
        assert!(problems[0].contains("complete ‹XX› byte marker"));
    }

    #[test]
    fn drafts_to_channels_preserves_indices_when_valid() {
        let conn = vec![serial_draft("A"), serial_draft("B")];
        let sched = vec![vec![message_draft("100")], vec![message_draft("200")]];
        let channels = drafts_to_channels(&conn, &sched).unwrap();
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[0].name, "A");
        assert_eq!(channels[1].name, "B");
        assert_eq!(channels[1].messages[0].interval_ms, 200);
    }
}
