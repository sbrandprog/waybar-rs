//! `network` — current connection state, driven by NetworkManager.
//!
//! State comes from `nmrs` (NetworkManager over D-Bus): the active connection
//! supplies the SSID/BSSID/signal, the device supplies the address, link speed
//! and frequency. D-Bus signals refresh the module immediately and a timer
//! refreshes the signal strength and the `/proc/net/dev` bandwidth counters.
//!
//! Upstream Waybar derives the same values from netlink/nl80211; the fields
//! that need a scan (`{rxBitrate}`/`{txBitrate}`) are approximated from the
//! active access point's maximum rate.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use futures_lite::StreamExt;
use gtk::prelude::*;
use nmrs::{ActiveConnection, Device, DeviceState, NetworkManager};
use serde_json::Value;
use tracing::{debug, warn};

use waybar_core::bridge;
use waybar_core::format::{self as fmt, Args, FmtArg, Pow};
use waybar_core::{Module, ModuleInit, Registry};

use crate::label::LabelModule;
use crate::sysinfo::net as netinfo;

const DEFAULT_FORMAT: &str = "{ifname}";
const NETDEV_FILE: &str = "/proc/net/dev";
const ROUTE_FILE: &str = "/proc/net/route";

/// Address family used for `{ipaddr}`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Family {
    Ipv4,
    Ipv6,
    Ipv4And6,
}

/// Parsed, static module configuration.
#[derive(Clone, Debug)]
struct NetConfig {
    interface: Option<String>,
    rfkill: bool,
    family: Family,
    interval: u64,
    want_bitrate: bool,
}

impl NetConfig {
    fn parse(config: &Value) -> Self {
        let family = match config.get("family").and_then(|v| v.as_str()) {
            Some("ipv6") => Family::Ipv6,
            Some("ipv4_6") => Family::Ipv4And6,
            _ => Family::Ipv4,
        };
        NetConfig {
            interface: config
                .get("interface")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            rfkill: config
                .get("rfkill")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            family,
            interval: config
                .get("interval")
                .and_then(|v| v.as_u64())
                .filter(|secs| *secs > 0)
                .unwrap_or(60),
            want_bitrate: config_uses(config, "rxBitrate") || config_uses(config, "txBitrate"),
        }
    }
}

/// Whether any string option references the `{key...}` placeholder.
fn config_uses(config: &Value, key: &str) -> bool {
    let needle = format!("{{{key}");
    config.as_object().is_some_and(|object| {
        object
            .values()
            .any(|v| v.as_str().is_some_and(|s| s.contains(&needle)))
    })
}

/// A plain, `Send` snapshot of the current connection.
#[derive(Clone, Debug, Default)]
struct NetState {
    ifname: String,
    state: String,
    wireless: bool,
    essid: String,
    bssid: String,
    ipaddr: String,
    cidr: u32,
    netmask: String,
    ipaddr6: String,
    cidr6: u32,
    netmask6: String,
    gwaddr: String,
    signal_dbm: i32,
    signal: u8,
    signal_app: String,
    frequency_ghz: f64,
    link_speed_mbps: u64,
    rx_bitrate: f64,
    tx_bitrate: f64,
    bandwidth_down: u64,
    bandwidth_up: u64,
}

/// Main-thread UI state.
struct Ui {
    data: NetState,
    state_class: Option<String>,
    alt: bool,
}

struct Network {
    inner: LabelModule,
    ui: Rc<RefCell<Ui>>,
    config: Value,
}

impl Module for Network {
    fn widget(&self) -> gtk::Widget {
        self.inner.widget()
    }

    fn start(&self) {
        let (tx, rx) = bridge::channel::<NetState>();
        {
            let config = self.config.clone();
            bridge::spawn(worker(config, tx));
        }

        // `format-alt` toggles on the configured button (left by default).
        if let Some(button) = alt_click_button(&self.config) {
            let ui = self.ui.clone();
            let inner = self.inner.clone();
            let gesture = gtk::GestureClick::new();
            gesture.set_button(button);
            gesture.connect_pressed(move |_, n_press, _, _| {
                if n_press != 1 {
                    return;
                }
                let mut ui = ui.borrow_mut();
                ui.alt = !ui.alt;
                render(&inner, &mut ui);
            });
            self.inner.widget().add_controller(gesture);
        }

        let inner = self.inner.clone();
        let ui = self.ui.clone();
        bridge::on_main(rx, move |state| {
            let mut ui = ui.borrow_mut();
            ui.data = state;
            render(&inner, &mut ui);
        });
    }
}

