//! Shared widget helpers: click/scroll actions and tooltip plumbing.

use std::cell::RefCell;
use std::collections::HashMap;
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

/// The scroll directions a module can respond to, used to pick its cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollAxis {
    Vertical,
    Horizontal,
    Both,
}

/// Show the "pointer" (hand) cursor while the pointer hovers over `widget`.
///
/// GTK4 has no `cursor` CSS property, so interactivity is advertised
/// programmatically. The cursor is applied to `widget` and its descendants
/// (see [`apply_cursor`]). A click cursor always wins over a scroll cursor (see
/// [`set_scroll_cursor`]).
pub fn set_click_cursor<W: IsA<gtk::Widget>>(widget: &W) {
    apply_cursor(widget, "pointer");
}

/// Show a directional scroll cursor while the pointer hovers over `widget`.
///
/// Unlike [`set_click_cursor`], this never overrides an already-set cursor:
/// when a module is both clickable and scrollable, the click cursor wins.
pub fn set_scroll_cursor<W: IsA<gtk::Widget>>(widget: &W, axis: ScrollAxis) {
    if widget.cursor().is_none() {
        apply_cursor(widget, scroll_cursor_name(axis));
    }
}

/// Apply the cursor called `name` to `widget` and every descendant.
///
/// GTK finds the cursor by walking up from the deepest pickable widget, but a
/// button press sets an implicit grab on that widget and `update_cursor()`
/// stops the walk there. A cursor set only on the module root would therefore
/// vanish for as long as the button is held (a flicker of the default arrow),
/// so it is applied to the labels/images inside as well.
fn apply_cursor<W: IsA<gtk::Widget>>(widget: &W, name: &str) {
    widget.set_cursor_from_name(Some(name));
    let mut child = widget.first_child();
    while let Some(node) = child {
        apply_cursor(&node, name);
        child = node.next_sibling();
    }
}

/// The GDK cursor name for a scroll direction.
fn scroll_cursor_name(axis: ScrollAxis) -> &'static str {
    match axis {
        ScrollAxis::Vertical => "ns-resize",
        ScrollAxis::Horizontal => "ew-resize",
        ScrollAxis::Both => "all-scroll",
    }
}

/// The scroll directions the `on-scroll-*` commands cover, if any.
fn scroll_axis(config: &Value) -> Option<ScrollAxis> {
    let vertical = has_string(config, "on-scroll-up") || has_string(config, "on-scroll-down");
    let horizontal = has_string(config, "on-scroll-left") || has_string(config, "on-scroll-right");
    match (vertical, horizontal) {
        (true, true) => Some(ScrollAxis::Both),
        (true, false) => Some(ScrollAxis::Vertical),
        (false, true) => Some(ScrollAxis::Horizontal),
        (false, false) => None,
    }
}

