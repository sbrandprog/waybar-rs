//! `sway/workspaces` — clickable workspace buttons driven by sway's IPC tree.
//!
//! A background worker listens for `workspace`/`window` events, re-reads the
//! layout tree on every change and pushes a sorted snapshot to the main thread.
//! The main thread owns the buttons and turns user clicks/scrolls back into
//! sway commands.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use futures_lite::StreamExt;
use gtk::prelude::*;
use regex::Regex;
use serde_json::Value;
use swayipc_async::{Connection, EventType, Node};
use tracing::{error, warn};

use waybar_core::bridge;
use waybar_core::format::{self as fmt, Args, FmtArg};
use waybar_core::{Module, ModuleInit, Registry};

/// A workspace as rendered in the bar (a plain, `Send` snapshot).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Workspace {
    pub name: String,
    /// Workspace number, or `-1` for named workspaces.
    pub num: i32,
    /// Output the workspace lives on, empty for persistent workspaces.
    pub output: String,
    /// `Some(output)` for persistent workspaces (empty string means "any").
    pub target_output: Option<String>,
    pub focused: bool,
    pub visible: bool,
    pub urgent: bool,
    /// No tiling and no floating children.
    pub empty: bool,
}

/// Parsed, reusable subset of the module configuration.
#[derive(Clone)]
struct SwayConfig {
    all_outputs: bool,
    ignore: Vec<Regex>,
    custom_sort: HashMap<String, u16>,
    alphabetical: bool,
    persistent: Value,
}

impl SwayConfig {
    fn parse(config: &Value) -> Self {
        let mut ignore = Vec::new();
        if let Some(entries) = config.get("ignore-workspaces").and_then(|v| v.as_array()) {
            for entry in entries {
                let Some(pattern) = entry.as_str() else {
                    continue;
                };
                // `std::regex_match` (upstream) matches the whole string, so
                // anchor the pattern to emulate it.
                match Regex::new(&format!("^(?:{pattern})$")) {
                    Ok(regex) => ignore.push(regex),
                    Err(err) => warn!("sway/workspaces: invalid ignore rule `{pattern}`: {err}"),
                }
            }
        }

        let mut custom_sort = HashMap::new();
        if let Some(entries) = config.get("custom-sort").and_then(|v| v.as_array()) {
            let mut priority = 0u16;
            for entry in entries {
                let Some(name) = entry.as_str() else {
                    continue;
                };
                custom_sort.insert(name.to_string(), priority);
                let trimmed = trim_workspace_name(name);
                if trimmed != name {
                    custom_sort.insert(trimmed.to_string(), priority);
                }
                priority = priority.saturating_add(1);
            }
        }

        SwayConfig {
            all_outputs: config
                .get("all-outputs")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            ignore,
            custom_sort,
            alphabetical: config
                .get("alphabetical_sort")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            persistent: config
                .get("persistent-workspaces")
                .cloned()
                .unwrap_or(Value::Null),
        }
    }

    fn is_ignored(&self, name: &str) -> bool {
        self.ignore.iter().any(|regex| regex.is_match(name))
    }
}

// --- pure helpers (unit-tested) -----------------------------------------

/// Assign a number to a workspace the way sway does (`ipc-json.c`).
fn convert_workspace_name_to_num(name: &str) -> i32 {
    if !name.starts_with(|c: char| c.is_ascii_digit()) {
        return -1;
    }
    let digits: String = name.chars().take_while(|c| c.is_ascii_digit()).collect();
    match digits.parse::<i64>() {
        Ok(n) if n <= i64::from(i32::MAX) => n as i32,
        _ => -1,
    }
}

/// Everything after the first `:` (sway's `name:display` convention).
fn trim_workspace_name(name: &str) -> &str {
    match name.find(':') {
        Some(pos) => &name[pos + 1..],
        None => name,
    }
}

