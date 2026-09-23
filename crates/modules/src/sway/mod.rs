//! Sway IPC modules (`sway/mode`, `sway/workspaces`, `sway/language`).
//!
//! Each module owns a background worker that talks to sway over its IPC socket
//! and forwards plain data to the GTK main thread through
//! [`waybar_core::bridge`]. Widgets are only ever touched in the main thread.

mod language;
mod mode;
mod workspaces;
mod xkb;

pub use language::register as register_language;
pub use mode::register as register_mode;
pub use workspaces::register as register_workspaces;
