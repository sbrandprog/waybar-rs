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
    }

    pub fn create(&self, init: &ModuleInit) -> Result<Box<dyn Module>> {
        let factory = self
            .map
            .get(init.kind.as_str())
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
