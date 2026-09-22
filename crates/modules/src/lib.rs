//! Bar module implementations.
//!
//! Each module is registered into the [`waybar_core::Registry`] behind a Cargo
//! feature flag (the analogue of waybar's `#ifdef HAVE_*`).

mod dummy;
mod label;

#[cfg(feature = "clock")]
mod clock;

pub use label::LabelModule;

use waybar_core::Registry;

/// Register every module compiled into this build.
pub fn register(reg: &mut Registry) {
    dummy::register(reg);
    #[cfg(feature = "clock")]
    clock::register(reg);
}
