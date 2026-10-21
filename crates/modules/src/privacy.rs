//! `privacy` — indicators for active screenshare / audio capture / playback.
//!
//! A background PipeWire thread watches `PipeWire:Interface:Node` globals and
//! classifies them by `media.class` (`Stream/Input/Video` → screenshare,
//! `Stream/Input/Audio` → audio-in, `Stream/Output/Audio` → audio-out); a node
//! counts as in use when its state is `RUNNING`. Each configured indicator is a
//! [`gtk::Revealer`] with a `#privacy-item` box and a per-type CSS class.
//!
//! GeoClue2 `location` is not ported: it is not in the default modules and is
//! skipped with a warning when configured.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use pipewire::channel::Sender;
use pipewire::context::ContextRc;
use pipewire::main_loop::MainLoopRc;
use pipewire::node::{Node, NodeInfoRef, NodeListener, NodeState};
use pipewire::registry::{GlobalObject, RegistryRc};
use pipewire::spa::utils::dict::DictRef;
use pipewire::types::ObjectType;
use serde_json::Value;
use tracing::{debug, warn};

use waybar_core::bridge;
use waybar_core::{Module, ModuleInit, Registry};

use crate::pw;

const DEFAULT_ICON_SIZE: i32 = 20;
const DEFAULT_ICON_SPACING: i32 = 4;
const DEFAULT_TRANSITION: u32 = 250;
const DEFAULT_TOOLTIP_ICON_SIZE: i32 = 24;
const FALLBACK_ICON: &str = "application-x-executable-symbolic";

/// The privacy indicator types, matching the `type` option.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NodeType {
    Screenshare,
    AudioIn,
    AudioOut,
    Location,
}

impl NodeType {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "screenshare" => Some(Self::Screenshare),
            "audio-in" => Some(Self::AudioIn),
            "audio-out" => Some(Self::AudioOut),
            "location" => Some(Self::Location),
            _ => None,
        }
    }

    fn from_media_class(class: &str) -> Option<Self> {
        match class {
            "Stream/Input/Video" => Some(Self::Screenshare),
            "Stream/Input/Audio" => Some(Self::AudioIn),
            "Stream/Output/Audio" => Some(Self::AudioOut),
            _ => None,
        }
    }

    fn css_class(self) -> &'static str {
        match self {
            Self::Screenshare => "screenshare",
            Self::AudioIn => "audio-in",
            Self::AudioOut => "audio-out",
            Self::Location => "location",
        }
    }

    fn default_icon(self) -> &'static str {
        match self {
            Self::Screenshare => "waybar-privacy-screen-share-symbolic",
            Self::AudioIn => "waybar-privacy-audio-input-symbolic",
            Self::AudioOut => "waybar-privacy-audio-output-symbolic",
            Self::Location => "waybar-privacy-location-symbolic",
        }
    }
}

/// A plain, `Send` snapshot of one PipeWire node.
#[derive(Clone, Debug)]
struct NodeInfo {
    kind: NodeType,
    running: bool,
    app_name: String,
    node_name: String,
    icon_name: String,
    binary: String,
    portal_app_id: String,
    is_monitor: bool,
}

/// A bound node on the PipeWire thread (the proxy and its listener are kept
/// alive here for as long as the node exists).
struct Bound {
    _node: Node,
    _listener: NodeListener,
    info: NodeInfo,
}

/// A command sent from the GTK thread into the PipeWire loop.
#[derive(Clone, Copy, Debug)]
enum Cmd {
    /// Stop the PipeWire loop and release the connection (module dropped).
    Stop,
}

// --- PipeWire thread ----------------------------------------------------

fn start_pipewire(tx: async_channel::Sender<Vec<NodeInfo>>) -> Sender<Cmd> {
    let (cmd_tx, cmd_rx) = pipewire::channel::channel::<Cmd>();
    pw::spawn("privacy", move || run(tx, cmd_rx));
    cmd_tx
}

