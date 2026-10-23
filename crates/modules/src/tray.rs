//! `tray` — a StatusNotifierItem host with its own GTK4 rendering.
//!
//! The [`system_tray`] client provides the Watcher/Host, watches SNI items and
//! their `DBusMenu` layouts, and exposes an event stream plus `activate` /
//! `about_to_show_menuitem` calls. This module renders one widget per item
//! (`#tray > .<status>`) and a [`gtk::PopoverMenu`] built from the `DBusMenu`
//! tree.
//!
//! Not ported: the SNI `Scroll` and `ContextMenu` methods (the crate does not
//! expose them), overlay icons, and per-app icon overrides (`icons`).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use gtk::gio;
use gtk::gio::prelude::*;
use gtk::glib::variant::ToVariant;
use gtk::prelude::*;
use serde_json::Value;
use system_tray::client::{ActivateRequest, Client, Event as TrayEvent};
use system_tray::data::BaseMap;
use system_tray::item::{IconPixmap, Status, StatusNotifierItem, Tooltip};
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
    popover: gtk::PopoverMenu,
    address: String,
    order_key: String,
    item_is_menu: bool,
    menu_path: Option<String>,
    status: Status,
    visible: bool,
    tooltip: Option<String>,
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
    popover: gtk::PopoverMenu,
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
                reorder_icons(icons, ui);
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
    let map = map.lock().unwrap_or_else(|err| err.into_inner());
    map.get(key).map(|(item, _)| item.clone())
}

fn lookup_menu(ui: &Rc<RefCell<TrayUi>>, key: &str) -> Option<TrayMenu> {
    let map = ui.borrow().map.clone()?;
    let map = map.lock().unwrap_or_else(|err| err.into_inner());
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
    apply_menu_model(&popover, &client, &address, &menu_path, &menu);
}

