//! `tray` — a StatusNotifierItem host with its own GTK4 rendering.
//!
//! The [`system_tray`] client provides the Watcher/Host, watches SNI items and
//! their `DBusMenu` layouts, and exposes an event stream plus `activate` /
//! `about_to_show_menuitem` calls. This module renders one widget per item
//! (`#tray > .<status>`) and a [`gtk::Popover`] menu built from the `DBusMenu`
//! tree.
//!
//! Not ported: the SNI `Scroll` and `ContextMenu` methods (the crate does not
//! expose them), overlay icons, and per-app icon overrides (`icons`).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use gtk::prelude::*;
use serde_json::Value;
use system_tray::client::{ActivateRequest, Client, Event as TrayEvent};
use system_tray::data::BaseMap;
use system_tray::item::{IconPixmap, Status, StatusNotifierItem};
use system_tray::menu::{MenuItem, MenuType, ToggleState, ToggleType, TrayMenu};
use tokio::sync::broadcast;
use tracing::{debug, trace, warn};

use waybar_core::bridge;
use waybar_core::widgets::attach_single_click;
use waybar_core::{Module, ModuleInit, Registry};

const DEFAULT_ICON_SIZE: i32 = 16;

/// A message from the tray client to the GTK thread.
enum Msg {
    Init(Arc<Mutex<BaseMap>>),
    Add(String, Box<StatusNotifierItem>),
    Update(String),
    Remove(String),
}

struct ItemWidget {
    container: gtk::Box,
    image: gtk::Image,
    popover: gtk::Popover,
    address: String,
    order_key: String,
    item_is_menu: bool,
    menu_path: Option<String>,
    status: Status,
    visible: bool,
}

struct TrayUi {
    client: Option<Arc<Client>>,
    map: Option<Arc<Mutex<BaseMap>>>,
    widgets: HashMap<String, ItemWidget>,
    icon_size: i32,
    show_passive: bool,
    ignore_list: Vec<String>,
    registered_icon_paths: HashSet<String>,
    open_menu: Option<OpenMenu>,
}

struct OpenMenu {
    key: String,
    address: String,
    menu_path: String,
    popover: gtk::Popover,
}

struct Tray {
    /// Unnamed wrapper so `.modules-left > * > *` matches the tray once, not
    /// every icon container.
    wrapper: gtk::Box,
    icons: gtk::Box,
    ui: Rc<RefCell<TrayUi>>,
}

impl Module for Tray {
    fn widget(&self) -> gtk::Widget {
        self.wrapper.clone().upcast()
    }

