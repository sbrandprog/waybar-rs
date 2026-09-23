//! `keyboard-state` — Num/Caps/Scroll lock indicators.
//!
//! Polls the LED state of the keyboards under `/dev/input` on an interval.
//! Re-enumerating devices every tick also picks up hotplugged keyboards, so no
//! inotify watch is required.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use evdev::{Device, LedCode};
use gtk::prelude::*;
use serde_json::Value;
use tracing::warn;

use waybar_core::format::{self as fmt, Args};
use waybar_core::{timer, Module, ModuleInit, Registry};

const DEVICES_DIR: &str = "/dev/input";

/// The three lock keys, in upstream's pack order.
const KEYS: [(&str, &str, &str); 3] = [
    ("numlock", "Num", "numlock"),
    ("capslock", "Caps", "capslock"),
    ("scrolllock", "Scroll", "scrolllock"),
];

struct KeyboardState {
    root: gtk::Box,
    config: Value,
    /// One label per enabled key: (key, name, label).
    labels: Vec<(&'static str, &'static str, gtk::Label)>,
    device_path: Option<String>,
    warned: Rc<Cell<bool>>,
}

impl Module for KeyboardState {
    fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    fn start(&self) {
        let interval = match self.config.get("interval") {
            Some(value) if value.is_number() => {
                warn!("keyboard-state: `interval` is deprecated");
                value.as_u64().unwrap_or(1).max(1)
            }
            _ => 1,
        };

        let config = self.config.clone();
        let device_path = self.device_path.clone();
        let warned = self.warned.clone();
        let labels = self.labels.clone();
        let update = move || {
            if let Some((num, caps, scroll)) =
                read_lock_states(device_path.as_deref(), &warned)
            {
                tracing::trace!("keyboard-state: num={num} caps={caps} scroll={scroll}");
                let states = [num, caps, scroll];
                for (index, (key, name, label)) in labels.iter().enumerate() {
                    update_label(&config, key, name, label, states[index]);
                }
            }
        };
        timer::every_seconds(interval, update);
    }
}

/// Update one lock label's markup and `locked` class.
fn update_label(config: &Value, key: &str, name: &str, label: &gtk::Label, locked: bool) {
    let icons = key_state_to_icons(config);
    let map_key = if is_common_format_icons(config) {
        "Lock"
    } else {
        name
    };
    let Some(icons) = icons.get(map_key) else {
        return;
    };
    let format = key_format(config, key);
    let mut args = Args::new();
    args.insert("name".into(), name.into());
    args.insert("icon".into(), icons[usize::from(locked)].clone().into());
    label.set_markup(&fmt::format(&format, &args));

    if locked {
        label.add_css_class("locked");
    } else {
        label.remove_css_class("locked");
    }
}

/// Whether `format-icons` uses the shared `{locked, unlocked}` form.
fn is_common_format_icons(config: &Value) -> bool {
    config
        .get("format-icons")
        .and_then(|v| v.as_object())
        .is_some_and(|icons| {
            icons.get("locked").is_some_and(|v| v.is_string())
                || icons.get("unlocked").is_some_and(|v| v.is_string())
        })
}

/// Build the `key -> [unlocked, locked]` icon table.
fn key_state_to_icons(config: &Value) -> HashMap<String, [String; 2]> {
    let mut out = HashMap::new();

    if is_common_format_icons(config) {
        let icons = config.get("format-icons").and_then(|v| v.as_object());
        let get = |key: &str| {
            icons
                .and_then(|icons| icons.get(key))
                .and_then(|v| v.as_str())
                .unwrap_or(key)
                .to_string()
        };
        out.insert("Lock".to_string(), [get("unlocked"), get("locked")]);
        return out;
    }

    let format_icons = config.get("format-icons").and_then(|v| v.as_object());
    for (key, name, _) in KEYS {
        let mut unlocked = "unlocked".to_string();
        let mut locked = "locked".to_string();
        if let Some(obj) = format_icons
            .and_then(|icons| icons.get(key))
            .and_then(|v| v.as_object())
        {
            if let Some(value) = obj.get("unlocked").and_then(|v| v.as_str()) {
                unlocked = value.to_string();
            }
            if let Some(value) = obj.get("locked").and_then(|v| v.as_str()) {
                locked = value.to_string();
            }
        }
        out.insert(name.to_string(), [unlocked, locked]);
    }
    out
}

