//! GUI presentation layer (spec §3, listener ADR-008).
//!
//! A thin egui App over the runtime [`bridge`]: on startup it spawns the
//! background [`Driver`](bridge::Driver) (which owns the [`Listener`](crate::runtime::Listener)) and then,
//! each frame, drains [`UiUpdate`](bridge::UiUpdate)s into its [`AppState`] and
//! lays out widgets that read that model and emit [`UiCommand`]s.
//! Per AGENTS §5 this layer never owns the runtime, never does I/O, and never
//! blocks — every runtime touch is a non-blocking channel send. The egui-free,
//! unit-tested pieces live in [`bridge`] and [`state`].

pub mod bind_scope;
pub mod bridge;
mod channels;
mod detail;

pub mod state;
mod view_prefs;
mod widgets;

use anyhow::anyhow;
use wiredata_ui::selection;

use crate::config::ChannelConfig;
use crate::core::ChannelId;
use crate::display::{CharacterRendering, DisplayMode, StreamRenderer};

use bridge::{BridgeHandle, UiCommand};

use state::{AppState, ChannelStatus, ResumeState};
use widgets::{
    config_incomplete, list_serial_ports, status_color, status_glyph, template_for, AddKind,
};

/// Detach the inherited console when going graphical. The binary is a
/// console-subsystem app so the headless CLI works when launched from a terminal
/// (output, Ctrl-C, and shell-wait all behave); the cost is that a double-click
/// allocates a console window. Freeing it here removes that empty window for the
/// GUI. A brief console flash on double-click is unavoidable without breaking
/// terminal CLI output, so we accept it. No-op off Windows.
#[cfg(windows)]
fn detach_console() {
    // SAFETY: `FreeConsole` takes no arguments and is always safe to call; it simply
    // detaches the process from its console if it has one.
    unsafe {
        let _ = windows_sys::Win32::System::Console::FreeConsole();
    }
}

#[cfg(not(windows))]
fn detach_console() {}

/// The window size every launch opens at (window geometry isn't persisted — see
/// `persist_window: false` in [`run`]).
const DEFAULT_WINDOW_SIZE: [f32; 2] = [1100.0, 740.0];
/// Smallest size the window can be dragged to (a usability floor).
const MIN_WINDOW_SIZE: [f32; 2] = [640.0, 480.0];

/// Launch the graphical interface (§3). Owns the eframe event loop on the calling
/// (main) thread; the runtime bridge runs on its own Tokio thread (ADR-008).
///
/// **Shared GUI-startup funnel (two-binary invariant — see `src/main.rs`).** Both
/// GUI entry points route through here: the `listener-gui.exe` (windows-subsystem)
/// binary, which avoids the brief console-window flash on a double-click, and
/// `listener.exe`'s bare-launch / `--gui` path. Put ALL GUI startup (console detach,
/// logging, window options) in this function so the two binaries stay identical —
/// never in either `main`.
pub fn run() -> anyhow::Result<()> {
    detach_console(); // drop the double-click console before the window opens
                      // §114, §118; non-fatal (§117). Held until the window closes, so the event
                      // log drains and flushes on exit.
    let event_log = crate::diagnostics::init_logging();
    let event_log_status = event_log.status();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            // Title carries the version, like talker's. The `run_native` app
            // name below stays the bare "Listener" — eframe keys its storage
            // location on it, so changing it would orphan persisted settings.
            .with_title(format!("Listener v{}", env!("CARGO_PKG_VERSION")))
            .with_inner_size(DEFAULT_WINDOW_SIZE)
            .with_min_inner_size(MIN_WINDOW_SIZE),
        // Don't persist/restore window geometry. eframe restores the saved window state
        // (size, position, and — the one that bit us — `maximized`) *after* the window
        // is shown, so the window appeared at the default size and then jumped to its
        // saved geometry: the double frame/title-bar flash on launch. With this off the
        // window always opens at DEFAULT_WINDOW_SIZE with no post-show move, and a bad
        // tiny geometry can't be restored either. Trade-off: it no longer reopens where
        // it was last; the recent-profiles list is persisted separately via `save`, so
        // that still survives.
        persist_window: false,
        ..Default::default()
    };
    let result = eframe::run_native(
        "Listener",
        options,
        Box::new(|cc| {
            apply_style(&cc.egui_ctx);
            // The driver wakes the UI by requesting a repaint when it pushes an
            // update, so a streaming source refreshes without busy-polling.
            // Coalesced (wiredata-ui): any number of pushes between frames —
            // a poll pass emits stats/snapshot/delta per channel — cost one
            // winit wake instead of one each.
            let repaint = wiredata_ui::repaint::RepaintCoalescer::for_ctx(cc.egui_ctx.clone());
            let notify = std::sync::Arc::clone(&repaint);
            let bridge = bridge::spawn(move || notify.notify()).map_err(
                |e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("failed to start the runtime bridge: {e}").into()
                },
            )?;
            Ok(Box::new(ListenerApp::new(
                bridge,
                repaint,
                &cc.egui_ctx,
                cc.storage,
                event_log_status,
            )))
        }),
    )
    .map_err(|e| anyhow!("{e}"));
    drop(event_log);
    result
}