// --- worker -------------------------------------------------------------

async fn worker(config: Value, tx: async_channel::Sender<NetState>) {
    let parsed = NetConfig::parse(&config);
    let nm = match NetworkManager::new().await {
        Ok(nm) => nm,
        Err(err) => {
            warn!("network: cannot connect to NetworkManager: {err}");
            let _ = tx.send(NetState::default()).await;
            return;
        }
    };

    let mut bandwidth = Bandwidth::new(parsed.interval);
    refresh(&nm, &parsed, &mut bandwidth, true, &tx).await;

    let mut events = match nm.network_events().await {
        Ok(events) => events,
        Err(err) => {
            warn!("network: cannot subscribe to NetworkManager events: {err}");
            return;
        }
    };
    let mut ticker = tokio::time::interval(Duration::from_secs(parsed.interval));
    ticker.tick().await;

    loop {
        tokio::select! {
            item = events.next() => match item {
                Some(Ok(_)) => refresh(&nm, &parsed, &mut bandwidth, false, &tx).await,
                Some(Err(err)) => debug!("network: event error: {err}"),
                None => break,
            },
            _ = ticker.tick() => {
                refresh(&nm, &parsed, &mut bandwidth, true, &tx).await;
            }
        }
    }
}

async fn refresh(
    nm: &NetworkManager,
    parsed: &NetConfig,
    bandwidth: &mut Bandwidth,
    sample_bandwidth: bool,
    tx: &async_channel::Sender<NetState>,
) {
    let devices = nm.list_devices().await.unwrap_or_default();
    let active = nm.list_active_connections().await.unwrap_or_default();
    let route = std::fs::read_to_string(ROUTE_FILE)
        .ok()
        .and_then(|content| netinfo::parse_default_route(&content));

    let mut state = NetState::default();
    if let Some(device) = select_device(
        &devices,
        parsed,
        route.as_ref().map(|(iface, _)| iface.as_str()),
    ) {
        fill_from_device(&mut state, device, &active);
        if state.wireless {
            if parsed.rfkill
                && state.state == "disconnected"
                && nm.wifi_state().await.is_ok_and(|radio| !radio.enabled)
            {
                state.state = "disabled".to_string();
            }
            if parsed.want_bitrate && state.state == "wifi" {
                fill_bitrate(nm, &mut state).await;
            }
        }
        if let Some((iface, gateway)) = &route
            && iface == &state.ifname
        {
            state.gwaddr = gateway.clone();
        }
    } else {
        state.state = "disconnected".to_string();
    }

    let (down, up) = bandwidth.sample(sample_bandwidth, &state.ifname);
    state.bandwidth_down = down;
    state.bandwidth_up = up;

    debug!(
        "network: {} state={} ip={}/{} essid={}",
        state.ifname, state.state, state.ipaddr, state.cidr, state.essid
    );
    let _ = tx.send(state).await;
}

/// Pick the interface to display: an explicit `interface` match wins, then the
/// default-route interface, then the first activated wired/wireless device.
fn select_device<'a>(
    devices: &'a [Device],
    parsed: &NetConfig,
    route_iface: Option<&str>,
) -> Option<&'a Device> {
    if let Some(pattern) = &parsed.interface {
        return devices
            .iter()
            .find(|device| wildcard_match(pattern, &device.interface));
    }
    if let Some(route_iface) = route_iface
        && let Some(device) = devices
            .iter()
            .find(|device| device.interface == route_iface)
    {
        return Some(device);
    }
    devices.iter().find(|device| {
        device.state == DeviceState::Activated
            && !device.is_loopback()
            && (device.is_wireless() || device.is_wired())
    })
}

