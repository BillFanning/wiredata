//! The pop-out display (listener spec §50.4, ADR-071): one channel's received
//! bytes in a window of its own.
//!
//! A pop-out reads the bytes the GUI already holds for the channel
//! ([`ChannelView::stream_bytes`](super::state::ChannelView)). It keeps only its
//! own clear point, pause copy, view settings and row cache, so nothing it does
//! reaches the channel, its display, its settings or its recordings.
//!
//! [`Popout`] holds no egui and is tested on plain slices. [`ListenerApp::show_popouts`]
//! draws the windows; it runs from `App::logic`, which eframe calls every frame
//! even while the main window is minimized.

use crate::core::ChannelId;

use super::detail::stream_view::{show_stream_rows, StreamInput};
use super::state::{AppState, ChannelView};
use super::view_prefs::ViewPrefs;
use super::{ListenerApp, StreamRenderCache};
use wiredata_ui::hover::{Hover, ShowHover};

/// Where a pop-out window opens, before the person resizes it.
const DEFAULT_SIZE: [f32; 2] = [900.0, 600.0];
/// The size of Pause and Clear: the size of Linemon's buttons and of
/// Listener's channel Start and Stop.
const BUTTON_SIZE: egui::Vec2 = egui::vec2(96.0, 32.0);
/// The smallest a pop-out window can be made.
const MIN_SIZE: [f32; 2] = [420.0, 240.0];

pub(super) const POP_OUT_HOVER: Hover = Hover::new(
    "Opens this channel's display in a window of its own.\n\n\
     The window shows the received bytes only, without timestamps or other \
     insertions. You can pause it, clear it, and change its font and view without \
     changing this display or the channel.\n\n\
     Pausing this display pauses the window too, because both show the same bytes.",
);

const PAUSE_HOVER: Hover = Hover::new(
    "Stops this window's display so you can read it.\n\n\
     The channel keeps receiving, any recording carries on, and its own display \
     keeps moving.",
);

const RESUME_HOVER: Hover = Hover::new(
    "Shows live data in this window again, from the newest bytes.\n\n\
     What arrived while it was paused is shown too, as far back as the channel's \
     display still holds it.",
);

/// Said while the channel's own display is paused: it and the pop-out show the
/// same bytes, so nothing new reaches the pop-out either.
const HELD_TEXT: &str = "This channel's display is paused in the main window, so nothing \
     new appears here until it is resumed there.";

const CLEAR_HOVER: Hover = Hover::new(
    "Empties this window.\n\n\
     The channel's own display is not cleared.",
);

/// The picture a paused pop-out holds still: a copy of the bytes it was showing.
struct Frozen {
    /// Absolute stream offset of `bytes[0]`.
    start: u64,
    bytes: Vec<u8>,
}

/// What the controls asked for in one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Pause,
    Resume,
    Clear,
}

/// One channel's pop-out: its own pause, clear point, view settings and rows.
pub(super) struct Popout {
    channel: ChannelId,
    /// The window's own copy of the channel's view settings. Edits stay here.
    prefs: ViewPrefs,
    cache: Option<StreamRenderCache>,
    /// The key the row cache is built under. It is replaced whenever the window's
    /// content stops being a continuation of what the cache holds (a clear, a
    /// resume, a restart), which makes the cache rebuild whatever the offsets.
    key: ChannelId,
    /// Bytes at offsets below this are not shown.
    clear_at: u64,
    /// The pipeline run the clear point belongs to.
    generation: Option<u64>,
    /// Set while the window is paused.
    frozen: Option<Frozen>,
    /// One-shot: the next draw jumps to the newest bytes.
    jump_to_bottom: bool,
}

impl Popout {
    /// A pop-out for `channel`, starting with the channel's own settings and
    /// showing everything the channel's display holds. `generation` is the run
    /// the channel's bytes belong to now.
    pub(super) fn new(channel: ChannelId, prefs: ViewPrefs, generation: Option<u64>) -> Self {
        Self {
            channel,
            prefs,
            cache: None,
            key: ChannelId::new(),
            clear_at: 0,
            generation,
            frozen: None,
            jump_to_bottom: false,
        }
    }

