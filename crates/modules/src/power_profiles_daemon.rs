//! `power-profiles-daemon` — show and cycle the active power profile.
//!
//! Talks to `net.hadess.PowerProfiles` on the system bus through a `zbus`
//! proxy: reads `ActiveProfile`/`Profiles`, listens for `ActiveProfile`
//! changes, and writes the property when the module is clicked. The active
//! profile name is also applied as a CSS class, matching upstream.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use futures_lite::StreamExt;
use tracing::{debug, warn};

use waybar_core::bridge;
use waybar_core::format::{Args, FmtArg};
use waybar_core::widgets::attach_single_click;
use waybar_core::{Module, ModuleInit, Registry};

use crate::label::LabelModule;

/// Delay before rebuilding the D-Bus session after a failure.
const RETRY: Duration = Duration::from_secs(2);

#[zbus::proxy(
    interface = "net.hadess.PowerProfiles",
    default_service = "net.hadess.PowerProfiles",
    default_path = "/net/hadess/PowerProfiles"
)]
trait PowerProfiles {
    #[zbus(property)]
    fn active_profile(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn set_active_profile(&self, active_profile: &str) -> zbus::Result<()>;

    #[zbus(property)]
    fn profiles(&self) -> zbus::Result<Vec<HashMap<String, String>>>;
}

/// One entry of the `Profiles` property.
#[derive(Clone, Debug, Default, PartialEq)]
struct ProfileInfo {
    name: String,
    driver: String,
    cpu_driver: String,
    platform_driver: String,
}

/// A snapshot sent from the worker to the main thread.
#[derive(Clone, Debug, Default)]
struct ProfileState {
    connected: bool,
    active: ProfileInfo,
    profiles: Vec<ProfileInfo>,
}

/// A request from the main thread to the worker.
#[derive(Clone, Debug)]
enum Cmd {
    Set(String),
}

/// Main-thread UI state: the last snapshot plus the applied CSS class.
#[derive(Default)]
struct Ui {
    data: ProfileState,
    class: Option<String>,
}

struct PowerProfilesDaemon {
    inner: LabelModule,
    ui: Rc<RefCell<Ui>>,
    cmd_tx: async_channel::Sender<Cmd>,
    cmd_rx: RefCell<Option<async_channel::Receiver<Cmd>>>,
    stop_tx: async_channel::Sender<()>,
    stop_rx: RefCell<Option<async_channel::Receiver<()>>>,
}

impl Drop for PowerProfilesDaemon {
    fn drop(&mut self) {
        let _ = self.stop_tx.try_send(());
    }
}

impl Module for PowerProfilesDaemon {
    fn widget(&self) -> gtk::Widget {
        self.inner.widget()
    }

