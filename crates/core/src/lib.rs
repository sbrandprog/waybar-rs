//! Core of waybar-rs: config parsing, the layer-shell bar window, the module
//! registry and shared helpers (format engine, async bridge, click actions,
//! CSS loading).
//!
//! The module implementations live in the `waybar-modules` crate; this crate
//! only defines the contracts and the UI shell.

pub mod action;
pub mod bar;
pub mod bridge;
pub mod config;
pub mod css;
pub mod format;
pub mod module;
pub mod widgets;

pub use bar::Bar;
pub use config::{BarConfig, Config, OutputFilter, Position};
pub use module::{Module, ModuleFactory, ModuleInit, Registry};