    fn start(&self) {
        let (tx, rx) = bridge::channel::<Msg>();
        let (client_tx, client_rx) = bridge::channel::<Arc<Client>>();

        bridge::spawn(async move {
            let client = match Client::new().await {
                Ok(client) => Arc::new(client),
                Err(err) => {
                    warn!("tray: cannot start the StatusNotifier client: {err}");
                    return;
                }
            };
            // Subscribe before snapshotting; otherwise an item added in
            // between would be missing from both.
            let mut events = client.subscribe();
            let map = client.items();
            let _ = client_tx.send(client.clone()).await;
            let _ = tx.send(Msg::Init(map)).await;
            loop {
                match events.recv().await {
                    Ok(TrayEvent::Add(key, item)) => {
                        let _ = tx.send(Msg::Add(key, item)).await;
                    }
                    Ok(TrayEvent::Update(key, _event)) => {
                        let _ = tx.send(Msg::Update(key)).await;
                    }
                    Ok(TrayEvent::Remove(key)) => {
                        let _ = tx.send(Msg::Remove(key)).await;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });

        let ui = self.ui.clone();
        bridge::on_main(client_rx, move |client| {
            ui.borrow_mut().client = Some(client);
        });

        let icons = self.icons.clone();
        let wrapper = self.wrapper.clone();
        let ui = self.ui.clone();
        bridge::on_main(rx, move |msg| handle_msg(&icons, &wrapper, &ui, msg));
    }
}

fn handle_msg(icons: &gtk::Box, wrapper: &gtk::Box, ui: &Rc<RefCell<TrayUi>>, msg: Msg) {
    match msg {
        Msg::Init(map) => {
            let entries: Vec<(String, StatusNotifierItem)> = {
                let map = map.lock().unwrap_or_else(|err| err.into_inner());
                map.iter()
                    .map(|(key, (item, _))| (key.clone(), item.clone()))
                    .collect()
            };
            ui.borrow_mut().map = Some(map);
            for (key, item) in entries {
                refresh(icons, ui, &key, &item);
            }
            reorder_icons(icons, ui);
            update_visibility(wrapper, ui);
        }
        Msg::Add(key, item) => {
            refresh(icons, ui, &key, &item);
            reorder_icons(icons, ui);
            update_visibility(wrapper, ui);
        }
        Msg::Update(key) => {
            if let Some(item) = lookup_item(ui, &key) {
                refresh(icons, ui, &key, &item);
                update_visibility(wrapper, ui);
            }
            refresh_open_menu(ui, &key);
        }
        Msg::Remove(key) => {
            let widget = ui.borrow_mut().widgets.remove(&key);
            if let Some(widget) = widget {
                if widget.popover.is_visible() {
                    widget.popover.popdown();
                }
                widget.popover.unparent();
                icons.remove(&widget.container);
                update_visibility(wrapper, ui);
            }
            let mut ui = ui.borrow_mut();
            if ui.open_menu.as_ref().is_some_and(|menu| menu.key == key) {
                ui.open_menu = None;
            }
        }
    }
}

fn lookup_item(ui: &Rc<RefCell<TrayUi>>, key: &str) -> Option<StatusNotifierItem> {
    let map = ui.borrow().map.clone()?;
    let map = map.lock().ok()?;
    map.get(key).map(|(item, _)| item.clone())
}

fn lookup_menu(ui: &Rc<RefCell<TrayUi>>, key: &str) -> Option<TrayMenu> {
    let map = ui.borrow().map.clone()?;
    let map = map.lock().ok()?;
    map.get(key).and_then(|(_, menu)| menu.clone())
}

/// Rebuild the shown menu after a `Menu`/`MenuDiff` update.
fn refresh_open_menu(ui: &Rc<RefCell<TrayUi>>, key: &str) {
    let (address, menu_path, popover) = {
        let ui = ui.borrow();
        let Some(open) = &ui.open_menu else {
            return;
        };
        if open.key != key || !open.popover.is_visible() {
            return;
        }
        (
            open.address.clone(),
            open.menu_path.clone(),
            open.popover.clone(),
        )
    };
    let Some(client) = ui.borrow().client.clone() else {
        return;
    };
    let Some(menu) = lookup_menu(ui, key) else {
        return;
    };
    popover.set_child(Some(&build_menu(&client, &address, &menu_path, &menu)));
}

/// Create the item widget if needed, then apply its current state.
fn refresh(icons: &gtk::Box, ui: &Rc<RefCell<TrayUi>>, key: &str, item: &StatusNotifierItem) {
    if !ui.borrow().widgets.contains_key(key) {
        let widget = create_widget(icons, ui, key, item);
        ui.borrow_mut().widgets.insert(key.to_string(), widget);
    }
    apply(ui, key, item);
}

fn create_widget(
    icons: &gtk::Box,
    ui: &Rc<RefCell<TrayUi>>,
    key: &str,
    item: &StatusNotifierItem,
) -> ItemWidget {
    debug!("tray: adding item `{key}` ({})", item.id);
    let container = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let image = gtk::Image::new();
    container.append(&image);

    let popover = gtk::Popover::new();
    popover.set_parent(&container);

    {
        let weak = Rc::downgrade(ui);
        let key = key.to_string();
        popover.connect_closed(move |_| {
            if let Some(ui) = weak.upgrade() {
                let mut ui = ui.borrow_mut();
                if ui.open_menu.as_ref().is_some_and(|menu| menu.key == key) {
                    ui.open_menu = None;
                }
            }
        });
    }

    {
        let ui = ui.clone();
        let key = key.to_string();
        attach_single_click(&container, &Value::Null, 0, move |button| {
            on_click(&ui, &key, button);
        });
    }

    icons.append(&container);

    ItemWidget {
        container,
        image,
        popover,
        address: key.to_string(),
        order_key: order_key(&item.id, key),
        item_is_menu: item.item_is_menu,
        menu_path: item.menu.clone(),
        status: Status::Unknown,
        visible: true,
    }
}

fn apply(ui: &Rc<RefCell<TrayUi>>, key: &str, item: &StatusNotifierItem) {
    let (icon_size, show_passive, ignored) = {
        let ui = ui.borrow();
        (
            ui.icon_size,
            ui.show_passive,
            is_ignored(&ui.ignore_list, key, item),
        )
    };

    let mut ui = ui.borrow_mut();

    if let Some(path) = &item.icon_theme_path
        && ui.registered_icon_paths.insert(path.clone())
        && let Some(theme) = icon_theme()
    {
        theme.add_search_path(path);
    }

    let Some(widget) = ui.widgets.get_mut(key) else {
        return;
    };

    widget.item_is_menu = item.item_is_menu;
    widget.menu_path = item.menu.clone();
    widget.status = item.status;

    apply_icon(&widget.image, item, icon_size);

    for class in ["active", "passive", "needs-attention"] {
        widget.container.remove_css_class(class);
    }
    if let Some(class) = status_class(item.status) {
        widget.container.add_css_class(class);
    }

    match tooltip_markup(item) {
        Some(markup) => widget.container.set_tooltip_markup(Some(&markup)),
        None => widget.container.set_tooltip_markup(None::<&str>),
    }

    let hidden = ignored || (item.status == Status::Passive && !show_passive);
    let visible = !hidden;
    if widget.visible != visible {
        debug!("tray: `{key}` visible={visible}");
    }
    widget.visible = visible;
    widget.container.set_visible(visible);
    trace!(
        "tray: `{key}` status={:?} hidden={hidden} ignored={ignored} icon_name={:?} pixmap={}",
        item.status,
        item.icon_name,
        item.icon_pixmap.as_ref().map_or(0, Vec::len)
    );
}

fn status_class(status: Status) -> Option<&'static str> {
    match status {
        Status::Active => Some("active"),
        Status::Passive => Some("passive"),
        Status::NeedsAttention => Some("needs-attention"),
        Status::Unknown => None,
    }
}

fn is_ignored(ignore_list: &[String], key: &str, item: &StatusNotifierItem) -> bool {
    if ignore_list.is_empty() {
        return false;
    }
    let category = format!("{:?}", item.category);
    let icon = item.icon_name.clone().unwrap_or_default();
    let title = item.title.clone().unwrap_or_default();
    ignore_list.iter().any(|pattern| {
        key.contains(pattern)
            || category.contains(pattern)
            || icon.contains(pattern)
            || item.id.contains(pattern)
            || title.contains(pattern)
    })
}

fn update_visibility(wrapper: &gtk::Box, ui: &Rc<RefCell<TrayUi>>) {
    let any = ui.borrow().widgets.values().any(|widget| widget.visible);
    wrapper.set_visible(any);
}

/// Position key: the app id, or the bus name when the app has none.
fn order_key(id: &str, key: &str) -> String {
    if id.is_empty() {
        key.to_string()
    } else {
        id.to_string()
    }
}

/// Keep the item containers in fixed [`order_key`] order (ties by bus name).
fn reorder_icons(icons: &gtk::Box, ui: &Rc<RefCell<TrayUi>>) {
    let ui = ui.borrow();
    let mut entries: Vec<&ItemWidget> = ui.widgets.values().collect();
    entries.sort_by(|a, b| {
        a.order_key
            .cmp(&b.order_key)
            .then_with(|| a.address.cmp(&b.address))
    });
    let mut prev: Option<gtk::Widget> = None;
    for widget in entries {
        icons.reorder_child_after(&widget.container, prev.as_ref());
        prev = Some(widget.container.clone().upcast());
    }
}

// --- icons --------------------------------------------------------------

fn apply_icon(image: &gtk::Image, item: &StatusNotifierItem, size: i32) {
    let theme = icon_theme();
    if let Some(name) = &item.icon_name
        && !name.is_empty()
        && theme.as_ref().is_some_and(|theme| theme.has_icon(name))
    {
        trace!("tray: `{}` icon from theme `{name}`", item.id);
        image.set_icon_name(Some(name));
        image.set_pixel_size(size);
        return;
    }
    if let Some(pixmaps) = &item.icon_pixmap
        && let Some(texture) = pixmap_texture(pixmaps)
    {
        trace!(
            "tray: `{}` icon from pixmap ({}x{})",
            item.id,
            texture.width(),
            texture.height()
        );
        image.set_paintable(Some(&texture));
        image.set_pixel_size(size);
        return;
    }
    debug!(
        "tray: `{}` has no usable icon (name={:?}, pixmap={})",
        item.id,
        item.icon_name,
        item.icon_pixmap.is_some()
    );
    image.set_icon_name(Some("image-missing"));
    image.set_pixel_size(size);
}

fn icon_theme() -> Option<gtk::IconTheme> {
    gtk::gdk::Display::default().map(|display| gtk::IconTheme::for_display(&display))
}

/// Convert the largest ARGB pixmap (network byte order) into an RGBA texture.
fn pixmap_texture(pixmaps: &[IconPixmap]) -> Option<gtk::gdk::MemoryTexture> {
    let best = pixmaps
        .iter()
        .max_by_key(|pixmap| pixmap.width.saturating_mul(pixmap.height))?;
    let (width, height) = (best.width, best.height);
    if width <= 0 || height <= 0 {
        return None;
    }
    let expected = width as usize * height as usize * 4;
    if best.pixels.len() < expected {
        return None;
    }
    let mut rgba = best.pixels[..expected].to_vec();
    for pixel in rgba.as_chunks_mut::<4>().0 {
        // ARGB -> RGBA.
        let alpha = pixel[0];
        pixel[0] = pixel[1];
        pixel[1] = pixel[2];
        pixel[2] = pixel[3];
        pixel[3] = alpha;
    }
    let bytes = gtk::glib::Bytes::from_owned(rgba);
    Some(gtk::gdk::MemoryTexture::new(
        width,
        height,
        gtk::gdk::MemoryFormat::R8g8b8a8,
        &bytes,
        width as usize * 4,
    ))
}

// --- clicks and menus ---------------------------------------------------

fn on_click(ui: &Rc<RefCell<TrayUi>>, key: &str, button: u32) {
    let (item_is_menu, popover, address, menu_path) = {
        let ui = ui.borrow();
        let Some(widget) = ui.widgets.get(key) else {
            return;
        };
        (
            widget.item_is_menu,
            widget.popover.clone(),
            widget.address.clone(),
            widget.menu_path.clone(),
        )
    };

    if button == 1 && !item_is_menu {
        activate(
            ui,
            ActivateRequest::Default {
                address,
                x: 0,
                y: 0,
            },
        );
        return;
    }
    if button == 1 || button == 3 {
        open_menu(ui, key, &popover, &address, menu_path.as_deref());
        return;
    }
    if button == 2 {
        activate(
            ui,
            ActivateRequest::Secondary {
                address,
                x: 0,
                y: 0,
            },
        );
    }
}

fn activate(ui: &Rc<RefCell<TrayUi>>, request: ActivateRequest) {
    let Some(client) = ui.borrow().client.clone() else {
        return;
    };
    bridge::spawn(async move {
        if let Err(err) = client.activate(request).await {
            debug!("tray: activate failed: {err}");
        }
    });
}

fn open_menu(
    ui: &Rc<RefCell<TrayUi>>,
    key: &str,
    popover: &gtk::Popover,
    address: &str,
    menu_path: Option<&str>,
) {
    let Some(menu_path) = menu_path else {
        debug!("tray: `{key}` has no menu");
        return;
    };
    let Some(client) = ui.borrow().client.clone() else {
        return;
    };
    let Some(menu) = lookup_menu(ui, key) else {
        debug!("tray: `{key}` has no DBusMenu layout");
        return;
    };

    let menu_box = build_menu(&client, address, menu_path, &menu);
    popover.set_child(Some(&menu_box));
    popover.popup();

    ui.borrow_mut().open_menu = Some(OpenMenu {
        key: key.to_string(),
        address: address.to_string(),
        menu_path: menu_path.to_string(),
        popover: popover.clone(),
    });

    trigger_about_to_show(&client, address, menu_path, &menu);
}

/// Request `aboutToShow` for the root menu and every submenu so applications
/// that populate their layout lazily send the updated `DBusMenu`.
fn trigger_about_to_show(client: &Arc<Client>, address: &str, menu_path: &str, menu: &TrayMenu) {
    let mut ids = vec![0i32];
    collect_submenu_ids(menu, &mut ids);
    let client = client.clone();
    let address = address.to_string();
    let menu_path = menu_path.to_string();
    bridge::spawn(async move {
        for id in ids {
            let _ = client
                .about_to_show_menuitem(address.clone(), menu_path.clone(), id)
                .await;
        }
    });
}

fn collect_submenu_ids(menu: &TrayMenu, ids: &mut Vec<i32>) {
    for item in &menu.submenus {
        if !item.visible {
            continue;
        }
        if !item.submenu.is_empty() || item.children_display.as_deref() == Some("submenu") {
            ids.push(item.id);
            collect_submenu_ids(
                &TrayMenu {
                    id: item.id as u32,
                    submenus: item.submenu.clone(),
                },
                ids,
            );
        }
    }
}

fn build_menu(client: &Arc<Client>, address: &str, menu_path: &str, menu: &TrayMenu) -> gtk::Box {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    container.set_widget_name("menu");
    for item in &menu.submenus {
        if !item.visible {
            continue;
        }
        match item.menu_type {
            MenuType::Separator => {
                container.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            }
            MenuType::Standard => {
                if !item.submenu.is_empty() || item.children_display.as_deref() == Some("submenu") {
                    container.append(&build_submenu(client, address, menu_path, item));
                } else {
                    container.append(&build_entry(client, address, menu_path, item));
                }
            }
        }
    }
    container
}

fn build_entry(
    client: &Arc<Client>,
    address: &str,
    menu_path: &str,
    item: &MenuItem,
) -> gtk::Button {
    let label = menu_label(item);
    let button = gtk::Button::with_label(&label);
    button.set_has_frame(false);
    button.set_sensitive(item.enabled);

    let client = client.clone();
    let address = address.to_string();
    let menu_path = menu_path.to_string();
    let id = item.id;
    button.connect_clicked(move |button| {
        if let Some(popover) = button
            .ancestor(gtk::Popover::static_type())
            .and_downcast::<gtk::Popover>()
        {
            popover.popdown();
        }
        let client = client.clone();
        let address = address.clone();
        let menu_path = menu_path.clone();
        bridge::spawn(async move {
            let request = ActivateRequest::MenuItem {
                address,
                menu_path,
                submenu_id: id,
            };
            let _ = client.activate(request).await;
        });
    });
    button
}

fn build_submenu(
    client: &Arc<Client>,
    address: &str,
    menu_path: &str,
    item: &MenuItem,
) -> gtk::MenuButton {
    let button = gtk::MenuButton::new();
    button.set_label(&menu_label(item));
    button.set_has_frame(false);
    button.set_sensitive(item.enabled);
    let popover = gtk::Popover::new();
    let submenu = build_menu(
        client,
        address,
        menu_path,
        &TrayMenu {
            id: item.id as u32,
            submenus: item.submenu.clone(),
        },
    );
    popover.set_child(Some(&submenu));
    button.set_popover(Some(&popover));
    button
}

/// Clean a `DBusMenu` label: `__` is a literal underscore and a single `_`
/// marks the next character as an access key.
fn menu_label(item: &MenuItem) -> String {
    let raw = item.label.as_deref().unwrap_or_default();
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '_' if chars.peek() == Some(&'_') => {
                out.push('_');
                chars.next();
            }
            '_' => {}
            other => out.push(other),
        }
    }
    if matches!(item.toggle_type, ToggleType::Checkmark | ToggleType::Radio)
        && item.toggle_state == ToggleState::On
    {
        format!("✓ {out}")
    } else {
        out
    }
}

