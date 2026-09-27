//! `keyboard-state` — Num/Caps/Scroll lock indicators.
//!
//! All device work happens on one dedicated thread. It opens the lock-capable
//! `/dev/input/event*` nodes, keeps them for cheap `EVIOCGLED` ioctls, drains
//! their event queues and turns an `EV_LED` event into a state update on the GTK
//! main thread. `evdev::Device` is `Send`, so the thread owns the devices
//! outright — the main thread never touches an fd and nothing has to be mirrored
//! to it.
//!
//! Draining is not optional. An evdev queue belongs to one open file, is bounded
//! and is never emptied on its own: a device whose queue is left unread reports
//! `POLLIN` forever, so a `poll`-only loop would spin on the first keypress.
//!
//! The interval tick stays the source of truth — `EV_LED` is only an
//! accelerator, a driver may change the LED mask without emitting an event — and
//! `input` uevents add or remove exactly one device on hotplug.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::Duration;

use evdev::{AttributeSet, Device, EventType, LedCode};
use gtk::glib;
use gtk::prelude::*;
use serde_json::Value;
use tracing::{debug, warn};

use waybar_core::format::{self as fmt, Args};
use waybar_core::{Module, ModuleInit, Registry, bridge, timer, udev};

const DEVICES_DIR: &str = "/dev/input";

/// How long the device thread waits in `poll(2)` before it looks at its command
/// queue. It bounds how long a resync or a hotplug command waits for the next
/// turn, not how long a lock change takes: an `EV_LED` event wakes the thread
/// immediately.
const COMMAND_POLL_MS: libc::c_int = 100;

/// How often to retry an event node that could not be opened yet when its
/// uevent arrived (the node may not be readable that early).
const ADD_ATTEMPTS: u32 = 3;
const ADD_RETRY_DELAY: Duration = Duration::from_millis(300);

/// The three lock keys: config key, label name, CSS class.
const KEYS: [(&str, &str, &str); 3] = [
    ("numlock", "Num", "numlock"),
    ("capslock", "Caps", "capslock"),
    ("scrolllock", "Scroll", "scrolllock"),
];

/// The lock LEDs to report, in the same order as [`KEYS`].
const LEDS: [LedCode; 3] = [LedCode::LED_NUML, LedCode::LED_CAPSL, LedCode::LED_SCROLLL];

/// The lock state of every open device, OR-ed, in [`LEDS`] order.
type LedState = [bool; 3];

/// One label plus its config-derived strings, resolved once at construction.
struct KeyLabel {
    /// Index into [`LedState`], so a label is always driven by its own LED
    /// rather than by its position among the enabled keys.
    led: usize,
    name: &'static str,
    format: String,
    /// `[unlocked, locked]`.
    icons: [String; 2],
    label: gtk::Label,
}

/// An opened lock-capable device, owned by the device thread.
struct LockDevice {
    path: PathBuf,
    device: Device,
}

/// A request for the device thread.
enum LedCommand {
    /// Re-read the LEDs of every open device (the periodic source of truth).
    Resync,
    /// Open `path` when it supports the lock LEDs (hotplug).
    Add(PathBuf),
    /// Drop the device behind an `eventN` sysname (hotplug).
    Remove(String),
    /// Stop the thread.
    Stop,
}

