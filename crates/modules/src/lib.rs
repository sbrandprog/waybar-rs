//! Bar module implementations.
//!
//! Each module is registered into the [`waybar_core::Registry`] behind a Cargo
//! feature flag (the analogue of waybar's `#ifdef HAVE_*`).

mod dummy;
mod label;

#[cfg(feature = "clock")]
mod clock;

#[cfg(feature = "cpu")]
mod cpu;

#[cfg(feature = "memory")]
mod memory;

#[cfg(feature = "battery")]
mod battery;

#[cfg(feature = "backlight")]
mod backlight;

#[cfg(feature = "sway")]
mod sway;

#[cfg(feature = "keyboard")]
mod keyboard_state;

#[cfg(feature = "custom")]
mod custom;

#[cfg(any(
    feature = "cpu",
    feature = "memory",
    feature = "battery",
    feature = "backlight"
))]
mod sysinfo;

pub use label::LabelModule;

use waybar_core::Registry;

/// Register every module compiled into this build.
pub fn register(reg: &mut Registry) {
    dummy::register(reg);
    #[cfg(feature = "clock")]
    clock::register(reg);
    #[cfg(feature = "cpu")]
    cpu::register(reg);
    #[cfg(feature = "memory")]
    memory::register(reg);
    #[cfg(feature = "battery")]
    battery::register(reg);
    #[cfg(feature = "backlight")]
    backlight::register(reg);
    #[cfg(feature = "sway")]
    {
        sway::register_mode(reg);
        sway::register_workspaces(reg);
        sway::register_language(reg);
    }
    #[cfg(feature = "keyboard")]
    keyboard_state::register(reg);
    #[cfg(feature = "custom")]
    custom::register(reg);
}
