//! Backlight module (`/sys/class/backlight`).

use std::path::Path;

use gtk::prelude::*;
use waybar_core::format::{Args, FmtArg};
use waybar_core::{timer, Module, ModuleInit, Registry};

use crate::label::LabelModule;
use crate::sysinfo::backlight::{best_device, brightness_path, enumerate, percent, percent_exp};

const BACKLIGHT_DIR: &str = "/sys/class/backlight";

struct Backlight {
    inner: LabelModule,
}

impl Module for Backlight {
    fn widget(&self) -> gtk::Widget {
        self.inner.widget()
    }

    fn start(&self) {
        let inner = self.inner.clone();
        let config = inner.config().clone();

        // Only drive brightness ourselves when the user has not bound scrolling
        // to a command.
        let has_scroll_command = ["on-scroll-up", "on-scroll-down", "on-scroll-left", "on-scroll-right"]
            .iter()
            .any(|key| config.get(*key).and_then(|v| v.as_str()).is_some());

        if !has_scroll_command {
            let preferred = inner.option_str("device", "");
            let step = inner.option_f64("scroll-step", 1.0);
            let min = inner.option_f64("min-brightness", 0.0);
            let reverse = inner.option_bool("reverse-scrolling", false);

            let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
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
                adjust_brightness(&preferred, step, min, increase);
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
    let state = inner.get_state(pct as u8, false);
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

/// Change the brightness by `step` percent, clamped to `[min, 100]`.
fn adjust_brightness(preferred: &str, step: f64, min: f64, increase: bool) {
    let devices = enumerate(Path::new(BACKLIGHT_DIR));
    let Some(device) = best_device(&devices, preferred) else {
        return;
    };
    let max = device.max as f64;
    if max <= 0.0 {
        return;
    }
    let current = f64::from(percent(device.actual, device.max));
    let target = if increase {
        (current + step).min(100.0)
    } else {
        if current <= min {
            return;
        }
        (current - step).max(min)
    };

    let absolute = (target * max / 100.0).round().clamp(0.0, max) as u64;
    let path = brightness_path(Path::new(BACKLIGHT_DIR), &device.name);
    if let Err(err) = std::fs::write(&path, absolute.to_string()) {
        tracing::warn!(
            "backlight: cannot write {}: {err} (a udev rule or logind is needed)",
            path.display()
        );
    }
}

pub fn register(reg: &mut Registry) {
    reg.register("backlight", |init: &ModuleInit| {
        Ok(Box::new(Backlight {
            inner: LabelModule::new("backlight", &init.id, &init.config).with_context(&init.name),
        }))
    });
}