/// `hasFlag` from upstream: a workspace's own flag or any descendant's.
fn has_flag(node: &Node, flag: &str) -> bool {
    let own = match flag {
        "focused" => node.focused,
        "visible" => node.visible.unwrap_or(false),
        "urgent" => node.urgent,
        _ => false,
    };
    if own {
        return true;
    }
    node.nodes.iter().any(|child| has_flag(child, flag))
        || node
            .floating_nodes
            .iter()
            .any(|child| has_flag(child, flag))
}

/// Sort workspaces the way sway orders them, honouring `custom-sort` and
/// `alphabetical_sort`. Persistent (unnumbered) workspaces keep sway's order.
fn sort_workspaces(workspaces: &mut [Workspace], config: &SwayConfig) {
    let mut max_num = -1;
    for workspace in workspaces.iter() {
        max_num = max_num.max(workspace.num);
    }
    let mut next = max_num;
    let mut sorts: Vec<i32> = Vec::with_capacity(workspaces.len());
    for workspace in workspaces.iter() {
        if workspace.num > -1 {
            sorts.push(workspace.num);
        } else {
            next += 1;
            sorts.push(next);
        }
    }

    let mut order: Vec<usize> = (0..workspaces.len()).collect();
    order.sort_by(|&a, &b| {
        let (lhs, rhs) = (&workspaces[a], &workspaces[b]);
        if !config.custom_sort.is_empty() {
            let lcustom = custom_sort_index(config, &lhs.name);
            let rcustom = custom_sort_index(config, &rhs.name);
            match (lcustom, rcustom) {
                (Some(l), Some(r)) if l != r => return l.cmp(&r),
                (Some(_), None) => return std::cmp::Ordering::Less,
                (None, Some(_)) => return std::cmp::Ordering::Greater,
                _ => {}
            }
        }
        let (l, r) = (sorts[a], sorts[b]);
        if l == r || config.alphabetical {
            lhs.name.cmp(&rhs.name)
        } else {
            l.cmp(&r)
        }
    });

    // Apply the permutation in place.
    let original: Vec<Workspace> = workspaces.to_vec();
    for (target, &source) in order.iter().enumerate() {
        workspaces[target] = original[source].clone();
    }
}

fn custom_sort_index(config: &SwayConfig, name: &str) -> Option<u16> {
    if config.custom_sort.is_empty() {
        return None;
    }
    if let Some(priority) = config.custom_sort.get(name) {
        return Some(*priority);
    }
    let trimmed = trim_workspace_name(name);
    if trimmed != name {
        return config.custom_sort.get(trimmed).copied();
    }
    None
}

/// Pick an icon from `format-icons` using upstream's key priority.
fn workspace_icon(config: &Value, name: &str, workspace: &Workspace) -> String {
    let Some(icons) = config.get("format-icons").and_then(|v| v.as_object()) else {
        return name.to_string();
    };

    if let Some(high_priority) = icons.get("high-priority-named").and_then(|v| v.as_array()) {
        for member in high_priority {
            let Some(member) = member.as_str() else {
                continue;
            };
            if member == name
                && let Some(icon) = icons.get(name).and_then(|v| v.as_str())
            {
                return icon.to_string();
            }
            let trimmed = trim_workspace_name(member);
            if trimmed == trim_workspace_name(name)
                && let Some(icon) = icons.get(trimmed).and_then(|v| v.as_str())
            {
                return icon.to_string();
            }
        }
    }

    for key in ["urgent", "focused", name, "default"] {
        if key == "focused" || key == "urgent" {
            let active = if key == "focused" {
                workspace.focused
            } else {
                workspace.urgent
            };
            if active && let Some(icon) = icons.get(key).and_then(|v| v.as_str()) {
                return icon.to_string();
            }
        } else if let Some(icon) = icons.get("persistent").and_then(|v| v.as_str()) {
            if workspace.target_output.is_some() {
                return icon.to_string();
            }
        } else if let Some(icon) = icons.get(key).and_then(|v| v.as_str()) {
            return icon.to_string();
        } else if let Some(icon) = icons.get(trim_workspace_name(key)).and_then(|v| v.as_str()) {
            return icon.to_string();
        }
    }

    name.to_string()
}

