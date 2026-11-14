//! The Raw and Display recording setup editors (§53–§59): destination, overwrite
//! policy, rotation. Config-driven (applied via a §13 Reconfigure); the live Record
//! toggle (ADR-012) is separate. Both editors share `recording_file_fields`.

use std::path::PathBuf;

use crate::config::ChannelConfig;
use crate::record::{effective_overwrite, FileRotationPolicy, OverwritePolicy};

use wiredata_ui::hover::{Hover, ShowHover};
use wiredata_ui::palette::active as palette;

/// The shared destination / overwrite / rotation controls for a recording (§55–§59),
/// used by both the Raw and Display editors (their config structs carry the same
/// fields; ADR-013).
fn recording_file_fields(
    ui: &mut egui::Ui,
    destination: &mut Option<PathBuf>,
    overwrite_policy: &mut OverwritePolicy,
    file_rotation: &mut FileRotationPolicy,
) {
    // A single file when not rotating; a directory of <channel>_<period> files
    // otherwise (§59). `rotating` reflects this frame's start — a one-frame lag when
    // the user flips rotation below is harmless.
    let rotating = *file_rotation != FileRotationPolicy::None;
    ui.horizontal(|ui| {
        ui.label(if rotating { "Folder" } else { "File" });
        let mut path = destination
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        if ui
            .add(egui::TextEdit::singleline(&mut path).desired_width(180.0))
            .changed()
        {
            let trimmed = path.trim();
            *destination = (!trimmed.is_empty()).then(|| PathBuf::from(trimmed));
        }
        if ui.button("Browse…").clicked() {
            // The native picker is modal and synchronous, so the egui UI (and the live
            // view) freeze while it's open — the same intentional exception to "the UI
            // thread never blocks" (AGENTS §5) as the profile picker. Acceptable: it's a
            // brief user-driven modal, and *reception* never stops (it runs on the
            // runtime thread); only the on-screen view pauses until the dialog closes.
            let picked = if rotating {
                rfd::FileDialog::new().pick_folder()
            } else {
                rfd::FileDialog::new().save_file()
            };
            if let Some(p) = picked {
                *destination = Some(p);
            }
        }
    });
    if destination.is_none() {
        ui.label(
            egui::RichText::new("⚠ set a destination — recording won't start without one")
                .color(palette(ui).warning),
        );
    }
    // With rotation on, each period gets a fresh file, so "Refuse" makes no
    // sense — disable it and move a Refuse selection to Append, via the same
    // shared §59 rule the runtime settings builders apply.
    *overwrite_policy = effective_overwrite(*overwrite_policy, *file_rotation);
    ui.horizontal(|ui| {
        ui.label("On exists").on_hover(&ON_EXISTS_HOVER);
        ui.add_enabled_ui(!rotating, |ui| {
            let refuse = ui.radio_value(overwrite_policy, OverwritePolicy::Refuse, "Refuse");
            if rotating {
                refuse.on_hover(&REFUSE_ROTATING_HOVER);
            } else {
                refuse.on_hover_text(
                    "Does not start recording if the file already exists, and leaves \
                     that file untouched.",
                );
            }
        });
        ui.radio_value(overwrite_policy, OverwritePolicy::Overwrite, "Overwrite")
            .on_hover_text("Replaces the existing file; its current contents are lost.");
        ui.radio_value(overwrite_policy, OverwritePolicy::AppendIfExists, "Append")
            .on_hover(&APPEND_HOVER);
    });
    ui.horizontal(|ui| {
        ui.label("Rotate").on_hover(&ROTATE_HOVER);
        ui.radio_value(file_rotation, FileRotationPolicy::None, "None")
            .on_hover(&ROTATE_NONE_HOVER);
        ui.radio_value(file_rotation, FileRotationPolicy::Hourly, "Hourly")
            .on_hover(&ROTATE_HOURLY_HOVER);
        ui.radio_value(file_rotation, FileRotationPolicy::Daily, "Daily")
            .on_hover(&ROTATE_DAILY_HOVER);
    });
}

const ON_EXISTS_HOVER: Hover = Hover::new(
    "What Listener does when the recording's first file already exists as recording \
     starts.\n\n\
     This applies only to the first file. Every later file of the same recording, \
     whether it follows a new rotation period, the size limit, or recording resuming \
     after an interruption, is a new numbered file, so nothing is overwritten.",
);
const REFUSE_ROTATING_HOVER: Hover = Hover::new(
    "Not available with rotation.\n\n\
     A rotating recording may find the current period's file already there, for \
     example after a restart. Append is used instead, so recording adds to that file \
     rather than failing.",
);
const APPEND_HOVER: Hover = Hover::new(
    "Keeps what is already recorded and adds the new data after it.\n\n\
     If the recording was earlier continued in numbered files such as GPS_2.raw, \
     the data is added to the newest of them, or to a new numbered file if that one \
     has reached the size limit.",
);
const ROTATE_HOVER: Hover = Hover::new(
    "Starts a new file each hour or day instead of writing one file continuously.\n\n\
     With rotation on, the destination is a folder and the files in it are named \
     <channel>_<time period>. The time period is local time.\n\n\
     A new file is started when the first data of the next period arrives, so a \
     period with no data produces no file.",
);
const ROTATE_NONE_HOVER: Hover = Hover::new(
    "Writes one file for the whole recording.\n\n\
     If the file grows past the size limit, about 2 GB unless the profile sets a \
     different limit, or if recording resumes after an interruption, Listener \
     continues in a new numbered file such as GPS_2.raw.",
);
const ROTATE_HOURLY_HOVER: Hover = Hover::new(
    "Starts a new file every hour, named <channel>_<date>_<hour>, for example \
     GPS_2026-06-03_08.\n\n\
     The date and hour are local time, and the name ends in .raw or .disp. The channel \
     name must be safe to use in a file name. If one hour's file grows past the size \
     limit, Listener continues in GPS_2026-06-03_08_2, then _3, and so on.",
);
const ROTATE_DAILY_HOVER: Hover = Hover::new(
    "Starts a new file every day, named <channel>_<date>, for example \
     GPS_2026-06-03.\n\n\
     The date is local time, and the name ends in .raw or .disp. The channel name \
     must be safe to use in a file name. If one day's file grows past the size \
     limit, Listener continues in GPS_2026-06-03_2, then _3, and so on.",
);

