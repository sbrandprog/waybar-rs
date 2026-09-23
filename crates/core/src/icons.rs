//! Bundled icon search paths.
//!
//! Some modules (`privacy`) use symbolic icon names that upstream Waybar ships
//! inside its binary. We keep the same SVGs in `resources/icons/` and add that
//! directory to the GTK icon theme so the default icon names resolve.

use std::path::PathBuf;

use tracing::debug;

/// Add every existing bundled icon directory to the default icon theme.
///
/// The first existing candidate wins; the directory is also honoured via the
/// `WAYBAR_RS_ICONS` environment variable.
pub fn install() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let theme = gtk::IconTheme::for_display(&display);
    for dir in candidates() {
        if dir.is_dir() {
            debug!("icons: added search path {}", dir.display());
            theme.add_search_path(&dir);
            return;
        }
    }
}

fn candidates() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(dir) = std::env::var("WAYBAR_RS_ICONS")
        && !dir.is_empty()
    {
        dirs.push(PathBuf::from(dir));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        dirs.push(parent.join("icons"));
        dirs.push(parent.join("../share/waybar-rs/icons"));
    }
    // Development build: the workspace's `resources/icons`.
    dirs.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../resources/icons"));
    dirs
}