/// Build a snapshot of the visible workspaces from a sway layout tree.
fn build_snapshot(tree: &Node, config: &SwayConfig, output_name: &str) -> Vec<Workspace> {
    let mut workspaces = Vec::new();

    for output in &tree.nodes {
        let name = output.name.as_deref().unwrap_or("");
        // When no monitor could be resolved (empty output name) fall back to
        // every output rather than showing nothing. The scratchpad output is
        // never shown.
        let selected =
            name != "__i3" && (output_name.is_empty() || config.all_outputs || name == output_name);
        if !selected {
            continue;
        }
        for node in output
            .nodes
            .iter()
            .filter(|node| !config.is_ignored(node.name.as_deref().unwrap_or("")))
        {
            workspaces.push(workspace_from_node(node));
        }
        for node in &output.floating_nodes {
            workspaces.push(workspace_from_node(node));
        }
    }

    // Add persistent workspaces that sway doesn't know about.
    if let Some(persistent) = config.persistent.as_object() {
        for (name, targets) in persistent {
            if workspaces.iter().any(|workspace| &workspace.name == name) {
                continue;
            }
            let target = persistent_target(targets, output_name);
            if let Some(target_output) = target {
                workspaces.push(Workspace {
                    name: name.clone(),
                    num: convert_workspace_name_to_num(name),
                    output: String::new(),
                    target_output: Some(target_output),
                    focused: false,
                    visible: false,
                    urgent: false,
                    empty: true,
                });
            }
        }
    }

    sort_workspaces(&mut workspaces, config);
    workspaces
}

/// Whether a `persistent-workspaces` entry applies to this bar, and on which
/// output. Returns `Some("")` when it should show on every output.
fn persistent_target(targets: &Value, output_name: &str) -> Option<String> {
    match targets {
        Value::Array(outputs) if !outputs.is_empty() => {
            for output in outputs {
                if output.as_str() == Some(output_name) {
                    return Some(output_name.to_string());
                }
            }
            None
        }
        _ => Some(String::new()),
    }
}

fn workspace_from_node(node: &Node) -> Workspace {
    let name = node.name.clone().unwrap_or_default();
    let empty = node.nodes.is_empty() && node.floating_nodes.is_empty();
    Workspace {
        num: node
            .num
            .unwrap_or_else(|| convert_workspace_name_to_num(&name)),
        output: node.output.clone().unwrap_or_default(),
        focused: has_flag(node, "focused"),
        visible: has_flag(node, "visible"),
        urgent: has_flag(node, "urgent"),
        target_output: None,
        empty,
        name,
    }
}

/// The sway command a workspace button click should run.
fn switch_command(workspace: &Workspace, config: &Value) -> String {
    if let Some(target) = &workspace.target_output {
        return format!(
            "workspace --no-auto-back-and-forth \"{name}\"; \
             move workspace to output \"{target}\"; \
             workspace --no-auto-back-and-forth \"{name}\"",
            name = workspace.name,
            target = target,
        );
    }
    let flag = if config
        .get("disable-auto-back-and-forth")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        "--no-auto-back-and-forth "
    } else {
        ""
    };
    if config
        .get("no-switch-output")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        format!(
            "[workspace=\"^{name}$\"] move workspace to output current; \
             workspace {flag}number \"{name}\"",
            name = workspace.name,
        )
    } else if workspace.num >= 0 {
        format!("workspace {flag}number {}", workspace.num)
    } else {
        format!("workspace {flag}\"{}\"", workspace.name)
    }
}

