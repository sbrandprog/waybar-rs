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
use std::time::{Duration, Instant};

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
use waybar_core::widgets::{ScrollAxis, set_scroll_cursor};
use waybar_core::{Module, ModuleInit, Registry};

use crate::label::LabelModule;
use crate::pw;

const DEFAULT_FORMAT: &str = "{volume}%";

/// A command sent from the GTK thread into the PipeWire loop.
#[derive(Clone, Copy, Debug)]
enum Cmd {
    /// Set the linear gain.
    SetVolume(f32),
    /// Stop the PipeWire loop and release the connection (module dropped).
    Stop,
}

const PENDING_TTL: Duration = Duration::from_millis(500);

/// Decibel floor for silence (both `volume_db` and the `db` scroll domain).
const DB_FLOOR: f64 = -60.0;

/// A plain, `Send` snapshot of the current sink state.
#[derive(Clone, Debug, Default)]
struct Snapshot {
    volume: u8,
    muted: bool,
    bluetooth: bool,
    /// `node.name` of the sink (icon keys).
    sink_name: String,
    node_name: String,
    volume_linear: f64,
    volume_cubic: f64,
    volume_db: f64,
    step_linear: f64,
}

/// State owned by the PipeWire thread.
#[derive(Default)]
struct PwState {
    /// `node.name` of every node global, keyed by global id.
    node_names: HashMap<u32, String>,
    default_sink: Option<String>,
    metadata: Option<Metadata>,
    metadata_listener: Option<MetadataListener>,
    metadata_id: Option<u32>,
    sink_id: Option<u32>,
    sink: Option<Node>,
    sink_listener: Option<NodeListener>,
    /// The sink node's own `Props` (software volume/mute).
    node_props: pw::Props,
    /// The sink device's active `Route` props (hardware volume/mute).
    route_props: pw::Props,
    device_id: Option<u32>,
    device: Option<Rc<Device>>,
    device_listener: Option<DeviceListener>,
    route_index: Option<i32>,
    route_device: Option<i32>,
    node_nick: String,
    node_desc: String,
    node_name: String,
    tx: Option<async_channel::Sender<Option<Snapshot>>>,
}

impl PwState {
    /// The `Props` that carry the effective volume: the active route for a
    /// routed sink, else the node's own. The route index is cleared when the
    /// route cannot be read anymore (see `AGENTS.md`).
    fn effective_props(&self) -> &pw::Props {
        if self.route_index.is_some() {
            &self.route_props
        } else {
            &self.node_props
        }
    }

    fn effective_linear(&self) -> Option<f32> {
        self.effective_props().linear()
    }

    /// Whether the effective source is muted; route mute wins for a routed sink.
    fn effective_mute(&self) -> bool {
        self.known_mute().unwrap_or(false)
    }

    /// The displayed mute, `None` while unknown.
    fn known_mute(&self) -> Option<bool> {
        if self.route_index.is_some() {
            self.route_props.mute.or(self.node_props.mute)
        } else {
            self.node_props.mute
        }
    }

    /// The mute to write, `None` to leave it untouched.
    fn write_mute(&self) -> Option<bool> {
        if self.route_index.is_some() {
            self.route_props.mute
        } else {
            self.node_props.mute
        }
    }

    /// The linear volume step of the effective source, or 0 when unknown.
    fn effective_step(&self) -> f64 {
        f64::from(self.effective_props().step.unwrap_or(0.0))
    }
}

/// Scale known channel volumes to `linear`, keeping their balance. An unknown
/// channel count means a stereo pair.
fn scale_volumes(current: &[f32], linear: f32) -> Vec<f32> {
    if current.is_empty() {
        return vec![linear; 2];
    }
    let peak = current.iter().copied().fold(0.0f32, f32::max);
    if peak <= 0.0 {
        return vec![linear; current.len()];
    }
    current
        .iter()
        .map(|volume| (volume * linear / peak).max(0.0))
        .collect()
}

/// Keep the route index across sparse `info` updates of the same device.
fn carry_route_index(
    reported: Option<i32>,
    current: Option<i32>,
    same_device: bool,
) -> Option<i32> {
    reported.or(if same_device { current } else { None })
}

/// Main-thread UI state.
#[derive(Default)]
struct Ui {
    snapshot: Snapshot,
    pending: Option<(f64, Instant)>,
}

impl Ui {
    /// The linear gain a new scroll starts from.
    fn scroll_base(&self) -> f64 {
        match self.pending {
            Some((linear, at)) if at.elapsed() < PENDING_TTL => linear,
            _ => self.snapshot.volume_linear,
        }
    }

