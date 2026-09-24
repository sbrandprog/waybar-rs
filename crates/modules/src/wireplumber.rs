//! `wireplumber` — the default audio sink's volume, driven by PipeWire.
//!
//! The module opens its own PipeWire main loop on a background thread, watches
//! the `default` metadata for `default.audio.sink`, binds that node and
//! subscribes to its `Props` parameter (`channelVolumes` + `mute`). Scrolling
//! writes the parameter back.
//!
//! Upstream Waybar uses libwireplumber's `mixer-api`/`default-nodes-api`; the
//! values are the same, but the display volume is the cubic root of the linear
//! gain (`{volume} = round(cbrt(linear) * 100)`), not `linear^3`.
//!
//! Only the options the local config uses are ported. `only-physical`, the
//! secondary source (`{source_*}` / `format-source`) and reconnection handling
//! are intentionally left out.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk::prelude::*;
use pipewire::channel::Sender;
use pipewire::context::ContextRc;
use pipewire::device::{Device, DeviceChangeMask, DeviceListener};
use pipewire::main_loop::MainLoopRc;
use pipewire::metadata::{Metadata, MetadataListener};
use pipewire::node::{Node, NodeInfoRef, NodeListener};
use pipewire::registry::{GlobalObject, RegistryRc};
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::Pod;
use pipewire::spa::utils::dict::DictRef;
use pipewire::types::ObjectType;
use serde_json::Value;
use tracing::{debug, warn};

use waybar_core::bridge;
use waybar_core::format::{Args, FmtArg};
use waybar_core::{Module, ModuleInit, Registry};

use crate::label::LabelModule;
use crate::pw;

const DEFAULT_FORMAT: &str = "{volume}%";

/// A command sent from the GTK thread into the PipeWire loop.
#[derive(Clone, Copy, Debug)]
enum Cmd {
    /// Set the linear gain.
    SetVolume(f32),
}

/// A plain, `Send` snapshot of the current sink state.
#[derive(Clone, Debug, Default)]
struct Snapshot {
    volume: u8,
    muted: bool,
    bluetooth: bool,
    node_name: String,
    volume_linear: f64,
    volume_cubic: f64,
    volume_db: f64,
}

/// State owned by the PipeWire thread.
#[derive(Default)]
struct PwState {
    /// `node.name` of every node global, keyed by global id.
    node_names: HashMap<u32, String>,
    default_sink: Option<String>,
    metadata: Option<Metadata>,
    metadata_listener: Option<MetadataListener>,
    sink_id: Option<u32>,
    sink: Option<Node>,
    sink_listener: Option<NodeListener>,
    /// The sink's device and its active route (hardware volume/mute).
    device_id: Option<u32>,
    device: Option<Rc<Device>>,
    device_listener: Option<DeviceListener>,
    route_index: Option<i32>,
    route_device: Option<i32>,
    volumes: Vec<f32>,
    mute: bool,
    node_nick: String,
    node_desc: String,
    node_name: String,
    tx: Option<async_channel::Sender<Snapshot>>,
}

/// Main-thread UI state.
struct Ui {
    snapshot: Snapshot,
}

/// Parsed scroll options.
#[derive(Clone, Debug)]
struct ScrollConfig {
    step: f64,
    scale: String,
    max_volume: f64,
    reverse: bool,
    reverse_mouse: bool,
}

struct Wireplumber {
    inner: LabelModule,
    ui: Rc<RefCell<Ui>>,
    cmd: Rc<RefCell<Option<Sender<Cmd>>>>,
    scroll: ScrollConfig,
}

impl Module for Wireplumber {
    fn widget(&self) -> gtk::Widget {
        self.inner.widget()
    }

