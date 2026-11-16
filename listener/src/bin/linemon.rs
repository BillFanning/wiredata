//! Linemon's entry point → `linemon.exe` (listener spec §50.3).
//!
//! A Windows "windows" subsystem app, like `listener-gui`, so a double-click
//! opens the window with no console flash. Keep it a thin wrapper: everything
//! Linemon does lives in the `listener` library.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> anyhow::Result<()> {
    listener::gui::run_linemon()
}
