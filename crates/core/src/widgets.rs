//! Shared widget helpers: click/scroll actions and tooltip plumbing.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use serde_json::Value;

use crate::action::spawn_command;

/// Whether tooltips are enabled for a module (default: yes).
pub fn tooltip_enabled(config: &Value) -> bool {
    config
        .get("tooltip")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

fn button_suffix(button: u32) -> &'static str {
    match button {
        2 => "-middle",
        3 => "-right",
        8 => "-backward",
        9 => "-forward",
        _ => "",
    }
}

fn click_key(button: u32, n_press: i32) -> Option<String> {
    let action = match n_press {
        1 => "click",
        2 => "double-click",
        3 => "triple-click",
        _ => return None,
    };
    if button == 10 {
        // on-click-copy is special and handled by ALabel-style modules.
        return None;
    }
    Some(format!("on-{}{}", action, button_suffix(button)))
}

/// Attach `on-click*`/`on-scroll*` command handlers from the module config.
///
/// Controllers are only installed when a matching command is configured, so
/// modules without actions don't consume click/scroll events.
pub fn attach_actions<W: IsA<gtk::Widget>>(widget: &W, config: &Value) {
    if has_click_command(config) {
        attach_clicks(widget, config);
    }
    if has_scroll_command(config) {
        attach_scroll(widget, config);
    }
}

/// Every `(button, n_press)` pair [`attach_clicks`] dispatches.
const CLICK_EVENTS: &[(u32, i32)] = &[
    (1, 1),
    (1, 2),
    (1, 3),
    (2, 1),
    (2, 2),
    (2, 3),
    (3, 1),
    (3, 2),
    (3, 3),
    (8, 1),
    (8, 2),
    (8, 3),
    (9, 1),
    (9, 2),
    (9, 3),
];

/// Whether any command [`attach_clicks`] can dispatch is configured.
fn has_click_command(config: &Value) -> bool {
    CLICK_EVENTS.iter().any(|&(button, n_press)| {
        click_key(button, n_press).is_some_and(|key| has_string(config, &key))
    })
}

/// The scroll keys [`attach_scroll`] can dispatch.
const SCROLL_KEYS: &[&str] = &[
    "on-scroll-up",
    "on-scroll-down",
    "on-scroll-left",
    "on-scroll-right",
];

/// Whether any command [`attach_scroll`] can dispatch is configured.
fn has_scroll_command(config: &Value) -> bool {
    SCROLL_KEYS.iter().any(|key| has_string(config, key))
}

fn has_string(config: &Value, key: &str) -> bool {
    config.get(key).is_some_and(|v| v.is_string())
}

fn attach_clicks<W: IsA<gtk::Widget>>(widget: &W, config: &Value) {
    let gesture = gtk::GestureClick::new();
    gesture.set_button(0);
    let config = config.clone();
    gesture.connect_pressed(move |gesture, n_press, _x, _y| {
        let Some(key) = click_key(gesture.current_button(), n_press) else {
            return;
        };
        if let Some(cmd) = config.get(&key).and_then(|v| v.as_str()) {
            spawn_command(cmd);
        }
    });
    widget.add_controller(gesture);
}

fn attach_scroll<W: IsA<gtk::Widget>>(widget: &W, config: &Value) {
    let controller = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    let config = config.clone();
    controller.connect_scroll(move |_, dx, dy| {
        let key = if dy < 0.0 {
            "on-scroll-up"
        } else if dy > 0.0 {
            "on-scroll-down"
        } else if dx < 0.0 {
            "on-scroll-left"
        } else {
            "on-scroll-right"
        };
        if let Some(cmd) = config.get(key).and_then(|v| v.as_str()) {
            spawn_command(cmd);
        }
        gtk::glib::Propagation::Proceed
    });
    widget.add_controller(controller);
}

/// A tooltip whose markup can be updated from module code.
#[derive(Clone)]
pub struct Tooltip {
    inner: Rc<RefCell<Option<String>>>,
}

impl Tooltip {
    /// Enable tooltips on `label` and return a handle to update the markup.
    pub fn attach(label: &gtk::Label) -> Self {
        let inner: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        label.set_has_tooltip(true);
        let handle = inner.clone();
        label.connect_query_tooltip(move |_, _, _, _, tooltip| match handle.borrow().as_ref() {
            Some(markup) if !markup.is_empty() => {
                tooltip.set_markup(Some(markup));
                true
            }
            _ => false,
        });
        Tooltip { inner }
    }

    pub fn set(&self, markup: &str) {
        *self.inner.borrow_mut() = Some(markup.to_string());
    }
}