fn run(tx: async_channel::Sender<Vec<NodeInfo>>, cmd_rx: pipewire::channel::Receiver<Cmd>) {
    let main_loop = match MainLoopRc::new(None) {
        Ok(main_loop) => main_loop,
        Err(err) => {
            warn!("privacy: cannot create PipeWire main loop: {err}");
            let _ = tx.try_send(Vec::new());
            return;
        }
    };
    let context = match ContextRc::new(&main_loop, None) {
        Ok(context) => context,
        Err(err) => {
            warn!("privacy: cannot create PipeWire context: {err}");
            let _ = tx.try_send(Vec::new());
            return;
        }
    };
    let core = match context.connect_rc(None) {
        Ok(core) => core,
        Err(err) => {
            warn!("privacy: cannot connect to PipeWire: {err}");
            let _ = tx.try_send(Vec::new());
            return;
        }
    };
    let registry = match core.get_registry_rc() {
        Ok(registry) => registry,
        Err(err) => {
            warn!("privacy: cannot get the PipeWire registry: {err}");
            let _ = tx.try_send(Vec::new());
            return;
        }
    };

    let state: Rc<RefCell<HashMap<u32, Bound>>> = Rc::new(RefCell::new(HashMap::new()));
    let _listener = registry
        .add_listener_local()
        .global({
            let state = state.clone();
            let registry = registry.clone();
            let tx = tx.clone();
            move |global| on_global(&state, &registry, &tx, global)
        })
        .global_remove({
            let state = state.clone();
            let tx = tx.clone();
            move |id| {
                state.borrow_mut().remove(&id);
                emit(&state, &tx);
            }
        })
        .register();

    let _cmd_receiver = cmd_rx.attach(main_loop.loop_(), {
        let main_loop = main_loop.clone();
        move |cmd| match cmd {
            Cmd::Stop => main_loop.quit(),
        }
    });

    main_loop.run();
}

fn on_global(
    state: &Rc<RefCell<HashMap<u32, Bound>>>,
    registry: &RegistryRc,
    tx: &async_channel::Sender<Vec<NodeInfo>>,
    global: &GlobalObject<&DictRef>,
) {
    if !matches!(&global.type_, ObjectType::Node) {
        return;
    }
    let kind = global
        .props
        .and_then(|props| props.get("media.class"))
        .and_then(NodeType::from_media_class);
    let Some(kind) = kind else {
        return;
    };
    let id = global.id;
    if state.borrow().contains_key(&id) {
        return;
    }

    let node = match registry.bind::<Node, _>(global) {
        Ok(node) => node,
        Err(err) => {
            warn!("privacy: cannot bind node {id}: {err}");
            return;
        }
    };
    let listener = {
        // Weak: the map owns this listener; a strong Rc would be a cycle.
        let state = Rc::downgrade(state);
        let tx = tx.clone();
        node.add_listener_local()
            .info(move |info| {
                if let Some(state) = state.upgrade() {
                    on_info(&state, &tx, id, info);
                }
            })
            .register()
    };
    state.borrow_mut().insert(
        id,
        Bound {
            _node: node,
            _listener: listener,
            info: NodeInfo {
                kind,
                running: false,
                app_name: String::new(),
                node_name: String::new(),
                icon_name: String::new(),
                binary: String::new(),
                portal_app_id: String::new(),
                is_monitor: false,
            },
        },
    );
    debug!("privacy: tracking node {id} ({:?})", kind);
}

fn on_info(
    state: &Rc<RefCell<HashMap<u32, Bound>>>,
    tx: &async_channel::Sender<Vec<NodeInfo>>,
    id: u32,
    info: &NodeInfoRef,
) {
    {
        let mut state = state.borrow_mut();
        let Some(bound) = state.get_mut(&id) else {
            return;
        };
        bound.info.running = matches!(info.state(), NodeState::Running);
        if let Some(props) = info.props() {
            if let Some(value) = props.get("application.name") {
                bound.info.app_name = value.to_string();
            }
            if let Some(value) = props.get("node.name") {
                bound.info.node_name = value.to_string();
            }
            if let Some(value) = props.get("application.icon-name") {
                bound.info.icon_name = value.to_string();
            }
            if let Some(value) = props.get("application.process.binary") {
                bound.info.binary = value.to_string();
            }
            if let Some(value) = props.get("pipewire.access.portal.app_id") {
                bound.info.portal_app_id = value.to_string();
            }
            if let Some(value) = props.get("stream.monitor") {
                bound.info.is_monitor = value == "true";
            }
        }
    }
    emit(state, tx);
}

