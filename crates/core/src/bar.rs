//! The layer-shell bar window and module population.

use anyhow::Result;
use gtk::prelude::*;
use gtk_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use tracing::{debug, warn};

use crate::config::{BarConfig, Position};
use crate::module::{Module, ModuleInit, Registry, split_name};

/// A GDK monitor together with the names waybar matches its `output` filter
/// against: the connector (`DP-1`, `eDP-1`) and the output description.
pub struct MonitorInfo {
    pub monitor: gtk::gdk::Monitor,
    /// Waybar's `name` (the Wayland connector).
    pub name: String,
    /// Waybar's `identifier` (the output description).
    pub identifier: String,
}

/// Enumerate every monitor, in GDK order.
pub fn monitors() -> Vec<MonitorInfo> {
    let Some(display) = gtk::gdk::Display::default() else {
        return Vec::new();
    };
    let list = display.monitors();
    let mut result = Vec::new();
    for i in 0..list.n_items() {
        let Some(monitor) = list
            .item(i)
            .and_then(|o| o.downcast::<gtk::gdk::Monitor>().ok())
        else {
            continue;
        };
        result.push(MonitorInfo {
            name: monitor
                .connector()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            identifier: monitor.model().map(|s| s.to_string()).unwrap_or_default(),
            monitor,
        });
    }
    result
}

/// A live bar: the window plus the modules it owns (kept alive here).
pub struct Bar {
    window: gtk::ApplicationWindow,
    monitor: gtk::gdk::Monitor,
    /// The connector of the output this bar lives on.
    output_name: String,
    #[allow(dead_code)]
    modules: Vec<Box<dyn Module>>,
}

impl Bar {
    /// Build a bar for `cfg` on `monitor`.
    ///
    /// The caller decides which monitors a config matches (see
    /// [`crate::config::Config::configs_for_output`]); a bar without an
    /// `output` filter is created once per monitor.
    pub fn new(
        app: &gtk::Application,
        cfg: &BarConfig,
        registry: &Registry,
        monitor: &gtk::gdk::Monitor,
    ) -> Result<Self> {
        let window = gtk::ApplicationWindow::builder().application(app).build();
        window.set_widget_name("waybar");
        window.set_decorated(false);
        window.add_css_class(cfg.position.as_str());
        if cfg.name != "waybar" {
            window.add_css_class(&cfg.name);
        }

        // Must happen before the window is realized.
        window.init_layer_shell();
        window.set_namespace(Some(&cfg.name));
        window.set_layer(layer_from_str(&cfg.layer));
        window.set_keyboard_mode(KeyboardMode::None);
        if cfg.exclusive {
            window.auto_exclusive_zone_enable();
        } else {
            window.set_exclusive_zone(0);
        }

        window.set_monitor(Some(monitor));

        // The connector of the output the bar lives on. Modules that need to
        // know their output (`sway/workspaces`, `WAYBAR_OUTPUT_NAME`) read it
        // from `ModuleInit::output_name`.
        let output_name = monitor
            .connector()
            .map(|s| s.to_string())
            .unwrap_or_default();

        window.set_margin(Edge::Top, cfg.margin_top);
        window.set_margin(Edge::Right, cfg.margin_right);
        window.set_margin(Edge::Bottom, cfg.margin_bottom);
        window.set_margin(Edge::Left, cfg.margin_left);

        let width = if cfg.width > 0 { cfg.width } else { -1 };
        let height = if cfg.height > 0 { cfg.height } else { -1 };
        window.set_size_request(width, height);

        set_anchors(&window, cfg);

        let orientation = if cfg.position.is_vertical() {
            gtk::Orientation::Vertical
        } else {
            gtk::Orientation::Horizontal
        };

        let center = gtk::CenterBox::new();
        center.set_orientation(orientation);

        let left = gtk::Box::new(orientation, cfg.spacing);
        left.add_css_class("modules-left");
        let middle = gtk::Box::new(orientation, cfg.spacing);
        middle.add_css_class("modules-center");
        let right = gtk::Box::new(orientation, cfg.spacing);
        right.add_css_class("modules-right");

        // `center-anchor` keeps one center module on the bar center rather than
        // centering the whole `modules-center` group. The anchor becomes the
        // center widget of the center box; modules before/after it live in the
        // start/end halves, packed against the anchor by expanding spacers.
        // GTK then keeps the anchor centered on its own, even when the modules
        // around it change width.
        let center_names = cfg.module_list("modules-center");
        let anchor = anchor_index(&center_names, cfg.center_anchor.as_deref(), |name| {
            registry.contains(&split_name(name).0)
        })
        .map(|index| CenterAnchor::attach(&center, orientation, cfg.spacing, &left, &right, index));
        if anchor.is_none() {
            center.set_start_widget(Some(&left));
            center.set_center_widget(Some(&middle));
            center.set_end_widget(Some(&right));
        }
        window.set_child(Some(&center));

        let mut modules = Vec::new();
        for (section, container) in [
            ("modules-left", &left),
            ("modules-center", &middle),
            ("modules-right", &right),
        ] {
            for (index, name) in cfg.module_list(section).into_iter().enumerate() {
                let (kind, id) = split_name(&name);
                if !registry.contains(&kind) {
                    warn!("unknown module `{}` in {}", kind, section);
                    continue;
                }
                let init = ModuleInit {
                    name: name.clone(),
                    kind,
                    id,
                    config: cfg.module_config(&name),
                    bar: cfg.clone(),
                    window: window.clone(),
                    output_name: output_name.clone(),
                    section: section.to_string(),
                };
                match registry.create(&init) {
                    Ok(module) => {
                        let widget = module.widget();
                        let target = match &anchor {
                            Some(anchor) if section == "modules-center" => anchor.slot(index),
                            _ => container,
                        };
                        target.append(&widget);
                        module.start();
                        modules.push(module);
                        debug!("created module `{}`", name);
                    }
                    Err(err) => warn!("failed to create module `{}`: {err:#}", name),
                }
            }
        }
        if let Some(anchor) = &anchor {
            anchor.apply_spacing(cfg.spacing);
        }

        Ok(Bar {
            window,
            monitor: monitor.clone(),
            output_name,
            modules,
        })
    }