/// Apply the app's base style. This fork keeps visuals *per theme* and the active
/// theme separately, so a plain `set_global_style` is ignored — visuals must go
/// through `set_visuals_of` + `set_theme` and sizes through `all_styles_mut` (the
/// same path talker uses). Sets the Noto font, a light theme with the grey backdrop
/// and darker/heavier text, and a nudged text size — no custom UI scale (the OS DPI
/// drives scaling, see below), and no 3-D / outline / button-sizing overrides.
fn apply_style(ctx: &egui::Context) {
    // No custom UI scale: the OS DPI setting drives sizing. (We deliberately do not
    // call `set_pixels_per_point` / `set_zoom_factor` — overriding the scale here both
    // ignores the user's system setting and gets persisted into eframe storage, where
    // a stale value then sticks across launches. Text size is nudged by the shared
    // style tweaks instead.)

    // The shared wiredata look (ADR-019): fonts, both themes' visuals, and the
    // style tweaks come from `wiredata-ui` so talker and listener read as one
    // product. Which theme is *active* is restored in `ListenerApp::new`.
    wiredata_ui::install_chrome(ctx);
}

/// The eframe application root: the runtime bridge, the folded view-model, the
/// selected channel, and the add-channel form's draft state.
struct ListenerApp {
    bridge: BridgeHandle,
    /// The driver's repaint coalescer (shared with the bridge callback);
    /// re-armed at the top of each frame, before the update drain.
    repaint: std::sync::Arc<wiredata_ui::repaint::RepaintCoalescer>,
    state: AppState,
    selected: Option<ChannelId>,
    /// Last selection sent to the driver, so we only send `Select` on change. The
    /// driver full-snapshots only the selected channel (the rest get cheap stats).
    last_selected_sent: Option<ChannelId>,
    /// A working copy of the selected channel's config, edited in the Configure
    /// section and sent on Apply. Re-seeded when the selection changes.
    edit_draft: Option<(ChannelId, ChannelConfig)>,
    /// A pending "remove this channel?" confirmation (#1); `Some` while the dialog
    /// is up.
    confirm_remove: Option<ChannelId>,
    /// One-shot: open the Configure section on the next frame because focus moved to
    /// an unconfigured/faulted channel that needs attention.
    force_config_open: bool,
    /// Why the last in-progress rename was not committed, shown by the Name field:
    /// it duplicated another channel's name (§6, ADR-014), or a rotating
    /// recording could not use it in a filename (§59, §71).
    name_problem: Option<&'static str>,
    /// Per-kind monotonic counter for default channel names (§6): the Nth UDP channel is
    /// `UDP_Channel<N>`. Counts up and is **never** reused — deleting `UDP_Channel2` does
    /// not free the number 2; the next UDP add is 3. Each kind counts independently.
    channel_seq: std::collections::HashMap<AddKind, u32>,
    /// Whether the channel-list (tabs) column is collapsed to a thin strip (#1).
    channels_collapsed: bool,
    /// `true` = dark theme, `false` = light (the default). Persisted; toggled
    /// from the shared ◐ button in the channel-list header. The visuals for
    /// both themes come from `wiredata-ui` (ADR-019); chrome colors follow the
    /// active theme via `wiredata_ui::palette::active`.
    dark_mode: bool,
    /// Per-severity filters for the diagnostics log.
    show_info: bool,
    show_warn: bool,
    show_error: bool,
    /// Available serial port names for the serial port dropdown (§14.4); refreshed
    /// on demand via the ⟳ button.
    serial_ports: Vec<String>,
    /// Memoized stream-view rows for the detail pane. The scrollback can reach the
    /// 256 KB scroll cap, so rendering + line-splitting it every frame (even at
    /// 5 Hz) is wasteful and, with a non-virtualized layout, was stalling the UI
    /// (regression after the stream-only split). We recompute only when the inputs
    /// change; `show_rows` then lays out just the visible rows.
    stream_cache: Option<StreamRenderCache>,
    /// One-shot: on the next render of this channel's stream view, jump the
    /// scroll to the bottom. Set by the Resume button — egui's
    /// `stick_to_bottom` disengages when the user scrolls up (which is what
    /// Pause is for), so Resume forces the view back onto the newest bytes,
    /// where stickiness re-latches on its own.
    resume_scroll_bottom: Option<ChannelId>,
    /// The profile file the workspace is currently associated with (last saved or
    /// loaded). `Save` writes here silently; `Save As…` always re-prompts. `None`
    /// until the first save/load, so the first `Save` falls through to a picker.
    current_profile_path: Option<std::path::PathBuf>,
    /// The profile to resume when Listener starts (ADR-045), registered from the
    /// Profile menu. Kept in application state, never in a profile.
    resume_profile: Option<std::path::PathBuf>,
    /// Recently saved/loaded profile paths, most-recent-first (capped). Listed at the
    /// top of the Profile menu for one-click reload. Session-scoped for now.
    recent_profiles: Vec<std::path::PathBuf>,
    /// A UI command the bridge could not accept (§99 non-blocking send): the
    /// user's click did nothing, which must be *visible*, not just a tracing
    /// warning — a silently dropped "Stop all" cost a debugging session once.
    /// Shown as a top banner; auto-expires, or dismiss by button.
    command_drop: Option<(String, std::time::Instant)>,
    /// The persistent event log's status (§118). A problem is shown as a
    /// lasting top banner: unattended logging that silently stopped saving
    /// its log would leave nothing to diagnose later.
    event_log: crate::diagnostics::EventLogStatus,
}

