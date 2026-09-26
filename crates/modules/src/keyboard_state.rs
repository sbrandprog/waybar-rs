//! `keyboard-state` — Num/Caps/Scroll lock indicators.
//!
//! The lock-capable input devices under `/dev/input` are opened once; each
//! interval tick then reads their LED state with a cheap `EVIOCGLED` ioctl
//! instead of reopening every device. `input` uevents add or remove devices on
//! hotplug, so the GTK main loop never rescans `/dev/input`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use evdev::{Device, LedCode};
use gtk::prelude::*;
use serde_json::Value;
use tracing::warn;

use waybar_core::format::{self as fmt, Args};
use waybar_core::{Module, ModuleInit, Registry, bridge, timer, udev};

const DEVICES_DIR: &str = "/dev/input";

/// The three lock keys, in upstream's pack order.
const KEYS: [(&str, &str, &str); 3] = [
    ("numlock", "Num", "numlock"),
    ("capslock", "Caps", "capslock"),
    ("scrolllock", "Scroll", "scrolllock"),
];

/// Devices that support the lock LEDs, kept open for cheap per-tick reads.
type Devices = Rc<RefCell<Vec<(PathBuf, Device)>>>;

struct KeyboardState {
    root: gtk::Box,
    config: Value,
    /// One label per enabled key: (key, name, label).
    labels: Vec<(&'static str, &'static str, gtk::Label)>,
    device_path: Option<String>,
    warned: Rc<Cell<bool>>,
    /// Opened lock-capable devices, shared by the interval callback and the
    /// hotplug handler (both run on the GTK main thread).
    devices: Devices,
    /// The `input` monitor; dropping it stops the udev thread.
    subscription: RefCell<Option<udev::Subscription>>,
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
        let labels = self.labels.clone();
        let warned = self.warned.clone();
        let devices = self.devices.clone();

        // Open the relevant devices once; the tick then only issues an ioctl.
        scan_devices(&devices, self.device_path.as_deref(), &warned);

        let update = {
            let devices = devices.clone();
            move || {
                if let Some((num, caps, scroll)) = read_open_leds(&devices) {
                    tracing::trace!("keyboard-state: num={num} caps={caps} scroll={scroll}");
                    let states = [num, caps, scroll];
                    for (index, (key, name, label)) in labels.iter().enumerate() {
                        update_label(&config, key, name, label, states[index]);
                    }
                }
            }
        };
        timer::every_seconds(interval, update);

        // Hotplug: an `input` uevent names the event node, so exactly one
        // device is opened or dropped instead of rescanning the directory.
        if let Some(subscription) = udev::watch(&["input"]) {
            let configured = self.device_path.clone();
            bridge::on_main(subscription.receiver(), move |event| {
                if !event.sysname.starts_with("event") {
                    return;
                }
                let path = Path::new(DEVICES_DIR).join(&event.sysname);
                match event.action.as_str() {
                    "add" => {
                        if !add_device(&devices, &path, configured.as_deref(), &warned) {
                            retry_add(devices.clone(), path, configured.clone(), warned.clone(), 3);
                        }
                    }
                    "remove" => remove_device(&devices, &event.sysname),
                    _ => {}
                }
            });
            *self.subscription.borrow_mut() = Some(subscription);
        }
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
    label.set_markup(&fmt::format_ctx(&format, &args, "keyboard-state"));

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

/// Read the OR-ed LED state of the open devices.
///
/// Devices that no longer answer are dropped; `None` means no lock-capable
/// device is currently available.
fn read_open_leds(devices: &Devices) -> Option<(bool, bool, bool)> {
    let mut found = false;
    let (mut num, mut caps, mut scroll) = (false, false, false);
    devices
        .borrow_mut()
        .retain(|(_, device)| match device.get_led_state() {
            Ok(state) => {
                found = true;
                num |= state.contains(LedCode::LED_NUML);
                caps |= state.contains(LedCode::LED_CAPSL);
                scroll |= state.contains(LedCode::LED_SCROLLL);
                true
            }
            Err(_) => false,
        });
    found.then_some((num, caps, scroll))
}

/// Open the lock-capable devices, replacing whatever was tracked before.
fn scan_devices(devices: &Devices, configured: Option<&str>, warned: &Cell<bool>) {
    devices.borrow_mut().clear();
    match configured {
        Some(path) => {
            add_device(devices, Path::new(path), Some(path), warned);
        }
        None => {
            for path in event_devices() {
                add_device(devices, &path, None, warned);
            }
        }
    }
}

/// A device is relevant only when it matches a configured `device-path`.
fn accepted(configured: Option<&str>, path: &Path) -> bool {
    configured.is_none_or(|configured| Path::new(configured) == path)
}

/// Open one event node and keep it when it supports the lock LEDs.
///
/// Returns `false` when the node could not be opened (it may not be ready yet),
/// so the caller can retry; a node that opens but has no lock LEDs is settled.
fn add_device(
    devices: &Devices,
    path: &Path,
    configured: Option<&str>,
    warned: &Cell<bool>,
) -> bool {
    if !accepted(configured, path)
        || devices
            .borrow()
            .iter()
            .any(|(existing, _)| existing == path)
    {
        return true;
    }
    match open_lock_device(path, warned) {
        Ok(Some(device)) => {
            devices.borrow_mut().push((path.to_path_buf(), device));
            true
        }
        Ok(None) => true,
        Err(()) => false,
    }
}

/// Retry opening a device that was not ready when its uevent arrived.
fn retry_add(
    devices: Devices,
    path: PathBuf,
    configured: Option<String>,
    warned: Rc<Cell<bool>>,
    attempts: u32,
) {
    if add_device(&devices, &path, configured.as_deref(), &warned) || attempts == 0 {
        return;
    }
    gtk::glib::timeout_add_local_once(Duration::from_millis(300), move || {
        retry_add(devices, path, configured, warned, attempts - 1);
    });
}

/// Drop the device behind an `eventN` sysname.
fn remove_device(devices: &Devices, sysname: &str) {
    devices
        .borrow_mut()
        .retain(|(path, _)| path.file_name().and_then(|name| name.to_str()) != Some(sysname));
}

/// Open `path`, returning `Ok(Some(device))` when it supports the lock LEDs,
/// `Ok(None)` when it is not likewise capable, and `Err(())` when opening it
/// failed (for example the permission is missing or the node is not ready).
fn open_lock_device(path: &Path, warned: &Cell<bool>) -> Result<Option<Device>, ()> {
    let device = match Device::open(path) {
        Ok(device) => device,
        Err(err) => {
            if err.kind() == std::io::ErrorKind::PermissionDenied && !warned.get() {
                warn!(
                    "keyboard-state: cannot open {} (are you in the input group?): {err}",
                    path.display()
                );
                warned.set(true);
            }
            return Err(());
        }
    };
    let supports = device.supported_leds().is_some_and(|leds| {
        leds.contains(LedCode::LED_NUML)
            && leds.contains(LedCode::LED_CAPSL)
            && leds.contains(LedCode::LED_SCROLLL)
    });
    Ok(supports.then_some(device))
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
            if init
                .config
                .get(key)
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
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
            devices: Rc::new(RefCell::new(Vec::new())),
            subscription: RefCell::new(None),
        }))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn device_path_filter() {
        assert!(accepted(None, Path::new("/dev/input/event3")));
        assert!(accepted(
            Some("/dev/input/event3"),
            Path::new("/dev/input/event3")
        ));
        assert!(!accepted(
            Some("/dev/input/event3"),
            Path::new("/dev/input/event4")
        ));
    }

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