    pub fn window(&self) -> &gtk::ApplicationWindow {
        &self.window
    }

    /// The monitor this bar is placed on.
    pub fn monitor(&self) -> &gtk::gdk::Monitor {
        &self.monitor
    }

    /// The connector of this bar's output (`DP-1`, `eDP-1`).
    pub fn output_name(&self) -> &str {
        &self.output_name
    }

    /// Consume the bar, returning its window so the caller can close it when
    /// the output disappears.
    pub fn into_window(self) -> gtk::ApplicationWindow {
        self.window
    }

    pub fn present(&self) {
        self.window.present();
    }
}

/// Index in `modules-center` of the `center-anchor` module, or `None` when the
/// anchor is absent, is the only center module, or is not a usable module.
/// An unknown module is skipped silently: the module loop already warns about it.
fn anchor_index(
    names: &[String],
    anchor: Option<&str>,
    is_known: impl Fn(&str) -> bool,
) -> Option<usize> {
    let name = anchor?;
    let Some(index) = names.iter().position(|n| n == name) else {
        warn!("center-anchor `{name}` is not listed in modules-center; ignoring");
        return None;
    };
    if names.len() < 2 || !is_known(name) {
        return None;
    }
    Some(index)
}

/// Center-box layout with a `center-anchor` module in the middle.
///
/// The anchor is the center widget of the `GtkCenterBox`; modules before it sit
/// in the start half and modules after it in the end half, packed against the
/// anchor by expanding spacers. Left and right sections stay at the bar edges.
struct CenterAnchor {
    index: usize,
    before: gtk::Box,
    anchor: gtk::Box,
    after: gtk::Box,
}

