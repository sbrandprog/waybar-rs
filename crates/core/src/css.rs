//! Loading of the user stylesheet.

use std::path::Path;

use tracing::{info, warn};

/// Load a CSS file into the default display.
///
/// A missing file is not an error (waybar happily runs unstyled). The provider
/// also supports `@import`, so user stylesheets that pull in extra files work.
pub fn load(path: &Path) {
    if !path.exists() {
        warn!("stylesheet not found: {}", path.display());
        return;
    }

    let provider = gtk::CssProvider::new();
    provider.load_from_path(path);

    match gtk::gdk::Display::default() {
        Some(display) => {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_USER,
            );
            info!("loaded stylesheet: {}", path.display());
        }
        None => warn!("no display available, stylesheet not applied"),
    }
}