fn apply_menu_model(
    popover: &gtk::PopoverMenu,
    client: &Arc<Client>,
    address: &str,
    menu_path: &str,
    menu: &TrayMenu,
) {
    let actions = gio::SimpleActionGroup::new();
    let model = build_menu_model(client, address, menu_path, menu, &actions);
    popover.insert_action_group("tray", Some(&actions));
    popover.set_menu_model(Some(&model));
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

    let popover = gtk::PopoverMenu::builder().build();
    popover.set_parent(&container);
    popover.add_css_class("tray-menu");

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
        tooltip: None,
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
    widget.order_key = order_key(&item.id, key);

    apply_icon(&widget.image, item, icon_size);

    for class in ["active", "passive", "needs-attention"] {
        widget.container.remove_css_class(class);
    }
    if let Some(class) = status_class(item.status) {
        widget.container.add_css_class(class);
    }

    apply_tooltip(widget, item);

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
    if let Some(name) = &item.icon_name
        && !name.is_empty()
    {
        // Some tray apps put a file path (or a name with an extension) into
        // `IconName` instead of a theme name. The original Waybar resolves the
        // string as a file before the theme lookup; keep that order.
        if is_icon_file(name)
            && let Some(texture) = file_texture(name, size)
        {
            trace!("tray: `{}` icon from file `{name}`", item.id);
            image.set_paintable(Some(&texture));
            image.set_pixel_size(size);
            return;
        }
        if icon_theme().is_some_and(|theme| theme.has_icon(name)) {
            trace!("tray: `{}` icon from theme `{name}`", item.id);
            image.set_icon_name(Some(name));
            image.set_pixel_size(size);
            return;
        }
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

/// Whether `name` points at an actual file (a relative path is resolved
/// against the bar's working directory, like the original `ifstream` check).
fn is_icon_file(name: &str) -> bool {
    std::path::Path::new(name).is_file()
}

/// Load an icon file through gdk-pixbuf (which also handles SVG) and scale it
/// to `size` logical pixels, preserving the aspect ratio. `GtkImage` ignores
/// `pixel-size` for paintables, so the scaling has to happen here.
fn file_texture(name: &str, size: i32) -> Option<gtk::gdk::Texture> {
    let size = size.max(1);
    let pixbuf = match gtk::gdk_pixbuf::Pixbuf::from_file(name) {
        Ok(pixbuf) => pixbuf,
        Err(err) => {
            warn!("tray: cannot read icon file `{name}`: {err}");
            return None;
        }
    };
    let scale = if pixbuf.height() > 0 && pixbuf.height() != size {
        size as f64 / pixbuf.height() as f64
    } else {
        1.0
    };
    let width = ((pixbuf.width() as f64 * scale).round() as i32).max(1);
    let pixbuf = if scale == 1.0 {
        pixbuf
    } else {
        match pixbuf.scale_simple(width, size, gtk::gdk_pixbuf::InterpType::Bilinear) {
            Some(pixbuf) => pixbuf,
            None => {
                warn!("tray: cannot scale icon file `{name}`");
                return None;
            }
        }
    };
    Some(gtk::gdk::Texture::for_pixbuf(&pixbuf))
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
    popover: &gtk::PopoverMenu,
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

    apply_menu_model(popover, &client, address, menu_path, &menu);
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

#[derive(Clone, Copy)]
enum ItemToggle {
    Plain,
    Check,
    Radio,
}

fn build_menu_model(
    client: &Arc<Client>,
    address: &str,
    menu_path: &str,
    menu: &TrayMenu,
    actions: &gio::SimpleActionGroup,
) -> gio::Menu {
    let model = gio::Menu::new();
    for items in split_sections(menu) {
        let section = gio::Menu::new();
        for item in items {
            if !item.submenu.is_empty() || item.children_display.as_deref() == Some("submenu") {
                let submenu = build_menu_model(
                    client,
                    address,
                    menu_path,
                    &TrayMenu {
                        id: item.id as u32,
                        submenus: item.submenu.clone(),
                    },
                    actions,
                );
                section.append_submenu(Some(&menu_label(item)), &submenu);
            } else {
                section.append_item(&build_entry(client, address, menu_path, item, actions));
            }
        }
        model.append_section(None, &section);
    }
    model
}

fn split_sections(menu: &TrayMenu) -> Vec<Vec<&MenuItem>> {
    let mut sections: Vec<Vec<&MenuItem>> = Vec::new();
    let mut current: Vec<&MenuItem> = Vec::new();
    for item in &menu.submenus {
        if !item.visible {
            continue;
        }
        match item.menu_type {
            MenuType::Separator => {
                if !current.is_empty() {
                    sections.push(std::mem::take(&mut current));
                }
            }
            MenuType::Standard => current.push(item),
        }
    }
    if !current.is_empty() {
        sections.push(current);
    }
    sections
}

fn build_entry(
    client: &Arc<Client>,
    address: &str,
    menu_path: &str,
    item: &MenuItem,
    actions: &gio::SimpleActionGroup,
) -> gio::MenuItem {
    let name = format!("item-{}", item.id);
    let toggle = match item.toggle_type {
        ToggleType::CannotBeToggled => ItemToggle::Plain,
        ToggleType::Checkmark => ItemToggle::Check,
        ToggleType::Radio => ItemToggle::Radio,
    };
    let on = item.toggle_state == ToggleState::On;
    let action = match toggle {
        ItemToggle::Plain => gio::SimpleAction::new(&name, None),
        ItemToggle::Check => gio::SimpleAction::new_stateful(&name, None, &on.to_variant()),
        ItemToggle::Radio => {
            let state = if on {
                item.id.to_string()
            } else {
                String::new()
            };
            gio::SimpleAction::new_stateful(
                &name,
                Some(gtk::glib::VariantTy::STRING),
                &state.to_variant(),
            )
        }
    };
    action.set_enabled(item.enabled);
    connect_item(&action, toggle, item.id, client, address, menu_path);
    actions.add_action(&action);

    let entry = gio::MenuItem::new(Some(&menu_label(item)), None);
    let target = match toggle {
        ItemToggle::Radio => Some(item.id.to_string().to_variant()),
        _ => None,
    };
    entry.set_action_and_target_value(Some(&format!("tray.{name}")), target.as_ref());
    entry
}

fn connect_item(
    action: &gio::SimpleAction,
    toggle: ItemToggle,
    id: i32,
    client: &Arc<Client>,
    address: &str,
    menu_path: &str,
) {
    let client = client.clone();
    let address = address.to_string();
    let menu_path = menu_path.to_string();
    action.connect_activate(move |action, parameter| {
        match toggle {
            ItemToggle::Plain => {}
            ItemToggle::Check => {
                let on = action
                    .state()
                    .and_then(|state| state.get::<bool>())
                    .unwrap_or(false);
                action.set_state(&(!on).to_variant());
            }
            ItemToggle::Radio => {
                if let Some(parameter) = parameter {
                    action.set_state(parameter);
                }
            }
        }
        activate_item(&client, &address, &menu_path, id);
    });
}

fn activate_item(client: &Arc<Client>, address: &str, menu_path: &str, id: i32) {
    let client = client.clone();
    let address = address.to_string();
    let menu_path = menu_path.to_string();
    bridge::spawn(async move {
        let request = ActivateRequest::MenuItem {
            address,
            menu_path,
            submenu_id: id,
        };
        let _ = client.activate(request).await;
    });
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
    out
}

fn apply_tooltip(widget: &mut ItemWidget, item: &StatusNotifierItem) {
    if let Some(markup) = tooltip_markup(item.tool_tip.as_ref()) {
        widget.tooltip = Some(markup.clone());
        widget.container.set_tooltip_markup(Some(&markup));
        return;
    }
    if let Some(markup) = &widget.tooltip {
        widget.container.set_tooltip_markup(Some(markup));
        return;
    }
    let fallback = item
        .title
        .as_deref()
        .map(|title| gtk::glib::markup_escape_text(title).to_string());
    widget.container.set_tooltip_markup(fallback.as_deref());
}

fn tooltip_markup(tooltip: Option<&Tooltip>) -> Option<String> {
    let tooltip = tooltip?;
    if tooltip.title.is_empty() && tooltip.description.is_empty() {
        return None;
    }
    Some(render_tooltip(&tooltip.title, &tooltip.description))
}

fn render_tooltip(title: &str, description: &str) -> String {
    let title = gtk::glib::markup_escape_text(title);
    if description.is_empty() {
        title.to_string()
    } else {
        format!(
            "<b>{title}</b>\n{}",
            gtk::glib::markup_escape_text(description)
        )
    }
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
        assert_eq!(menu_label(&item), "Checked");
        let item = MenuItem {
            label: Some("Plain".to_string()),
            toggle_state: ToggleState::On,
            ..Default::default()
        };
        assert_eq!(menu_label(&item), "Plain");
    }

    #[test]
    fn tooltip_bolds_the_title_only_with_a_description() {
        assert_eq!(render_tooltip("Telegram", ""), "Telegram");
        assert_eq!(
            render_tooltip("Telegram", "New messages"),
            "<b>Telegram</b>\nNew messages"
        );
        assert_eq!(
            render_tooltip("a & b", "c < d"),
            "<b>a &amp; b</b>\nc &lt; d"
        );
    }

    #[test]
    fn empty_tool_tip_yields_nothing_so_the_last_one_is_kept() {
        let tooltip = |title: &str, description: &str| Tooltip {
            icon_name: String::new(),
            icon_data: Vec::new(),
            title: title.to_string(),
            description: description.to_string(),
        };
        assert_eq!(tooltip_markup(None), None);
        assert_eq!(tooltip_markup(Some(&tooltip("", ""))), None);
        assert_eq!(
            tooltip_markup(Some(&tooltip("AmneziaVPN", "VPN Connected"))),
            Some("<b>AmneziaVPN</b>\nVPN Connected".to_string())
        );
        assert_eq!(
            tooltip_markup(Some(&tooltip("AmneziaVPN", ""))),
            Some("AmneziaVPN".to_string())
        );
    }

    #[test]
    fn icon_file_detection_requires_an_existing_file() {
        let here = concat!(env!("CARGO_MANIFEST_DIR"), "/src/tray.rs");
        assert!(is_icon_file(here));
        assert!(!is_icon_file("/nonexistent/waybar-rs/icon.svg"));
        assert!(!is_icon_file("firefox"));
        assert!(!is_icon_file(""));
    }

    #[test]
    fn splits_sections_at_visible_separators() {
        let entry = |id| MenuItem {
            id,
            visible: true,
            ..Default::default()
        };
        let separator = MenuItem {
            id: 9,
            menu_type: MenuType::Separator,
            visible: true,
            ..Default::default()
        };
        let hidden_separator = MenuItem {
            id: 8,
            menu_type: MenuType::Separator,
            visible: false,
            ..Default::default()
        };
        let hidden = MenuItem {
            id: 7,
            visible: false,
            ..Default::default()
        };
        let menu = TrayMenu {
            id: 0,
            submenus: vec![
                entry(1),
                separator,
                entry(2),
                hidden,
                hidden_separator,
                entry(3),
            ],
        };
        let ids: Vec<Vec<i32>> = split_sections(&menu)
            .into_iter()
            .map(|section| section.into_iter().map(|item| item.id).collect())
            .collect();
        assert_eq!(ids, vec![vec![1], vec![2, 3]]);

        let menu = TrayMenu {
            id: 0,
            submenus: vec![
                MenuItem {
                    id: 9,
                    menu_type: MenuType::Separator,
                    visible: true,
                    ..Default::default()
                },
                entry(1),
                MenuItem {
                    id: 8,
                    menu_type: MenuType::Separator,
                    visible: true,
                    ..Default::default()
                },
            ],
        };
        assert_eq!(split_sections(&menu).len(), 1);
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