fn fill_from_device(state: &mut NetState, device: &Device, active: &[ActiveConnection]) {
    state.ifname = device.interface.clone();
    state.wireless = device.is_wireless();

    if let Some((ip, cidr)) = device.ip4_address.as_deref().and_then(netinfo::parse_cidr) {
        state.ipaddr = ip;
        state.cidr = cidr;
        state.netmask = netinfo::netmask_from_cidr(cidr);
    }
    if let Some((ip, cidr)) = device.ip6_address.as_deref().and_then(netinfo::parse_cidr) {
        state.ipaddr6 = ip;
        state.cidr6 = cidr;
        state.netmask6 = netinfo::netmask6_from_cidr(cidr);
    }
    if let Some(frequency) = device.frequency {
        state.frequency_ghz = f64::from(frequency) / 1000.0;
    }
    state.link_speed_mbps = u64::from(device.speed_mbps.unwrap_or(0));

    for connection in active {
        if let ActiveConnection::Wifi(wifi) = connection
            && wifi.interface.as_deref() == Some(device.interface.as_str())
        {
            state.essid = wifi.ssid.clone();
            state.bssid = wifi.bssid.clone().unwrap_or_default();
            state.signal = wifi.strength.unwrap_or(0);
        }
    }
    if state.signal > 0 {
        // Invert NetworkManager's strength formula to recover an approximate
        // dBm value (upstream reports the raw mBm).
        state.signal_dbm = i32::from(state.signal) - 120;
        state.signal_app = netinfo::signal_strength_app(state.signal_dbm).to_string();
    }

    // A device with carrier up may still be waiting for an address, which
    // upstream reports as `linked`. Treat the pre-activation IP states as
    // "carrier up" so the linked/ethernet/wifi split matches.
    let carrier = matches!(
        device.state,
        DeviceState::Activated
            | DeviceState::Config
            | DeviceState::IpConfig
            | DeviceState::IpCheck
            | DeviceState::Secondaries
    );
    state.state = if !carrier {
        "disconnected"
    } else if state.ipaddr.is_empty() && state.ipaddr6.is_empty() {
        "linked"
    } else if state.essid.is_empty() {
        "ethernet"
    } else {
        "wifi"
    }
    .to_string();
}

async fn fill_bitrate(nm: &NetworkManager, state: &mut NetState) {
    let Ok(points) = nm.list_access_points(Some(&state.ifname)).await else {
        return;
    };
    if let Some(ap) = points.into_iter().find(|ap| ap.is_active) {
        let bits = f64::from(ap.max_bitrate_kbps) * 1000.0;
        state.rx_bitrate = bits;
        state.tx_bitrate = bits;
    }
}

/// Sliding bandwidth sampler mirroring upstream's elapsed-time guard.
struct Bandwidth {
    interval: u64,
    last_time: Instant,
    last_rx: u64,
    last_tx: u64,
    prev_down: u64,
    prev_up: u64,
    ready: bool,
}

impl Bandwidth {
    fn new(interval: u64) -> Self {
        Bandwidth {
            interval,
            last_time: Instant::now(),
            last_rx: 0,
            last_tx: 0,
            prev_down: 0,
            prev_up: 0,
            ready: false,
        }
    }

    fn sample(&mut self, allow: bool, ifname: &str) -> (u64, u64) {
        if ifname.is_empty() || (self.ready && !allow) {
            return (self.prev_down, self.prev_up);
        }
        let now = Instant::now();
        let Some((rx, tx)) = read_netdev(ifname) else {
            return (self.prev_down, self.prev_up);
        };
        if !self.ready {
            self.last_rx = rx;
            self.last_tx = tx;
            self.last_time = now;
            self.ready = true;
            return (0, 0);
        }

        let elapsed = now.duration_since(self.last_time).as_secs_f64();
        let min_elapsed = self.interval as f64 * 0.5;
        if allow && elapsed >= min_elapsed && elapsed > 0.0 {
            let down = rx.saturating_sub(self.last_rx);
            let up = tx.saturating_sub(self.last_tx);
            self.last_rx = rx;
            self.last_tx = tx;
            self.last_time = now;
            self.prev_down = (down as f64 / elapsed) as u64;
            self.prev_up = (up as f64 / elapsed) as u64;
        }
        (self.prev_down, self.prev_up)
    }
}

fn read_netdev(ifname: &str) -> Option<(u64, u64)> {
    let content = std::fs::read_to_string(NETDEV_FILE).ok()?;
    netinfo::parse_netdev(&content, ifname)
}

// --- rendering ----------------------------------------------------------

fn render(inner: &LabelModule, ui: &mut Ui) {
    let data = &ui.data;
    let threshold = if ui.alt {
        String::new()
    } else {
        inner.get_state(data.signal, false)
    };

    let format = if ui.alt {
        inner
            .config()
            .get("format-alt")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_FORMAT)
            .to_string()
    } else {
        resolve_format(inner, &data.state, &threshold)
    };

    if !ui.alt {
        if let Some(previous) = ui.state_class.take() {
            inner.set_class(&previous, false);
        }
        if !data.state.is_empty() {
            inner.set_class(&data.state, true);
            ui.state_class = Some(data.state.clone());
        }
    }

    let args = build_args(inner, data);
    let text = fmt::format(&format, &args);
    if text.is_empty() {
        inner.set_visible(false);
        return;
    }
    inner.set_markup(&text);

    let tooltip_format = if ui.alt {
        String::new()
    } else {
        resolve_tooltip_format(inner, &data.state, &threshold)
    };
    let tooltip = if tooltip_format.is_empty() {
        text.clone()
    } else {
        fmt::format(&tooltip_format, &args)
    };
    inner.set_tooltip(&tooltip);
    inner.set_visible(true);
}

