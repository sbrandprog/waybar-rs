//! The layer-shell bar window and module population.

use anyhow::Result;
use gtk::prelude::*;
use gtk_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use tracing::{debug, warn};

use crate::config::{BarConfig, OutputFilter, Position};
use crate::module::{split_name, Module, ModuleInit, Registry};

/// A live bar: the window plus the modules it owns (kept alive here).
pub struct Bar {
    window: gtk::ApplicationWindow,
    #[allow(dead_code)]
    modules: Vec<Box<dyn Module>>,
}

impl Bar {
    pub fn new(app: &gtk::Application, cfg: &BarConfig, registry: &Registry) -> Result<Self> {
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

        if cfg.output != OutputFilter::All {
            match find_monitor(&cfg.output) {
                Some(monitor) => window.set_monitor(Some(&monitor)),
                None => warn!("no monitor matches the configured `output`; using the default"),
            }
        }

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

        center.set_start_widget(Some(&left));
        center.set_center_widget(Some(&middle));
        center.set_end_widget(Some(&right));
        window.set_child(Some(&center));

        let mut modules = Vec::new();
        for (section, container) in [
            ("modules-left", &left),
            ("modules-center", &middle),
            ("modules-right", &right),
        ] {
            for name in cfg.module_list(section) {
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
                    section: section.to_string(),
                };
                match registry.create(&init) {
                    Ok(module) => {
                        let widget = module.widget();
                        container.append(&widget);
                        module.start();
                        modules.push(module);
                        debug!("created module `{}`", name);
                    }
                    Err(err) => warn!("failed to create module `{}`: {err:#}", name),
                }
            }
        }

        Ok(Bar { window, modules })
    }

    pub fn window(&self) -> &gtk::ApplicationWindow {
        &self.window
    }

    pub fn present(&self) {
        self.window.present();
    }
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

/// Find the first GDK monitor selected by an [`OutputFilter`].
///
/// Waybar names outputs by their Wayland connector (e.g. `DP-1`, `eDP-1`).
/// GDK exposes that as `Monitor::connector`, with `Monitor::model` holding the
/// output description used by waybar's `identifier` matching.
fn find_monitor(filter: &OutputFilter) -> Option<gtk::gdk::Monitor> {
    let display = gtk::gdk::Display::default()?;
    let monitors = display.monitors();
    for i in 0..monitors.n_items() {
        let Some(monitor) = monitors.item(i).and_then(|o| o.downcast::<gtk::gdk::Monitor>().ok())
        else {
            continue;
        };
        let connector = monitor.connector();
        let model = monitor.model();
        let name = connector.as_deref().unwrap_or("");
        let identifier = model.as_deref().unwrap_or("");
        if filter.matches(name, identifier) {
            return Some(monitor);
        }
    }
    None
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