/// The device thread; dropping it stops the watch.
struct LedThread {
    cmd: Option<async_channel::Sender<LedCommand>>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for LedThread {
    fn drop(&mut self) {
        // Explicit, not just closing the queue: the hotplug handler's glib
        // future outlives the module and still holds a sender.
        if let Some(cmd) = self.cmd.take() {
            let _ = cmd.send_blocking(LedCommand::Stop);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct KeyboardState {
    root: gtk::Box,
    /// Restricts the open set to one node; `None` watches every lock-capable
    /// device.
    device_path: Option<String>,
    /// Resync period in seconds; `0` means "no periodic resync".
    interval: u64,
    /// The labels, drained into the state handler by `start`.
    labels: RefCell<Vec<KeyLabel>>,
    /// The device thread; dropping it stops the watch.
    thread: RefCell<Option<LedThread>>,
    /// The periodic resync, stopped with the module.
    timer: RefCell<Option<glib::SourceId>>,
    /// The `input` monitor; dropping it stops the udev thread.
    subscription: RefCell<Option<udev::Subscription>>,
}

impl Drop for KeyboardState {
    fn drop(&mut self) {
        // Stop the tick with the module: its closure owns a command sender, and
        // a bar that is gone has no use for a per-second resync.
        if let Some(id) = self.timer.borrow_mut().take() {
            id.remove();
        }
    }
}

impl Module for KeyboardState {
    fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    fn start(&self) {
        let labels: Vec<KeyLabel> = self.labels.borrow_mut().drain(..).collect();
        let (cmd, commands) = async_channel::unbounded::<LedCommand>();
        let (updates, received) = bridge::channel::<LedState>();

        match spawn_device_thread(cmd.clone(), commands, updates, self.device_path.clone()) {
            Ok(thread) => *self.thread.borrow_mut() = Some(thread),
            Err(err) => warn!(
                "keyboard-state: cannot start the device thread ({err}), lock state will not update"
            ),
        }

        // The thread reports a state only when it differs from the last one, so
        // this touches the labels on real transitions only.
        bridge::on_main(received, move |state| render(&labels, state));

        // The source of truth: re-read every device on a timer, because a driver
        // may change the LED mask without emitting `EV_LED`. `interval: 0` or
        // `"once"` leaves the module purely event-driven.
        if self.interval > 0 {
            let tick = cmd.clone();
            *self.timer.borrow_mut() = Some(timer::every_seconds(self.interval, move || {
                let _ = tick.send_blocking(LedCommand::Resync);
            }));
        }

        // Hotplug: an `input` uevent names the event node, so exactly one device
        // is opened or dropped instead of rescanning the directory.
        if let Some(subscription) = udev::watch(&["input"]) {
            bridge::on_main(subscription.receiver(), move |event| {
                if !event.sysname.starts_with("event") {
                    return;
                }
                let command = match event.action.as_str() {
                    "add" => LedCommand::Add(Path::new(DEVICES_DIR).join(&event.sysname)),
                    "remove" => LedCommand::Remove(event.sysname),
                    _ => return,
                };
                let _ = cmd.send_blocking(command);
            });
            *self.subscription.borrow_mut() = Some(subscription);
        }
    }
}

/// The module's `interval` in seconds; `0` means "no periodic resync".
///
/// A positive number or numeric string sets the period (rounded, at least one
/// second); a non-positive value or `"once"` disables the timer. The default is
/// one second, matching the historical polling behaviour.
fn interval_secs(config: &Value) -> u64 {
    let secs = match config.get("interval") {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(1.0),
        Some(Value::String(s)) if s == "once" => return 0,
        Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(1.0),
        _ => 1.0,
    };
    if secs <= 0.0 {
        0
    } else {
        secs.round().max(1.0) as u64
    }
}

/// Render every key from a state the device thread reported.
fn render(labels: &[KeyLabel], state: LedState) {
    debug!("keyboard-state: {state:?}");
    for entry in labels {
        let locked = state[entry.led];
        let mut args = Args::new();
        args.insert("name".into(), entry.name.into());
        args.insert(
            "icon".into(),
            entry.icons[usize::from(locked)].clone().into(),
        );
        let markup = fmt::format_ctx(&entry.format, &args, "keyboard-state");
        entry.label.set_markup(&markup);
        if locked {
            entry.label.add_css_class("locked");
        } else {
            entry.label.remove_css_class("locked");
        }
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

/// The lock keys enabled in the config, in [`KEYS`] order, as
/// `(led index, config key, label name, CSS class)`.
///
/// The LED index travels with the key so that a config enabling only some of
/// them still wires every label to its own LED.
fn enabled_keys(
    config: &Value,
) -> impl Iterator<Item = (usize, &'static str, &'static str, &'static str)> {
    KEYS.iter()
        .enumerate()
        .filter_map(|(led, (key, name, class))| {
            let on = config.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
            on.then_some((led, *key, *name, *class))
        })
}

/// Spawn the thread that owns the lock-capable devices.
fn spawn_device_thread(
    cmd: async_channel::Sender<LedCommand>,
    commands: async_channel::Receiver<LedCommand>,
    updates: async_channel::Sender<LedState>,
    configured: Option<String>,
) -> std::io::Result<LedThread> {
    let handle = std::thread::Builder::new()
        .name("waybar-keyboard".to_string())
        .spawn(move || device_thread(commands, updates, configured))?;
    Ok(LedThread {
        cmd: Some(cmd),
        handle: Some(handle),
    })
}

/// Open the relevant devices and keep their LEDs current until told to stop.
///
/// The thread owns the devices, so it both drains their event queues and answers
/// the periodic resync; nothing here touches GTK.
fn device_thread(
    commands: async_channel::Receiver<LedCommand>,
    updates: async_channel::Sender<LedState>,
    configured: Option<String>,
) {
    let mut devices: Vec<LockDevice> = Vec::new();
    // Paths whose failed open was already reported, so a missing permission is
    // logged once per device instead of once per process.
    let mut warned: HashMap<String, ()> = HashMap::new();
    let mut last: Option<LedState> = None;

    // Open the relevant devices once; afterwards a resync is just an ioctl each.
    scan_devices(&mut devices, configured.as_deref(), &mut warned);
    if !publish(&mut devices, &mut last, &updates) {
        return;
    }

    loop {
        // Rebuilt every turn, so a device dropped below leaves the watch set
        // without any separate bookkeeping.
        let mut poll_fds: Vec<libc::pollfd> = devices
            .iter()
            .map(|entry| libc::pollfd {
                fd: entry.device.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let ready = if poll_fds.is_empty() {
            0
        } else {
            let watched = poll_fds.len() as libc::nfds_t;
            let ready = unsafe { libc::poll(poll_fds.as_mut_ptr(), watched, COMMAND_POLL_MS) };
            if ready < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                debug!("keyboard-state: poll failed: {err}");
                return;
            }
            ready
        };

        let mut announced = false;
        if ready > 0 {
            let mut gone = Vec::new();
            for (index, poll_fd) in poll_fds.iter().enumerate() {
                if poll_fd.revents & libc::POLLIN != 0 {
                    let entry = &mut devices[index];
                    match drain(&mut entry.device, &entry.path) {
                        Ok(led) => announced |= led,
                        // Unreadable: the node is gone, and keeping it would let
                        // `poll` report it on every turn.
                        Err(()) => gone.push(index),
                    }
                } else {
                    // POLLERR/POLLHUP: same thing.
                    gone.push(index);
                }
            }
            for index in gone.into_iter().rev() {
                debug!(
                    "keyboard-state: dropping {} after it stopped answering",
                    devices[index].path.display()
                );
                devices.remove(index);
            }
        }
        if announced && !publish(&mut devices, &mut last, &updates) {
            return;
        }

        // Handle every queued command, so a burst of uevents or a resync does
        // not wait for the next `poll`.
        loop {
            match commands.try_recv() {
                Ok(LedCommand::Resync) => {
                    if !publish(&mut devices, &mut last, &updates) {
                        return;
                    }
                }
                Ok(LedCommand::Add(path)) => {
                    add_device_later(&mut devices, path, configured.as_deref(), &mut warned)
                }
                Ok(LedCommand::Remove(sysname)) => remove_device(&mut devices, &sysname),
                Ok(LedCommand::Stop) | Err(async_channel::TryRecvError::Closed) => return,
                Err(async_channel::TryRecvError::Empty) => break,
            }
        }
    }
}

/// Read whatever the device has queued and report whether it announced an
/// `EV_LED` change, or `Err(())` when the node stopped answering.
///
/// The queue must be emptied on every wake-up. It belongs to this open file
/// alone, but leaving it unread keeps `POLLIN` asserted and spins the thread.
/// `fetch_events` reads exactly once, and only ever from the batch it just
/// filled, so it cannot block after `poll` reported readiness.
fn drain(device: &mut Device, path: &Path) -> Result<bool, ()> {
    match device.fetch_events() {
        Ok(mut events) => Ok(events.any(|event| event.event_type() == EventType::LED)),
        Err(err) => {
            debug!("keyboard-state: cannot read {}: {err}", path.display());
            Err(())
        }
    }
}

/// Read the OR-ed LED state and forward it when it changed.
///
/// Returns `false` when the GTK side is gone and the thread should stop.
fn publish(
    devices: &mut Vec<LockDevice>,
    last: &mut Option<LedState>,
    updates: &async_channel::Sender<LedState>,
) -> bool {
    let Some(state) = read_leds(devices) else {
        return true;
    };
    if *last == Some(state) {
        return true;
    }
    *last = Some(state);
    updates.send_blocking(state).is_ok()
}

/// The OR-ed LED state of the open devices.
///
/// Devices that no longer answer are dropped; `None` means no lock-capable
/// device is currently available.
fn read_leds(devices: &mut Vec<LockDevice>) -> Option<LedState> {
    let mut found = false;
    let mut state: LedState = [false; 3];
    devices.retain(|entry| match entry.device.get_led_state() {
        Ok(leds) => {
            found = true;
            for (slot, flag) in state.iter_mut().zip(led_flags(&leds)) {
                *slot |= flag;
            }
            true
        }
        Err(err) => {
            debug!("keyboard-state: dropping {}: {err}", entry.path.display());
            false
        }
    });
    found.then_some(state)
}

/// One device's LED set as the three lock flags, in [`LEDS`] order.
fn led_flags(leds: &AttributeSet<LedCode>) -> LedState {
    let mut state: LedState = [false; 3];
    for (slot, led) in state.iter_mut().zip(LEDS) {
        *slot = leds.contains(led);
    }
    state
}

/// A device is relevant only when it matches a configured `device-path`.
fn accepted(configured: Option<&str>, path: &Path) -> bool {
    configured.is_none_or(|configured| Path::new(configured) == path)
}

/// Open the lock-capable devices, replacing whatever was tracked before.
fn scan_devices(
    devices: &mut Vec<LockDevice>,
    configured: Option<&str>,
    warned: &mut HashMap<String, ()>,
) {
    devices.clear();
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

/// Open one event node and keep it when it supports the lock LEDs.
///
/// Returns `false` when the node could not be opened (it may not be ready yet),
/// so the caller can retry; a node that opens but has no lock LEDs is settled.
fn add_device(
    devices: &mut Vec<LockDevice>,
    path: &Path,
    configured: Option<&str>,
    warned: &mut HashMap<String, ()>,
) -> bool {
    if !accepted(configured, path) || devices.iter().any(|entry| entry.path == path) {
        return true;
    }
    match open_lock_device(path, warned) {
        Ok(Some(device)) => {
            devices.push(LockDevice {
                path: path.to_path_buf(),
                device,
            });
            true
        }
        Ok(None) => true,
        Err(()) => false,
    }
}

/// Open a hotplugged node, retrying while the device file is not ready yet.
fn add_device_later(
    devices: &mut Vec<LockDevice>,
    path: PathBuf,
    configured: Option<&str>,
    warned: &mut HashMap<String, ()>,
) {
    for _ in 0..ADD_ATTEMPTS {
        if add_device(devices, &path, configured, warned) {
            return;
        }
        std::thread::sleep(ADD_RETRY_DELAY);
    }
    debug!(
        "keyboard-state: giving up on {} after {ADD_ATTEMPTS} attempts",
        path.display()
    );
}

/// Whether `path` is the node behind an `eventN` sysname.
fn is_sysname(path: &Path, sysname: &str) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some(sysname)
}

/// Drop the device behind an `eventN` sysname.
fn remove_device(devices: &mut Vec<LockDevice>, sysname: &str) {
    devices.retain(|entry| !is_sysname(&entry.path, sysname));
}

/// Open `path`, returning `Ok(Some(device))` when it supports the lock LEDs,
/// `Ok(None)` when it is not likewise capable, and `Err(())` when opening it
/// failed (for example the permission is missing or the node is not ready).
fn open_lock_device(path: &Path, warned: &mut HashMap<String, ()>) -> Result<Option<Device>, ()> {
    let device = match Device::open(path) {
        Ok(device) => device,
        Err(err) => {
            if err.kind() == std::io::ErrorKind::PermissionDenied
                && !warned.contains_key(&path.display().to_string())
            {
                warn!(
                    "keyboard-state: cannot open {} (are you in the input group?): {err}",
                    path.display()
                );
                warned.insert(path.display().to_string(), ());
            }
            return Err(());
        }
    };
    let supports = device
        .supported_leds()
        .is_some_and(|leds| LEDS.iter().all(|led| leds.contains(*led)));
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
        let config = &init.config;
        let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        box_.set_widget_name("keyboard-state");
        box_.add_css_class("module");
        if !init.id.is_empty() {
            box_.add_css_class(&init.id);
        }

        let icons = key_state_to_icons(config);
        let common = is_common_format_icons(config);
        let mut labels = Vec::new();
        for (led, key, name, class) in enabled_keys(config) {
            // A key without icons gets no label at all, rather than an empty one.
            let map_key = if common { "Lock" } else { name };
            let Some(pair) = icons.get(map_key) else {
                continue;
            };
            let label = gtk::Label::new(None);
            label.add_css_class(class);
            labels.push(KeyLabel {
                led,
                name,
                format: key_format(config, key),
                icons: pair.clone(),
                label,
            });
        }
        // Upstream packs with `pack_end` (numlock first, so it ends up on the
        // right); appending in reverse reproduces the same left-to-right order.
        for entry in labels.iter().rev() {
            box_.append(&entry.label);
        }

        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.append(&box_);

        Ok(Box::new(KeyboardState {
            root,
            device_path: config
                .get("device-path")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            interval: interval_secs(config),
            labels: RefCell::new(labels),
            thread: RefCell::new(None),
            timer: RefCell::new(None),
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

    #[test]
    fn interval_option() {
        assert_eq!(interval_secs(&json!({})), 1);
        assert_eq!(interval_secs(&json!({"interval": 2})), 2);
        assert_eq!(interval_secs(&json!({"interval": "3"})), 3);
        assert_eq!(interval_secs(&json!({"interval": 0})), 0);
        assert_eq!(interval_secs(&json!({"interval": "once"})), 0);
        // A fractional period is rounded, and nonsense falls back to the default.
        assert_eq!(interval_secs(&json!({"interval": 0.4})), 1);
        assert_eq!(interval_secs(&json!({"interval": "nonsense"})), 1);
    }

    /// Every key must map to its own LED, whatever else is enabled.
    #[test]
    fn keys_keep_their_led_index() {
        let all: Vec<usize> = enabled_keys(&json!({
            "numlock": true, "capslock": true, "scrolllock": true
        }))
        .map(|(led, ..)| led)
        .collect();
        assert_eq!(all, [0, 1, 2]);

        // A single enabled key must not inherit the LED of a disabled one.
        let caps: Vec<usize> = enabled_keys(&json!({"capslock": true}))
            .map(|(led, ..)| led)
            .collect();
        assert_eq!(caps, [1]);
        let scroll: Vec<usize> = enabled_keys(&json!({"scrolllock": true}))
            .map(|(led, ..)| led)
            .collect();
        assert_eq!(scroll, [2]);
        assert!(enabled_keys(&json!({})).next().is_none());
    }

    #[test]
    fn led_flags_follow_key_order() {
        let only_caps = AttributeSet::from_iter([LedCode::LED_CAPSL]);
        assert_eq!(led_flags(&only_caps), [false, true, false]);
        let all =
            AttributeSet::from_iter([LedCode::LED_NUML, LedCode::LED_CAPSL, LedCode::LED_SCROLLL]);
        assert_eq!(led_flags(&all), [true, true, true]);
        assert_eq!(led_flags(&AttributeSet::new()), [false; 3]);
    }

    #[test]
    fn sysname_matching() {
        assert!(is_sysname(Path::new("/dev/input/event3"), "event3"));
        assert!(!is_sysname(Path::new("/dev/input/event3"), "event4"));
        assert!(!is_sysname(Path::new("/dev/input/mice"), "event3"));
    }
}