/// Pick the next/previous workspace name for scroll cycling.
fn cycle_name(workspaces: &[Workspace], index: usize, prev: bool, disable_wrap: bool) -> String {
    if prev {
        if index == 0 {
            if disable_wrap {
                return workspaces[0].name.clone();
            }
            return workspaces[workspaces.len() - 1].name.clone();
        }
        workspaces[index - 1].name.clone()
    } else {
        if index + 1 >= workspaces.len() {
            if disable_wrap {
                if index == 0 {
                    return workspaces[0].name.clone();
                }
                return workspaces[index - 1].name.clone();
            }
            return workspaces[0].name.clone();
        }
        workspaces[index + 1].name.clone()
    }
}

// --- UI -----------------------------------------------------------------

/// A command for the worker to run through sway.
#[derive(Clone, Debug)]
enum Command {
    Run(String),
}

/// The main-thread side of the module, cloneable so the update closure and the
/// scroll/click controllers can share it.
#[derive(Clone)]
struct Ui {
    root: gtk::Box,
    box_: gtk::Box,
    buttons: Rc<RefCell<HashMap<String, gtk::Button>>>,
    snapshot: Rc<RefCell<Vec<Workspace>>>,
    config: Value,
    output_name: String,
    command_tx: async_channel::Sender<Command>,
}

impl Ui {
    fn send(&self, command: impl Into<String>) {
        let _ = self.command_tx.try_send(Command::Run(command.into()));
    }

