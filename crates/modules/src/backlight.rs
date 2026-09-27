//! Backlight module (`/sys/class/backlight`).
//!
//! Brightness is read from sysfs, but changed through `systemd-logind`'s
//! `Session.SetBrightness` D-Bus method. logind writes the sysfs attribute as
//! root, so the module needs neither a udev rule nor write access to `/sys`.
//! This requires an active local session (systemd or elogind); over SSH or in a
//! headless environment the display still works but scrolling cannot change the
//! brightness. Scroll steps are applied by a single worker that reads the
//! current brightness and writes the next value one command at a time, so a
//! burst of events advances step by step instead of racing itself.
//!
//! The label is refreshed on the configured interval and, in addition, on
//! `backlight` uevents (see [`waybar_core::udev`]): an external brightness
//! change (hotkeys, `brightnessctl`) refreshes immediately, so a held key steps
//! smoothly, while a hotplug burst (`add`/`remove`) is coalesced and triggers a
//! full rescan. The periodic timer stays the source of truth: a missed uevent
//! is corrected within one interval.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use gtk::prelude::*;
use tracing::{debug, warn};

use waybar_core::bridge;
use waybar_core::format::{Args, FmtArg};
use waybar_core::widgets::{ScrollAxis, set_scroll_cursor};
use waybar_core::{Module, ModuleInit, Registry, timer, udev};

use crate::label::LabelModule;
use crate::sysinfo::backlight::{
    BacklightDevice, best_device, enumerate, percent, percent_exp, read_device, step_value,
};

const BACKLIGHT_DIR: &str = "/sys/class/backlight";

/// A relative brightness change requested from the GTK thread.
///
/// The worker owns the read-modify-write: it resolves the device, reads the
/// current `brightness`, computes the next value and writes it, all before it
/// handles the next command. The GTK thread never reads sysfs for a step, so
/// rapid scroll events cannot all compute from the same stale value and
/// collapse into one step when they arrive faster than a logind write.
#[derive(Clone, Debug)]
enum Cmd {
    Step {
        preferred: String,
        step: f64,
        min: f64,
        increase: bool,
    },
}

struct Backlight {
    inner: LabelModule,
    cmd_tx: async_channel::Sender<Cmd>,
    cmd_rx: RefCell<Option<async_channel::Receiver<Cmd>>>,
    /// The `backlight` monitor; dropping it stops the udev thread.
    subscription: RefCell<Option<udev::Subscription>>,
}

impl Module for Backlight {
    fn widget(&self) -> gtk::Widget {
        self.inner.widget()
    }