/// `format-<state>-<threshold>` -> `format-<state>` -> `format` -> default.
fn resolve_format(inner: &LabelModule, state: &str, threshold: &str) -> String {
    let config = inner.config();
    if !threshold.is_empty()
        && let Some(format) = config
            .get(format!("format-{state}-{threshold}"))
            .and_then(|v| v.as_str())
    {
        return format.to_string();
    }
    if !state.is_empty()
        && let Some(format) = config
            .get(format!("format-{state}"))
            .and_then(|v| v.as_str())
    {
        return format.to_string();
    }
    if let Some(format) = config.get("format").and_then(|v| v.as_str()) {
        return format.to_string();
    }
    DEFAULT_FORMAT.to_string()
}

/// `tooltip-format-<state>-<threshold>` -> `tooltip-format-<state>` ->
/// `tooltip-format` (empty means "use the rendered text").
fn resolve_tooltip_format(inner: &LabelModule, state: &str, threshold: &str) -> String {
    let config = inner.config();
    if !threshold.is_empty()
        && let Some(format) = config
            .get(format!("tooltip-format-{state}-{threshold}"))
            .and_then(|v| v.as_str())
    {
        return format.to_string();
    }
    if !state.is_empty()
        && let Some(format) = config
            .get(format!("tooltip-format-{state}"))
            .and_then(|v| v.as_str())
    {
        return format.to_string();
    }
    config
        .get("tooltip-format")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn build_args(inner: &LabelModule, data: &NetState) -> Args {
    let family = NetConfig::parse(inner.config()).family;
    let final_ipaddr = match family {
        Family::Ipv4 => data.ipaddr.clone(),
        Family::Ipv6 => data.ipaddr6.clone(),
        Family::Ipv4And6 => format!("{}\n{}", data.ipaddr, data.ipaddr6),
    };

    let icon = inner.get_icon(u16::from(data.signal), &[&data.state], 0);
    let down = data.bandwidth_down;
    let up = data.bandwidth_up;
    let total = down + up;

    let mut args = Args::new();
    args.insert("essid".into(), FmtArg::from(data.essid.clone()));
    args.insert("bssid".into(), FmtArg::from(data.bssid.clone()));
    args.insert("signaldBm".into(), FmtArg::Int(i64::from(data.signal_dbm)));
    args.insert("signalStrength".into(), FmtArg::Int(i64::from(data.signal)));
    args.insert(
        "signalStrengthApp".into(),
        FmtArg::from(data.signal_app.clone()),
    );
    args.insert("ifname".into(), FmtArg::from(data.ifname.clone()));
    args.insert("netmask".into(), FmtArg::from(data.netmask.clone()));
    args.insert("netmask6".into(), FmtArg::from(data.netmask6.clone()));
    args.insert("ipaddr".into(), FmtArg::from(final_ipaddr));
    args.insert("gwaddr".into(), FmtArg::from(data.gwaddr.clone()));
    args.insert("cidr".into(), FmtArg::Int(i64::from(data.cidr)));
    args.insert("cidr6".into(), FmtArg::Int(i64::from(data.cidr6)));
    args.insert(
        "frequency".into(),
        FmtArg::from(format!("{:.1}", data.frequency_ghz)),
    );
    args.insert("icon".into(), FmtArg::from(icon));
    args.insert(
        "bandwidthDownBits".into(),
        FmtArg::Pow(Pow::new((down * 8) as i64, "b/s")),
    );
    args.insert(
        "bandwidthUpBits".into(),
        FmtArg::Pow(Pow::new((up * 8) as i64, "b/s")),
    );
    args.insert(
        "bandwidthTotalBits".into(),
        FmtArg::Pow(Pow::new((total * 8) as i64, "b/s")),
    );
    args.insert(
        "bandwidthDownOctets".into(),
        FmtArg::Pow(Pow::new(down as i64, "o/s")),
    );
    args.insert(
        "bandwidthUpOctets".into(),
        FmtArg::Pow(Pow::new(up as i64, "o/s")),
    );
    args.insert(
        "bandwidthTotalOctets".into(),
        FmtArg::Pow(Pow::new(total as i64, "o/s")),
    );
    args.insert(
        "bandwidthDownBytes".into(),
        FmtArg::Pow(Pow::new(down as i64, "B/s")),
    );
    args.insert(
        "bandwidthUpBytes".into(),
        FmtArg::Pow(Pow::new(up as i64, "B/s")),
    );
    args.insert(
        "bandwidthTotalBytes".into(),
        FmtArg::Pow(Pow::new(total as i64, "B/s")),
    );
    args.insert(
        "bandwidthDownBytesCompact".into(),
        FmtArg::Pow(Pow::new(down as i64, "B").min_pow_for_decimal(2)),
    );
    args.insert(
        "bandwidthUpBytesCompact".into(),
        FmtArg::Pow(Pow::new(up as i64, "B").min_pow_for_decimal(2)),
    );
    args.insert(
        "rxBitrate".into(),
        FmtArg::Pow(Pow::new(data.rx_bitrate as i64, "b/s")),
    );
    args.insert(
        "txBitrate".into(),
        FmtArg::Pow(Pow::new(data.tx_bitrate as i64, "b/s")),
    );
    args.insert(
        "linkSpeed".into(),
        FmtArg::Pow(Pow::new((data.link_speed_mbps * 1_000_000) as i64, "b/s").skip_decimal(true)),
    );
    args
}

/// The GTK button that toggles `format-alt`, mirroring `bar.cpp`'s
/// normalisation (left button when unset).
fn alt_click_button(config: &Value) -> Option<u32> {
    config.get("format-alt").and_then(|v| v.as_str())?;
    match config.get("format-alt-click") {
        Some(value) if value.is_u64() => value.as_u64().map(|button| button as u32),
        Some(Value::String(name)) => Some(match name.as_str() {
            "click-right" => 3,
            "click-middle" => 2,
            "click-backward" => 8,
            "click-forward" => 9,
            _ => 1,
        }),
        _ => Some(1),
    }
}

/// Glob matching with `*` and `?`, ported from upstream's `wildcardMatch`.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    let (mut fallback_p, mut fallback_t) = (None, 0usize);

    while t < text.len() {
        if p < pattern.len() && pattern[p] == '*' {
            fallback_p = Some(p);
            fallback_t = t;
            p += 1;
        } else if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if let Some(fallback) = fallback_p {
            p = fallback + 1;
            fallback_t += 1;
            t = fallback_t;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

pub fn register(reg: &mut Registry) {
    reg.register("network", |init: &ModuleInit| {
        Ok(Box::new(Network {
            inner: LabelModule::new("network", &init.id, &init.config),
            ui: Rc::new(RefCell::new(Ui {
                data: NetState::default(),
                state_class: None,
                alt: false,
            })),
            config: init.config.clone(),
        }))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wildcard_matches_like_upstream() {
        assert!(wildcard_match("wlan*", "wlan0"));
        assert!(wildcard_match("wl?n0", "wlan0"));
        assert!(wildcard_match("en*", "enp3s0"));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("wlan0", "wlan0"));
        assert!(!wildcard_match("eth*", "wlan0"));
        assert!(!wildcard_match("wlan", "wlan0"));
        assert!(!wildcard_match("w?an0", "wlan00"));
    }

    #[test]
    fn alt_button_defaults_to_left() {
        assert_eq!(alt_click_button(&json!({})), None);
        assert_eq!(alt_click_button(&json!({"format-alt": "x"})), Some(1));
        assert_eq!(
            alt_click_button(&json!({"format-alt": "x", "format-alt-click": "click-right"})),
            Some(3)
        );
        assert_eq!(
            alt_click_button(&json!({"format-alt": "x", "format-alt-click": 2})),
            Some(2)
        );
    }

    #[test]
    fn detects_placeholder_use() {
        assert!(config_uses(&json!({"format": "{rxBitrate}"}), "rxBitrate"));
        assert!(!config_uses(&json!({"format": "{essid}"}), "rxBitrate"));
        assert!(config_uses(
            &json!({"tooltip-format-wifi": "tx {txBitrate}"}),
            "txBitrate"
        ));
    }

    #[test]
    fn family_selection() {
        let config = NetConfig::parse(&json!({"family": "ipv6", "interval": 10}));
        assert_eq!(config.family, Family::Ipv6);
        assert_eq!(config.interval, 10);
        let config = NetConfig::parse(&json!({"family": "ipv4_6"}));
        assert_eq!(config.family, Family::Ipv4And6);
        let config = NetConfig::parse(&json!({}));
        assert_eq!(config.family, Family::Ipv4);
        assert_eq!(config.interval, 60);
    }
}
