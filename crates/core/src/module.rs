//! Module contract and registry.
//!
//! A module is a small factory-driven object that owns its GTK widget(s) and
//! optionally spawns background work. The bar keeps the returned trait objects
//! alive for the lifetime of the window.

use std::collections::HashMap;

use anyhow::Result;
use serde_json::Value;

use crate::config::BarConfig;

/// Everything a module needs at construction time.
pub struct ModuleInit {
    /// Full name from the config list, e.g. `battery#bat2` or `custom/foo`.
    pub name: String,
    /// Base kind used for factory dispatch, e.g. `battery` or `custom/foo`.
    pub kind: String,
    /// Optional `#id` suffix, without the `#`.
    pub id: String,
    /// The module's option object from the config (may be `Null`).
    pub config: Value,
    /// Bar-level options.
    pub bar: BarConfig,
    /// The bar's layer-shell window. Modules that need window-level state
    /// (e.g. battery's `battery-<state>` class) can use this.
    pub window: gtk::ApplicationWindow,
    /// The connector of the output the bar was placed on (`DP-1`, `eDP-1`),
    /// or empty when no monitor could be resolved. Used by `sway/workspaces`
    /// for the `current_output` class and for `WAYBAR_OUTPUT_NAME`.
    pub output_name: String,
    /// Section the module was listed in: `modules-left|center|right`.
    pub section: String,
}

/// A bar module.
pub trait Module: 'static {
    /// The widget to insert into the bar.
    fn widget(&self) -> gtk::Widget;

    /// Called once after the widget has been inserted. Spawn timers/tasks here.
    fn start(&self) {}
}

pub type ModuleFactory = fn(&ModuleInit) -> Result<Box<dyn Module>>;

/// Maps a module kind to its factory.
#[derive(Default)]
pub struct Registry {
    map: HashMap<&'static str, ModuleFactory>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, kind: &'static str, factory: ModuleFactory) {
        self.map.insert(kind, factory);
    }

    pub fn contains(&self, kind: &str) -> bool {
        self.map.contains_key(kind)
            || kind
                .split_once('/')
                .is_some_and(|(base, _)| self.map.contains_key(base))
    }

    pub fn create(&self, init: &ModuleInit) -> Result<Box<dyn Module>> {
        // Exact kind first (`sway/mode`), then the namespace prefix, which lets
        // `custom/foo` fall back to the `custom` factory.
        let factory = self
            .map
            .get(init.kind.as_str())
            .or_else(|| {
                init.kind
                    .split_once('/')
                    .and_then(|(base, _)| self.map.get(base))
            })
            .ok_or_else(|| anyhow::anyhow!("unknown module `{}`", init.kind))?;
        factory(init)
    }
}

/// Split `battery#bat2` into (`battery`, `bat2`).
pub fn split_name(name: &str) -> (String, String) {
    match name.find('#') {
        Some(pos) => (name[..pos].to_string(), name[pos + 1..].to_string()),
        None => (name.to_string(), String::new()),
    }
}