/// How long the dropped-command banner stays up if not dismissed.
const COMMAND_DROP_NOTICE_TTL: std::time::Duration = std::time::Duration::from_secs(8);

/// How many recent profiles to keep in the Profile menu.
pub(super) const MAX_RECENT_PROFILES: usize = 8;

/// Incrementally maintained, line-split render of a channel's accumulated
/// stream bytes for the live viewer.
///
/// The earlier cache re-rendered the **whole** retained window (up to the
/// 256 KB scroll cap) and re-split every row each time the stream cursor advanced —
/// O(buffer) per accepted delta at up to the 5 Hz poll rate, a milder
/// recurrence of the symptom ADR-011 removed. Now only the **new** bytes are
/// rendered, through a persistent [`StreamRenderer`] (ADR-018 — the same
/// carry/tab-column/hex-continuation state the `.disp` recorder uses, so the
/// incremental output equals a one-shot render; chunking-invariance is pinned
/// by the renderer's own tests), and appended as rows. Front rows are trimmed
/// in per-append batches as the byte window evicts.
///
/// A full rebuild happens only when a **setting** changes (channel selection,
/// view mode, ctrl-chars, wrap width), when a Mark appears for (or is pruned
/// from) an already-rendered offset, or when the stream resets/evicts past
/// the rendered point. `wrap_cols` is the wrap width in monospace columns:
/// rows are pre-wrapped so each is exactly one visual line (uniform height),
/// which lets the viewer both soft-wrap *and* virtualize with `show_rows`.
struct StreamRenderCache {
    channel: ChannelId,
    mode: DisplayMode,
    chars: CharacterRendering,
    wrap_cols: usize,
    /// Hex grouping and resolved line length the cached rows were rendered at.
    ///
    /// Part of the rebuild key because they change the *text* of rows already
    /// rendered, not just the text of rows to come — without them, changing the
    /// grouping restyled only newly arriving bytes and left the scrollback in
    /// the old shape until something else forced a rebuild.
    hex_bytes_per_group: usize,
    hex_bytes_per_line: usize,
    /// Signature of the marks already spliced into rendered rows (offset <
    /// `rendered_cursor`). A change — a late mark for an already-rendered
    /// byte, or front-pruning of the mark list — forces a rebuild; marks for
    /// not-yet-rendered bytes ride the incremental path.
    history_marks_sig: u64,
    /// The channel's `marks_version` at the last refresh. The signature above
    /// is recomputed only when this moved — an idle frame compares two u64s
    /// instead of re-hashing the whole mark history.
    marks_version: u64,
    /// Absolute stream offset rendered so far (== the view's `stream_cursor`
    /// at the last refresh).
    rendered_cursor: u64,
    /// The persistent incremental renderer (ADR-018 state).
    renderer: StreamRenderer,
    rows: Vec<String>,
    /// Per-append trim accounting: (`end_offset`, rows owned). When the byte
    /// window's start passes a batch's end offset, its rows are dropped from
    /// the front. Ownership is kept exact across the open-row seam (an append
    /// pops the previous open row, so the previous batch is debited one).
    row_batches: std::collections::VecDeque<(u64, usize)>,
}