    fn attach_scroll(&self) {
        if self
            .config
            .get("disable-scroll")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return;
        }
        let ui = self.clone();
        let controller =
            gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        controller.connect_scroll(move |_, dx, dy| {
            let down = dy > 0.0 || (dy == 0.0 && dx > 0.0);
            let up = dy < 0.0 || (dy == 0.0 && dx < 0.0);
            if !down && !up {
                return gtk::glib::Propagation::Proceed;
            }
            ui.scroll(down);
            gtk::glib::Propagation::Proceed
        });
        self.box_.add_controller(controller);
    }

    fn scroll(&self, down: bool) {
        let all_outputs = self
            .config
            .get("all-outputs")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let snapshot = self.snapshot.borrow();
        let Some(index) = snapshot.iter().position(|workspace| {
            if all_outputs {
                workspace.focused
            } else {
                workspace.visible || (!workspace.output.is_empty() && workspace.empty)
            }
        }) else {
            return;
        };
        let current = snapshot[index].name.clone();
        let reverse = self
            .config
            .get("reverse-scroll")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let prev = if down { reverse } else { !reverse };
        let disable_wrap = self
            .config
            .get("disable-scroll-wraparound")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let next = cycle_name(&snapshot, index, prev, disable_wrap);
        drop(snapshot);
        if next == current {
            return;
        }

        let warp = self.config.get("warp-on-scroll").and_then(|v| v.as_bool());
        if warp == Some(false) {
            self.send("mouse_warping none");
        }
        self.send(format!("workspace --no-auto-back-and-forth \"{next}\""));
        if warp == Some(false) {
            self.send("mouse_warping container");
        }
    }

    fn apply(&self, workspaces: Vec<Workspace>) {
        // Drop buttons for workspaces that disappeared.
        let live: Vec<&str> = workspaces.iter().map(|w| w.name.as_str()).collect();
        let stale: Vec<String> = self
            .buttons
            .borrow()
            .keys()
            .filter(|name| !live.contains(&name.as_str()))
            .cloned()
            .collect();
        for name in stale {
            if let Some(button) = self.buttons.borrow_mut().remove(&name) {
                self.box_.remove(&button);
            }
        }

        let mut ordered = Vec::with_capacity(workspaces.len());
        for workspace in &workspaces {
            let existing = self.buttons.borrow().get(&workspace.name).cloned();
            let button = match existing {
                Some(button) => button,
                None => {
                    let button = self.add_button(workspace);
                    self.box_.append(&button);
                    self.buttons
                        .borrow_mut()
                        .insert(workspace.name.clone(), button.clone());
                    button
                }
            };
            self.update_button(&button, workspace);
            ordered.push(button);
        }
        self.reorder(&ordered);

        tracing::debug!("sway/workspaces: {} workspaces", workspaces.len());
        *self.snapshot.borrow_mut() = workspaces;
    }

    /// Make the box children match `ordered`, re-appending only when needed.
    ///
    /// GTK4's `GtkBox` has no absolute `reorder_child`, so a mismatch is fixed
    /// by removing and appending the buttons in order.
    fn reorder(&self, ordered: &[gtk::Button]) {
        let mut current = Vec::new();
        let mut child = self.box_.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            current.push(widget);
        }
        let matches = current.len() == ordered.len()
            && current
                .iter()
                .zip(ordered)
                .all(|(widget, button)| widget == button.upcast_ref::<gtk::Widget>());
        if matches {
            return;
        }
        for widget in &current {
            self.box_.remove(widget);
        }
        for button in ordered {
            self.box_.append(button);
        }
    }

    fn add_button(&self, workspace: &Workspace) -> gtk::Button {
        let label = gtk::Label::new(None);
        let button = gtk::Button::new();
        button.set_has_frame(false);
        button.set_child(Some(&label));
        button.set_widget_name(&format!("sway-workspace-{}", workspace.name));

        if !self
            .config
            .get("disable-click")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            let ui = self.clone();
            let workspace = workspace.clone();
            button.connect_clicked(move |_| {
                ui.send(switch_command(&workspace, &ui.config));
            });
        }
        button
    }

    fn update_button(&self, button: &gtk::Button, workspace: &Workspace) {
        let set = |class: &str, on: bool| {
            if on {
                button.add_css_class(class);
            } else {
                button.remove_css_class(class);
            }
        };
        set("focused", workspace.focused);
        set(
            "visible",
            workspace.visible || (!workspace.output.is_empty() && workspace.empty),
        );
        set("urgent", workspace.urgent);
        set("persistent", workspace.target_output.is_some());
        set("empty", workspace.empty);

        // Reset the configured output classes, then apply the matching one.
        if let Some(classes) = self
            .config
            .get("output-classes")
            .and_then(|v| v.as_object())
        {
            for class in classes.values().filter_map(|v| v.as_str()) {
                button.remove_css_class(class);
            }
        }
        if !workspace.output.is_empty() {
            if let Some(classes) = self
                .config
                .get("output-classes")
                .and_then(|v| v.as_object())
                && let Some(class) = classes.get(&workspace.output).and_then(|v| v.as_str())
            {
                button.add_css_class(class);
            }
            set("current_output", workspace.output == self.output_name);
        } else {
            set("current_output", false);
        }

        let markup = !self
            .config
            .get("disable-markup")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut full_name = if markup {
            gtk::glib::markup_escape_text(&workspace.name).to_string()
        } else {
            workspace.name.clone()
        };
        if let Some(format) = self.config.get("format").and_then(|v| v.as_str()) {
            let index = workspace.num;
            let format = if index < 0 {
                self.config
                    .get("format-for-negative-index")
                    .and_then(|v| v.as_str())
                    .unwrap_or(format)
            } else {
                format
            };
            let name = trim_workspace_name(&full_name).to_string();
            let icon = workspace_icon(&self.config, &full_name, workspace);
            let mut args = Args::new();
            args.insert("index".into(), FmtArg::Int(i64::from(index)));
            args.insert("name".into(), FmtArg::from(name));
            args.insert("value".into(), FmtArg::from(full_name.clone()));
            args.insert("output".into(), FmtArg::from(workspace.output.clone()));
            args.insert("icon".into(), FmtArg::from(icon));
            args.insert("windows".into(), FmtArg::from(""));
            full_name = fmt::format_ctx(format, &args, "sway/workspaces");
        }

        if let Some(label) = button.child().and_then(|w| w.downcast::<gtk::Label>().ok()) {
            if markup {
                label.set_markup(&full_name);
            } else {
                label.set_label(&full_name);
            }
        }

        let current_only = self
            .config
            .get("current-only")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        button.set_visible(!current_only || workspace.focused);
    }
}