    fn start(&self) {
        let (tx, rx) = bridge::channel::<Snapshot>();
        let cmd = start_pipewire(tx);
        *self.cmd.borrow_mut() = Some(cmd);

        let inner = self.inner.clone();
        let ui = self.ui.clone();
        bridge::on_main(rx, move |snapshot| {
            ui.borrow_mut().snapshot = snapshot;
            render(&inner, &ui.borrow());
        });

        // Volume scroll, unless the user mapped `on-scroll-*` themselves (in
        // which case `attach_actions` already installed a command controller).
        if has_scroll_command(self.inner.config()) {
            return;
        }
        let controller = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let ui = self.ui.clone();
        let cmd = self.cmd.clone();
        let scroll = self.scroll.clone();
        controller.connect_scroll(move |controller, _dx, dy| {
            if dy == 0.0 {
                return gtk::glib::Propagation::Proceed;
            }
            let mut up = dy < 0.0;
            let reverse = match controller.unit() {
                gtk::gdk::ScrollUnit::Wheel => scroll.reverse_mouse,
                _ => scroll.reverse,
            };
            if reverse {
                up = !up;
            }
            let current = ui.borrow().snapshot.volume_linear;
            let linear =
                scroll_new_linear(current, up, scroll.step, &scroll.scale, scroll.max_volume);
            if let Some(cmd) = cmd.borrow().as_ref() {
                let _ = cmd.send(Cmd::SetVolume(linear as f32));
            }
            gtk::glib::Propagation::Stop
        });
        self.inner.widget().add_controller(controller);
    }
}

/// Whether an `on-scroll-*` command is configured (which overrides the volume
/// scroll, as in upstream).
fn has_scroll_command(config: &Value) -> bool {
    ["on-scroll-up", "on-scroll-down"]
        .iter()
        .any(|key| config.get(key).is_some_and(|v| v.is_string()))
}

/// Compute the new linear gain for a scroll in the configured scale.
///
/// Mirrors upstream's `handleScroll`: the current gain is converted into the
/// scroll domain (`cubic`, `cubic_percent`, `db` or `linear`), moved by `step`
/// and converted back, then clamped to `[0, (max_volume/100)^3]`.
fn scroll_new_linear(current: f64, up: bool, step: f64, scale: &str, max_volume: f64) -> f64 {
    let domain = match scale {
        "cubic" => current.cbrt(),
        "db" => {
            if current > 0.0 {
                20.0 * current.log10()
            } else {
                -60.0
            }
        }
        "cubic_percent" => current.cbrt() * 100.0,
        _ => current,
    };
    let moved = domain + if up { step } else { -step };
    let linear = match scale {
        "cubic" => moved.powi(3),
        "db" => 10f64.powf(moved / 20.0),
        "cubic_percent" => (moved / 100.0).powi(3),
        _ => moved,
    };
    let max_linear = (max_volume / 100.0).powi(3);
    linear.clamp(0.0, max_linear)
}

// --- PipeWire thread ----------------------------------------------------

fn start_pipewire(tx: async_channel::Sender<Snapshot>) -> Sender<Cmd> {
    let (cmd_tx, cmd_rx) = pipewire::channel::channel::<Cmd>();
    pw::spawn(move || run(tx, cmd_rx));
    cmd_tx
}

fn run(tx: async_channel::Sender<Snapshot>, cmd_rx: pipewire::channel::Receiver<Cmd>) {
    let main_loop = match MainLoopRc::new(None) {
        Ok(main_loop) => main_loop,
        Err(err) => {
            warn!("wireplumber: cannot create PipeWire main loop: {err}");
            return;
        }
    };
    let context = match ContextRc::new(&main_loop, None) {
        Ok(context) => context,
        Err(err) => {
            warn!("wireplumber: cannot create PipeWire context: {err}");
            return;
        }
    };
    let core = match context.connect_rc(None) {
        Ok(core) => core,
        Err(err) => {
            warn!("wireplumber: cannot connect to PipeWire: {err}");
            let _ = tx.try_send(Snapshot::default());
            return;
        }
    };
    let registry = match core.get_registry_rc() {
        Ok(registry) => registry,
        Err(err) => {
            warn!("wireplumber: cannot get the PipeWire registry: {err}");
            return;
        }
    };

    let state = Rc::new(RefCell::new(PwState {
        tx: Some(tx),
        ..Default::default()
    }));

    let _registry_listener = {
        let state_global = state.clone();
        let registry_global = registry.clone();
        let state_remove = state.clone();
        registry
            .add_listener_local()
            .global(move |global| on_global(&state_global, &registry_global, global))
            .global_remove(move |id| on_global_remove(&state_remove, id))
            .register()
    };

    let _cmd_receiver = cmd_rx.attach(main_loop.loop_(), {
        let state = state.clone();
        move |cmd| on_cmd(&state, cmd)
    });

    main_loop.run();
}