    pub(super) fn channel(&self) -> ChannelId {
        self.channel
    }

    pub(super) fn is_paused(&self) -> bool {
        self.frozen.is_some()
    }

    /// Follow the channel into a new run: offsets start again from zero, so the
    /// clear point of the old run means nothing and the window starts empty. A
    /// paused window keeps its picture until Resume.
    pub(super) fn sync(&mut self, generation: Option<u64>) {
        if generation != self.generation {
            self.generation = generation;
            self.clear_at = 0;
            self.key = ChannelId::new();
        }
    }

    /// Freeze what the window shows now. `live` holds the channel's bytes, the
    /// first at offset `base`.
    pub(super) fn pause(&mut self, live: &[u8], base: u64) {
        if self.frozen.is_none() {
            let (bytes, start) = window_of(None, self.clear_at, live, base);
            self.frozen = Some(Frozen {
                start,
                bytes: bytes.to_vec(),
            });
        }
    }

    /// Show live data again, from the newest bytes.
    pub(super) fn resume(&mut self) {
        if self.frozen.take().is_some() {
            self.key = ChannelId::new();
            self.jump_to_bottom = true;
        }
    }

    /// Empty the window. `cursor` is the offset one past the channel's newest
    /// byte, so bytes that arrive afterwards are the first shown.
    pub(super) fn clear(&mut self, cursor: u64) {
        self.clear_at = cursor;
        self.key = ChannelId::new();
        if self.frozen.is_some() {
            self.frozen = Some(Frozen {
                start: cursor,
                bytes: Vec::new(),
            });
        }
    }

    /// The bytes the window shows, and the absolute offset of the first.
    #[cfg(test)]
    fn window<'a>(&'a self, live: &'a [u8], base: u64) -> (&'a [u8], u64) {
        window_of(self.frozen.as_ref(), self.clear_at, live, base)
    }

    /// The viewer's input for `bytes` starting at offset `start`: no marks and
    /// nothing trailing, so nothing is spliced into the bytes.
    fn stream_input(key: ChannelId, bytes: &[u8], start: u64) -> StreamInput<'_> {
        StreamInput {
            key,
            window: bytes,
            window_start: start,
            cursor: start + bytes.len() as u64,
            marks: &[],
            marks_version: 0,
            trailing: &[],
        }
    }

    /// Draw the window's contents: the controls above the viewer.
    fn show(&mut self, ui: &mut egui::Ui, view: &mut ChannelView) {
        // Contiguous first, so the immutable slices below can be taken.
        view.stream_bytes.make_contiguous();
        let (live, rest) = view.stream_bytes.as_slices();
        debug_assert!(rest.is_empty(), "make_contiguous left a split deque");
        let base = view.stream_base_offset;
        let cursor = view.stream_cursor;

        let was_paused = self.is_paused();
        let held = view.display_paused;
        // An id of this window's own: egui keeps a panel's height under its id
        // for every window, so two pop-outs sharing one would cut each other off.
        let action = egui::Panel::top(ui.id().with("popout_controls"))
            .resizable(false)
            .show_inside(ui, |ui| {
                show_controls(ui, &mut self.prefs, was_paused, held)
            })
            .inner;
        match action {
            Some(Action::Pause) => self.pause(live, base),
            Some(Action::Resume) => self.resume(),
            Some(Action::Clear) => self.clear(cursor),
            None => {}
        }

        let paused = self.is_paused();
        let (bytes, start) = window_of(self.frozen.as_ref(), self.clear_at, live, base);
        let note = bytes.is_empty().then_some(if paused {
            "This window is paused and has nothing to show."
        } else if held {
            HELD_TEXT
        } else {
            "Nothing to show yet. Bytes the channel receives will appear here."
        });
        let input = Self::stream_input(self.key, bytes, start);
        let jump = std::mem::take(&mut self.jump_to_bottom);
        egui::CentralPanel::default().show_inside(ui, |ui| {
            show_stream_rows(ui, &self.prefs, &mut self.cache, &input, note, jump);
        });
    }
}