// --- workers ------------------------------------------------------------

async fn event_worker(
    config: Value,
    output_name: String,
    tx: async_channel::Sender<Vec<Workspace>>,
) {
    let parsed = SwayConfig::parse(&config);

    let event_conn = match Connection::new().await {
        Ok(conn) => conn,
        Err(err) => {
            error!("sway/workspaces: cannot connect to sway: {err}");
            return;
        }
    };
    let mut events = match event_conn
        .subscribe([EventType::Workspace, EventType::Window])
        .await
    {
        Ok(events) => events,
        Err(err) => {
            error!("sway/workspaces: cannot subscribe: {err}");
            return;
        }
    };
    let mut tree_conn = match Connection::new().await {
        Ok(conn) => conn,
        Err(err) => {
            error!("sway/workspaces: cannot open command connection: {err}");
            return;
        }
    };

    refresh(&mut tree_conn, &parsed, &output_name, &tx).await;

    while let Some(event) = events.next().await {
        match event {
            Ok(_) => refresh(&mut tree_conn, &parsed, &output_name, &tx).await,
            Err(err) => warn!("sway/workspaces: event error: {err}"),
        }
    }
}

async fn refresh(
    conn: &mut Connection,
    config: &SwayConfig,
    output_name: &str,
    tx: &async_channel::Sender<Vec<Workspace>>,
) {
    match conn.get_tree().await {
        Ok(tree) => {
            let _ = tx.send(build_snapshot(&tree, config, output_name)).await;
        }
        Err(err) => warn!("sway/workspaces: cannot read tree: {err}"),
    }
}

async fn command_worker(rx: async_channel::Receiver<Command>) {
    let mut conn = match Connection::new().await {
        Ok(conn) => conn,
        Err(err) => {
            error!("sway/workspaces: cannot open command connection: {err}");
            return;
        }
    };
    while let Ok(Command::Run(command)) = rx.recv().await {
        if let Err(err) = conn.run_command(&command).await {
            warn!("sway/workspaces: command `{command}` failed: {err}");
        }
    }
}

// --- module -------------------------------------------------------------

struct Workspaces {
    ui: Ui,
    command_rx: RefCell<Option<async_channel::Receiver<Command>>>,
}

impl Module for Workspaces {
    fn widget(&self) -> gtk::Widget {
        self.ui.root.clone().upcast()
    }

    fn start(&self) {
        let (tx, rx) = bridge::channel::<Vec<Workspace>>();

        let config = self.ui.config.clone();
        let output_name = self.ui.output_name.clone();
        bridge::spawn(async move { event_worker(config, output_name, tx).await });

        if let Some(command_rx) = self.command_rx.borrow_mut().take() {
            bridge::spawn(command_worker(command_rx));
        }

        let ui = self.ui.clone();
        bridge::on_main(rx, move |workspaces| ui.apply(workspaces));
    }
}