impl CenterAnchor {
    fn attach(
        center: &gtk::CenterBox,
        orientation: gtk::Orientation,
        spacing: i32,
        left: &gtk::Box,
        right: &gtk::Box,
        index: usize,
    ) -> Self {
        let center_box = || {
            let b = gtk::Box::new(orientation, spacing);
            b.add_css_class("modules-center");
            b
        };
        let before = center_box();
        let anchor = center_box();
        let after = center_box();

        let start = gtk::Box::new(orientation, 0);
        start.append(left);
        start.append(&expanding_spacer(orientation));
        start.append(&before);
        set_expand(&start, orientation);

        let end = gtk::Box::new(orientation, 0);
        end.append(&after);
        end.append(&expanding_spacer(orientation));
        end.append(right);
        set_expand(&end, orientation);

        center.set_start_widget(Some(&start));
        center.set_center_widget(Some(&anchor));
        center.set_end_widget(Some(&end));

        Self {
            index,
            before,
            anchor,
            after,
        }
    }

    fn slot(&self, index: usize) -> &gtk::Box {
        match index.cmp(&self.index) {
            std::cmp::Ordering::Less => &self.before,
            std::cmp::Ordering::Equal => &self.anchor,
            std::cmp::Ordering::Greater => &self.after,
        }
    }

    /// Keeps the configured spacing between the satellite boxes and the anchor.
    /// Applied only to a non-empty box: a margin on an empty box still takes space.
    fn apply_spacing(&self, spacing: i32) {
        if self.before.first_child().is_some() {
            self.before.set_margin_end(spacing);
        }
        if self.after.first_child().is_some() {
            self.after.set_margin_start(spacing);
        }
    }
}

/// Makes `widget` grow along the bar's axis.
fn set_expand(widget: &impl IsA<gtk::Widget>, orientation: gtk::Orientation) {
    if orientation == gtk::Orientation::Horizontal {
        widget.set_hexpand(true);
    } else {
        widget.set_vexpand(true);
    }
}

/// An invisible spacer that grows along the bar's axis. Inside the start/end
/// halves of the center box it pushes the center-anchor satellites against the
/// anchor while letting the left/right sections stay at the bar edges.
fn expanding_spacer(orientation: gtk::Orientation) -> gtk::Box {
    let spacer = gtk::Box::new(orientation, 0);
    set_expand(&spacer, orientation);
    spacer
}

fn layer_from_str(value: &str) -> Layer {
    match value {
        "background" => Layer::Background,
        "top" => Layer::Top,
        "overlay" => Layer::Overlay,
        // waybar defaults to the bottom layer for unrecognised values.
        _ => Layer::Bottom,
    }
}

fn set_anchors(window: &gtk::ApplicationWindow, cfg: &BarConfig) {
    let (mut left, mut right, mut top, mut bottom) = match cfg.position {
        Position::Top => (true, true, true, false),
        Position::Bottom => (true, true, false, true),
        Position::Left => (true, false, true, true),
        Position::Right => (false, true, true, true),
    };

    // A fixed length along the bar's axis means it should not be stretched.
    if cfg.position.is_vertical() && cfg.height > 1 {
        top = false;
        bottom = false;
    } else if !cfg.position.is_vertical() && cfg.width > 1 {
        left = false;
        right = false;
    }

    window.set_anchor(Edge::Left, left);
    window.set_anchor(Edge::Right, right);
    window.set_anchor(Edge::Top, top);
    window.set_anchor(Edge::Bottom, bottom);
}

#[cfg(test)]
mod tests {
    use super::anchor_index;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn anchor_index_picks_position_in_modules_center() {
        let list = names(&["clock", "sway/language", "keyboard-state"]);
        assert_eq!(anchor_index(&list, Some("clock"), |_| true), Some(0));
        assert_eq!(
            anchor_index(&list, Some("keyboard-state"), |_| true),
            Some(2)
        );
    }

    #[test]
    fn anchor_index_is_none_without_anchor() {
        let list = names(&["clock", "cpu"]);
        assert_eq!(anchor_index(&list, None, |_| true), None);
    }

    #[test]
    fn anchor_index_ignores_single_module_and_unknown_modules() {
        let single = names(&["clock"]);
        assert_eq!(anchor_index(&single, Some("clock"), |_| true), None);

        let list = names(&["clock", "ghost"]);
        assert_eq!(anchor_index(&list, Some("ghost"), |n| n != "ghost"), None);
    }

    #[test]
    fn anchor_index_is_none_when_not_listed() {
        let list = names(&["clock", "cpu"]);
        assert_eq!(anchor_index(&list, Some("memory"), |_| true), None);
    }
}
