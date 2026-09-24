//! Periodic update helpers built on the GLib main loop.
//!
//! Cheap `/proc` and `/sys` reads are fine on the main loop, so the simple
//! informers use GLib timers. Genuinely blocking or async work (D-Bus,
//! PipeWire, child processes) uses the tokio runtime instead.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

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

/// Milliseconds from `now_ms` to the next multiple of `interval_ms`.
///
/// A clock that must land on whole seconds (or minutes) cannot use
/// `timeout_add_seconds_local`: GLib measures that interval in *monotonic* time,
/// so the callback fires at an arbitrary phase of the wall clock and can show a
/// stale second. Aligning to epoch multiples keeps every tick on the boundary.
/// When `now_ms` is already on a boundary the full interval is returned so the
/// timer never fires immediately.
pub(crate) fn next_delay_ms(now_ms: u64, interval_ms: u64) -> u64 {
    let interval_ms = interval_ms.max(1);
    let rem = now_ms % interval_ms;
    if rem == 0 {
        interval_ms
    } else {
        interval_ms - rem
    }
}

/// Smallest re-arm delay. Without a floor, a tick that fires a hair before the
/// boundary re-arms with a ~1 ms timeout and can spin for a few iterations.
const MIN_DELAY_MS: u64 = 10;

/// The delay to the next aligned tick, never below [`MIN_DELAY_MS`].
fn aligned_delay(now_ms: u64, interval_ms: u64) -> u64 {
    next_delay_ms(now_ms, interval_ms).max(MIN_DELAY_MS)
}

/// A running aligned timer; dropping it stops the timer.
pub struct TimerHandle {
    state: Rc<RefCell<TimerState>>,
}

struct TimerState {
    source: Option<glib::SourceId>,
    cancelled: bool,
}

impl Drop for TimerHandle {
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        state.cancelled = true;
        if let Some(id) = state.source.take() {
            id.remove();
        }
    }
}

/// Call `f` at every multiple of `interval` seconds of the wall clock.
///
/// Unlike [`every_seconds`], `f` is *not* called immediately — the caller is
/// expected to have rendered once already — and the timer re-arms itself after
/// every tick so it stays on the wall-clock boundary even across NTP steps.
/// Dropping the returned [`TimerHandle`] stops the timer.
pub fn schedule_aligned<F>(interval: u64, f: F) -> TimerHandle
where
    F: FnMut() + 'static,
{
    let interval = interval.max(1);
    let state = Rc::new(RefCell::new(TimerState {
        source: None,
        cancelled: false,
    }));
    let callback = Rc::new(RefCell::new(f));
    schedule_next(interval, state.clone(), callback);
    TimerHandle { state }
}

fn schedule_next<F>(interval: u64, state: Rc<RefCell<TimerState>>, callback: Rc<RefCell<F>>)
where
    F: FnMut() + 'static,
{
    if state.borrow().cancelled {
        return;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let delay = aligned_delay(now_ms, interval.saturating_mul(1000));

    let tick_state = state.clone();
    let id = glib::timeout_add_local_once(Duration::from_millis(delay), move || {
        tick_state.borrow_mut().source = None;
        if tick_state.borrow().cancelled {
            return;
        }
        (callback.borrow_mut())();
        schedule_next(interval, tick_state, callback);
    });
    state.borrow_mut().source = Some(id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_lands_on_the_next_boundary() {
        assert_eq!(next_delay_ms(0, 1000), 1000);
        assert_eq!(next_delay_ms(1, 1000), 999);
        assert_eq!(next_delay_ms(999, 1000), 1);
        // 60 s interval: align to the minute, not the second.
        assert_eq!(next_delay_ms(61_000, 60_000), 59_000);
        assert_eq!(next_delay_ms(119_999, 60_000), 1);
        assert_eq!(next_delay_ms(120_000, 60_000), 60_000);
    }

    #[test]
    fn zero_interval_does_not_divide_by_zero() {
        assert_eq!(next_delay_ms(5, 0), 1);
    }

    #[test]
    fn aligned_delay_has_a_floor() {
        // Just before the boundary would otherwise re-arm with 1 ms.
        assert_eq!(aligned_delay(999, 1000), MIN_DELAY_MS);
        // On a boundary the full interval is used, well above the floor.
        assert_eq!(aligned_delay(0, 1000), 1000);
    }
}
