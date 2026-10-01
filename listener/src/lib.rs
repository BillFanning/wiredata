pub mod cli;
pub mod config;
pub mod core;
pub mod diagnostics;
pub mod display;
pub mod gui;
pub mod record;
pub mod retention;
pub mod runtime;
pub mod transport;

use anyhow::Result;

/// Binary entry point (§3): parse arguments, then dispatch to the graphical
/// interface or the headless CLI runner. Headless is the default whenever a source
/// is given (or `--cli`); a bare invocation (no source, no flag) opens the GUI, so
/// a double-clicked executable shows a window. The branch happens before the async
/// runtime is built, since the GUI owns its own runtime (ADR-008).
///
/// **Shared funnel (two-binary invariant — see `src/main.rs`).** The `listener.exe`
/// launcher calls this; the flash-free `listener-gui.exe` calls [`gui::run`]
/// directly. Process-wide startup that *both* binaries need belongs HERE (or in
/// [`gui::run`] for GUI-only startup), never inline in a `main`, so the two thin
/// launchers cannot drift.
pub fn run() -> Result<std::process::ExitCode> {
    let cli = cli::parse();
    if cli.wants_gui() {
        match gui::run() {
            Ok(()) => Ok(std::process::ExitCode::SUCCESS),
            // A bare launch on a headless box can't open a window — guide toward
            // headless mode instead of surfacing a cryptic windowing error.
            Err(e) if cli.is_bare_launch() => Err(anyhow::anyhow!(
                "could not open the graphical interface ({e}); for headless use pass \
                 a source (--udp/--tcp/--serial/--profile), or run with --cli"
            )),
            Err(e) => Err(e),
        }
    } else {
        cli::run(cli)
    }
}

/// Loopback-port reservation shared by the crate's unit tests (`gui::bridge`,
/// `runtime::listener`, …), which all run in one lib-test process and so must
/// share a single cursor to avoid colliding with each other.
#[cfg(test)]
pub(crate) mod test_ports {
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Reserve a loopback port for a channel to bind, without the classic
    /// bind-drop-rebind race.
    ///
    /// The naive helper let the OS pick an ephemeral port, read its number,
    /// dropped the socket, and returned the bare number — leaving a window in
    /// which a sibling test (the OS reuses a just-freed ephemeral port) grabbed
    /// the same number and won the rebind, failing the test with `AddrInUse`.
    /// Instead we walk a private cursor so concurrent in-process callers never
    /// pick the same candidate, seed it from the pid so separate test binaries
    /// don't march in lockstep, and verify each candidate is bindable before
    /// handing it out (skipping any the box already holds). A window against an
    /// *external* binder remains — there is no socket-handoff API — but the
    /// in-process collision the flake actually hit is gone.
    fn reserve(bindable: impl Fn(u16) -> bool) -> u16 {
        const BASE: u32 = 45_000;
        const SPAN: u32 = 20_000; // 45000..=64999
        static CURSOR: AtomicU32 = AtomicU32::new(0);
        let seed = std::process::id() % SPAN;
        for _ in 0..SPAN {
            let n = CURSOR.fetch_add(1, Ordering::Relaxed);
            let port = (BASE + (seed + n) % SPAN) as u16;
            if bindable(port) {
                return port;
            }
        }
        panic!("no free loopback port found for the test");
    }

    pub(crate) fn reserve_udp_port() -> u16 {
        reserve(|p| std::net::UdpSocket::bind(("127.0.0.1", p)).is_ok())
    }
}
