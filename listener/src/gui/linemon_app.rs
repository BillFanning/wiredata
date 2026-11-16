//! Linemon as a program of its own (listener ADR-070, spec §50.3).
//!
//! The program holds only Linemon's window. It starts the same runtime and
//! driver Listener does, with no Channels of its own, so Linemon's hidden
//! Channel works as it does anywhere. It keeps its settings in its own eframe
//! storage, apart from Listener's, and writes no event log.

use std::sync::Arc;

use anyhow::anyhow;
use wiredata_ui::repaint::RepaintCoalescer;

use super::bind_scope::LocalAddresses;
use super::bridge::{self, BridgeHandle, UiCommand, UiUpdate};
use super::linemon_window::{Linemon, DEFAULT_SIZE, LINEMON_KEY, MIN_SIZE};

/// Launch Linemon. Owns the eframe event loop on the calling (main) thread;
/// the runtime runs on its own thread, as Listener's does.
pub fn run() -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(format!("Linemon v{}", env!("CARGO_PKG_VERSION")))
            .with_inner_size(DEFAULT_SIZE)
            .with_min_inner_size(MIN_SIZE),
        // As Listener: a restored geometry is applied after the window shows,
        // which makes it jump.
        persist_window: false,
        ..Default::default()
    };
    eframe::run_native(
        // eframe keys the storage location on this name, so Linemon's settings
        // are kept apart from Listener's.
        "Linemon",
        options,
        Box::new(|cc| {
            super::apply_style(&cc.egui_ctx);
            let repaint = RepaintCoalescer::for_ctx(cc.egui_ctx.clone());
            let notify = Arc::clone(&repaint);
            let bridge = bridge::spawn(move || notify.notify()).map_err(
                |e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("failed to start the runtime: {e}").into()
                },
            )?;
            Ok(Box::new(LinemonApp::new(bridge, repaint, cc.storage)))
        }),
    )
    .map_err(|e| anyhow!("{e}"))
}

/// The program's state: the runtime handle and the window.
struct LinemonApp {
    bridge: BridgeHandle,
    repaint: Arc<RepaintCoalescer>,
    linemon: Linemon,
    /// This computer's addresses, for the UDP binding choice.
    local_addresses: LocalAddresses,
}

impl LinemonApp {
    fn new(
        bridge: BridgeHandle,
        repaint: Arc<RepaintCoalescer>,
        storage: Option<&dyn eframe::Storage>,
    ) -> Self {
        let mut linemon = Linemon::new(storage.and_then(|s| s.get_string(LINEMON_KEY)).as_deref());
        linemon.start_up();
        let mut app = Self {
            bridge,
            repaint,
            linemon,
            local_addresses: LocalAddresses::default(),
        };
        app.send(UiCommand::ListLocalAddresses);
        app
    }

    /// Send a command to the driver. Non-blocking: a full command channel drops
    /// it rather than stalling the window. A dropped request is not sent again
    /// on its own; the status line says so, and Start sends it again.
    fn send(&mut self, command: UiCommand) {
        if self.bridge.commands.try_send(command).is_err() {
            self.linemon.request_not_delivered();
        }
    }

    /// Fold what the runtime has sent. Only Linemon's updates and the address
    /// list concern this program.
    fn drain_updates(&mut self) {
        while let Ok(update) = self.bridge.updates.try_recv() {
            match update {
                UiUpdate::Linemon(update) => self.linemon.handle(update),
                UiUpdate::LocalAddresses(Ok(listed)) => {
                    self.local_addresses = LocalAddresses::Listed(listed);
                }
                UiUpdate::LocalAddresses(Err(why)) => {
                    self.local_addresses = LocalAddresses::Failed(why);
                }
                _ => {}
            }
        }
    }
}

impl eframe::App for LinemonApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string(LINEMON_KEY, self.linemon.to_stored());
    }

    /// Closing Linemon closes its port: the driver stops its hidden Channel on
    /// the way out.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.bridge.shutdown_and_join();
    }

    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Re-arm the coalescer before draining, as Listener does.
        self.repaint.frame_started();
        self.drain_updates();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let mut commands = Vec::new();
        self.linemon.show(ui, &self.local_addresses, &mut commands);
        for command in commands {
            self.send(command);
        }
    }
}