fn on_global(state: &Rc<RefCell<PwState>>, registry: &RegistryRc, global: &GlobalObject<&DictRef>) {
    match &global.type_ {
        ObjectType::Metadata => {
            let is_default = global.props.and_then(|p| p.get("metadata.name")) == Some("default");
            if !is_default {
                return;
            }
            let mut guard = state.borrow_mut();
            if guard.metadata.is_some() {
                return;
            }
            let metadata = match registry.bind::<Metadata, _>(global) {
                Ok(metadata) => metadata,
                Err(err) => {
                    warn!("wireplumber: cannot bind default metadata: {err}");
                    return;
                }
            };
            let listener = {
                let state = state.clone();
                let registry = registry.clone();
                metadata
                    .add_listener_local()
                    .property(move |_subject, key, _type, value| {
                        if key == Some("default.audio.sink") {
                            on_default_sink(&state, &registry, value);
                        }
                        0
                    })
                    .register()
            };
            guard.metadata = Some(metadata);
            guard.metadata_listener = Some(listener);
        }
        ObjectType::Node => {
            let name = global
                .props
                .and_then(|p| p.get("node.name"))
                .map(str::to_string);
            let id = global.id;
            let matches = {
                let mut state = state.borrow_mut();
                if let Some(name) = &name {
                    state.node_names.insert(id, name.clone());
                }
                state
                    .default_sink
                    .as_deref()
                    .is_some_and(|sink| name.as_deref() == Some(sink))
            };
            if matches {
                resolve_sink(state, registry);
            }
        }
        _ => {}
    }
}

fn on_default_sink(state: &Rc<RefCell<PwState>>, registry: &RegistryRc, value: Option<&str>) {
    let name = value
        .and_then(|value| serde_json::from_str::<Value>(value).ok())
        .and_then(|value| {
            value
                .get("name")
                .and_then(|name| name.as_str())
                .map(str::to_string)
        });
    state.borrow_mut().default_sink = name;
    resolve_sink(state, registry);
}

fn resolve_sink(state: &Rc<RefCell<PwState>>, registry: &RegistryRc) {
    let (name, current) = {
        let state = state.borrow();
        (state.default_sink.clone(), state.sink_id)
    };
    let Some(name) = name else {
        return;
    };
    let id = {
        let state = state.borrow();
        state
            .node_names
            .iter()
            .find(|(_, node_name)| **node_name == name)
            .map(|(id, _)| *id)
    };
    let Some(id) = id else {
        return;
    };
    if current == Some(id) {
        return;
    }

    let global = GlobalObject::<&DictRef> {
        id,
        permissions: pipewire::permissions::PermissionFlags::empty(),
        type_: ObjectType::Node,
        version: 0,
        props: None,
    };
    let node = match registry.bind::<Node, _>(&global) {
        Ok(node) => node,
        Err(err) => {
            warn!("wireplumber: cannot bind sink `{name}`: {err}");
            return;
        }
    };
    let listener = {
        let state_info = state.clone();
        let registry_info = registry.clone();
        let state_param = state.clone();
        node.add_listener_local()
            .info(move |info| on_node_info(&state_info, &registry_info, info))
            .param(move |_seq, _id, _index, _next, param| {
                if let Some(pod) = param {
                    on_node_param(&state_param, pod);
                }
            })
            .register()
    };
    node.subscribe_params(&[ParamType::Props]);
    node.enum_params(0, Some(ParamType::Props), 0, u32::MAX);

    debug!("wireplumber: bound sink `{name}` (id {id})");
    let mut state = state.borrow_mut();
    state.sink_id = Some(id);
    state.sink = Some(node);
    state.sink_listener = Some(listener);
    state.device_id = None;
    state.device = None;
    state.device_listener = None;
    state.route_index = None;
    state.route_device = None;
    state.volumes.clear();
    state.mute = false;
}