    fn start(&self) {
        let (state_tx, state_rx) = bridge::channel::<ProfileState>();
        let cmd_rx = self.cmd_rx.borrow_mut().take();

        if let (Some(cmd_rx), Some(stop_rx)) = (cmd_rx, self.stop_rx.borrow_mut().take()) {
            bridge::spawn(worker(state_tx, cmd_rx, stop_rx));
        }

        // Left click cycles forward, any other button cycles backward. The label
        // is advanced optimistically so a quick second click computes the next
        // target from the new profile, not the stale one.
        let inner = self.inner.clone();
        let ui = self.ui.clone();
        let cmd_tx = self.cmd_tx.clone();
        attach_single_click(
            &self.inner.widget(),
            self.inner.config(),
            0,
            move |button| {
                let mut ui = ui.borrow_mut();
                if !ui.data.connected || ui.data.profiles.is_empty() {
                    return;
                }
                let len = ui.data.profiles.len();
                let index = ui
                    .data
                    .profiles
                    .iter()
                    .position(|profile| profile.name == ui.data.active.name)
                    .unwrap_or(0);
                let next = if button == 1 {
                    (index + 1) % len
                } else {
                    (index + len - 1) % len
                };
                let target = ui.data.profiles[next].clone();
                let name = target.name.clone();
                ui.data.active = target;
                render(&inner, &mut ui);
                let _ = cmd_tx.try_send(Cmd::Set(name));
            },
        );

        let inner = self.inner.clone();
        let ui = self.ui.clone();
        bridge::on_main(state_rx, move |state| {
            let mut ui = ui.borrow_mut();
            ui.data = state;
            render(&inner, &mut ui);
        });
    }
}

async fn worker(
    state_tx: async_channel::Sender<ProfileState>,
    cmd_rx: async_channel::Receiver<Cmd>,
    stop: async_channel::Receiver<()>,
) {
    let mut warned = false;
    loop {
        if session(&state_tx, &cmd_rx, &stop, &mut warned).await {
            return;
        }
        // Keep the module hidden while the daemon is unreachable, then retry.
        let _ = state_tx.send(ProfileState::default()).await;
        if wait_or_stop(RETRY, &stop).await {
            return;
        }
    }
}

/// One proxy lifetime. Returns `true` when the module is being dropped.
///
/// A new proxy is built for every session: zbus caches a failed initial
/// `GetAll` per proxy, which would otherwise make it fail forever.
async fn session(
    state_tx: &async_channel::Sender<ProfileState>,
    cmd_rx: &async_channel::Receiver<Cmd>,
    stop: &async_channel::Receiver<()>,
    warned: &mut bool,
) -> bool {
    let conn = match zbus::Connection::system().await {
        Ok(conn) => conn,
        Err(err) => {
            report(
                warned,
                format_args!("cannot connect to the system bus: {err}"),
            );
            return false;
        }
    };
    let proxy = match PowerProfilesProxy::new(&conn).await {
        Ok(proxy) => proxy,
        Err(err) => {
            report(warned, format_args!("cannot create proxy: {err}"));
            return false;
        }
    };

    let mut owner_changes = match proxy.inner().receive_owner_changed().await {
        Ok(changes) => changes,
        Err(err) => {
            report(warned, format_args!("cannot watch the bus owner: {err}"));
            return false;
        }
    };
    let mut active_changes = proxy.receive_active_profile_changed().await;
    let mut profile_changes = proxy.receive_profiles_changed().await;

    let mut profiles = match fetch_profiles(&proxy).await {
        Ok(profiles) => profiles,
        Err(err) => {
            report(warned, format_args!("cannot read profiles: {err}"));
            return false;
        }
    };
    let active = match proxy.active_profile().await {
        Ok(active) => active,
        Err(err) => {
            report(
                warned,
                format_args!("cannot read the active profile: {err}"),
            );
            return false;
        }
    };

    *warned = false;
    debug!("power-profiles-daemon: connected, active profile `{active}`");
    if state_tx.send(make_state(&profiles, &active)).await.is_err() {
        return true;
    }

    loop {
        tokio::select! {
            _ = stop.recv() => return true,
            cmd = cmd_rx.recv() => match cmd {
                Ok(Cmd::Set(name)) => {
                    if let Err(err) = proxy.set_active_profile(&name).await {
                        warn!("power-profiles-daemon: cannot set `{name}`: {err}");
                        // Undo the optimistic update if the daemon still reports
                        // the old profile.
                        if let Ok(active) = proxy.active_profile().await
                            && state_tx.send(make_state(&profiles, &active)).await.is_err()
                        {
                            return true;
                        }
                    }
                }
                // The module is gone; its sender was dropped with it.
                Err(_) => return true,
            },
            // The daemon went away or was replaced. Rebuild the session so the
            // new instance gets a fresh property cache.
            owner = owner_changes.next() => {
                debug!("power-profiles-daemon: bus owner changed: {owner:?}");
                return false;
            },
            change = active_changes.next() => {
                if change.is_none() {
                    return false;
                }
                // A failed read keeps the last shown state; the next change retries.
                match proxy.active_profile().await {
                    Ok(active) => {
                        if state_tx.send(make_state(&profiles, &active)).await.is_err() {
                            return true;
                        }
                    }
                    Err(err) => debug!("power-profiles-daemon: cannot read the active profile: {err}"),
                }
            },
            change = profile_changes.next() => {
                if change.is_none() {
                    return false;
                }
                match fetch_profiles(&proxy).await {
                    Ok(list) => profiles = list,
                    Err(err) => {
                        debug!("power-profiles-daemon: cannot read profiles: {err}");
                        continue;
                    }
                }
                match proxy.active_profile().await {
                    Ok(active) => {
                        if state_tx.send(make_state(&profiles, &active)).await.is_err() {
                            return true;
                        }
                    }
                    Err(err) => debug!("power-profiles-daemon: cannot read the active profile: {err}"),
                }
            },
        }
    }
}

/// Log a session failure: loudly the first time, quietly while retrying.
fn report(warned: &mut bool, msg: std::fmt::Arguments<'_>) {
    if *warned {
        debug!("power-profiles-daemon: {msg}");
    } else {
        *warned = true;
        warn!("power-profiles-daemon: {msg}");
    }
}

/// Wait for `dur`, returning `true` when the module is dropped meanwhile.
async fn wait_or_stop(dur: Duration, stop: &async_channel::Receiver<()>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(dur) => false,
        _ = stop.recv() => true,
    }
}