/// The bytes a window shows and the absolute offset of the first: the frozen
/// copy while paused, else the channel's bytes from the clear point on.
/// `live[0]` sits at offset `base`.
fn window_of<'a>(
    frozen: Option<&'a Frozen>,
    clear_at: u64,
    live: &'a [u8],
    base: u64,
) -> (&'a [u8], u64) {
    if let Some(frozen) = frozen {
        return (&frozen.bytes, frozen.start);
    }
    // Bytes the channel has already dropped are gone, whatever the clear point.
    let start = clear_at.max(base);
    let skip = usize::try_from(start - base)
        .unwrap_or(usize::MAX)
        .min(live.len());
    (&live[skip..], base + skip as u64)
}

/// The pop-out's controls: Pause (Resume) and Clear, then the channel display's
/// view controls (view, control characters, hex grouping, font size, colours and
/// font). `held` says the channel's own display is paused in the main window.
/// Returns what was pressed.
fn show_controls(
    ui: &mut egui::Ui,
    prefs: &mut ViewPrefs,
    paused: bool,
    held: bool,
) -> Option<Action> {
    let mut action = None;
    // Linemon's look: a quarter of a row's height above the buttons and below
    // them, and buttons the size of Linemon's and of Listener's channel buttons.
    let row_gap = ui.spacing().interact_size.y * 0.25;
    let button = |text: &str| egui::Button::new(text).min_size(BUTTON_SIZE);
    ui.add_space(row_gap);
    ui.horizontal_wrapped(|ui| {
        if paused {
            if ui.add(button("Resume")).on_hover(&RESUME_HOVER).clicked() {
                action = Some(Action::Resume);
            }
        } else if ui.add(button("Pause")).on_hover(&PAUSE_HOVER).clicked() {
            action = Some(Action::Pause);
        }
        if ui.add(button("Clear")).on_hover(&CLEAR_HOVER).clicked() {
            action = Some(Action::Clear);
        }
        if paused {
            ui.label(
                "This window is paused. The channel keeps receiving, and any recording carries on.",
            );
        } else if held {
            ui.label(HELD_TEXT);
        }
    });
    ui.add_space(row_gap);
    // The channel display's own controls, without the scroll-buffer row: the
    // pop-out shows the channel's buffer, so its size is not the pop-out's to set.
    super::detail::stream_view::show_view_controls(ui, prefs, None);
    action
}

/// Drop the pop-outs whose channel has been removed.
fn drop_removed(popouts: &mut Vec<Popout>, state: &AppState) {
    popouts.retain(|popout| state.channel(popout.channel()).is_some());
}

/// The channels that have a pop-out, in a stable order, when they differ from
/// `sent`, the set the driver was last told; `None` when they are the same.
fn changed_set(popouts: &[Popout], sent: &[ChannelId]) -> Option<Vec<ChannelId>> {
    let mut channels: Vec<ChannelId> = popouts.iter().map(Popout::channel).collect();
    channels.sort_by_key(|id| format!("{id:?}"));
    (channels != sent).then_some(channels)
}

/// The id of a channel's pop-out viewport: one per channel.
fn viewport_id(channel: ChannelId) -> egui::ViewportId {
    egui::ViewportId::from_hash_of(("listener_popout", channel))
}

impl ListenerApp {
    /// The Pop out button: open the channel's pop-out, or bring the one that is
    /// open to the front.
    pub(super) fn pop_out(&mut self, ctx: &egui::Context, channel: ChannelId) {
        if self.popouts.iter().any(|p| p.channel() == channel) {
            ctx.send_viewport_cmd_to(viewport_id(channel), egui::ViewportCommand::Focus);
            return;
        }
        let Some(view) = self.state.channel(channel) else {
            return;
        };
        let popout = Popout::new(channel, view.view_prefs.clone(), view.stream_generation());
        self.popouts.push(popout);
        ctx.request_repaint();
    }

    /// Show every open pop-out, and tell the driver which channels have one.
    /// Called every frame from `App::logic`, so the windows survive the main
    /// window being minimized. A pop-out whose window was closed, or whose
    /// channel was removed, is dropped.
    pub(super) fn show_popouts(&mut self, ctx: &egui::Context) {
        drop_removed(&mut self.popouts, &self.state);
        let mut open = std::mem::take(&mut self.popouts);
        open.retain_mut(|popout| self.show_popout(ctx, popout));
        self.popouts = open;

        if let Some(channels) = changed_set(&self.popouts, &self.last_popouts_sent) {
            self.last_popouts_sent = channels.clone();
            self.send(super::bridge::UiCommand::SetPopouts(channels));
        }
    }

