//! The window icons of Listener and Linemon (ADR-085), drawn by
//! `assets/icons/make_icons.py`. The same images are embedded in each `.exe`
//! by `build.rs`.

use std::sync::{Arc, OnceLock};

const LISTENER_PNG: &[u8] = include_bytes!("../../../assets/icons/listener.png");
const LINEMON_PNG: &[u8] = include_bytes!("../../../assets/icons/linemon.png");

/// Listener's window icon, also used by its pop-out windows. Decoded once:
/// a pop-out builds its window settings every frame.
pub(super) fn listener() -> Arc<egui::IconData> {
    static ICON: OnceLock<Arc<egui::IconData>> = OnceLock::new();
    Arc::clone(ICON.get_or_init(|| load(LISTENER_PNG)))
}

/// Linemon's window icon.
pub(super) fn linemon() -> Arc<egui::IconData> {
    static ICON: OnceLock<Arc<egui::IconData>> = OnceLock::new();
    Arc::clone(ICON.get_or_init(|| load(LINEMON_PNG)))
}

/// An image that does not decode leaves the window without an icon rather
/// than with egui's: eframe shows none for an empty `IconData`. A test decodes
/// both, so this is only a guard.
fn load(png: &[u8]) -> Arc<egui::IconData> {
    Arc::new(eframe::icon_data::from_png_bytes(png).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_window_icons_are_256_pixel_images() {
        for png in [LISTENER_PNG, LINEMON_PNG] {
            let icon = eframe::icon_data::from_png_bytes(png).expect("the icon decodes");
            assert_eq!((icon.width, icon.height), (256, 256));
        }
        assert_ne!(*listener(), *linemon());
    }
}