async fn fetch_profiles(proxy: &PowerProfilesProxy<'_>) -> zbus::Result<Vec<ProfileInfo>> {
    let raw = proxy.profiles().await?;
    Ok(parse_profiles(&raw))
}

/// Decode `Profiles`, dropping entries without a name (they cannot be selected).
fn parse_profiles(raw: &[HashMap<String, String>]) -> Vec<ProfileInfo> {
    raw.iter()
        .map(parse_profile)
        .filter(|profile| !profile.name.is_empty())
        .collect()
}

fn make_state(profiles: &[ProfileInfo], active: &str) -> ProfileState {
    ProfileState {
        connected: true,
        active: resolve_active(profiles, active),
        profiles: profiles.to_vec(),
    }
}

/// `ActiveProfile` is authoritative; if it is missing from `Profiles` (the two
/// are read at different times), show it with `Unavailable` drivers.
fn resolve_active(profiles: &[ProfileInfo], active: &str) -> ProfileInfo {
    if active.is_empty() {
        return ProfileInfo::default();
    }
    match profiles.iter().find(|profile| profile.name == active) {
        Some(profile) => profile.clone(),
        None => ProfileInfo {
            name: active.to_string(),
            driver: "Unavailable".to_string(),
            cpu_driver: "Unavailable".to_string(),
            platform_driver: "Unavailable".to_string(),
        },
    }
}

/// Decode one `Profiles` entry, applying upstream's driver fallbacks.
fn parse_profile(raw: &HashMap<String, String>) -> ProfileInfo {
    let get = |key: &str| raw.get(key).cloned().unwrap_or_default();
    let name = get("Profile");
    let mut driver = get("Driver");
    let mut cpu_driver = get("CpuDriver");
    let mut platform_driver = get("PlatformDriver");

    // Older daemons only expose a single `Driver`; newer ones split it.
    if cpu_driver.is_empty() {
        cpu_driver = driver.clone();
    }
    if platform_driver.is_empty() {
        platform_driver = driver.clone();
    }
    if driver.is_empty() {
        driver = cpu_driver.clone();
    }
    if driver.is_empty() {
        driver = "Unavailable".to_string();
        cpu_driver = "Unavailable".to_string();
        platform_driver = "Unavailable".to_string();
    }

    ProfileInfo {
        name,
        driver,
        cpu_driver,
        platform_driver,
    }
}