fn on_node_info(state: &Rc<RefCell<PwState>>, registry: &RegistryRc, info: &NodeInfoRef) {
    let mut device_id = None;
    let mut route_index = None;
    {
        let mut state = state.borrow_mut();
        if let Some(props) = info.props() {
            state.node_nick = props.get("node.nick").unwrap_or_default().to_string();
            state.node_desc = props
                .get("node.description")
                .unwrap_or_default()
                .to_string();
            state.node_name = props.get("node.name").unwrap_or_default().to_string();
            device_id = props.get("device.id").and_then(|id| id.parse::<u32>().ok());
            route_index = props
                .get("card.profile.device")
                .and_then(|index| index.parse::<i32>().ok());
        }
    }
    if let Some(device_id) = device_id {
        ensure_device(state, registry, device_id, route_index);
    }
    emit(state);
}

/// Bind the sink's device and subscribe to its `Route` parameter, which holds
/// the effective hardware volume and mute (as libwireplumber's `mixer-api`).
fn ensure_device(
    state: &Rc<RefCell<PwState>>,
    registry: &RegistryRc,
    device_id: u32,
    route_index: Option<i32>,
) {
    let rebind = {
        let mut state = state.borrow_mut();
        state.route_index = route_index;
        state.device_id != Some(device_id) || state.device.is_none()
    };
    if !rebind {
        return;
    }

    let global = GlobalObject::<&DictRef> {
        id: device_id,
        permissions: pipewire::permissions::PermissionFlags::empty(),
        type_: ObjectType::Device,
        version: 0,
        props: None,
    };
    let device = match registry.bind::<Device, _>(&global) {
        Ok(device) => device,
        Err(err) => {
            warn!("wireplumber: cannot bind device {device_id}: {err}");
            return;
        }
    };
    let device = Rc::new(device);
    let listener = {
        let state_param = state.clone();
        let device_info = device.clone();
        device
            .add_listener_local()
            .info(move |info| {
                // The device does not emit `param` events for route changes, so
                // re-enumerate `Route` whenever the device reports new params.
                if info.change_mask().contains(DeviceChangeMask::PARAMS) {
                    device_info.enum_params(0, Some(ParamType::Route), 0, u32::MAX);
                }
            })
            .param(move |_seq, _id, _index, _next, param| {
                if let Some(pod) = param {
                    on_device_param(&state_param, pod);
                }
            })
            .register()
    };
    device.subscribe_params(&[ParamType::Route]);
    device.enum_params(0, Some(ParamType::Route), 0, u32::MAX);

    let mut state = state.borrow_mut();
    state.device_id = Some(device_id);
    state.device = Some(device);
    state.device_listener = Some(listener);
    state.route_device = None;
}

fn on_device_param(state: &Rc<RefCell<PwState>>, pod: &Pod) {
    let Some(route) = pw::parse_route(pod) else {
        return;
    };
    {
        let mut state = state.borrow_mut();
        let Some(index) = state.route_index else {
            return;
        };
        if route.index != index {
            return;
        }
        state.route_device = Some(route.device);
        if !route.props.volumes.is_empty() {
            state.volumes = route.props.volumes;
        }
        if let Some(mute) = route.props.mute {
            state.mute = mute;
        }
        debug!(
            "wireplumber: route index={} device={} volumes={:?} mute={:?}",
            route.index, route.device, state.volumes, state.mute
        );
    }
    emit(state);
}

fn on_node_param(state: &Rc<RefCell<PwState>>, pod: &Pod) {
    let Some(props) = pw::parse_props(pod) else {
        return;
    };
    debug!(
        "wireplumber: props volume={:?} mute={:?}",
        props.volumes, props.mute
    );
    {
        let mut state = state.borrow_mut();
        if !props.volumes.is_empty() {
            state.volumes = props.volumes;
        } else if let Some(volume) = props.volume {
            state.volumes = vec![volume];
        }
        // When the sink has a device, its route is authoritative for mute: the
        // node's own `Props.mute` does not reflect the hardware mute.
        if let Some(mute) = props.mute
            && state.device.is_none()
        {
            state.mute = mute;
        }
    }
    emit(state);
}

