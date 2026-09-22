//! Pure parsers for the Linux system interfaces used by the informer modules.
//!
//! Everything here takes file contents as input so it can be unit-tested
//! without touching the live system.

pub mod cpu;
pub mod memory;