fn render(inner: &LabelModule, ui: &mut Ui) {
    let profile = &ui.data.active;
    if !ui.data.connected || profile.name.is_empty() {
        if let Some(previous) = ui.class.take() {
            inner.set_class(&previous, false);
        }
        inner.set_visible(false);
        return;
    }

    if let Some(previous) = ui.class.take() {
        inner.set_class(&previous, false);
    }
    inner.set_class(&profile.name, true);
    ui.class = Some(profile.name.clone());

    let icon = inner.get_icon(0, &[profile.name.as_str()], 0);
    let mut args = Args::new();
    args.insert("profile".into(), FmtArg::from(profile.name.clone()));
    args.insert("driver".into(), FmtArg::from(profile.driver.clone()));
    args.insert(
        "cpu_driver".into(),
        FmtArg::from(profile.cpu_driver.clone()),
    );
    args.insert(
        "platform_driver".into(),
        FmtArg::from(profile.platform_driver.clone()),
    );
    args.insert("icon".into(), FmtArg::from(icon));

    let format = inner.resolve_format("{icon}", "");
    inner.set_markup(&inner.render(&format, &args));

    let default_tooltip =
        "Power profile: {profile}\nCPU driver: {cpu_driver}\nPlatform driver: {platform_driver}";
    let tooltip = inner.resolve_tooltip_format(default_tooltip, "");
    inner.set_tooltip(&inner.render(&tooltip, &args));
    inner.set_visible(true);
}

pub fn register(reg: &mut Registry) {
    reg.register("power-profiles-daemon", |init: &ModuleInit| {
        let (cmd_tx, cmd_rx) = bridge::channel::<Cmd>();
        let (stop_tx, stop_rx) = bridge::channel::<()>();
        Ok(Box::new(PowerProfilesDaemon {
            inner: LabelModule::new("power-profiles-daemon", &init.id, &init.config)
                .with_context(&init.name),
            ui: Rc::new(RefCell::new(Ui::default())),
            cmd_tx,
            cmd_rx: RefCell::new(Some(cmd_rx)),
            stop_tx,
            stop_rx: RefCell::new(Some(stop_rx)),
        }))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn profiles() -> Vec<ProfileInfo> {
        vec![
            parse_profile(&raw(&[("Profile", "balanced")])),
            parse_profile(&raw(&[("Profile", "performance")])),
        ]
    }

    #[test]
    fn parses_modern_split_drivers() {
        let profile = parse_profile(&raw(&[
            ("Profile", "balanced"),
            ("CpuDriver", "intel_pstate"),
            ("PlatformDriver", "placeholder"),
        ]));
        assert_eq!(profile.name, "balanced");
        assert_eq!(profile.driver, "intel_pstate");
        assert_eq!(profile.cpu_driver, "intel_pstate");
        assert_eq!(profile.platform_driver, "placeholder");
    }

    #[test]
    fn falls_back_to_legacy_driver() {
        let profile = parse_profile(&raw(&[("Profile", "power-saver"), ("Driver", "legacy")]));
        assert_eq!(profile.cpu_driver, "legacy");
        assert_eq!(profile.platform_driver, "legacy");
        assert_eq!(profile.driver, "legacy");
    }

    #[test]
    fn missing_driver_is_unavailable() {
        let profile = parse_profile(&raw(&[("Profile", "performance")]));
        assert_eq!(profile.driver, "Unavailable");
        assert_eq!(profile.cpu_driver, "Unavailable");
        assert_eq!(profile.platform_driver, "Unavailable");
    }

    #[test]
    fn resolves_active_profile() {
        let profiles = profiles();
        assert_eq!(resolve_active(&profiles, "performance").name, "performance");
        assert_eq!(resolve_active(&[], "balanced").name, "balanced");
        assert_eq!(resolve_active(&[], "").name, "");
    }

    #[test]
    fn keeps_unknown_active_profile_visible() {
        let profile = resolve_active(&profiles(), "power-saver");
        assert_eq!(profile.name, "power-saver");
        assert_eq!(profile.driver, "Unavailable");
    }

    #[test]
    fn state_lists_profiles_in_daemon_order() {
        let state = make_state(&profiles(), "performance");
        assert!(state.connected);
        assert_eq!(state.active.name, "performance");
        let names: Vec<_> = state.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["balanced", "performance"]);
        assert_eq!(state.profiles[1].driver, "Unavailable");
    }

    #[test]
    fn skips_nameless_profiles() {
        let raw_entries = [
            raw(&[("Profile", "balanced")]),
            raw(&[("Driver", "legacy")]),
        ];
        let parsed = parse_profiles(&raw_entries);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "balanced");
    }
}
