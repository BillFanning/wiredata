//! `listener` binary — the **console-subsystem** entry point: terminal CLI and
//! headless logging, with correct stdout, Ctrl-C, and shell-wait. Receive
//! byte-oriented data streams from serial and network connections (see
//! docs/listener_specification.md).
//!
//! ===========================================================================
//!  TWO-BINARY LAYOUT — MAINTENANCE INVARIANT (read before editing this file)
//! ===========================================================================
//!
//! listener ships as TWO thin entry points over ONE shared library:
//!
//! - this file → `listener.exe` (console subsystem): terminal CLI + headless
//!   logging.
//! - `src/bin/listener-gui.rs` → `listener-gui.exe` ("windows" subsystem): the
//!   double-click GUI, with no console flash.
//!
//! They are separate binaries ONLY because a Windows executable's subsystem is
//! fixed at link time: a *console* app flashes a console window on double-click,
//! and a *windows-subsystem* app can't do interactive terminal CLI (no stdout,
//! no Ctrl-C, the shell doesn't wait). So the **launch wrapper** differs — and
//! nothing else does.
//!
//! DRIFT RISK: the single way these two can rot is if **startup behavior**
//! diverges between the two `main`s (e.g. one gains a panic hook, logging tweak,
//! env setup, or single-instance guard the other lacks).
//!
//! INVARIANT THAT PREVENTS IT: keep BOTH launchers dumb. ALL real logic — and
//! any new startup step — lives in the `listener` LIBRARY crate
//! (`listener::run`, `listener::gui::run`), which both binaries call, so both
//! inherit it and cannot drift. Never add such a step inline in one `main`. If
//! you are about to add a line to either launcher, it almost certainly belongs
//! in the library instead.
//! ===========================================================================

use anyhow::Result;

fn main() -> Result<std::process::ExitCode> {
    // Keep this to a single call. CLI-vs-GUI dispatch lives in `listener::run`
    // so the GUI launcher (and any future entry point) share it, not this file.
    // (See the two-binary invariant above.)
    listener::run()
}