    /// Forget the pending write once it is reported back.
    fn settle_pending(&mut self, reported: f64) {
        if let Some((linear, _)) = self.pending
            && (linear - reported).abs() < 1e-4
        {
            self.pending = None;
        }
    }
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
        // Hidden until the first snapshot; a failed PipeWire connection leaves
        // the module hidden rather than showing a bogus 0%.
        self.inner.set_visible(false);
        let (tx, rx) = bridge::channel::<Option<Snapshot>>();
        let cmd = start_pipewire(tx);
        *self.cmd.borrow_mut() = Some(cmd);

        let inner = self.inner.clone();
        let ui = self.ui.clone();
        bridge::on_main(rx, move |snapshot| {
            let mut ui = ui.borrow_mut();
            match snapshot {
                Some(snapshot) => {
                    ui.settle_pending(snapshot.volume_linear);
                    ui.snapshot = snapshot;
                    render(&inner, &ui);
                }
                None => {
                    ui.snapshot = Snapshot::default();
                    ui.pending = None;
                    inner.set_visible(false);
                }
            }
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
            let (current, min_step) = {
                let ui = ui.borrow();
                (ui.scroll_base(), ui.snapshot.step_linear)
            };
            let linear = scroll_new_linear(
                current,
                up,
                scroll.step,
                min_step,
                &scroll.scale,
                scroll.max_volume,
            );
            if (linear - current).abs() > f64::EPSILON {
                if let Some(cmd) = cmd.borrow().as_ref() {
                    let _ = cmd.send(Cmd::SetVolume(linear as f32));
                }
                ui.borrow_mut().pending = Some((linear, Instant::now()));
            }
            gtk::glib::Propagation::Stop
        });
        self.inner.widget().add_controller(controller);
        set_scroll_cursor(&self.inner.widget(), ScrollAxis::Vertical);
    }
}

