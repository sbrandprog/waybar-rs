//! Hotplug notifications from libudev.
//!
//! `udev::MonitorSocket` and `udev::Event` are `!Send`, so the socket cannot be
//! driven from the shared tokio runtime. Each subscription instead owns a
//! dedicated thread that creates the monitor, waits on the socket with
//! `poll(2)` and forwards a `Send` snapshot of every matching uevent over an
//! [`async_channel`]. The receiver side hands values to the GTK main thread
//! with [`crate::bridge::on_main`].
//!
//! The monitor is only an *accelerator*: uevents do not carry the attribute
//! values, and a driver may not emit one for every change. Callers keep their
//! periodic timer as the source of truth and use this to react sooner.

use std::ffi::OsStr;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use tracing::debug;

/// How long a single `poll(2)` waits before the thread re-checks the stop flag.
const POLL_TIMEOUT_MS: libc::c_int = 500;

/// A `Send` snapshot of one udev uevent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UdevEvent {
    pub subsystem: String,
    pub sysname: String,
    /// `add`, `remove` or `change`.
    pub action: String,
    pub devtype: Option<String>,
}

/// A live udev subscription. Dropping it stops the monitor thread.
pub struct Subscription {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    rx: async_channel::Receiver<UdevEvent>,
}

impl Subscription {
    /// A receiver for every matching uevent, delivered from the monitor thread.
    pub fn receiver(&self) -> async_channel::Receiver<UdevEvent> {
        self.rx.clone()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Watch `subsystems` (`power_supply`, `backlight`, ...) for uevents.
///
/// Returns `None` when the monitor thread cannot be spawned. Failure to create
/// the monitor itself (no udev daemon, a restricted container, ...) is handled
/// inside the thread: it logs at debug level and the receiver simply yields
/// nothing. Callers must keep working off their periodic timer either way.
pub fn watch(subsystems: &[&str]) -> Option<Subscription> {
    let subsystems: Vec<String> = subsystems.iter().map(|s| s.to_string()).collect();
    let (tx, rx) = async_channel::unbounded();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();

    // `MonitorSocket` is `!Send` (it owns a raw libudev pointer), so the socket
    // is created and kept inside the thread rather than moved into it.
    let handle = std::thread::Builder::new()
        .name("waybar-udev".to_string())
        .spawn(move || {
            let Some(socket) = listen(&subsystems) else {
                return;
            };
            // libudev monitors are created non-blocking, so `iter()` never
            // blocks once `poll` has reported readability.
            let mut poll_fd = libc::pollfd {
                fd: socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            while !thread_stop.load(Ordering::Relaxed) {
                let ready = unsafe { libc::poll(&mut poll_fd, 1, POLL_TIMEOUT_MS) };
                if ready < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    debug!("udev: poll failed: {err}");
                    return;
                }
                if ready == 0 {
                    continue;
                }
                for event in socket.iter() {
                    if tx.send_blocking(snapshot(&event)).is_err() {
                        // Every receiver is gone; stop the monitor.
                        return;
                    }
                }
            }
        })
        .ok()?;

    Some(Subscription {
        stop,
        handle: Some(handle),
        rx,
    })
}

/// Create a libudev monitor for `subsystems`, logging and returning `None` on
/// failure. Runs on the monitor thread because the socket is `!Send`.
fn listen(subsystems: &[String]) -> Option<udev::MonitorSocket> {
    let mut builder = match udev::MonitorBuilder::new() {
        Ok(builder) => builder,
        Err(err) => {
            debug!("udev: cannot create monitor: {err}");
            return None;
        }
    };
    for subsystem in subsystems {
        builder = match builder.match_subsystem(subsystem) {
            Ok(builder) => builder,
            Err(err) => {
                debug!("udev: cannot filter subsystem `{subsystem}`: {err}");
                return None;
            }
        };
    }
    match builder.listen() {
        Ok(socket) => Some(socket),
        Err(err) => {
            debug!("udev: cannot listen for events: {err}");
            None
        }
    }
}

/// Extract the `Send` fields of an event, dropping the borrowed `Device`.
fn snapshot(event: &udev::Event) -> UdevEvent {
    convert(
        event.subsystem(),
        event.sysname(),
        event.action(),
        event.devtype(),
    )
}

/// The pure part of [`snapshot`], testable without a live udev monitor.
fn convert(
    subsystem: Option<&OsStr>,
    sysname: &OsStr,
    action: Option<&OsStr>,
    devtype: Option<&OsStr>,
) -> UdevEvent {
    UdevEvent {
        subsystem: os_string(subsystem),
        sysname: sysname.to_string_lossy().into_owned(),
        action: os_string(action),
        devtype: devtype.map(|value| value.to_string_lossy().into_owned()),
    }
}

fn os_string(value: Option<&OsStr>) -> String {
    value
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_event_fields() {
        let event = convert(
            Some(OsStr::new("power_supply")),
            OsStr::new("BAT0"),
            Some(OsStr::new("change")),
            None,
        );
        assert_eq!(event.subsystem, "power_supply");
        assert_eq!(event.sysname, "BAT0");
        assert_eq!(event.action, "change");
        assert_eq!(event.devtype, None);
    }

    #[test]
    fn missing_fields_become_empty() {
        let event = convert(
            None,
            OsStr::new("intel_backlight"),
            None,
            Some(OsStr::new("x")),
        );
        assert!(event.subsystem.is_empty());
        assert!(event.action.is_empty());
        assert_eq!(event.devtype.as_deref(), Some("x"));
    }
}
