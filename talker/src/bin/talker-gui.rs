//! Flash-free GUI entry point → `talker-gui.exe`.
//!
//! Built as a Windows **"windows" subsystem** app, so a double-click opens the
//! window without the OS allocating a console — no console flash. It does
//! nothing but launch the GUI. The sibling `talker` binary (src/main.rs) stays
//! a **console** app so the headless CLI behaves from a terminal (real stdout,
//! working Ctrl-C, the shell waits for it). A single binary can't be both —
//! hence two entry points, the same pattern as listener.
//!
//! ── TWO-BINARY INVARIANT ──
//! Keep this launcher DUMB. It must stay a thin wrapper that calls into the
//! shared `talker` library; any startup step (console handling, logging,
//! window options) belongs in the library (`talker::gui::run`), never inline
//! here, so this binary and `talker.exe --gui` cannot diverge.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> anyhow::Result<()> {
    // One call only — shared GUI startup lives in `gui::run`. The GUI opens
    // to the last-used profile via eframe storage, same as `talker --gui`
    // without a profile flag.
    talker::gui::run(None, None)
}