fn emit(state: &Rc<RefCell<HashMap<u32, Bound>>>, tx: &async_channel::Sender<Vec<NodeInfo>>) {
    let snapshot: Vec<NodeInfo> = state.borrow().values().map(|b| b.info.clone()).collect();
    let _ = tx.try_send(snapshot);
}

// --- GTK module ---------------------------------------------------------

/// Per-indicator UI state.
struct Item {
    kind: NodeType,
    revealer: gtk::Revealer,
    tooltip_window: gtk::Box,
    tooltip_enabled: bool,
    tooltip_icon_size: i32,
    active: RefCell<Vec<NodeInfo>>,
    /// The rows currently shown in the tooltip.
    tooltip_rows: RefCell<Vec<(String, String)>>,
    in_use: Rc<Cell<bool>>,
    initialized: Cell<bool>,
}

/// Parsed configuration shared by every render pass.
struct Ui {
    nodes: Vec<NodeInfo>,
    ignore_monitor: bool,
    ignore: Vec<(NodeType, String)>,
    transition: u32,
}

struct Privacy {
    root: gtk::Box,
    items: Vec<Item>,
    ui: Rc<RefCell<Ui>>,
    cmd: RefCell<Option<Sender<Cmd>>>,
}

impl Module for Privacy {
    fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    fn start(&self) {
        let (tx, rx) = bridge::channel::<Vec<NodeInfo>>();
        *self.cmd.borrow_mut() = Some(start_pipewire(tx));

        let root = self.root.clone();
        let items: Vec<Item> = self
            .items
            .iter()
            .map(|item| Item {
                kind: item.kind,
                revealer: item.revealer.clone(),
                tooltip_window: item.tooltip_window.clone(),
                tooltip_enabled: item.tooltip_enabled,
                tooltip_icon_size: item.tooltip_icon_size,
                active: RefCell::new(Vec::new()),
                tooltip_rows: RefCell::new(Vec::new()),
                in_use: item.in_use.clone(),
                initialized: Cell::new(item.initialized.get()),
            })
            .collect();
        let ui = self.ui.clone();
        bridge::on_main(rx, move |nodes| {
            ui.borrow_mut().nodes = nodes;
            render(&root, &items, &ui.borrow());
        });
    }
}

impl Drop for Privacy {
    fn drop(&mut self) {
        if let Some(cmd) = self.cmd.borrow_mut().take() {
            let _ = cmd.send(Cmd::Stop);
        }
    }
}

fn render(root: &gtk::Box, items: &[Item], ui: &Ui) {
    for item in items {
        let active: Vec<NodeInfo> = ui
            .nodes
            .iter()
            .filter(|node| {
                node.kind == item.kind
                    && node.running
                    && !(ui.ignore_monitor && node.is_monitor)
                    && !ui
                        .ignore
                        .iter()
                        .any(|(kind, name)| *kind == node.kind && *name == node.node_name)
            })
            .cloned()
            .collect();

        let in_use = !active.is_empty();
        *item.active.borrow_mut() = active;
        if in_use && item.tooltip_enabled {
            update_tooltip(item);
        }
        set_in_use(item, in_use, ui.transition);
    }

    let any = items.iter().any(|item| item.in_use.get());
    if any {
        root.set_visible(true);
    } else {
        let root = root.clone();
        let flags: Vec<Rc<Cell<bool>>> = items.iter().map(|item| item.in_use.clone()).collect();
        gtk::glib::timeout_add_local_once(
            Duration::from_millis(u64::from(ui.transition)),
            move || {
                if !flags.iter().any(|flag| flag.get()) {
                    root.set_visible(false);
                }
            },
        );
    }
}

