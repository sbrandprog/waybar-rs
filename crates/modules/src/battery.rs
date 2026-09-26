//! Battery module (`/sys/class/power_supply`).
//!
//! Polls the power supplies on the configured interval and also refreshes on
//! `power_supply` uevents (see [`waybar_core::udev`]). The interval timer stays
//! the source of truth: uevents do not carry the values and not every driver
//! emits one, so udev only shortens the reaction to plug/unplug and status
//! changes.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use serde_json::Value;
use waybar_core::format::{self as fmt, Args, FmtArg};
use waybar_core::{Module, ModuleInit, Registry, bridge, timer, udev};

use crate::label::LabelModule;
use crate::sysinfo::battery::{PowerSupply, aggregate, normalize_status, read_power_supply};

const SUPPLY_DIR: &str = "/sys/class/power_supply";

struct Battery {
    inner: LabelModule,
    window: gtk::ApplicationWindow,
    /// The `power_supply` monitor; dropping it stops the udev thread.
    subscription: RefCell<Option<udev::Subscription>>,
}

impl Module for Battery {
    fn widget(&self) -> gtk::Widget {
        self.inner.widget()
    }

    fn start(&self) {
        let inner = self.inner.clone();
        let window = self.window.clone();
        let config = inner.config().clone();
        let old_status: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let old_window_class: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

        // Shared between the interval timer and the uevent-triggered refresh.
        let update: Rc<dyn Fn()> = Rc::new({
            let inner = inner.clone();
            let window = window.clone();
            let old_status = old_status.clone();
            let old_window_class = old_window_class.clone();
            move || update(&inner, &window, &config, &old_status, &old_window_class)
        });

        // udev is an accelerator: a `power_supply` uevent refreshes the label
        // right away (plug/unplug, capacity/status change) instead of waiting
        // for the next tick. The timer stays the source of truth, because
        // uevents do not carry the values and not every driver emits one.
        if let Some(subscription) = udev::watch(&["power_supply"]) {
            let refresh = timer::debounce(150, {
                let update = update.clone();
                move || update()
            });
            bridge::on_main(subscription.receiver(), move |_| refresh());
            *self.subscription.borrow_mut() = Some(subscription);
        }

        match inner.interval_secs(60) {
            Some(secs) => {
                let update = update.clone();
                timer::every_seconds(secs, move || update());
            }
            None => update(),
        }
    }
}

fn update(
    inner: &LabelModule,
    window: &gtk::ApplicationWindow,
    config: &Value,
    old_status: &Rc<RefCell<Option<String>>>,
    old_window_class: &Rc<RefCell<Option<String>>>,
) {
    let batteries = read_batteries(config);
    if batteries.is_empty() {
        inner.set_visible(false);
        return;
    }

    let info = aggregate(&batteries, read_adapter_plugged(config));
    let state = inner.get_state(info.capacity, true);
    let status = normalize_status(&info.status);

    // The label carries the (normalised) status as a class.
    if let Some(previous) = old_status.borrow_mut().take() {
        inner.set_class(&previous, false);
    }
    inner.set_class(&status, true);
    *old_status.borrow_mut() = Some(status.clone());

    // The window carries `battery-<state>`.
    if let Some(previous) = old_window_class.borrow_mut().take() {
        window.remove_css_class(&previous);
    }
    if !state.is_empty() {
        let class = format!("battery-{state}");
        window.add_css_class(&class);
        *old_window_class.borrow_mut() = Some(class);
    }

    let format = resolve_format(inner, &status, &state);
    if format.is_empty() {
        inner.set_visible(false);
        return;
    }
    inner.set_visible(true);

    let time_format = inner.option_str("format-time", "{H} h {M} min");
    let time = format_time_remaining(info.time_remaining_hours, &time_format, inner.context());

    let mut args = Args::new();
    args.insert("capacity".into(), FmtArg::Int(i64::from(info.capacity)));
    args.insert("power".into(), FmtArg::Float(f64::from(info.power_watts)));
    args.insert("cycles".into(), FmtArg::Int(i64::from(info.cycles)));
    args.insert("time".into(), FmtArg::from(time.clone()));
    // waybar pre-formats health with three decimals and applies the config
    // spec to that string.
    args.insert("health".into(), FmtArg::from(format!("{:.3}", info.health)));
    let alts = [format!("{status}-{state}"), status.clone(), state.clone()];
    let alt_refs: Vec<&str> = alts.iter().map(String::as_str).collect();
    args.insert(
        "icon".into(),
        FmtArg::from(inner.get_icon(u16::from(info.capacity), &alt_refs, 0)),
    );

    inner.set_markup(&inner.render(&format, &args));

    let tooltip_default = if info.time_remaining_hours != 0.0 {
        if info.time_remaining_hours > 0.0 {
            format!("Empty in {time}")
        } else {
            format!("Full in {time}")
        }
    } else {
        info.status.clone()
    };
    let tooltip_format = resolve_tooltip(inner, &status, &state, &tooltip_default);
    // `{timeTo}` stands for a pre-rendered sentence rather than a plain
    // substitution, so it is filled in before formatting; any other placeholder
    // is left for the formatter.
    let tooltip = tooltip_format.replace("{timeTo}", &tooltip_default);
    inner.set_tooltip(&inner.render(&tooltip, &args));
}