/// Help for the timestamp sidecar (§57). It names the file, the format, and the
/// resolution the timestamps actually have — a nanosecond *field* is not
/// nanosecond accuracy, and the recorded time is when the read completed, not
/// when any particular byte reached the wire.
const TIMESTAMP_SIDECAR_TOOLTIP: Hover = Hover::new(
    "Writes the arrival time of each received block to a companion file beside the \
     Raw recording.\n\n\
     The companion file has the recording's name with .idx added, so GPS.raw gets \
     GPS.raw.idx. Each line holds the byte position where a block starts in the .raw \
     file, a comma, and the wall-clock time in nanoseconds since 1 January 1970 UTC \
     (Unix time).\n\n\
     It is plain text, so any tool can read it. It is kept out of the .raw file so \
     that file stays exactly the bytes received.\n\n\
     The time is taken once for each block, when it is received. It is not per byte, \
     and it is not device or wire time, so a nanosecond field does not mean \
     nanosecond accuracy.\n\n\
     The positions always refer to the .raw file beside the companion file. Appending \
     to an existing recording continues from the end of that file instead of starting \
     over, and every .raw file, including each rotation period's file, has its own \
     companion file counting from the start of that file.",
);

/// Help for the gap line in a Display recording (§56.1). It names the line, the
/// file it goes in and the one it never goes in.
const MARK_GAPS_HOVER: Hover = Hover::new(
    "Writes one line into the Display recording where recording resumed after \
     received bytes were lost.\n\n\
     The line reads like ‹GAP the recording queue overflowed · 2026-11-13T14:02:11.250-05:00 \
     to 2026-11-13T14:02:19.730-05:00 · not recorded›. It gives the reason and the local \
     times of the first lost block and of the first block recorded again.\n\n\
     It goes only in the .disp file, just before the text recording resumed with. \
     The Raw recording never gets it, so a .raw file stays exactly the bytes \
     received.\n\n\
     Nothing is written if no bytes arrived during the gap.",
);

/// Edit the channel's **Raw** recording setup (§53): destination, overwrite, rotation,
/// timestamp sidecar. The byte-exact verbatim stream (§53) — a separate pipeline tap
/// from Display (ADR-013). The live Record toggle and the "Record on start" flag
/// (`enabled`, ADR-012) live next to the Record button, not here.
pub(crate) fn edit_raw_recording(ui: &mut egui::Ui, config: &mut ChannelConfig) {
    let rec = &mut config.raw_recording;
    recording_file_fields(
        ui,
        &mut rec.destination,
        &mut rec.overwrite_policy,
        &mut rec.file_rotation,
    );
    // Raw only: the sidecar keys wall-clock times to byte offsets in the `.raw`,
    // which a rendered `.disp` has no stable offsets for (ADR-013 keeps the two
    // taps independent, and only this one is byte-exact).
    ui.horizontal(|ui| {
        ui.checkbox(&mut rec.timestamp_enabled, "Timestamp sidecar")
            .on_hover(&TIMESTAMP_SIDECAR_TOOLTIP);
        if rec.timestamp_enabled {
            let name = rec
                .destination
                .as_ref()
                .filter(|_| rec.file_rotation == FileRotationPolicy::None)
                .and_then(|path| path.file_name())
                .map(|name| format!("{}.idx", name.to_string_lossy()))
                // With rotation the file name is minted per period (§59), so
                // naming one would be a guess; name the suffix instead.
                .unwrap_or_else(|| "<recording>.idx".to_owned());
            ui.label(egui::RichText::new(name).weak())
                .on_hover(&TIMESTAMP_SIDECAR_TOOLTIP);
        }
    });
}

/// Edit the channel's **Display** recording setup (§54): destination, overwrite,
/// rotation — the same fields as Raw (ADR-013), records the rendered view output
/// (`.disp`). The "Record on start" flag (`enabled`) lives on the block's header
/// row, like Raw's. Config-driven: applied via a §13 Reconfigure.
pub(crate) fn edit_display_recording(ui: &mut egui::Ui, config: &mut ChannelConfig) {
    let rec = &mut config.display_recording;
    recording_file_fields(
        ui,
        &mut rec.destination,
        &mut rec.overwrite_policy,
        &mut rec.file_rotation,
    );
    // Display only: a `.raw` is exactly the bytes received, so it never gets one.
    ui.checkbox(&mut rec.mark_gaps, "Mark gaps")
        .on_hover(&MARK_GAPS_HOVER);
}