fn set_in_use(item: &Item, in_use: bool, transition: u32) {
    if item.initialized.get() && item.in_use.get() == in_use {
        return;
    }
    item.in_use.set(in_use);
    if !item.initialized.get() {
        item.initialized.set(true);
        item.revealer.set_reveal_child(in_use);
        item.revealer.set_visible(in_use);
        return;
    }
    if in_use {
        debug!("privacy: {:?} indicator shown", item.kind);
        item.revealer.set_visible(true);
        item.revealer.set_reveal_child(true);
    } else {
        debug!("privacy: {:?} indicator hidden", item.kind);
        item.revealer.set_reveal_child(false);
        let revealer = item.revealer.clone();
        let flag = item.in_use.clone();
        gtk::glib::timeout_add_local_once(
            Duration::from_millis(u64::from(transition)),
            move || {
                if !flag.get() {
                    revealer.set_visible(false);
                }
            },
        );
    }
}

fn update_tooltip(item: &Item) {
    let theme = icon_theme();
    let rows: Vec<(String, String)> = item
        .active
        .borrow()
        .iter()
        .map(|node| (display_name(node), resolve_icon(node, theme.as_ref())))
        .collect();
    if *item.tooltip_rows.borrow() == rows {
        return;
    }
    while let Some(child) = item.tooltip_window.first_child() {
        item.tooltip_window.remove(&child);
    }
    for (name, icon_name) in &rows {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let icon = gtk::Image::from_icon_name(icon_name);
        icon.set_pixel_size(item.tooltip_icon_size);
        row.append(&icon);
        row.append(&gtk::Label::new(Some(name)));
        item.tooltip_window.append(&row);
    }
    *item.tooltip_rows.borrow_mut() = rows;
}

fn icon_theme() -> Option<gtk::IconTheme> {
    gtk::gdk::Display::default().map(|display| gtk::IconTheme::for_display(&display))
}

/// `application.name` → `node.name` → `"Unknown Application"`, first letter
/// uppercased (upstream's `getName`).
fn display_name(node: &NodeInfo) -> String {
    let name = if !node.app_name.is_empty() {
        &node.app_name
    } else if !node.node_name.is_empty() {
        &node.node_name
    } else {
        return "Unknown Application".to_string();
    };
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => "Unknown Application".to_string(),
    }
}

/// First icon-theme name among `application.icon-name`,
/// `application.process.binary`, the portal app id, `application.name` and
/// `node.name`, retrying in lowercase, then a generic fallback.
fn resolve_icon(node: &NodeInfo, theme: Option<&gtk::IconTheme>) -> String {
    let Some(theme) = theme else {
        return FALLBACK_ICON.to_string();
    };
    let candidates = [
        &node.icon_name,
        &node.binary,
        &node.portal_app_id,
        &node.app_name,
        &node.node_name,
    ];
    for candidate in candidates {
        if !candidate.is_empty() && theme.has_icon(candidate) {
            return candidate.clone();
        }
    }
    for candidate in candidates {
        if candidate.is_empty() {
            continue;
        }
        let lower = candidate.to_lowercase();
        if lower != *candidate && theme.has_icon(&lower) {
            return lower;
        }
    }
    FALLBACK_ICON.to_string()
}

// --- construction -------------------------------------------------------

struct ItemConfig {
    kind: NodeType,
    icon_name: String,
    tooltip: bool,
    tooltip_icon_size: i32,
}

fn parse_items(config: &Value) -> Vec<ItemConfig> {
    let entries = config.get("modules").and_then(Value::as_array);
    match entries {
        Some(entries) if !entries.is_empty() => entries
            .iter()
            .filter_map(|entry| {
                let kind = entry
                    .get("type")
                    .and_then(Value::as_str)
                    .and_then(NodeType::from_name)?;
                if kind == NodeType::Location {
                    warn!("privacy: `location` is not supported; skipping it");
                    return None;
                }
                Some(parse_item(kind, Some(entry)))
            })
            .collect(),
        _ => ["screenshare", "audio-in"]
            .iter()
            .filter_map(|name| NodeType::from_name(name))
            .map(|kind| parse_item(kind, None))
            .collect(),
    }
}