impl Drop for Wireplumber {
    fn drop(&mut self) {
        // Ask the PipeWire thread to quit. This drops its snapshot sender, so
        // the `bridge::on_main` future ends and releases the GTK widgets it
        // holds; without it the loop thread and the widgets would leak whenever
        // a bar is removed (e.g. on monitor hotplug).
        if let Some(cmd) = self.cmd.borrow_mut().take() {
            let _ = cmd.send(Cmd::Stop);
        }
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
/// and converted back. A move of at least `min_step` (the hardware volume step)
/// is then enforced so repeated scrolls cannot get stuck below the device's
/// granularity, and the result is clamped to `[0, (max_volume/100)^3]`.
fn scroll_new_linear(
    current: f64,
    up: bool,
    step: f64,
    min_step: f64,
    scale: &str,
    max_volume: f64,
) -> f64 {
    let domain = match scale {
        "cubic" => current.cbrt(),
        "db" => {
            if current > 0.0 {
                20.0 * current.log10()
            } else {
                DB_FLOOR
            }
        }
        "cubic_percent" => current.cbrt() * 100.0,
        _ => current,
    };
    let moved = domain + if up { step } else { -step };
    let mut linear = match scale {
        "cubic" => moved.powi(3),
        "db" => 10f64.powf(moved / 20.0),
        "cubic_percent" => (moved / 100.0).powi(3),
        _ => moved,
    };
    if min_step > 0.0 {
        let target = if up {
            current + min_step
        } else {
            current - min_step
        };
        if (up && target > linear) || (!up && target < linear) {
            linear = target;
        }
    }
    let max_linear = (max_volume / 100.0).powi(3);
    linear.clamp(0.0, max_linear)
}

// --- PipeWire thread ----------------------------------------------------

fn start_pipewire(tx: async_channel::Sender<Option<Snapshot>>) -> Sender<Cmd> {
    let (cmd_tx, cmd_rx) = pipewire::channel::channel::<Cmd>();
    pw::spawn("wireplumber", move || run(tx, cmd_rx));
    cmd_tx
}

fn run(tx: async_channel::Sender<Option<Snapshot>>, cmd_rx: pipewire::channel::Receiver<Cmd>) {
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
        let main_loop = main_loop.clone();
        move |cmd| match cmd {
            Cmd::Stop => main_loop.quit(),
            Cmd::SetVolume(linear) => on_cmd(&state, linear),
        }
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
            let id = global.id;
            if state.borrow().metadata_id == Some(id) {
                return;
            }
            // First bind, or the default metadata global was replaced (e.g. a
            // WirePlumber restart): rebind and drop the previous proxy.
            let metadata = match registry.bind::<Metadata, _>(global) {
                Ok(metadata) => metadata,
                Err(err) => {
                    warn!("wireplumber: cannot bind default metadata: {err}");
                    return;
                }
            };
            let listener = {
                let state = Rc::downgrade(state);
                let registry = registry.clone();
                metadata
                    .add_listener_local()
                    .property(move |_subject, key, _type, value| {
                        if key == Some("default.audio.sink")
                            && let Some(state) = state.upgrade()
                        {
                            on_default_sink(&state, &registry, value);
                        }
                        0
                    })
                    .register()
            };
            let mut guard = state.borrow_mut();
            guard.metadata = Some(metadata);
            guard.metadata_listener = Some(listener);
            guard.metadata_id = Some(id);
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
    let changed = {
        let mut state = state.borrow_mut();
        if state.default_sink == name {
            false
        } else {
            state.default_sink = name;
            clear_sink(&mut state);
            true
        }
    };
    if changed {
        emit(state);
    }
    resolve_sink(state, registry);
}

/// Drop the bound sink and everything read from it.
fn clear_sink(state: &mut PwState) {
    state.sink_id = None;
    state.sink = None;
    state.sink_listener = None;
    state.node_props = pw::Props::default();
    state.node_nick.clear();
    state.node_desc.clear();
    state.node_name.clear();
    state.device_id = None;
    state.device = None;
    state.device_listener = None;
    state.route_index = None;
    state.route_device = None;
    state.route_props = pw::Props::default();
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
        let state_info = Rc::downgrade(state);
        let registry_info = registry.clone();
        let state_param = Rc::downgrade(state);
        node.add_listener_local()
            .info(move |info| {
                if let Some(state) = state_info.upgrade() {
                    on_node_info(&state, &registry_info, info);
                }
            })
            .param(move |_seq, _id, _index, _next, param| {
                if let (Some(state), Some(pod)) = (state_param.upgrade(), param) {
                    on_node_param(&state, pod);
                }
            })
            .register()
    };
    node.subscribe_params(&[ParamType::Props]);
    node.enum_params(0, Some(ParamType::Props), 0, u32::MAX);

    debug!("wireplumber: bound sink `{name}` (id {id})");
    let mut state = state.borrow_mut();
    // A different node: drop the old identity until its own `info` arrives.
    clear_sink(&mut state);
    state.sink_id = Some(id);
    state.sink = Some(node);
    state.sink_listener = Some(listener);
}

fn on_node_info(state: &Rc<RefCell<PwState>>, registry: &RegistryRc, info: &NodeInfoRef) {
    let mut device_id = None;
    let mut route_index = None;
    {
        let mut state = state.borrow_mut();
        // The `info` events that accompany a `Props` change carry a sparse
        // property dict without the identity keys, so a missing key must keep
        // the previous value instead of clearing the name.
        if let Some(props) = info.props() {
            if let Some(nick) = props.get("node.nick") {
                state.node_nick = nick.to_string();
            }
            if let Some(desc) = props.get("node.description") {
                state.node_desc = desc.to_string();
            }
            if let Some(name) = props.get("node.name") {
                state.node_name = name.to_string();
            }
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
    reported_route: Option<i32>,
) {
    let (rebind, route_changed, device) = {
        let mut state = state.borrow_mut();
        let route_index = carry_route_index(
            reported_route,
            state.route_index,
            state.device_id == Some(device_id),
        );
        let route_changed = state.route_index != route_index;
        if route_changed {
            state.route_device = None;
            state.route_props = pw::Props::default();
        }
        state.route_index = route_index;
        let rebind = state.device_id != Some(device_id) || state.device.is_none();
        (
            rebind,
            route_changed,
            if rebind { None } else { state.device.clone() },
        )
    };
    if !rebind {
        if route_changed && let Some(device) = device {
            // Re-read the newly active route.
            device.enum_params(0, Some(ParamType::Route), 0, u32::MAX);
        }
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
            // Fall back to the node's `Props` until a later `info` retries.
            let mut state = state.borrow_mut();
            state.route_index = None;
            state.route_device = None;
            state.route_props = pw::Props::default();
            return;
        }
    };
    let device = Rc::new(device);
    let listener = {
        let state_param = Rc::downgrade(state);
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
                if let (Some(state), Some(pod)) = (state_param.upgrade(), param) {
                    on_device_param(&state, pod);
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
    state.route_props = pw::Props::default();
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
        state.route_props.merge(&route.props);
        debug!(
            "wireplumber: route index={} device={} volumes={:?} mute={:?}",
            route.index, route.device, state.route_props.volumes, state.route_props.mute
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
    state.borrow_mut().node_props.merge(&props);
    emit(state);
}

fn on_global_remove(state: &Rc<RefCell<PwState>>, id: u32) {
    let changed = {
        let mut state = state.borrow_mut();
        state.node_names.remove(&id);
        let mut changed = false;
        if state.device_id == Some(id) {
            state.device_id = None;
            state.device = None;
            state.device_listener = None;
            // The route cannot be read without the device.
            state.route_index = None;
            state.route_device = None;
            state.route_props = pw::Props::default();
            changed = true;
        }
        if state.sink_id == Some(id) {
            clear_sink(&mut state);
            changed = true;
        }
        if state.metadata_id == Some(id) {
            state.metadata_id = None;
            state.metadata = None;
            state.metadata_listener = None;
        }
        changed
    };
    if changed {
        emit(state);
    }
}

fn on_cmd(state: &Rc<RefCell<PwState>>, linear: f32) {
    let state = state.borrow();
    let Some(node) = &state.sink else {
        return;
    };
    let mute = state.write_mute();
    // Writing goes through the route for a routed sink, as mixer-api does.
    if let Some(index) = state.route_index {
        let (Some(device), Some(route_device)) = (&state.device, state.route_device) else {
            return;
        };
        let volumes = scale_volumes(&state.route_props.volumes, linear);
        if let Some(bytes) = pw::route_bytes(index, route_device, &volumes, mute)
            && let Some(pod) = Pod::from_bytes(&bytes)
        {
            device.set_param(ParamType::Route, 0, pod);
        }
        return;
    }
    let volumes = scale_volumes(&state.node_props.volumes, linear);
    if let Some(bytes) = pw::props_bytes(&volumes, mute)
        && let Some(pod) = Pod::from_bytes(&bytes)
    {
        node.set_param(ParamType::Props, 0, pod);
    }
}

/// Publish the state; `None` tells the UI to hide the module.
fn emit(state: &Rc<RefCell<PwState>>) {
    let snapshot = {
        let state = state.borrow();
        state.effective_linear().map(|linear| {
            let linear = f64::from(linear);
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
                muted: state.effective_mute(),
                bluetooth: state.node_name.contains("bluez")
                    || state.node_name.contains("a2dp-sink"),
                sink_name: state.node_name.clone(),
                node_name: display_name,
                volume_linear: linear,
                volume_cubic: cubic,
                volume_db: if linear > 0.0 {
                    20.0 * linear.log10()
                } else {
                    DB_FLOOR
                },
                step_linear: state.effective_step(),
            }
        })
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
    // A missing bluetooth/muted-specific format falls back to the base
    // `format`, not straight to the module default (as upstream does).
    if let Some(format) = config.get("format").and_then(Value::as_str) {
        return format.to_string();
    }
    DEFAULT_FORMAT.to_string()
}

fn build_args(inner: &LabelModule, snapshot: &Snapshot) -> Args {
    let mut keys = Vec::new();
    if snapshot.muted {
        keys.push(format!("{}-muted", snapshot.sink_name));
    }
    keys.push(snapshot.sink_name.clone());
    if snapshot.muted {
        keys.push("default-muted".to_string());
    }
    let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
    let icon = inner.get_icon(u16::from(snapshot.volume), &keys, 0);
    // Names come from other clients: escape before markup.
    let node_name = gtk::glib::markup_escape_text(&snapshot.node_name);
    let mut args = Args::new();
    args.insert("volume".into(), FmtArg::Int(i64::from(snapshot.volume)));
    args.insert("icon".into(), FmtArg::from(icon));
    args.insert("node_name".into(), FmtArg::from(node_name.to_string()));
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
            ui: Rc::new(RefCell::new(Ui::default())),
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
        let linear = scroll_new_linear(0.125, true, 0.5, 0.0, "cubic_percent", 100.0);
        assert!((linear - 0.505f64.powi(3)).abs() < 1e-9);
    }

    #[test]
    fn scroll_clamps_to_max_volume() {
        let linear = scroll_new_linear(0.9, true, 10.0, 0.0, "cubic_percent", 100.0);
        assert!((linear - 1.0).abs() < 1e-9);
        let linear = scroll_new_linear(0.1, false, 50.0, 0.0, "cubic_percent", 100.0);
        assert_eq!(linear, 0.0);
    }

    #[test]
    fn scroll_respects_max_volume() {
        let linear = scroll_new_linear(0.5, true, 100.0, 0.0, "cubic_percent", 50.0);
        assert!((linear - 0.5f64.powi(3)).abs() < 1e-9);
    }

    #[test]
    fn scroll_enforces_min_step() {
        // The hardware step (0.1 linear) exceeds the scroll step: the move must
        // still reach `current +/- min_step`, or repeated scrolls would stick.
        let linear = scroll_new_linear(0.5, true, 0.001, 0.1, "cubic_percent", 100.0);
        assert!((linear - 0.6).abs() < 1e-9);
        let linear = scroll_new_linear(0.5, false, 0.001, 0.1, "cubic_percent", 100.0);
        assert!((linear - 0.4).abs() < 1e-9);
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

    #[test]
    fn scale_keeps_channel_balance() {
        // Left at half of the right: scaling to 0.4 keeps the 1:2 ratio.
        let volumes = scale_volumes(&[0.25, 0.5], 0.4);
        assert!((volumes[0] - 0.2).abs() < 1e-6);
        assert!((volumes[1] - 0.4).abs() < 1e-6);
    }

    #[test]
    fn scale_without_known_channels_writes_stereo() {
        assert_eq!(scale_volumes(&[], 0.3), vec![0.3, 0.3]);
        assert_eq!(scale_volumes(&[0.0, 0.0], 0.3), vec![0.3, 0.3]);
    }

    #[test]
    fn scale_keeps_the_channel_count_of_silence() {
        // A silent mono route stays mono: only an unknown channel count means
        // "write a stereo pair".
        assert_eq!(scale_volumes(&[0.0], 0.3), vec![0.3]);
    }

    #[test]
    fn route_write_never_borrows_node_mute() {
        let mut state = PwState::default();
        state.node_props.mute = Some(false);
        // Without a route the node's own mute is written.
        assert_eq!(state.write_mute(), Some(false));
        // A routed sink writes its route: an unknown route mute stays unknown
        // rather than becoming the node's software `false` (which would unmute).
        state.route_index = Some(1);
        assert_eq!(state.known_mute(), Some(false));
        assert_eq!(state.write_mute(), None);
        state.route_props.mute = Some(true);
        assert_eq!(state.write_mute(), Some(true));
    }

    #[test]
    fn removing_the_device_falls_back_to_node_volume() {
        let state = Rc::new(RefCell::new(PwState::default()));
        {
            let mut state = state.borrow_mut();
            state.node_props.volume = Some(0.5);
            state.device_id = Some(42);
            state.route_index = Some(1);
            state.route_device = Some(2);
            // The route has not reported a volume yet, so it hides the module.
            assert_eq!(state.effective_linear(), None);
        }
        on_global_remove(&state, 42);
        let state = state.borrow();
        // The stale route must not keep governing the volume.
        assert_eq!(state.route_index, None);
        assert_eq!(state.effective_linear(), Some(0.5));
    }

    #[test]
    fn sparse_info_keeps_route_of_same_device() {
        assert_eq!(carry_route_index(None, Some(7), true), Some(7));
        assert_eq!(carry_route_index(Some(9), Some(7), true), Some(9));
        // A different device must not inherit the old route index.
        assert_eq!(carry_route_index(None, Some(7), false), None);
    }

    #[test]
    fn route_governs_volume_once_known() {
        let mut state = PwState::default();
        state.node_props.volume = Some(1.0);
        state.route_index = Some(3);
        // Route index known but no route read yet: unknown, not the node's unity.
        assert_eq!(state.effective_linear(), None);
        state.route_props.volume = Some(0.25);
        assert_eq!(state.effective_linear(), Some(0.25));
        // Without a route index the node's own volume is the source.
        state.route_index = None;
        assert_eq!(state.effective_linear(), Some(1.0));
    }

    #[test]
    fn pending_write_is_settled_by_matching_report() {
        let mut ui = Ui::default();
        ui.snapshot.volume_linear = 0.5;
        ui.pending = Some((0.6, Instant::now()));
        assert_eq!(ui.scroll_base(), 0.6);
        ui.settle_pending(0.5);
        assert!(ui.pending.is_some());
        ui.settle_pending(0.6);
        assert!(ui.pending.is_none());
        assert_eq!(ui.scroll_base(), 0.5);
    }

    #[test]
    fn format_falls_back_to_base() {
        // A bluetooth sink without a bt-specific format keeps the base `format`.
        let config = json!({"format": "base", "format-muted": "muted"});
        assert_eq!(resolve_format(&config, true, false, ""), "base");
        // Muted without a muted format keeps the base `format` too.
        let config = json!({"format": "base"});
        assert_eq!(resolve_format(&config, false, true, ""), "base");
        // With nothing configured at all, the module default applies.
        assert_eq!(resolve_format(&json!({}), false, false, ""), DEFAULT_FORMAT);
    }
}