fn tooltip_markup(item: &StatusNotifierItem) -> Option<String> {
    if let Some(tooltip) = &item.tool_tip
        && (!tooltip.title.is_empty() || !tooltip.description.is_empty())
    {
        return Some(format!(
            "<b>{}</b>\n{}",
            gtk::glib::markup_escape_text(&tooltip.title),
            gtk::glib::markup_escape_text(&tooltip.description)
        ));
    }
    item.title
        .as_deref()
        .map(|title| gtk::glib::markup_escape_text(title).to_string())
}

pub fn register(reg: &mut Registry) {
    reg.register("tray", |init: &ModuleInit| {
        let config = &init.config;
        let horizontal = !init.bar.position.is_vertical();
        let orientation = if horizontal {
            gtk::Orientation::Horizontal
        } else {
            gtk::Orientation::Vertical
        };
        let spacing = config
            .get("spacing")
            .and_then(Value::as_u64)
            .map(|spacing| spacing as i32)
            .unwrap_or(0);

        let icons = gtk::Box::new(orientation, spacing);
        icons.set_widget_name("tray");
        icons.add_css_class("module");
        if !init.id.is_empty() {
            icons.add_css_class(&init.id);
        }

        let wrapper = gtk::Box::new(orientation, 0);
        wrapper.append(&icons);
        wrapper.set_visible(false);

        let ui = TrayUi {
            client: None,
            map: None,
            widgets: HashMap::new(),
            icon_size: config
                .get("icon-size")
                .and_then(Value::as_u64)
                .map(|size| size as i32)
                .unwrap_or(DEFAULT_ICON_SIZE),
            show_passive: config
                .get("show-passive-items")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            ignore_list: config
                .get("ignore-list")
                .and_then(Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|entry| entry.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            registered_icon_paths: HashSet::new(),
            open_menu: None,
        };

        Ok(Box::new(Tray {
            wrapper,
            icons,
            ui: Rc::new(RefCell::new(ui)),
        }))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_labels_strip_mnemonics() {
        let item = MenuItem {
            label: Some("_Open".to_string()),
            ..Default::default()
        };
        assert_eq!(menu_label(&item), "Open");
        let item = MenuItem {
            label: Some("a__b".to_string()),
            ..Default::default()
        };
        assert_eq!(menu_label(&item), "a_b");
        let item = MenuItem {
            label: Some("Checked".to_string()),
            toggle_type: ToggleType::Checkmark,
            toggle_state: ToggleState::On,
            ..Default::default()
        };
        assert_eq!(menu_label(&item), "✓ Checked");
        let item = MenuItem {
            label: Some("Plain".to_string()),
            toggle_state: ToggleState::On,
            ..Default::default()
        };
        assert_eq!(menu_label(&item), "Plain");
    }

    #[test]
    fn collects_nested_submenu_ids() {
        let leaf = MenuItem {
            id: 3,
            visible: true,
            ..Default::default()
        };
        let nested = MenuItem {
            id: 2,
            visible: true,
            submenu: vec![leaf],
            ..Default::default()
        };
        let top = MenuItem {
            id: 1,
            visible: true,
            submenu: vec![nested],
            ..Default::default()
        };
        let plain = MenuItem {
            id: 4,
            visible: true,
            ..Default::default()
        };
        let menu = TrayMenu {
            id: 0,
            submenus: vec![top, plain],
        };
        let mut ids = vec![0];
        collect_submenu_ids(&menu, &mut ids);
        assert_eq!(ids, vec![0, 1, 2]);
    }

    #[test]
    fn order_key_prefers_app_id_then_bus_name() {
        assert_eq!(order_key("firefox", ":1.42"), "firefox");
        assert_eq!(order_key("", ":1.42"), ":1.42");
    }
}