    fn start(&self) {
        let inner = self.inner.clone();
        let config = inner.config().clone();
        // The chosen device is cached so a brightness edit re-reads only its
        // attributes instead of rescanning `/sys/class/backlight`. The interval
        // tick and hotplug use the full path, which refreshes the cache.
        let cache: Rc<RefCell<Option<BacklightDevice>>> = Rc::new(RefCell::new(None));

        // Writes go through a worker; it reports back after a change so the
        // label refreshes immediately instead of waiting for the next tick.
        if let Some(cmd_rx) = self.cmd_rx.borrow_mut().take() {
            let (refresh_tx, refresh_rx) = bridge::channel::<()>();
            bridge::spawn(worker(cmd_rx, refresh_tx));
            let refresh_inner = inner.clone();
            let refresh_cache = cache.clone();
            bridge::on_main(refresh_rx, move |_| {
                refresh_fast(&refresh_inner, &refresh_cache)
            });
        }

        // Only drive brightness ourselves when the user has not bound scrolling
        // to a command.
        let has_scroll_command = [
            "on-scroll-up",
            "on-scroll-down",
            "on-scroll-left",
            "on-scroll-right",
        ]
        .iter()
        .any(|key| config.get(*key).and_then(|v| v.as_str()).is_some());

        if !has_scroll_command {
            let preferred = inner.option_str("device", "");
            let step = inner.option_f64("scroll-step", 1.0);
            let min = inner.option_f64("min-brightness", 0.0);
            let reverse = inner.option_bool("reverse-scrolling", false);
            let cmd_tx = self.cmd_tx.clone();

            let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
            scroll.connect_scroll(move |_, _dx, dy| {
                // Waybar maps up to "increase" and down to "decrease";
                // `reverse-scrolling` flips the axis.
                let increase = if dy < 0.0 {
                    !reverse
                } else if dy > 0.0 {
                    reverse
                } else {
                    return gtk::glib::Propagation::Proceed;
                };
                // The worker reads the current value itself, so a burst of
                // events is applied step by step instead of collapsing.
                let _ = cmd_tx.try_send(Cmd::Step {
                    preferred: preferred.clone(),
                    step,
                    min,
                    increase,
                });
                gtk::glib::Propagation::Proceed
            });
            inner.widget().add_controller(scroll);
            set_scroll_cursor(&inner.widget(), ScrollAxis::Vertical);
        }

        let fast: Rc<dyn Fn()> = Rc::new({
            let inner = inner.clone();
            let cache = cache.clone();
            move || refresh_fast(&inner, &cache)
        });
        let full: Rc<dyn Fn()> = Rc::new({
            let inner = inner.clone();
            let cache = cache.clone();
            move || refresh_full(&inner, &cache)
        });

        // udev is an accelerator. A brightness edit (`change`) refreshes right
        // away so a held hotkey steps smoothly; a hotplug burst is coalesced and
        // triggers a full rescan, since one hotplug can produce several events.
        if let Some(subscription) = udev::watch(&["backlight"]) {
            let hotplug = timer::debounce(150, {
                let full = full.clone();
                move || full()
            });
            bridge::on_main(subscription.receiver(), move |event| {
                if event.action == "change" {
                    fast();
                } else {
                    hotplug();
                }
            });
            *self.subscription.borrow_mut() = Some(subscription);
        }

        match inner.interval_secs(2) {
            Some(secs) => {
                let full = full.clone();
                timer::every_seconds(secs, move || full());
            }
            None => full(),
        }
    }
}

/// Refresh from the cached device, falling back to a full enumeration when it
/// has disappeared. This is the per-`change` fast path: it re-reads only one
/// device's attributes instead of rescanning `/sys/class/backlight`.
fn refresh_fast(inner: &LabelModule, cache: &RefCell<Option<BacklightDevice>>) {
    let current = {
        let cached = cache.borrow();
        cached.as_ref().and_then(|device| {
            read_device(Path::new(BACKLIGHT_DIR), &device.name, &device.subsystem)
        })
    };
    match current {
        Some(device) => {
            render(inner, &device);
            *cache.borrow_mut() = Some(device);
        }
        None => refresh_full(inner, cache),
    }
}

/// Rescan every device and cache the chosen one. Used on the interval tick and
/// after hotplug, so a cache left stale by a missed uevent is corrected.
fn refresh_full(inner: &LabelModule, cache: &RefCell<Option<BacklightDevice>>) {
    let preferred = inner.option_str("device", "");
    let devices = enumerate(Path::new(BACKLIGHT_DIR));
    let device = best_device(&devices, &preferred).cloned();
    match &device {
        Some(device) => render(inner, device),
        None => inner.set_visible(false),
    }
    *cache.borrow_mut() = device;
}

fn render(inner: &LabelModule, device: &BacklightDevice) {
    if !device.powered {
        inner.set_visible(false);
        return;
    }

    let pct = percent(device.actual, device.max);
    let pct_exp = percent_exp(device.actual, device.max);
    let state = inner.get_state(pct.min(u8::MAX as u16) as u8, false);
    let format = inner.resolve_format("{percent}%", &state);
    if format.is_empty() {
        inner.set_visible(false);
        return;
    }
    inner.set_visible(true);

    let mut args = Args::new();
    args.insert("percent".into(), FmtArg::Int(i64::from(pct)));
    args.insert("percent_exp".into(), FmtArg::Int(i64::from(pct_exp)));
    args.insert(
        "icon".into(),
        FmtArg::from(inner.get_icon(pct, &[state.as_str()], 0)),
    );
    args.insert(
        "icon_exp".into(),
        FmtArg::from(inner.get_icon(pct_exp, &[state.as_str()], 0)),
    );

    inner.set_markup(&inner.render(&format, &args));

    let tooltip_format = inner.resolve_tooltip_format(&format, &state);
    inner.set_tooltip(&inner.render(&tooltip_format, &args));
}

