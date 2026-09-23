//! Pure parsers for the Linux system interfaces used by the informer modules.
//!
//! Everything here takes file contents as input so it can be unit-tested
//! without touching the live system.

#[cfg(feature = "backlight")]
pub mod backlight;
#[cfg(feature = "battery")]
pub mod battery;
#[cfg(feature = "cpu")]
pub mod cpu;
#[cfg(feature = "memory")]
pub mod memory;