/// eframe storage key for the persisted recent-profiles list (newline-joined paths).
const RECENT_PROFILES_KEY: &str = "recent_profiles";
/// eframe storage key for the profile to resume on launch (a path; empty for
/// none). ADR-045.
const RESUME_PROFILE_KEY: &str = "listener.resume_profile";
/// eframe storage key for the dark-theme preference ("true"/"false"; same key
/// talker uses). Light is the default.
const DARK_MODE_KEY: &str = "dark_mode";

impl ListenerApp {
    fn new(
        bridge: BridgeHandle,
        repaint: std::sync::Arc<wiredata_ui::repaint::RepaintCoalescer>,
        ctx: &egui::Context,
        storage: Option<&dyn eframe::Storage>,
        event_log: crate::diagnostics::EventLogStatus,
    ) -> Self {
        // Restore the recent-profiles list from eframe storage (survives restarts).
        let recent_profiles = storage
            .and_then(|s| s.get_string(RECENT_PROFILES_KEY))
            .map(|s| {
                s.lines()
                    .filter(|l| !l.is_empty())
                    .map(std::path::PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
        // Restore the theme preference (light default) and activate it —
        // both visuals were installed by `apply_style`; this only picks.
        let dark_mode = storage
            .and_then(|s| s.get_string(DARK_MODE_KEY))
            .map(|v| v == "true")
            .unwrap_or(false);
        ctx.set_theme(if dark_mode {
            egui::ThemePreference::Dark
        } else {
            egui::ThemePreference::Light
        });
        let resume_profile = storage
            .and_then(|s| s.get_string(RESUME_PROFILE_KEY))
            .filter(|path| !path.is_empty())
            .map(std::path::PathBuf::from);
        let mut app = Self {
            bridge,
            repaint,
            state: AppState::default(),
            selected: None,
            last_selected_sent: None,
            edit_draft: None,
            confirm_remove: None,
            force_config_open: false,
            name_problem: None,
            channel_seq: std::collections::HashMap::new(),
            channels_collapsed: false,
            dark_mode,
            show_info: true,
            show_warn: true,
            show_error: true,
            serial_ports: list_serial_ports(),
            stream_cache: None,
            resume_scroll_bottom: None,
            current_profile_path: None,
            resume_profile,
            recent_profiles,
            command_drop: None,
            event_log,
        };
        // Resume on launch (ADR-045): the driver checks the registered profile,
        // opening nothing; the countdown follows only if the check passes.
        if let Some(path) = app.resume_profile.clone() {
            app.state.start_resume_check();
            app.send(UiCommand::CheckResume(path));
        }
        app.refresh_local_addresses();
        app
    }

    /// Record a profile path as recently used: move/insert it at the front, dedup, and
    /// cap the list. Called on every save/load so the Profile menu's recents are live.
    pub(super) fn remember_recent_profile(&mut self, path: std::path::PathBuf) {
        self.recent_profiles.retain(|p| p != &path);
        self.recent_profiles.insert(0, path);
        self.recent_profiles.truncate(MAX_RECENT_PROFILES);
    }

    pub(super) fn refresh_serial_ports(&mut self) {
        self.serial_ports = list_serial_ports();
    }

    /// Ask the driver to list the host's local addresses for the UDP bind
    /// choice (§15). The OS is asked off the UI thread; the list arrives as an
    /// update.
    pub(super) fn refresh_local_addresses(&mut self) {
        self.send(UiCommand::ListLocalAddresses);
    }

    /// Drain every pending update into the view-model (non-blocking, §99). A newly
    /// added channel takes focus; a removed one that was selected clears it.
    fn drain_updates(&mut self) {
        while let Ok(update) = self.bridge.updates.try_recv() {
            match &update {
                bridge::UiUpdate::ChannelAdded(id, ..) => self.selected = Some(*id),
                // When the selected channel goes away, fall back to the one above it
                // (or below, if it was the first) instead of clearing focus (#2).
                bridge::UiUpdate::ChannelRemoved(id) if self.selected == Some(*id) => {
                    self.selected = self.state.neighbor(*id);
                }
                _ => {}
            }
            self.state.apply(update);
        }
    }

    /// Send a command to the driver. Non-blocking: a full command channel drops the
    /// command rather than stalling the UI thread (AGENTS §5). A drop is surfaced in
    /// the [`Self::command_drop`] banner *and* logged — it means the driver is
    /// saturated or gone, and the user's click did nothing; silence here cost a
    /// debugging session once (Stop all, before batching).
    pub(super) fn send(&mut self, command: UiCommand) {
        if let Err(e) = self.bridge.commands.try_send(command) {
            tracing::warn!("UI command dropped ({e}) — the driver is busy or stopped; retry");
            self.command_drop = Some((
                "a command was not delivered (the runtime is busy or stopped) — \
                 the last click did nothing; retry it"
                    .to_owned(),
                std::time::Instant::now(),
            ));
        }
    }

    /// The event log banner (§118): a lasting top strip while the persistent
    /// event log cannot be written, or once any of its lines were lost. It is
    /// not dismissable, because the condition it reports is still true.
    fn show_event_log_banner(&mut self, ui: &mut egui::Ui) {
        let problem = self.event_log.problem();
        let lost = self.event_log.lost_entries();
        if problem.is_none() && lost == 0 {
            return;
        }
        let mut message = match &problem {
            Some(problem) => format!("The event log is not being saved: {problem}"),
            None => "The event log could not keep up".to_owned(),
        };
        if lost > 0 {
            message.push_str(&format!(" — {lost} log lines were not saved"));
        }
        egui::Panel::top("event_log_notice")
            .resizable(false)
            .show_inside(ui, |ui| {
                let warn = ui.visuals().warn_fg_color;
                ui.colored_label(warn, format!("\u{26A0} {message}"));
            });
    }

    /// Resuming the registered profile on launch (ADR-045): the countdown with
    /// Cancel, why nothing started, or when it resumed. When the countdown
    /// runs out, the resume is sent from here.
    fn show_resume_banner(&mut self, ui: &mut egui::Ui) {
        if let Some(path) = self.state.resume_due(std::time::Instant::now()) {
            self.send(UiCommand::Resume(path));
        }
        let (message, button) = match self.state.resume() {
            ResumeState::Idle | ResumeState::Checking => return,
            ResumeState::Counting { name, until, .. } => {
                let left = until.saturating_duration_since(std::time::Instant::now());
                // Keep the count moving without input.
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(250));
                (
                    format!(
                        "Resuming \u{201C}{name}\u{201D} in {} s",
                        left.as_secs() + u64::from(left.subsec_nanos() > 0)
                    ),
                    "Cancel",
                )
            }
            ResumeState::Resuming => ("Resuming…".to_owned(), "Dismiss"),
            ResumeState::Refused(why) => (format!("\u{26A0} Did not resume: {why}"), "Dismiss"),
            ResumeState::Resumed(name, at) => {
                let at: chrono::DateTime<chrono::Local> = (*at).into();
                (
                    format!(
                        "Resumed \u{201C}{name}\u{201D} automatically at {}",
                        at.format("%H:%M")
                    ),
                    "Dismiss",
                )
            }
        };
        let clicked = egui::Panel::top("resume_notice")
            .resizable(false)
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(message);
                    ui.button(button).clicked()
                })
                .inner
            })
            .inner;
        if clicked {
            self.state.clear_resume();
        }
    }

    /// Lasting recording faults (§56.1, §56.2): low disk and "recording
    /// unstable" stay on screen until they clear, whichever Channel is
    /// selected, rather than scrolling away in the log. The glyph and the words
    /// carry the fault; the colour only reinforces them.
    fn show_recording_faults_banner(&mut self, ui: &mut egui::Ui) {
        let lines = self.state.lasting_recording_faults();
        if lines.is_empty() {
            return;
        }
        egui::Panel::top("recording_faults_notice")
            .resizable(false)
            .show_inside(ui, |ui| {
                let fault = wiredata_ui::palette::active(ui).fault;
                for line in lines {
                    ui.colored_label(fault, format!("\u{26A0} {line}"));
                }
            });
    }

    /// The dropped-command banner (see [`Self::send`]): a top strip in the
    /// warning color, dismissable, auto-expiring after
    /// [`COMMAND_DROP_NOTICE_TTL`]. Drawn before the panels so it pushes the
    /// whole workspace down — impossible to miss, gone when stale.
    fn show_command_drop_banner(&mut self, ui: &mut egui::Ui) {
        let Some((message, at)) = &self.command_drop else {
            return;
        };
        if at.elapsed() > COMMAND_DROP_NOTICE_TTL {
            self.command_drop = None;
            return;
        }
        let message = message.clone();
        let dismissed = egui::Panel::top("command_drop_notice")
            .resizable(false)
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    let warn = ui.visuals().warn_fg_color;
                    ui.colored_label(warn, format!("\u{26A0} {message}"));
                    ui.button("Dismiss").clicked()
                })
                .inner
            })
            .inner;
        if dismissed {
            self.command_drop = None;
        }
    }

    /// Add a fresh channel of `kind` (from the "Add" menu by the list heading). It is
    /// auto-selected, and configured in the Configure section above the view. The default
    /// name is `<base><N>` (e.g. `UDP_Channel3`), where `N` is a per-kind monotonic
    /// counter that is **never reused** — deleting a channel does not free its number
    /// (§6, ADR-014). Each kind counts independently.
    pub(super) fn add_channel(&mut self, kind: AddKind) {
        let mut config = template_for(kind);
        config.name = self.next_channel_name(kind, config.name.as_str());
        self.send(UiCommand::AddChannel(Box::new(config)));
    }

    /// The next default name for `kind`: `<base><seq>` with a per-kind monotonic, never-
    /// reused sequence (§6, ADR-014). If that name somehow already exists (e.g. a loaded
    /// profile used it), keep advancing the counter until it is free — still never
    /// reusing a lower number.
    fn next_channel_name(&mut self, kind: AddKind, base: &str) -> crate::core::ChannelName {
        let taken: std::collections::HashSet<String> = self
            .state
            .channels()
            .map(|v| v.name.to_ascii_lowercase())
            .collect();
        loop {
            let seq = self.channel_seq.entry(kind).or_insert(0);
            *seq += 1;
            let candidate = format!("{base}{seq}");
            if !taken.contains(&candidate.to_ascii_lowercase()) {
                return crate::core::ChannelName::new(candidate);
            }
        }
    }

    /// Re-seed the edit draft from the selected channel's config whenever the
    /// selection changes, so the Configure section edits a fresh working copy.
    pub(super) fn sync_edit_draft(&mut self, id: ChannelId) {
        if self.edit_draft.as_ref().map(|(eid, _)| *eid) != Some(id) {
            if let Some(view) = self.state.channel(id) {
                // Pop the Configure section open when focus lands on a channel that
                // still needs setup (missing port) or is faulted (e.g. a bind
                // conflict) — so the fix is right there, not hidden behind a header.
                self.force_config_open =
                    config_incomplete(&view.config) || view.status == ChannelStatus::Faulted;
                self.edit_draft = Some((id, view.config.clone()));
                self.name_problem = None; // clear any stale rename warning on switch
            }
        }
    }

    /// Start a channel on the config currently in the editor (the unified "go" action,
    /// also reached as "Apply & Restart" / "Retry"). Commits the edited config and
    /// starts in one server-side step (`CommitAndStart` → `commit_and_start`), so Start
    /// can never run on a stale config and the Faulted/Running→restart sequencing lives
    /// in the runtime. Refuses, with an inline complaint, if the config is incomplete —
    /// e.g. a UDP channel with no port would otherwise bind an ephemeral port and
    /// silently "run" (#3, #6).
    pub(super) fn try_start(&mut self, id: ChannelId) {
        let Some(config) = self.start_config(id) else {
            return;
        };
        // Optimistically clear the prior error on a (re)start click, so a stale fault
        // message doesn't linger until the ChannelStarted echo arrives. A fresh fault
        // re-sets it if the restart fails again.
        if let Some(view) = self.state.channel_mut(id) {
            view.last_error = None;
        }
        self.send(UiCommand::CommitAndStart {
            id,
            config: Some(Box::new(config)),
            start: true,
        });
    }

    /// The config to start channel `id` with: the edit draft if one is loaded for this
    /// channel (what the editor shows), else its committed config. Returns `None` —
    /// raising an inline "unconfigured" complaint — if that config is incomplete (e.g.
    /// a UDP channel with no port, which would otherwise bind an ephemeral port and
    /// silently "run", #3/#6). Shared by `try_start` and `start_all` so both honor the
    /// same draft-preference and validation rule.
    fn start_config(&mut self, id: ChannelId) -> Option<ChannelConfig> {
        let config = match &self.edit_draft {
            Some((eid, cfg)) if *eid == id => cfg.clone(),
            _ => self.state.channel(id)?.config.clone(),
        };
        if config_incomplete(&config) {
            self.complain_unconfigured(id);
            return None;
        }
        Some(config)
    }

    /// "Start all": validate every Stopped channel the same way `try_start` does
    /// (inline complaint for an unconfigured one), and send the ready ones as a single
    /// `StartAll` batch. One command, not a 2N `Reconfigure`+`Start` burst — a burst
    /// could overflow the bounded command channel and silently drop starts.
    pub(super) fn start_all(&mut self) {
        let mut batch = Vec::new();
        for id in self.state.startable_channel_ids() {
            if let Some(config) = self.start_config(id) {
                batch.push((id, Box::new(config)));
            }
        }
        if !batch.is_empty() {
            self.send(UiCommand::StartAll(batch));
        }
    }

    /// Surface an "unconfigured" complaint for a channel inline (red ⚠ line) and open
    /// its Configure section if it's the one in view.
    fn complain_unconfigured(&mut self, id: ChannelId) {
        self.state.apply(bridge::UiUpdate::ChannelError(
            id,
            "Not configured — set a port before starting.".to_string(),
        ));
        if self.selected == Some(id) {
            self.force_config_open = true;
        }
    }

    /// Tab-style keyboard switching between channels (#3): Ctrl+Tab / Ctrl+Shift+Tab
    /// cycle forward / back (wrapping), like editor tabs. No-op with no selection.
    fn handle_tab_keys(&mut self, ctx: &egui::Context) {
        let (tab, shift, ctrl) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Tab),
                i.modifiers.shift,
                i.modifiers.ctrl,
            )
        });
        if tab && ctrl {
            let next = match self.selected {
                Some(id) => self.state.cycle(id, !shift),
                None => self.state.first(),
            };
            if next.is_some() {
                self.selected = next;
            }
        }
    }

    /// The "are you sure?" dialog for Remove (#1). A modal so it can't be ignored;
    /// confirming sends the removal (focus then falls to the neighbour, #2).
    fn show_remove_confirm(&mut self, ctx: &egui::Context) {
        let Some(id) = self.confirm_remove else {
            return;
        };
        let name = self
            .state
            .channel(id)
            .map(|v| v.name.clone())
            .unwrap_or_default();
        match wiredata_ui::dialog::confirm_remove_channel(ctx, &name) {
            wiredata_ui::dialog::Confirm::Pending => {}
            wiredata_ui::dialog::Confirm::Cancelled => self.confirm_remove = None,
            wiredata_ui::dialog::Confirm::Confirmed => {
                self.send(UiCommand::RemoveChannel(id));
                self.confirm_remove = None;
            }
        }
    }
}