/// Attach `on-click*`/`on-scroll*` command handlers from the module config.
///
/// Controllers are only installed when a matching command is configured, so
/// modules without actions don't consume click/scroll events. The module's
/// cursor follows the same rule: `pointer` for clicks, a directional scroll
/// cursor otherwise.
pub fn attach_actions<W: IsA<gtk::Widget>>(widget: &W, config: &Value) {
    let clickable = has_click_command(config);
    let scroll = scroll_axis(config);
    if clickable {
        attach_clicks(widget, config);
        set_click_cursor(widget);
    } else if let Some(axis) = scroll {
        set_scroll_cursor(widget, axis);
    }
    if scroll.is_some() {
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

/// Whether a left-button click command (`on-click`, `on-double-click`,
/// `on-triple-click`) is configured.
///
/// Modules that bind their own single-left-click gesture (e.g. `clock`'s
/// `format-alt` toggle) use this to yield to an explicit user command.
pub fn has_left_click_command(config: &Value) -> bool {
    [(1, 1), (1, 2), (1, 3)].iter().any(|&(button, n_press)| {
        click_key(button, n_press).is_some_and(|key| has_string(config, &key))
    })
}

fn has_string(config: &Value, key: &str) -> bool {
    config.get(key).is_some_and(|v| v.is_string())
}

/// Whether a built-in single click registered for `builtin` applies to a press
/// on `button` (`0` means "any button").
fn builtin_matches(builtin: u32, button: u32) -> bool {
    builtin == 0 || builtin == button
}

/// The click levels configured for one button, merged with a module's built-in
/// single-click behaviour.
#[derive(Clone, Copy, Default)]
struct ClickLevels {
    single: bool,
    double: bool,
    triple: bool,
}

impl ClickLevels {
    /// The `on-click*` commands configured for `button`, plus a built-in single
    /// click when `builtin_button` matches (`0` = any button).
    fn for_button(config: &Value, button: u32, builtin_button: Option<u32>) -> Self {
        let configured =
            |n_press| click_key(button, n_press).is_some_and(|key| has_string(config, &key));
        let builtin = builtin_button.is_some_and(|b| builtin_matches(b, button));
        ClickLevels {
            single: configured(1) || builtin,
            double: configured(2),
            triple: configured(3),
        }
    }

    /// The highest configured level (1..=3), if any.
    fn highest(&self) -> Option<i32> {
        [(3, self.triple), (2, self.double), (1, self.single)]
            .into_iter()
            .find_map(|(level, on)| on.then_some(level))
    }

    /// The highest configured level not exceeding `count`, used to resolve a
    /// click sequence once GTK reports it has stopped.
    fn resolve(&self, count: i32) -> Option<i32> {
        [(1, self.single), (2, self.double), (3, self.triple)]
            .into_iter()
            .filter(|&(level, on)| on && level <= count)
            .map(|(level, _)| level)
            .next_back()
    }
}

/// Drive GTK click resolution and call `dispatch(button, level)` for every click
/// the user actually completed.
///
/// `GtkGestureClick` counts presses that follow each other within the system
/// double-click timeout (`n_press = 1, 2, 3, …`) and resets the counter only
/// after the timeout has elapsed. A raw press count therefore does not mean the
/// user asked for a double/triple click — it only matters when such a command is
/// actually configured. This resolver:
///
/// * fires a single click immediately when no higher level is configured, so a
///   series of clicks is never swallowed after the first one;
/// * otherwise waits for [`gtk::GestureClick::stopped`] — GTK's own
///   time/distance signal, emitted right before the click counter resets — and
///   dispatches the highest configured level the user reached.
fn connect_clicks<W, F>(widget: &W, config: &Value, builtin_button: Option<u32>, dispatch: F)
where
    W: IsA<gtk::Widget>,
    F: Fn(u32, i32) + 'static,
{
    let dispatch = Rc::new(dispatch);
    let config = config.clone();
    // Presses accumulated in the current, not-yet-resolved sequence, per button.
    let pending: Rc<RefCell<HashMap<u32, i32>>> = Rc::new(RefCell::new(HashMap::new()));

    let gesture = gtk::GestureClick::new();
    gesture.set_button(0);

    {
        let config = config.clone();
        let pending = pending.clone();
        let dispatch = dispatch.clone();
        gesture.connect_pressed(move |gesture, _n_press, _x, _y| {
            let button = gesture.current_button();
            let levels = ClickLevels::for_button(&config, button, builtin_button);
            let Some(highest) = levels.highest() else {
                return;
            };
            let count = {
                let mut pending = pending.borrow_mut();
                let count = pending.entry(button).or_insert(0);
                *count += 1;
                *count
            };
            // Only wait when a higher configured level could still arrive;
            // otherwise the click is already final and needs no timeout.
            if count >= highest {
                pending.borrow_mut().remove(&button);
                if let Some(level) = levels.resolve(count) {
                    dispatch(button, level);
                }
            }
        });
    }

    {
        let config = config.clone();
        let pending = pending.clone();
        let dispatch = dispatch.clone();
        // `stopped` is emitted when the time/distance between clicks exceeded
        // GTK's defaults and the click counter is about to reset — the moment an
        // unfinished single/double/… click becomes final.
        gesture.connect_stopped(move |_| {
            let resolved: Vec<(u32, i32)> = pending.borrow_mut().drain().collect();
            for (button, count) in resolved {
                let levels = ClickLevels::for_button(&config, button, builtin_button);
                if let Some(level) = levels.resolve(count) {
                    dispatch(button, level);
                }
            }
        });
    }

    widget.add_controller(gesture);
}

fn attach_clicks<W: IsA<gtk::Widget>>(widget: &W, config: &Value) {
    let dispatch_config = config.clone();
    connect_clicks(widget, config, None, move |button, level| {
        let Some(key) = click_key(button, level) else {
            return;
        };
        if let Some(cmd) = dispatch_config.get(&key).and_then(|v| v.as_str()) {
            spawn_command(cmd);
        }
    });
}

/// Attach a module-specific action that runs on a resolved single click.
///
/// The callback receives the clicked button. When no `on-double-click`/
/// `on-triple-click` is configured for that button, every press fires it
/// immediately; otherwise the action waits for GTK's click sequence to stop, so
/// a double/triple click does not also trigger the single-click action.
pub fn attach_single_click<W, F>(widget: &W, config: &Value, button: u32, on_single: F)
where
    W: IsA<gtk::Widget>,
    F: Fn(u32) + 'static,
{
    set_click_cursor(widget);
    connect_clicks(widget, config, Some(button), move |clicked, level| {
        if level == 1 && builtin_matches(button, clicked) {
            on_single(clicked);
        }
    });
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

/// A tooltip whose markup can be set directly or computed on demand.
///
/// Modules with cheap, already-rendered text use [`Tooltip::set`]; modules whose
/// tooltip depends on the current time or on data that changes faster than the
/// module's refresh interval use [`Tooltip::set_provider`], so the markup is only
/// built when the tooltip is about to be shown.
#[derive(Clone)]
pub struct Tooltip {
    inner: Rc<RefCell<TooltipState>>,
}

#[derive(Default)]
struct TooltipState {
    markup: Option<String>,
    provider: Option<Rc<dyn Fn() -> String>>,
}

impl Tooltip {
    /// Enable tooltips on `label` and return a handle to update the markup.
    pub fn attach(label: &gtk::Label) -> Self {
        let inner: Rc<RefCell<TooltipState>> = Rc::new(RefCell::new(TooltipState::default()));
        label.set_has_tooltip(true);
        let handle = inner.clone();
        label.connect_query_tooltip(move |_, _, _, _, tooltip| {
            let (markup, provider) = {
                let state = handle.borrow();
                (state.markup.clone(), state.provider.clone())
            };
            // The provider is called with the borrow released: it must not touch
            // this tooltip's state (and does not, in the clock module).
            let markup = match provider {
                Some(render) => render(),
                None => markup.unwrap_or_default(),
            };
            if markup.is_empty() {
                return false;
            }
            tooltip.set_markup(Some(&markup));
            true
        });
        Tooltip { inner }
    }

    pub fn set(&self, markup: &str) {
        self.inner.borrow_mut().markup = Some(markup.to_string());
    }

    /// Compute the tooltip markup on demand, when it is about to be shown.
    ///
    /// Takes precedence over any markup stored with [`Tooltip::set`].
    pub fn set_provider<F: Fn() -> String + 'static>(&self, render: F) {
        self.inner.borrow_mut().provider = Some(Rc::new(render));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_left_click_commands() {
        assert!(!has_left_click_command(&json!({})));
        // Other buttons and scroll do not count.
        assert!(!has_left_click_command(&json!({"on-click-right": "x"})));
        assert!(!has_left_click_command(&json!({"on-scroll-up": "x"})));
        assert!(has_left_click_command(&json!({"on-click": "x"})));
        assert!(has_left_click_command(&json!({"on-double-click": "x"})));
        assert!(has_left_click_command(&json!({"on-triple-click": "x"})));
    }

    #[test]
    fn scroll_axis_reflects_configured_directions() {
        assert_eq!(scroll_axis(&json!({})), None);
        assert_eq!(scroll_axis(&json!({"on-click": "x"})), None);
        assert_eq!(
            scroll_axis(&json!({"on-scroll-up": "x"})),
            Some(ScrollAxis::Vertical)
        );
        assert_eq!(
            scroll_axis(&json!({"on-scroll-down": "x"})),
            Some(ScrollAxis::Vertical)
        );
        assert_eq!(
            scroll_axis(&json!({"on-scroll-left": "x"})),
            Some(ScrollAxis::Horizontal)
        );
        assert_eq!(
            scroll_axis(&json!({"on-scroll-right": "x"})),
            Some(ScrollAxis::Horizontal)
        );
        assert_eq!(
            scroll_axis(&json!({"on-scroll-up": "x", "on-scroll-left": "y"})),
            Some(ScrollAxis::Both)
        );
        // A non-string value is not a command.
        assert_eq!(scroll_axis(&json!({"on-scroll-up": 5})), None);
    }

    #[test]
    fn scroll_cursor_names_match_axes() {
        assert_eq!(scroll_cursor_name(ScrollAxis::Vertical), "ns-resize");
        assert_eq!(scroll_cursor_name(ScrollAxis::Horizontal), "ew-resize");
        assert_eq!(scroll_cursor_name(ScrollAxis::Both), "all-scroll");
    }

    fn levels(config: &Value, button: u32) -> ClickLevels {
        ClickLevels::for_button(config, button, None)
    }

    #[test]
    fn a_lone_single_click_is_always_final() {
        // The root of the bug: GTK reports n_press = 2, 3, … for clicks that
        // follow each other quickly. With no multi-click command configured,
        // every press must still resolve to a single click.
        let config = json!({"on-click": "x"});
        let levels = levels(&config, 1);
        assert_eq!(levels.highest(), Some(1));
        assert_eq!(levels.resolve(1), Some(1));
        assert_eq!(levels.resolve(2), Some(1));
        assert_eq!(levels.resolve(7), Some(1));
    }

    #[test]
    fn double_click_waits_for_the_second_press() {
        let config = json!({"on-click": "x", "on-double-click": "y"});
        let levels = levels(&config, 1);
        assert_eq!(levels.highest(), Some(2));
        assert_eq!(levels.resolve(1), Some(1));
        assert_eq!(levels.resolve(2), Some(2));
        // A count past the highest configured level resolves to that level
        // (unreachable in the resolver, which resolves as soon as it is hit).
        assert_eq!(levels.resolve(3), Some(2));
    }

    #[test]
    fn a_double_click_alone_ignores_a_single_press() {
        let config = json!({"on-double-click": "y"});
        let levels = levels(&config, 1);
        assert_eq!(levels.highest(), Some(2));
        assert_eq!(levels.resolve(1), None);
        assert_eq!(levels.resolve(2), Some(2));
    }

    #[test]
    fn a_triple_click_falls_back_to_a_single() {
        let config = json!({"on-click": "x", "on-triple-click": "z"});
        let levels = levels(&config, 1);
        assert_eq!(levels.highest(), Some(3));
        assert_eq!(levels.resolve(1), Some(1));
        assert_eq!(levels.resolve(2), Some(1));
        assert_eq!(levels.resolve(3), Some(3));
    }

    #[test]
    fn click_levels_are_per_button() {
        let config = json!({"on-click-right": "x", "on-double-click-middle": "y"});
        assert_eq!(levels(&config, 3).highest(), Some(1));
        assert_eq!(levels(&config, 2).highest(), Some(2));
        assert_eq!(levels(&config, 1).highest(), None);
        // `on-click-copy` is handled by ALabel-style modules, not here.
        assert_eq!(levels(&json!({"on-click": "x"}), 10).highest(), None);
    }

    #[test]
    fn a_builtin_click_only_matches_its_button() {
        assert!(builtin_matches(0, 1));
        assert!(builtin_matches(0, 3));
        assert!(builtin_matches(1, 1));
        assert!(!builtin_matches(1, 3));
        assert!(!builtin_matches(3, 1));
    }

    #[test]
    fn a_builtin_single_click_merges_with_the_config() {
        // A built-in single click must wait when a double-click command exists…
        let config = json!({"on-double-click": "y"});
        let levels = ClickLevels::for_button(&config, 1, Some(1));
        assert_eq!(levels.highest(), Some(2));
        assert_eq!(levels.resolve(1), Some(1));
        assert_eq!(levels.resolve(2), Some(2));

        // …and button 0 registers the built-in single click on every button.
        let any = ClickLevels::for_button(&json!({}), 3, Some(0));
        assert_eq!(any.highest(), Some(1));
        assert_eq!(any.resolve(1), Some(1));
    }
}