    /// Draw one pop-out's window. Returns whether it stays open.
    fn show_popout(&mut self, ctx: &egui::Context, popout: &mut Popout) -> bool {
        let Some(view) = self.state.channel_mut(popout.channel()) else {
            return false;
        };
        popout.sync(view.stream_generation());
        let builder = egui::ViewportBuilder::default()
            .with_title(format!(
                "Listener pop-out display for channel {}",
                view.name
            ))
            .with_inner_size(DEFAULT_SIZE)
            .with_min_inner_size(MIN_SIZE)
            .with_resizable(true);
        let mut keep = true;
        ctx.show_viewport_immediate(viewport_id(popout.channel()), builder, |ui, _class| {
            if ui.input(|i| i.viewport().close_requested()) {
                keep = false;
                return;
            }
            popout.show(ui, view);
        });
        keep
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::DisplayMode;
    use crate::display::{CharacterRendering, DisplayEncoding, DisplayView, WrappingMode};
    use crate::gui::detail::stream_view::build_rows;

    fn popout() -> Popout {
        Popout::new(ChannelId::new(), ViewPrefs::default(), Some(1))
    }

    fn state_with(channels: &[ChannelId]) -> AppState {
        let mut state = AppState::default();
        for &id in channels {
            state.apply(crate::gui::bridge::UiUpdate::ChannelAdded(
                id,
                "c".into(),
                String::new(),
                Box::new(crate::config::templates::udp_template()),
            ));
        }
        state
    }

    #[test]
    fn a_removed_channels_pop_out_is_dropped_and_the_driver_is_told() {
        let (a, b) = (ChannelId::new(), ChannelId::new());
        let mut popouts = vec![
            Popout::new(a, ViewPrefs::default(), None),
            Popout::new(b, ViewPrefs::default(), None),
        ];
        let sent = changed_set(&popouts, &[]).expect("the first set is sent");
        assert_eq!(sent.len(), 2);
        assert_eq!(changed_set(&popouts, &sent), None, "nothing changed");

        // b is removed from the main window.
        drop_removed(&mut popouts, &state_with(&[a]));
        assert_eq!(popouts.len(), 1);
        assert_eq!(popouts[0].channel(), a);
        assert_eq!(changed_set(&popouts, &sent), Some(vec![a]));
    }

    #[test]
    fn closing_the_last_pop_out_tells_the_driver_the_set_is_empty() {
        let a = ChannelId::new();
        let sent = vec![a];
        assert_eq!(changed_set(&[], &sent), Some(vec![]));
    }

    fn view(mode: DisplayMode) -> DisplayView {
        DisplayView {
            mode,
            encoding: DisplayEncoding::Utf8,
            character_rendering: CharacterRendering::Native,
            wrapping: WrappingMode::NoWrap,
            wrap_width: None,
            hex_separator: " ".to_string(),
            hex_bytes_per_group: 1,
            hex_bytes_per_line: 16,
        }
    }

    /// The rows the window draws for a channel holding `live` from `base`.
    fn rows(p: &mut Popout, live: &[u8], base: u64, mode: DisplayMode) -> Vec<String> {
        let (bytes, start) = window_of(p.frozen.as_ref(), p.clear_at, live, base);
        let input = Popout::stream_input(p.key, bytes, start);
        let r = build_rows(&mut p.cache, &input, &view(mode), 80);
        (0..r.len()).filter_map(|i| r.get(i).cloned()).collect()
    }

    fn strings(rows: &[&str]) -> Vec<String> {
        rows.iter().map(|r| r.to_string()).collect()
    }

    const TEXT: DisplayMode = DisplayMode::Rendered;

    #[test]
    fn a_new_pop_out_shows_everything_the_channel_holds() {
        let p = popout();
        assert_eq!(p.window(b"alpha", 0), (&b"alpha"[..], 0));
        assert_eq!(p.window(b"cdef", 2), (&b"cdef"[..], 2));
    }

    #[test]
    fn the_viewer_is_given_no_marks_and_nothing_trailing() {
        let key = ChannelId::new();
        let input = Popout::stream_input(key, b"abc", 4);
        assert!(input.marks.is_empty());
        assert!(input.trailing.is_empty());
        assert_eq!((input.window_start, input.cursor), (4, 7));
        assert_eq!(input.window, b"abc");
    }

    #[test]
    fn the_rows_are_the_received_bytes_and_nothing_else() {
        let mut p = popout();
        assert_eq!(
            rows(&mut p, b"$GPGGA,1\r\n$GPRMC,2", 0, TEXT),
            strings(&["$GPGGA,1", "$GPRMC,2"])
        );
    }

    #[test]
    fn clear_hides_what_was_there_and_shows_what_arrives_after() {
        let mut p = popout();
        assert_eq!(p.window(b"alpha", 0).0, b"alpha");
        p.clear(5);
        assert_eq!(p.window(b"alpha", 0), (&b""[..], 5));
        assert_eq!(p.window(b"alphabravo", 0), (&b"bravo"[..], 5));
    }

    #[test]
    fn rows_after_a_clear_have_no_row_from_before_it() {
        let mut p = popout();
        assert_eq!(rows(&mut p, b"alpha", 0, TEXT), strings(&["alpha"]));
        p.clear(5);
        // The channel still holds the old bytes, and the cache has rendered up
        // to offset 5, where the window now starts and ends.
        assert_eq!(rows(&mut p, b"alpha", 0, TEXT), strings(&[""]));
        assert_eq!(rows(&mut p, b"alphabravo", 0, TEXT), strings(&["bravo"]));
        p.clear(10);
        assert_eq!(rows(&mut p, b"alphabravo", 0, TEXT), strings(&[""]));
    }

    #[test]
    fn a_clear_point_the_channel_has_dropped_changes_nothing() {
        let mut p = popout();
        p.clear(3);
        // The channel dropped everything before offset 8.
        assert_eq!(p.window(b"hijk", 8), (&b"hijk"[..], 8));
    }

    #[test]
    fn pause_holds_the_picture_while_the_channel_moves_on() {
        let mut p = popout();
        assert_eq!(rows(&mut p, b"alpha", 0, TEXT), strings(&["alpha"]));
        p.pause(b"alpha", 0);
        assert!(p.is_paused());
        // More bytes arrive, and then the channel's own window slides on.
        assert_eq!(rows(&mut p, b"alphabravo", 0, TEXT), strings(&["alpha"]));
        assert_eq!(rows(&mut p, b"bravocharlie", 5, TEXT), strings(&["alpha"]));
        assert_eq!(p.window(b"bravocharlie", 5), (&b"alpha"[..], 0));
    }

    #[test]
    fn pausing_twice_keeps_the_first_picture() {
        let mut p = popout();
        p.pause(b"alpha", 0);
        p.pause(b"alphabravo", 0);
        assert_eq!(p.window(b"alphabravo", 0).0, b"alpha");
    }

    #[test]
    fn pause_holds_only_what_was_shown_after_a_clear() {
        let mut p = popout();
        p.clear(5);
        p.pause(b"alphabravo", 0);
        assert_eq!(p.window(b"alphabravocharlie", 0).0, b"bravo");
    }

    #[test]
    fn resume_shows_live_data_again_and_jumps_to_the_newest() {
        let mut p = popout();
        assert_eq!(rows(&mut p, b"alpha", 0, TEXT), strings(&["alpha"]));
        p.pause(b"alpha", 0);
        p.resume();
        assert!(!p.is_paused());
        assert!(p.jump_to_bottom);
        assert_eq!(
            rows(&mut p, b"alphabravo", 0, TEXT),
            strings(&["alphabravo"])
        );
        // Resuming a window that is not paused does not ask for a jump.
        p.jump_to_bottom = false;
        p.resume();
        assert!(!p.jump_to_bottom);
    }

    #[test]
    fn resume_after_the_channel_dropped_old_bytes_draws_the_live_window() {
        let mut p = popout();
        assert_eq!(rows(&mut p, b"alpha", 0, TEXT), strings(&["alpha"]));
        p.pause(b"alpha", 0);
        p.resume();
        assert_eq!(rows(&mut p, b"xyz", 40, TEXT), strings(&["xyz"]));
    }

    #[test]
    fn clearing_a_paused_window_empties_it_and_resume_follows_the_clear_point() {
        let mut p = popout();
        p.pause(b"alpha", 0);
        // Bytes that arrived while paused are cleared with the rest.
        p.clear(10);
        assert_eq!(p.window(b"alphabravo", 0), (&b""[..], 10));
        assert_eq!(rows(&mut p, b"alphabravo", 0, TEXT), strings(&[""]));
        p.resume();
        assert_eq!(rows(&mut p, b"alphabravo", 0, TEXT), strings(&[""]));
        assert_eq!(
            rows(&mut p, b"alphabravocharlie", 0, TEXT),
            strings(&["charlie"])
        );
    }

    #[test]
    fn a_restart_clears_the_window_and_forgets_the_old_clear_point() {
        let mut p = popout();
        assert_eq!(rows(&mut p, b"alpha", 0, TEXT), strings(&["alpha"]));
        p.clear(3);
        assert_eq!(rows(&mut p, b"alpha", 0, TEXT), strings(&["ha"]));
        // The channel restarts: offsets begin again at zero, and the new run
        // is as long as the old one was.
        p.sync(Some(2));
        assert_eq!(p.window(b"gamma", 0), (&b"gamma"[..], 0));
        assert_eq!(rows(&mut p, b"gamma", 0, TEXT), strings(&["gamma"]));
    }

    #[test]
    fn a_restart_redraws_even_when_the_new_run_matches_the_old_offsets() {
        let mut p = popout();
        assert_eq!(rows(&mut p, b"alpha", 0, TEXT), strings(&["alpha"]));
        p.sync(Some(2));
        assert_eq!(rows(&mut p, b"gamma", 0, TEXT), strings(&["gamma"]));
    }

    #[test]
    fn the_same_run_leaves_the_clear_point_alone() {
        let mut p = popout();
        p.clear(3);
        p.sync(Some(1));
        assert_eq!(p.window(b"alphabravo", 0).0, b"habravo");
    }

    #[test]
    fn a_channel_cleared_for_a_new_run_clears_the_window() {
        let mut p = popout();
        p.clear(3);
        // The channel's view is emptied on a restart before its first delta.
        p.sync(None);
        assert_eq!(p.window(b"", 0), (&b""[..], 0));
        p.sync(Some(2));
        assert_eq!(p.window(b"ab", 0), (&b"ab"[..], 0));
    }

    #[test]
    fn a_paused_window_keeps_its_picture_through_a_restart() {
        let mut p = popout();
        p.pause(b"alpha", 0);
        p.sync(Some(2));
        assert_eq!(p.window(b"gamma", 0), (&b"alpha"[..], 0));
        p.resume();
        assert_eq!(rows(&mut p, b"gamma", 0, TEXT), strings(&["gamma"]));
    }

    #[test]
    fn view_changes_stay_in_the_pop_out_and_start_from_the_channels() {
        let channel = ViewPrefs {
            mode: DisplayMode::Hex,
            font_size: 20.0,
            ..ViewPrefs::default()
        };
        let mut p = Popout::new(ChannelId::new(), channel.clone(), None);
        assert_eq!(p.prefs.mode, DisplayMode::Hex);
        assert_eq!(p.prefs.font_size, 20.0);
        p.prefs.mode = DisplayMode::Raw;
        p.prefs.font_size = 30.0;
        assert_eq!(channel.mode, DisplayMode::Hex);
        assert_eq!(channel.font_size, 20.0);
    }

    #[test]
    fn a_view_change_redraws_the_same_bytes() {
        let mut p = popout();
        assert_eq!(
            rows(&mut p, b"AB", 0, DisplayMode::Hex),
            strings(&["41 42"])
        );
        assert_eq!(
            rows(&mut p, b"AB", 0, DisplayMode::Rendered),
            strings(&["AB"])
        );
    }
}