fn parse_item(kind: NodeType, entry: Option<&Value>) -> ItemConfig {
    ItemConfig {
        kind,
        icon_name: entry
            .and_then(|entry| entry.get("icon-name"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| kind.default_icon().to_string()),
        tooltip: entry
            .and_then(|entry| entry.get("tooltip"))
            .and_then(Value::as_bool)
            .unwrap_or(true),
        tooltip_icon_size: entry
            .and_then(|entry| entry.get("tooltip-icon-size"))
            .and_then(Value::as_u64)
            .and_then(|size| i32::try_from(size).ok())
            .unwrap_or(DEFAULT_TOOLTIP_ICON_SIZE),
    }
}

fn parse_ignore(config: &Value) -> Vec<(NodeType, String)> {
    config
        .get("ignore")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let kind = entry
                        .get("type")
                        .and_then(Value::as_str)
                        .and_then(NodeType::from_name)?;
                    let name = entry.get("name").and_then(Value::as_str)?.to_string();
                    Some((kind, name))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn transition_for(section: &str, horizontal: bool) -> gtk::RevealerTransitionType {
    match section {
        "modules-left" => {
            if horizontal {
                gtk::RevealerTransitionType::SlideRight
            } else {
                gtk::RevealerTransitionType::SlideDown
            }
        }
        "modules-center" => gtk::RevealerTransitionType::Crossfade,
        _ => {
            if horizontal {
                gtk::RevealerTransitionType::SlideLeft
            } else {
                gtk::RevealerTransitionType::SlideUp
            }
        }
    }
}

fn build_item(
    cfg: &ItemConfig,
    horizontal: bool,
    section: &str,
    icon_size: i32,
    transition: u32,
) -> Item {
    let icon = gtk::Image::from_icon_name(&cfg.icon_name);
    if icon_size > 0 {
        icon.set_pixel_size(icon_size);
    }

    let inner = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    inner.set_widget_name("privacy-item");
    inner.add_css_class(cfg.kind.css_class());
    inner.append(&icon);

    let revealer = gtk::Revealer::new();
    revealer.set_transition_type(transition_for(section, horizontal));
    revealer.set_transition_duration(transition);
    revealer.set_child(Some(&inner));
    revealer.set_reveal_child(false);
    revealer.set_visible(false);

    let tooltip_window = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let in_use = Rc::new(Cell::new(false));
    if cfg.tooltip {
        inner.set_has_tooltip(true);
        let tooltip_window = tooltip_window.clone();
        let flag = in_use.clone();
        inner.connect_query_tooltip(move |_, _, _, _, tooltip| {
            if flag.get() {
                tooltip.set_custom(Some(&tooltip_window));
                true
            } else {
                false
            }
        });
    }

    Item {
        kind: cfg.kind,
        revealer,
        tooltip_window,
        tooltip_enabled: cfg.tooltip,
        tooltip_icon_size: cfg.tooltip_icon_size,
        active: RefCell::new(Vec::new()),
        tooltip_rows: RefCell::new(Vec::new()),
        in_use,
        initialized: Cell::new(false),
    }
}

pub fn register(reg: &mut Registry) {
    reg.register("privacy", |init: &ModuleInit| {
        let config = &init.config;
        let horizontal = !init.bar.position.is_vertical();
        let orientation = if horizontal {
            gtk::Orientation::Horizontal
        } else {
            gtk::Orientation::Vertical
        };
        let icon_size = config
            .get("icon-size")
            .and_then(Value::as_u64)
            .and_then(|size| i32::try_from(size).ok())
            .unwrap_or(DEFAULT_ICON_SIZE);
        let icon_spacing = config
            .get("icon-spacing")
            .and_then(Value::as_u64)
            .and_then(|spacing| i32::try_from(spacing).ok())
            .unwrap_or(DEFAULT_ICON_SPACING);
        let transition = config
            .get("transition-duration")
            .and_then(Value::as_u64)
            .and_then(|duration| u32::try_from(duration).ok())
            .unwrap_or(DEFAULT_TRANSITION);

        let content = gtk::Box::new(orientation, icon_spacing);
        content.set_widget_name("privacy");
        content.add_css_class("module");
        if !init.id.is_empty() {
            content.add_css_class(&init.id);
        }

        let items: Vec<Item> = parse_items(config)
            .iter()
            .map(|cfg| build_item(cfg, horizontal, &init.section, icon_size, transition))
            .collect();
        for item in &items {
            content.append(&item.revealer);
        }

        let wrapper = gtk::Box::new(orientation, 0);
        wrapper.append(&content);
        wrapper.set_visible(false);

        let ui = Ui {
            nodes: Vec::new(),
            ignore_monitor: config
                .get("ignore-monitor")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            ignore: parse_ignore(config),
            transition,
        };

        Ok(Box::new(Privacy {
            root: wrapper,
            items,
            ui: Rc::new(RefCell::new(ui)),
            cmd: RefCell::new(None),
        }))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_media_class() {
        assert_eq!(
            NodeType::from_media_class("Stream/Input/Video"),
            Some(NodeType::Screenshare)
        );
        assert_eq!(
            NodeType::from_media_class("Stream/Input/Audio"),
            Some(NodeType::AudioIn)
        );
        assert_eq!(
            NodeType::from_media_class("Stream/Output/Audio"),
            Some(NodeType::AudioOut)
        );
        assert_eq!(NodeType::from_media_class("Audio/Sink"), None);
    }

    #[test]
    fn default_modules_are_screenshare_and_audio_in() {
        let items = parse_items(&json!({}));
        let kinds: Vec<NodeType> = items.iter().map(|item| item.kind).collect();
        assert_eq!(kinds, vec![NodeType::Screenshare, NodeType::AudioIn]);
    }

    #[test]
    fn location_is_skipped() {
        let items = parse_items(&json!({
            "modules": [{"type": "location"}, {"type": "audio-out"}]
        }));
        let kinds: Vec<NodeType> = items.iter().map(|item| item.kind).collect();
        assert_eq!(kinds, vec![NodeType::AudioOut]);
    }

    #[test]
    fn explicit_modules_and_options() {
        let config = json!({
            "modules": [
                {"type": "screenshare", "icon-name": "custom-icon", "tooltip": false},
                {"type": "audio-out"}
            ]
        });
        let items = parse_items(&config);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, NodeType::Screenshare);
        assert_eq!(items[0].icon_name, "custom-icon");
        assert!(!items[0].tooltip);
        assert_eq!(items[1].kind, NodeType::AudioOut);
        assert!(items[1].tooltip);
        assert_eq!(items[1].tooltip_icon_size, DEFAULT_TOOLTIP_ICON_SIZE);
    }

    #[test]
    fn parses_ignore_list() {
        let ignore = parse_ignore(&json!({
            "ignore": [{"type": "audio-in", "name": "foo"}]
        }));
        assert_eq!(ignore, vec![(NodeType::AudioIn, "foo".to_string())]);
    }

    #[test]
    fn display_name_falls_back_and_capitalises() {
        let mut node = NodeInfo {
            kind: NodeType::AudioIn,
            running: true,
            app_name: String::new(),
            node_name: "firefox".to_string(),
            icon_name: String::new(),
            binary: String::new(),
            portal_app_id: String::new(),
            is_monitor: false,
        };
        assert_eq!(display_name(&node), "Firefox");
        node.app_name = "mpv".to_string();
        assert_eq!(display_name(&node), "Mpv");
        node.app_name.clear();
        node.node_name.clear();
        assert_eq!(display_name(&node), "Unknown Application");
    }
}
