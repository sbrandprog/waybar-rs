//! A GTK label module, the Rust analogue of waybar's `ALabel`.
//!
//! The widget tree mirrors waybar: an unnamed wrapper box around a named
//! [`gtk::Label`], so selectors such as `#clock`, `.module` and
//! `.modules-left > * > *` keep working.

use gtk::prelude::*;
use serde_json::Value;

use waybar_core::widgets::{attach_actions, tooltip_enabled, Tooltip};

#[derive(Clone)]
pub struct LabelModule {
    root: gtk::Box,
    label: gtk::Label,
    tooltip: Option<Tooltip>,
    config: Value,
}

impl LabelModule {
    pub fn new(name: &str, id: &str, config: &Value) -> Self {
        let label = gtk::Label::new(None);
        label.set_widget_name(name);
        label.add_css_class("module");
        if !id.is_empty() {
            label.add_css_class(id);
        }

        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.append(&label);

        attach_actions(&root, config);
        let tooltip = tooltip_enabled(config).then(|| Tooltip::attach(&label));

        LabelModule {
            root,
            label,
            tooltip,
            config: config.clone(),
        }
    }

    /// The wrapper widget to insert into the bar.
    pub fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    /// Replace the label contents (Pango markup).
    pub fn set_markup(&self, markup: &str) {
        self.label.set_markup(markup);
    }

    /// Update the tooltip markup (Pango markup).
    pub fn set_tooltip(&self, markup: &str) {
        if let Some(tooltip) = &self.tooltip {
            tooltip.set(markup);
        }
    }

    /// The module's option object.
    pub fn config(&self) -> &Value {
        &self.config
    }

    /// Read a string option, falling back to `default`.
    pub fn option_str(&self, key: &str, default: &str) -> String {
        self.config
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or(default)
            .to_string()
    }
}