fn on_global_remove(state: &Rc<RefCell<PwState>>, id: u32) {
    let removed = {
        let mut state = state.borrow_mut();
        state.node_names.remove(&id);
        if state.sink_id == Some(id) {
            state.sink_id = None;
            state.sink = None;
            state.sink_listener = None;
            true
        } else {
            false
        }
    };
    if removed {
        emit(state);
    }
}

fn on_cmd(state: &Rc<RefCell<PwState>>, cmd: Cmd) {
    let state = state.borrow();
    if state.sink.is_none() {
        return;
    }
    let channels = if state.volumes.is_empty() {
        2
    } else {
        state.volumes.len()
    };
    let (volumes, mute) = match cmd {
        Cmd::SetVolume(linear) => (vec![linear.max(0.0); channels], state.mute),
    };
    // Prefer the device route (hardware volume/mute), as mixer-api does.
    if let (Some(device), Some(index), Some(route_device)) =
        (&state.device, state.route_index, state.route_device)
        && let Some(bytes) = pw::route_bytes(index, route_device, &volumes, Some(mute))
        && let Some(pod) = Pod::from_bytes(&bytes)
    {
        device.set_param(ParamType::Route, 0, pod);
        return;
    }
    if let Some(node) = &state.sink
        && let Some(bytes) = pw::props_bytes(&volumes, Some(mute))
        && let Some(pod) = Pod::from_bytes(&bytes)
    {
        node.set_param(ParamType::Props, 0, pod);
    }
}

fn emit(state: &Rc<RefCell<PwState>>) {
    let snapshot = {
        let state = state.borrow();
        let props = pw::Props {
            volumes: state.volumes.clone(),
            volume: None,
            mute: None,
        };
        let linear = f64::from(props.linear());
        let cubic = linear.cbrt();
        let display_name = if !state.node_nick.is_empty() {
            state.node_nick.clone()
        } else if !state.node_desc.is_empty() {
            state.node_desc.clone()
        } else {
            "Unknown node name".to_string()
        };
        Snapshot {
            volume: (cubic * 100.0).round().clamp(0.0, 255.0) as u8,
            muted: state.mute,
            bluetooth: state.node_name.contains("bluez") || state.node_name.contains("a2dp-sink"),
            node_name: display_name,
            volume_linear: linear,
            volume_cubic: cubic,
            volume_db: if linear > 0.0 {
                20.0 * linear.log10()
            } else {
                f64::NEG_INFINITY
            },
        }
    };
    let state = state.borrow();
    if let Some(tx) = &state.tx {
        let _ = tx.try_send(snapshot);
    }
}

// --- rendering ----------------------------------------------------------

fn render(inner: &LabelModule, ui: &Ui) {
    let snapshot = &ui.snapshot;

    inner.set_class("muted", snapshot.muted);
    inner.set_class("sink-muted", snapshot.muted);
    inner.set_class("bluetooth", snapshot.bluetooth);

    let state = inner.get_state(snapshot.volume, false);
    let format = resolve_format(inner.config(), snapshot.bluetooth, snapshot.muted, &state);
    let args = build_args(inner, snapshot);
    inner.set_markup(&inner.render(&format, &args));

    let tooltip_format = inner.resolve_tooltip_format("{node_name}", "");
    inner.set_tooltip(&inner.render(&tooltip_format, &args));
    inner.set_visible(true);
}

