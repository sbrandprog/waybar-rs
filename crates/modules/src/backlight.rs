//! Backlight module (`/sys/class/backlight`).
//!
//! Brightness is read from sysfs, but changed through `systemd-logind`'s
//! `Session.SetBrightness` D-Bus method. logind writes the sysfs attribute as
//! root, so the module needs neither a udev rule nor write access to `/sys`.
//! This requires an active local session (systemd or elogind); over SSH or in a
//! headless environment the display still works but scrolling cannot change the
//! brightness.

use std::cell::RefCell;
use std::path::Path;

use gtk::prelude::*;
use tracing::{debug, warn};

use waybar_core::bridge;
use waybar_core::format::{Args, FmtArg};
use waybar_core::{Module, ModuleInit, Registry, timer};

use crate::label::LabelModule;
use crate::sysinfo::backlight::{best_device, enumerate, percent, percent_exp, step_value};

const BACKLIGHT_DIR: &str = "/sys/class/backlight";

/// A brightness change request from the GTK thread to the worker.
#[derive(Clone, Debug)]
enum Cmd {
    Set { name: String, value: u64 },
}

struct Backlight {
    inner: LabelModule,
    cmd_tx: async_channel::Sender<Cmd>,
    cmd_rx: RefCell<Option<async_channel::Receiver<Cmd>>>,
}

impl Module for Backlight {
    fn widget(&self) -> gtk::Widget {
        self.inner.widget()
    }

    fn start(&self) {
        let inner = self.inner.clone();
        let config = inner.config().clone();

        // Writes go through a worker; it reports back after a change so the
        // label refreshes immediately instead of waiting for the next tick.
        if let Some(cmd_rx) = self.cmd_rx.borrow_mut().take() {
            let (refresh_tx, refresh_rx) = bridge::channel::<()>();
            bridge::spawn(worker(cmd_rx, refresh_tx));
            let refresh_inner = inner.clone();
            bridge::on_main(refresh_rx, move |_| update(&refresh_inner));
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

            let scroll =
                gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
            scroll.connect_scroll(move |_, dx, dy| {
                // Waybar maps up/right to "increase" and down/left to
                // "decrease"; `reverse-scrolling` only flips the vertical axis.
                let increase = if dy < 0.0 {
                    !reverse
                } else if dy > 0.0 {
                    reverse
                } else if dx < 0.0 {
                    false
                } else if dx > 0.0 {
                    true
                } else {
                    return gtk::glib::Propagation::Proceed;
                };
                if let Some((name, value)) = target(&preferred, step, min, increase) {
                    let _ = cmd_tx.try_send(Cmd::Set { name, value });
                }
                gtk::glib::Propagation::Proceed
            });
            inner.widget().add_controller(scroll);
        }

        let update = {
            let inner = inner.clone();
            move || update(&inner)
        };
        match inner.interval_secs(2) {
            Some(secs) => {
                timer::every_seconds(secs, update);
            }
            None => update(),
        }
    }
}

/// The device name and absolute brightness for one scroll step.
fn target(preferred: &str, step: f64, min: f64, increase: bool) -> Option<(String, u64)> {
    let devices = enumerate(Path::new(BACKLIGHT_DIR));
    let device = best_device(&devices, preferred)?;
    let value = step_value(device.requested, device.max, step, min, increase)?;
    Some((device.name.clone(), value))
}

fn update(inner: &LabelModule) {
    let preferred = inner.option_str("device", "");
    let devices = enumerate(Path::new(BACKLIGHT_DIR));
    let Some(device) = best_device(&devices, &preferred) else {
        inner.set_visible(false);
        return;
    };
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

/// Apply brightness changes through `systemd-logind`.
async fn worker(cmd_rx: async_channel::Receiver<Cmd>, refresh_tx: async_channel::Sender<()>) {
    let mut session: Option<LoginSessionProxy<'static>> = None;
    while let Ok(Cmd::Set { name, value }) = cmd_rx.recv().await {
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

        match proxy.set_brightness("backlight", &name, value).await {
            Ok(()) => {
                let _ = refresh_tx.send(()).await;
            }
            Err(err) => {
                warn!("backlight: SetBrightness({name}, {value}) failed: {err}");
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
        }))
    });
}