/// The `format` string for a lock key.
fn key_format(config: &Value, key: &str) -> String {
    if let Some(format) = config.get("format").and_then(|v| v.as_str()) {
        return format.to_string();
    }
    if let Some(format) = config
        .get("format")
        .and_then(|v| v.get(key))
        .and_then(|v| v.as_str())
    {
        return format.to_string();
    }
    "{name} {icon}".to_string()
}

/// Read the OR-ed LED state of every keyboard, or `None` when none was found.
fn read_lock_states(device_path: Option<&str>, warned: &Cell<bool>) -> Option<(bool, bool, bool)> {
    let paths: Vec<PathBuf> = match device_path {
        Some(path) => vec![PathBuf::from(path)],
        None => event_devices(),
    };

    let mut found = false;
    let (mut num, mut caps, mut scroll) = (false, false, false);
    for path in paths {
        let device = match Device::open(&path) {
            Ok(device) => device,
            Err(err) => {
                if err.kind() == std::io::ErrorKind::PermissionDenied && !warned.get() {
                    warn!(
                        "keyboard-state: cannot open {} (are you in the input group?): {err}",
                        path.display()
                    );
                    warned.set(true);
                }
                continue;
            }
        };
        let supports = device.supported_leds().is_some_and(|leds| {
            leds.contains(LedCode::LED_NUML)
                && leds.contains(LedCode::LED_CAPSL)
                && leds.contains(LedCode::LED_SCROLLL)
        });
        if !supports {
            continue;
        }
        if let Ok(state) = device.get_led_state() {
            found = true;
            num |= state.contains(LedCode::LED_NUML);
            caps |= state.contains(LedCode::LED_CAPSL);
            scroll |= state.contains(LedCode::LED_SCROLLL);
        }
    }

    found.then_some((num, caps, scroll))
}

/// Every `/dev/input/event*` path, sorted for a stable order.
fn event_devices() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(DEVICES_DIR) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("event"))
        })
        .collect();
    paths.sort();
    paths
}

pub fn register(reg: &mut Registry) {
    reg.register("keyboard-state", |init: &ModuleInit| {
        let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        box_.set_widget_name("keyboard-state");
        box_.add_css_class("module");
        if !init.id.is_empty() {
            box_.add_css_class(&init.id);
        }

        // Upstream packs with `pack_end` (numlock first, so it ends up on the
        // right); appending in reverse reproduces the same left-to-right order.
        let mut labels = Vec::new();
        for (key, name, class) in KEYS {
            if init.config.get(key).and_then(|v| v.as_bool()).unwrap_or(false) {
                let label = gtk::Label::new(None);
                label.add_css_class(class);
                labels.push((key, name, label));
            }
        }
        for (_, _, label) in labels.iter().rev() {
            box_.append(label);
        }

        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.append(&box_);

        Ok(Box::new(KeyboardState {
            root,
            config: init.config.clone(),
            labels,
            device_path: init
                .config
                .get("device-path")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            warned: Rc::new(Cell::new(false)),
        }))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_icons_per_key() {
        let icons = key_state_to_icons(&json!({}));
        assert_eq!(icons["Num"], ["unlocked", "locked"]);
        assert_eq!(icons["Caps"], ["unlocked", "locked"]);
    }

    #[test]
    fn per_key_icon_overrides() {
        let config = json!({
            "format-icons": {
                "numlock": {"unlocked": "n0", "locked": "n1"},
                "capslock": {"locked": "c1"}
            }
        });
        let icons = key_state_to_icons(&config);
        assert_eq!(icons["Num"], ["n0", "n1"]);
        assert_eq!(icons["Caps"], ["unlocked", "c1"]);
    }

    #[test]
    fn common_icons_share_one_entry() {
        let config = json!({"format-icons": {"unlocked": "off", "locked": "on"}});
        assert!(is_common_format_icons(&config));
        let icons = key_state_to_icons(&config);
        assert_eq!(icons.len(), 1);
        assert_eq!(icons["Lock"], ["off", "on"]);
    }

    #[test]
    fn format_resolution_order() {
        let config = json!({"format": {"capslock": "C:{icon}"}});
        assert_eq!(key_format(&config, "capslock"), "C:{icon}");
        assert_eq!(key_format(&config, "numlock"), "{name} {icon}");
        let config = json!({"format": "{icon}"});
        assert_eq!(key_format(&config, "capslock"), "{icon}");
    }
}