impl eframe::App for ListenerApp {
    /// Persist the recent-profiles list (eframe calls this periodically and on exit),
    /// so the Profile menu's recents survive a restart. Paths are newline-joined.
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let joined = self
            .recent_profiles
            .iter()
            .filter_map(|p| p.to_str())
            .collect::<Vec<_>>()
            .join("\n");
        storage.set_string(RECENT_PROFILES_KEY, joined);
        storage.set_string(DARK_MODE_KEY, self.dark_mode.to_string());
        let resume = self
            .resume_profile
            .as_deref()
            .and_then(std::path::Path::to_str)
            .unwrap_or_default();
        storage.set_string(RESUME_PROFILE_KEY, resume.to_owned());
    }

    /// On window close (the X button) or any app exit, shut the driver down and wait
    /// for it — so every open recording is finalized before the process dies. Without
    /// this the runtime thread was detached and the last buffered bytes of a `.raw`/
    /// `.disp` could be lost on exit.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.bridge.shutdown_and_join();
    }

    // This workspace's eframe surfaces a `Ui` directly (App::ui), like talker's GUI.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Re-arm the coalescer BEFORE draining, so a push arriving mid-drain
        // either lands in this frame's batch or triggers a fresh wake.
        self.repaint.frame_started();
        self.drain_updates();
        self.handle_tab_keys(ui.ctx());
        self.show_command_drop_banner(ui);
        self.show_resume_banner(ui);
        self.show_event_log_banner(ui);
        self.show_recording_faults_banner(ui);
        let channels_collapsed = self.channels_collapsed;
        let channel_panel = if channels_collapsed {
            // Collapsed: a thin strip — an expand button plus mini tabs (a status dot
            // per channel, click to select, name on hover) (#1).
            egui::Panel::left("channel_list_collapsed")
                .resizable(false)
                .show_separator_line(false)
                .show_inside(ui, |ui| {
                    if ui
                        .button("\u{25B6}")
                        .on_hover_text("Show channels")
                        .clicked()
                    {
                        self.channels_collapsed = false;
                    }
                    ui.separator();
                    let clip_rect = ui.clip_rect();
                    ui.add_space(selection::TAB_JOIN_MARGIN);
                    let mini: Vec<(ChannelId, ChannelStatus, String)> = self
                        .state
                        .channels()
                        .enumerate()
                        .map(|(index, v)| {
                            (v.id, v.status, selection::channel_title(index + 1, &v.name))
                        })
                        .collect();
                    let base = egui::TextStyle::Body.resolve(ui.style()).size;
                    let mut selected_tab_rect = None;
                    for (cid, status, name) in mini {
                        let selected = self.selected == Some(cid);
                        let (glyph, scale) = status_glyph(status);
                        let dot = egui::RichText::new(glyph)
                            .size(base * scale)
                            .color(status_color(status, wiredata_ui::palette::active(ui)));
                        let response = selection::mini_tab(ui, selected, dot).on_hover_text(name);
                        if selected {
                            selected_tab_rect = Some(response.rect);
                        }
                        if response.clicked() {
                            self.selected = Some(cid);
                        }
                        ui.add_space(selection::TAB_JOIN_MARGIN);
                    }
                    selected_tab_rect.map(|rect| selection::SelectedTab::new(rect, clip_rect))
                })
        } else {
            egui::Panel::left("channel_list")
                .resizable(true)
                .default_size(320.0)
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
        self.show_remove_confirm(ui.ctx());
        // Tell the driver which channel is on screen so it full-snapshots only that
        // one (others get cheap stats). Sent only on change.
        if self.selected != self.last_selected_sent {
            self.last_selected_sent = self.selected;
            self.send(UiCommand::Select(self.selected));
        }
    }
}
