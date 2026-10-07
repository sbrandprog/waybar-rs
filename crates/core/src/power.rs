//! Suspend/resume notifications from logind.
//!
//! GLib timeouts use CLOCK_MONOTONIC, which stops during suspend, so a timer
//! armed before sleeping fires late after wake-up. Timed modules subscribe here
//! and resync when logind reports the machine is resuming.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::gio;
use gtk::glib;
use tracing::{debug, warn};

type Callback = Rc<dyn Fn()>;

/// Keep-alive handle for a callback registered with [`on_resume`]. Dropping it
/// unregisters the callback so a removed module is never called again.
#[must_use = "dropping the handle immediately unregisters the callback"]
pub struct ResumeHandle {
    slot: usize,
}

impl Drop for ResumeHandle {
    fn drop(&mut self) {
        REGISTRY.with_borrow_mut(|reg| reg.remove(self.slot));
    }
}

/// Register `callback` to run every time the machine resumes from suspend.
pub fn on_resume<F: Fn() + 'static>(callback: F) -> ResumeHandle {
    let slot = REGISTRY.with_borrow_mut(|reg| reg.add(Rc::new(callback)));
    connect();
    ResumeHandle { slot }
}

#[derive(Default)]
struct Registry {
    callbacks: Vec<Option<Callback>>,
    subscription: Option<gio::SignalSubscription>,
    connecting: bool,
}

impl Registry {
    fn add(&mut self, callback: Callback) -> usize {
        self.callbacks.push(Some(callback));
        self.callbacks.len() - 1
    }

    fn remove(&mut self, slot: usize) {
        if let Some(entry) = self.callbacks.get_mut(slot) {
            *entry = None;
        }
    }

    /// Snapshot live callbacks so a callback that registers/unregisters while
    /// dispatching cannot invalidate the iteration.
    fn live(&self) -> Vec<Callback> {
        self.callbacks.iter().filter_map(Clone::clone).collect()
    }
}

thread_local! {
    static REGISTRY: RefCell<Registry> = RefCell::new(Registry::default());
}

fn connect() {
    let should_connect = REGISTRY.with_borrow_mut(|reg| {
        if reg.subscription.is_some() || reg.connecting {
            false
        } else {
            reg.connecting = true;
            true
        }
    });
    if !should_connect {
        return;
    }

    gio::bus_get(gio::BusType::System, gio::Cancellable::NONE, |result| {
        let connection = match result {
            Ok(connection) => connection,
            Err(err) => {
                warn!(
                    "power: cannot connect to the system bus: {err}; \
                     suspend/resume notifications disabled"
                );
                REGISTRY.with_borrow_mut(|reg| reg.connecting = false);
                return;
            }
        };
        let subscription = connection.subscribe_to_signal(
            Some("org.freedesktop.login1"),
            Some("org.freedesktop.login1.Manager"),
            Some("PrepareForSleep"),
            Some("/org/freedesktop/login1"),
            None,
            gio::DBusSignalFlags::NONE,
            |signal| {
                if is_resume(signal.parameters) {
                    dispatch();
                }
            },
        );
        REGISTRY.with_borrow_mut(|reg| reg.subscription = Some(subscription));
        debug!("power: subscribed to logind PrepareForSleep");
    });
}

fn dispatch() {
    let callbacks = REGISTRY.with_borrow(Registry::live);
    for callback in callbacks {
        callback();
    }
}

/// `PrepareForSleep` carries a single boolean: `true` when entering sleep,
/// `false` when waking up. Only the latter is a resume.
fn is_resume(parameters: &glib::Variant) -> bool {
    parameters.get::<(bool,)>() == Some((false,))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::glib::variant::ToVariant;
    use std::cell::Cell;

    #[test]
    fn resume_signal_is_recognized() {
        assert!(is_resume(&(false,).to_variant()));
        assert!(!is_resume(&(true,).to_variant()));
        assert!(!is_resume(&"nope".to_variant()));
    }

    #[test]
    fn callbacks_run_until_unregistered() {
        let hits = Rc::new(Cell::new(0));
        let mut registry = Registry::default();
        let first = registry.add(Rc::new({
            let hits = hits.clone();
            move || hits.set(hits.get() + 1)
        }));
        let _keep = registry.add(Rc::new(|| {}));

        for callback in registry.live() {
            callback();
        }
        assert_eq!(hits.get(), 1);

        registry.remove(first);
        for callback in registry.live() {
            callback();
        }
        assert_eq!(hits.get(), 1);
    }
}
