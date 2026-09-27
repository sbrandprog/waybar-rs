//! `power-profiles-daemon` — show and cycle the active power profile.
//!
//! Talks to `net.hadess.PowerProfiles` on the system bus through a `zbus`
//! proxy: reads `ActiveProfile`/`Profiles`, listens for `ActiveProfile`
//! changes, and writes the property when the module is clicked. The active
//! profile name is also applied as a CSS class, matching upstream.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use futures_lite::StreamExt;
use tracing::{debug, warn};

use waybar_core::bridge;
use waybar_core::format::{Args, FmtArg};
use waybar_core::widgets::attach_single_click;
use waybar_core::{Module, ModuleInit, Registry};

use crate::label::LabelModule;

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
    profiles: Vec<String>,
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
}

impl Module for PowerProfilesDaemon {
    fn widget(&self) -> gtk::Widget {
        self.inner.widget()
    }

    fn start(&self) {
        let (state_tx, state_rx) = bridge::channel::<ProfileState>();
        let cmd_rx = self.cmd_rx.borrow_mut().take();

        if let Some(cmd_rx) = cmd_rx {
            bridge::spawn(worker(state_tx, cmd_rx));
        }

        // Left click cycles forward, any other button cycles backward.
        let ui = self.ui.clone();
        let cmd_tx = self.cmd_tx.clone();
        attach_single_click(
            &self.inner.widget(),
            self.inner.config(),
            0,
            move |button| {
                let ui = ui.borrow();
                if !ui.data.connected || ui.data.profiles.is_empty() {
                    return;
                }
                let len = ui.data.profiles.len();
                let index = ui
                    .data
                    .profiles
                    .iter()
                    .position(|name| name == &ui.data.active.name)
                    .unwrap_or(0);
                let next = if button == 1 {
                    (index + 1) % len
                } else {
                    (index + len - 1) % len
                };
                let _ = cmd_tx.try_send(Cmd::Set(ui.data.profiles[next].clone()));
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
) {
    let conn = match zbus::Connection::system().await {
        Ok(conn) => conn,
        Err(err) => {
            warn!("power-profiles-daemon: cannot connect to the system bus: {err}");
            let _ = state_tx.send(ProfileState::default()).await;
            return;
        }
    };
    let proxy = match PowerProfilesProxy::new(&conn).await {
        Ok(proxy) => proxy,
        Err(err) => {
            warn!("power-profiles-daemon: cannot create proxy: {err}");
            let _ = state_tx.send(ProfileState::default()).await;
            return;
        }
    };

    // Read the initial profile list once; it is effectively static.
    let profiles = match proxy.profiles().await {
        Ok(raw) => raw.iter().map(parse_profile).collect::<Vec<_>>(),
        Err(err) => {
            debug!("power-profiles-daemon: cannot read profiles: {err}");
            Vec::new()
        }
    };
    let names: Vec<String> = profiles.iter().map(|p| p.name.clone()).collect();

    let mut changes = proxy.receive_active_profile_changed().await;

    let send = |active: ProfileInfo| {
        let profiles = names.clone();
        let state_tx = state_tx.clone();
        async move {
            debug!("power-profiles-daemon: active profile `{}`", active.name);
            let _ = state_tx
                .send(ProfileState {
                    connected: true,
                    active,
                    profiles,
                })
                .await;
        }
    };

    let active = resolve_active(&profiles, proxy.active_profile().await.ok());
    send(active).await;

    // Commands are handled on a separate task so the change stream stays simple.
    {
        let proxy = proxy.clone();
        tokio::spawn(async move {
            while let Ok(Cmd::Set(name)) = cmd_rx.recv().await {
                if let Err(err) = proxy.set_active_profile(&name).await {
                    warn!("power-profiles-daemon: cannot set `{name}`: {err}");
                }
            }
        });
    }

    while let Some(_change) = changes.next().await {
        let active = resolve_active(&profiles, proxy.active_profile().await.ok());
        send(active).await;
    }
}

/// Find the active profile in the list, falling back to the first entry.
fn resolve_active(profiles: &[ProfileInfo], active: Option<String>) -> ProfileInfo {
    if let Some(name) = active
        && let Some(profile) = profiles.iter().find(|p| p.name == name)
    {
        return profile.clone();
    }
    profiles.first().cloned().unwrap_or_default()
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
        Ok(Box::new(PowerProfilesDaemon {
            inner: LabelModule::new("power-profiles-daemon", &init.id, &init.config)
                .with_context(&init.name),
            ui: Rc::new(RefCell::new(Ui::default())),
            cmd_tx,
            cmd_rx: RefCell::new(Some(cmd_rx)),
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
        let profiles = vec![
            parse_profile(&raw(&[("Profile", "balanced")])),
            parse_profile(&raw(&[("Profile", "performance")])),
        ];
        assert_eq!(
            resolve_active(&profiles, Some("performance".into())).name,
            "performance"
        );
        assert_eq!(
            resolve_active(&profiles, Some("missing".into())).name,
            "balanced"
        );
        assert_eq!(resolve_active(&[], None).name, "");
    }
}