/// Resolve `format[-bluetooth][-muted][-<state>]`, falling back as upstream.
fn resolve_format(config: &Value, bluetooth: bool, muted: bool, state: &str) -> String {
    let mut name = "format".to_string();
    if bluetooth {
        name.push_str("-bluetooth");
    }
    if muted {
        if name != "format"
            && !config
                .get(format!("{name}-muted"))
                .is_some_and(Value::is_string)
        {
            name = "format".to_string();
        }
        name.push_str("-muted");
    }
    if !state.is_empty()
        && let Some(format) = config
            .get(format!("{name}-{state}"))
            .and_then(Value::as_str)
    {
        return format.to_string();
    }
    if let Some(format) = config.get(&name).and_then(Value::as_str) {
        return format.to_string();
    }
    DEFAULT_FORMAT.to_string()
}

fn build_args(inner: &LabelModule, snapshot: &Snapshot) -> Args {
    let icon = inner.get_icon(u16::from(snapshot.volume), &[&snapshot.node_name], 0);
    let mut args = Args::new();
    args.insert("volume".into(), FmtArg::Int(i64::from(snapshot.volume)));
    args.insert("icon".into(), FmtArg::from(icon));
    args.insert("node_name".into(), FmtArg::from(snapshot.node_name.clone()));
    args.insert(
        "volume_linear".into(),
        FmtArg::Float(snapshot.volume_linear),
    );
    args.insert("volume_cubic".into(), FmtArg::Float(snapshot.volume_cubic));
    args.insert("volume_db".into(), FmtArg::Float(snapshot.volume_db));
    args
}

pub fn register(reg: &mut Registry) {
    reg.register("wireplumber", |init: &ModuleInit| {
        let config = &init.config;
        let scroll = ScrollConfig {
            step: config
                .get("scroll-step")
                .and_then(Value::as_f64)
                .unwrap_or(1.0),
            scale: config
                .get("scroll-scale")
                .and_then(Value::as_str)
                .unwrap_or("cubic_percent")
                .to_string(),
            max_volume: config
                .get("max-volume")
                .and_then(Value::as_f64)
                .unwrap_or(100.0),
            reverse: config
                .get("reverse-scrolling")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            reverse_mouse: config
                .get("reverse-mouse-scrolling")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };
        Ok(Box::new(Wireplumber {
            inner: LabelModule::new("wireplumber", &init.id, &init.config).with_context(&init.name),
            ui: Rc::new(RefCell::new(Ui {
                snapshot: Snapshot::default(),
            })),
            cmd: Rc::new(RefCell::new(None)),
            scroll,
        }))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scroll_uses_cubic_percent() {
        // 50% cubic -> linear 0.125; +0.5 cubic-percent -> 50.5% -> linear 0.1288...
        let linear = scroll_new_linear(0.125, true, 0.5, "cubic_percent", 100.0);
        assert!((linear - 0.505f64.powi(3)).abs() < 1e-9);
    }

    #[test]
    fn scroll_clamps_to_max_volume() {
        let linear = scroll_new_linear(0.9, true, 10.0, "cubic_percent", 100.0);
        assert!((linear - 1.0).abs() < 1e-9);
        let linear = scroll_new_linear(0.1, false, 50.0, "cubic_percent", 100.0);
        assert_eq!(linear, 0.0);
    }

    #[test]
    fn scroll_respects_max_volume() {
        let linear = scroll_new_linear(0.5, true, 100.0, "cubic_percent", 50.0);
        assert!((linear - 0.5f64.powi(3)).abs() < 1e-9);
    }

    #[test]
    fn format_chain_prefers_muted() {
        let config = json!({
            "format": "{volume}%",
            "format-muted": "muted",
            "format-bluetooth": "bt",
            "format-bluetooth-muted": "bt muted"
        });
        assert_eq!(resolve_format(&config, false, false, ""), "{volume}%");
        assert_eq!(resolve_format(&config, false, true, ""), "muted");
        assert_eq!(resolve_format(&config, true, false, ""), "bt");
        assert_eq!(resolve_format(&config, true, true, ""), "bt muted");
    }

    #[test]
    fn format_state_overrides() {
        let config = json!({"format": "base", "format-warning": "warn"});
        assert_eq!(resolve_format(&config, false, false, "warning"), "warn");
        assert_eq!(resolve_format(&config, false, false, ""), "base");
    }
}
