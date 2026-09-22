//! Periodic update helper built on the GLib main loop.
//!
//! Cheap `/proc` and `/sys` reads are fine on the main loop, so the simple
//! informers use GLib timers. Genuinely blocking or async work (D-Bus,
//! PipeWire, child processes) will use the tokio runtime introduced in a later
//! stage instead.

use gtk::glib;

/// Call `f` once immediately, then every `interval` seconds.
///
/// The returned source id can be used to stop the timer; ignoring it keeps the
/// timer running for the lifetime of the process.
pub fn every_seconds<F>(interval: u64, mut f: F) -> glib::SourceId
where
    F: FnMut() + 'static,
{
    f();
    let seconds = interval.clamp(1, u32::MAX as u64) as u32;
    glib::timeout_add_seconds_local(seconds, move || {
        f();
        glib::ControlFlow::Continue
    })
}