/// `format-<status>-<state>` -> `format-<status>` -> `format-<state>` -> `format`.
fn resolve_format(inner: &LabelModule, status: &str, state: &str) -> String {
    let config = inner.config();
    for key in [
        format!("format-{status}-{state}"),
        format!("format-{status}"),
        format!("format-{state}"),
    ] {
        if let Some(value) = config.get(&key).and_then(|v| v.as_str()) {
            return value.to_string();
        }
    }
    inner.resolve_format("{capacity}%", state)
}

/// The tooltip equivalent of [`resolve_format`], with `default` as the last
/// fallback.
fn resolve_tooltip(inner: &LabelModule, status: &str, state: &str, default: &str) -> String {
    let config = inner.config();
    for key in [
        format!("tooltip-format-{status}-{state}"),
        format!("tooltip-format-{status}"),
        format!("tooltip-format-{state}"),
    ] {
        if let Some(value) = config.get(&key).and_then(|v| v.as_str()) {
            return value.to_string();
        }
    }
    inner.resolve_tooltip_format(default, state)
}

fn read_batteries(config: &Value) -> Vec<PowerSupply> {
    let wanted = config.get("bat").and_then(|v| v.as_str());
    let compatibility = config
        .get("bat-compatibility")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut batteries = Vec::new();
    let Ok(entries) = std::fs::read_dir(SUPPLY_DIR) else {
        return batteries;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(wanted) = wanted
            && name != wanted
        {
            continue;
        }
        let supply = read_power_supply(&entry.path());
        if !supply.is_battery() {
            continue;
        }
        if wanted.is_none() && supply.scope_device {
            continue;
        }
        if supply.status.is_none() && !compatibility {
            continue;
        }
        batteries.push(supply);
    }
    batteries
}

/// Whether an external adapter is powering the system.
///
/// Mirrors waybar: the adapter is the first supply exposing an `online` file,
/// and it only counts when `online` is non-zero and it does not itself report
/// `Discharging` (some USB-C supplies keep `online` set while discharging).
fn read_adapter_plugged(config: &Value) -> bool {
    let wanted = config.get("adapter").and_then(|v| v.as_str());
    let Ok(entries) = std::fs::read_dir(SUPPLY_DIR) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(wanted) = wanted
            && name != wanted
        {
            continue;
        }
        let Ok(online) = std::fs::read_to_string(entry.path().join("online")) else {
            continue;
        };
        let status = std::fs::read_to_string(entry.path().join("status"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        return online.trim() != "0" && status != "Discharging";
    }
    false
}

/// Render a duration like waybar's `format-time` (`{H}` hours, `{M}` minutes,
/// `{m}` zero-padded minutes).
fn format_time_remaining(hours: f32, format: &str, ctx: &str) -> String {
    let hours = hours.abs();
    let full_hours = hours.floor();
    let minutes = (60.0 * (hours - full_hours)) as u32;
    let full_hours = full_hours as u32;
    if full_hours == 0 && minutes == 0 {
        return String::new();
    }
    let mut args = Args::new();
    args.insert("H".into(), FmtArg::Int(i64::from(full_hours)));
    args.insert("M".into(), FmtArg::Int(i64::from(minutes)));
    args.insert("m".into(), FmtArg::from(format!("{minutes:02}")));
    fmt::format_ctx(format, &args, ctx)
}

pub fn register(reg: &mut Registry) {
    reg.register("battery", |init: &ModuleInit| {
        Ok(Box::new(Battery {
            inner: LabelModule::new("battery", &init.id, &init.config).with_context(&init.name),
            window: init.window.clone(),
            subscription: RefCell::new(None),
        }))
    });
}