/// Resolve the device to act on, re-reading the cached one when it is still
/// present and falling back to a full enumeration otherwise.
///
/// Runs on the worker thread, so the blocking sysfs reads never touch the GTK
/// main loop. The cache is a plain local variable: the worker is the only writer.
fn resolve_device(preferred: &str, cache: &mut Option<BacklightDevice>) -> Option<BacklightDevice> {
    if let Some(cached) = cache.as_ref()
        && let Some(device) = read_device(Path::new(BACKLIGHT_DIR), &cached.name, &cached.subsystem)
    {
        *cache = Some(device.clone());
        return Some(device);
    }
    let devices = enumerate(Path::new(BACKLIGHT_DIR));
    let device = best_device(&devices, preferred)?.clone();
    *cache = Some(device.clone());
    Some(device)
}

/// Apply brightness changes through `systemd-logind`.
///
/// Each `Step` is a full read-modify-write: the current `brightness` is read
/// after the previous write has returned, so consecutive commands advance by
/// exactly one step instead of racing each other.
async fn worker(cmd_rx: async_channel::Receiver<Cmd>, refresh_tx: async_channel::Sender<()>) {
    let mut session: Option<LoginSessionProxy<'static>> = None;
    let mut cache: Option<BacklightDevice> = None;

    while let Ok(Cmd::Step {
        preferred,
        step,
        min,
        increase,
    }) = cmd_rx.recv().await
    {
        // Read the value only once the previous `SetBrightness` has returned.
        let Some(device) = resolve_device(&preferred, &mut cache) else {
            continue;
        };
        let Some(value) = step_value(device.requested, device.max, step, min, increase) else {
            continue;
        };
        let value = match u32::try_from(value) {
            Ok(value) => value,
            Err(_) => {
                warn!("backlight: brightness value {value} does not fit in u32");
                continue;
            }
        };

        // Connect lazily: sysfs reads never touch D-Bus, and a machine where the
        // bar is never scrolled pays nothing.
        if session.is_none() {
            match connect_session().await {
                Ok(proxy) => session = Some(proxy),
                Err(err) => {
                    warn!("backlight: cannot reach systemd-logind: {err}; brightness not changed");
                    continue;
                }
            }
        }
        let Some(proxy) = session.as_ref() else {
            continue;
        };

        match proxy.set_brightness("backlight", &device.name, value).await {
            Ok(()) => {
                let _ = refresh_tx.send(()).await;
            }
            Err(err) => {
                warn!(
                    "backlight: SetBrightness({}, {value}) failed: {err}",
                    device.name
                );
            }
        }
    }
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait LoginManager {
    fn get_session(&self, id: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1"
)]
trait LoginSession {
    fn set_brightness(&self, subsystem: &str, name: &str, brightness: u32) -> zbus::Result<()>;
}

/// Resolve the session the bar runs in and return a proxy for it.
async fn connect_session() -> zbus::Result<LoginSessionProxy<'static>> {
    let conn = zbus::Connection::system().await?;
    let manager = LoginManagerProxy::new(&conn).await?;
    // "self" is the session of the calling process; "auto" is a safe fallback
    // when logind cannot resolve the caller (e.g. inside a container).
    let path = match manager.get_session("self").await {
        Ok(path) => path,
        Err(err) => {
            debug!("backlight: GetSession(\"self\") failed: {err}; trying \"auto\"");
            manager.get_session("auto").await?
        }
    };
    let session = LoginSessionProxy::<'static>::builder(&conn)
        .path(path)?
        .build()
        .await?;
    Ok(session)
}

pub fn register(reg: &mut Registry) {
    reg.register("backlight", |init: &ModuleInit| {
        let (cmd_tx, cmd_rx) = bridge::channel::<Cmd>();
        Ok(Box::new(Backlight {
            inner: LabelModule::new("backlight", &init.id, &init.config).with_context(&init.name),
            cmd_tx,
            cmd_rx: RefCell::new(Some(cmd_rx)),
            subscription: RefCell::new(None),
        }))
    });
}