pub fn register(reg: &mut Registry) {
    reg.register("sway/workspaces", |init: &ModuleInit| {
        let orientation = if init.bar.position.is_vertical() {
            gtk::Orientation::Vertical
        } else {
            gtk::Orientation::Horizontal
        };

        let box_ = gtk::Box::new(orientation, 0);
        box_.set_widget_name("workspaces");
        box_.add_css_class("module");
        if !init.id.is_empty() {
            box_.add_css_class(&init.id);
        }

        // The extra wrapper mirrors upstream's event box so selectors such as
        // `.modules-left > widget:first-child > #workspaces` keep working.
        let root = gtk::Box::new(orientation, 0);
        root.append(&box_);

        let (command_tx, command_rx) = bridge::channel::<Command>();
        let ui = Ui {
            root,
            box_,
            buttons: Rc::new(RefCell::new(HashMap::new())),
            snapshot: Rc::new(RefCell::new(Vec::new())),
            config: init.config.clone(),
            output_name: init.output_name.clone(),
            command_tx,
        };
        ui.attach_scroll();

        Ok(Box::new(Workspaces {
            ui,
            command_rx: RefCell::new(Some(command_rx)),
        }))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ws(name: &str, num: i32) -> Workspace {
        Workspace {
            name: name.to_string(),
            num,
            output: String::new(),
            target_output: None,
            focused: false,
            visible: false,
            urgent: false,
            empty: false,
        }
    }

    fn empty_config() -> SwayConfig {
        SwayConfig::parse(&json!({}))
    }

    #[test]
    fn converts_numeric_names() {
        assert_eq!(convert_workspace_name_to_num("1"), 1);
        assert_eq!(convert_workspace_name_to_num("10:web"), 10);
        assert_eq!(convert_workspace_name_to_num("name"), -1);
        assert_eq!(convert_workspace_name_to_num("99999999999999999999"), -1);
    }

    #[test]
    fn trims_at_colon() {
        assert_eq!(trim_workspace_name("1:web"), "web");
        assert_eq!(trim_workspace_name("web"), "web");
        assert_eq!(trim_workspace_name("2:"), "");
    }

    #[test]
    fn sorts_numbered_then_named() {
        let mut workspaces = vec![ws("web", -1), ws("2", 2), ws("1", 1)];
        sort_workspaces(&mut workspaces, &empty_config());
        let names: Vec<&str> = workspaces.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, vec!["1", "2", "web"]);
    }

    #[test]
    fn alphabetical_sort() {
        let config = SwayConfig::parse(&json!({"alphabetical_sort": true}));
        let mut workspaces = vec![ws("2", 2), ws("10", 10), ws("1", 1)];
        sort_workspaces(&mut workspaces, &config);
        let names: Vec<&str> = workspaces.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, vec!["1", "10", "2"]);
    }

    #[test]
    fn custom_sort_takes_priority() {
        let config = SwayConfig::parse(&json!({"custom-sort": ["web", "1"]}));
        let mut workspaces = vec![ws("2", 2), ws("web", -1), ws("1", 1)];
        sort_workspaces(&mut workspaces, &config);
        let names: Vec<&str> = workspaces.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, vec!["web", "1", "2"]);
    }

    #[test]
    fn cycles_with_wraparound() {
        let workspaces = vec![ws("1", 1), ws("2", 2), ws("3", 3)];
        assert_eq!(cycle_name(&workspaces, 0, false, false), "2");
        assert_eq!(cycle_name(&workspaces, 2, false, false), "1");
        assert_eq!(cycle_name(&workspaces, 0, true, false), "3");
        assert_eq!(cycle_name(&workspaces, 1, true, false), "1");
        // no wraparound: stay put at the ends
        assert_eq!(cycle_name(&workspaces, 0, true, true), "1");
        assert_eq!(cycle_name(&workspaces, 2, false, true), "2");
    }

    #[test]
    fn switch_command_for_numbered_and_named() {
        let mut numbered = ws("3", 3);
        numbered.output = "DP-1".into();
        assert_eq!(switch_command(&numbered, &json!({})), "workspace number 3");

        let named = ws("web", -1);
        assert_eq!(switch_command(&named, &json!({})), "workspace \"web\"");

        let persistent = Workspace {
            target_output: Some("DP-1".into()),
            ..ws("web", -1)
        };
        assert!(switch_command(&persistent, &json!({})).contains("move workspace to output"));
    }

    #[test]
    fn icon_prefers_focused_then_default() {
        let config = json!({"format-icons": {"focused": "F", "default": "D", "1": "ONE"}});
        let mut focused = ws("1", 1);
        focused.focused = true;
        assert_eq!(workspace_icon(&config, "1", &focused), "F");
        let plain = ws("1", 1);
        assert_eq!(workspace_icon(&config, "1", &plain), "ONE");
        let other = ws("2", 2);
        assert_eq!(workspace_icon(&config, "2", &other), "D");
    }
}
