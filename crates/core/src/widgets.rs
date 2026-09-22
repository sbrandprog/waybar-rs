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
pub fn attach_actions<W: IsA<gtk::Widget>>(widget: &W, config: &Value) {
    attach_clicks(widget, config);
    attach_scroll(widget, config);
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
