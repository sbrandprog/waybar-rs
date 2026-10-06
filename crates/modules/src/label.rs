//! A GTK label module, the Rust analogue of waybar's `ALabel`.
//!
//! The widget tree mirrors waybar: an unnamed wrapper box around a named
//! [`gtk::Label`], so selectors such as `#clock`, `.module` and
//! `.modules-left > * > *` keep working.
//!
//! This type also carries the shared `ALabel` behaviour the simple informers
//! rely on: state classes, `format-<state>` / `tooltip-format-<state>`
//! resolution, `format-icons` lookup and interval parsing.
//!
//! The selection logic is kept in free functions so it can be unit-tested
//! without a display.

use gtk::prelude::*;
use serde_json::Value;

use waybar_core::format::{self as fmt, Args};
use waybar_core::widgets::{Tooltip, attach_actions, tooltip_enabled};

#[derive(Clone)]
pub struct LabelModule {
    /// The named container that carries the CSS classes and state.
    root: gtk::Box,
    /// The widget exposed to the bar. For an ordinary label this is `root`
    /// itself (the unnamed wrapper around the named label); `new_icon_label`
    /// adds an extra unnamed wrapper so `.modules-* > * > *` still matches the
    /// module once, despite the named box being the root of its own subtree.
    outer: gtk::Box,
    label: gtk::Label,
    tooltip: Option<Tooltip>,
    config: Value,
    /// Full module name from the config (`battery#bat2`, `custom/foo`), used as
    /// the context in format warnings.
    context: String,
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
            outer: root.clone(),
            root,
            label,
            tooltip,
            config: config.clone(),
            context: full_name(name, id),
        }
    }

    /// Construct a label whose widget name and CSS classes live on a named box
    /// wrapping the label, mirroring upstream's `AIconLabel` (used by
    /// `custom/*`, where `#custom-<name>` is the box rather than the label).
    /// The named box is itself wrapped in an unnamed box returned to the bar.
    pub fn new_icon_label(name: &str, id: &str, config: &Value) -> Self {
        let label = gtk::Label::new(None);

        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.set_widget_name(name);
        root.add_css_class("module");
        if !id.is_empty() {
            root.add_css_class(id);
        }
        root.append(&label);

        let outer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        outer.append(&root);

        attach_actions(&root, config);
        let tooltip = tooltip_enabled(config).then(|| Tooltip::attach(&label));

        LabelModule {
            outer,
            root,
            label,
            tooltip,
            config: config.clone(),
            context: full_name(name, id),
        }
    }

    /// Override the context used in format warnings with the module's full name
    /// from the config (e.g. `custom/foo`, `sway/language`).
    pub fn with_context(mut self, context: &str) -> Self {
        self.context = context.to_string();
        self
    }

    /// The module name used as the context in format warnings.
    pub fn context(&self) -> &str {
        &self.context
    }

    /// Render `template` against `args`, reporting unknown placeholders under
    /// this module's name.
    pub fn render(&self, template: &str, args: &Args) -> String {
        fmt::format_ctx(template, args, &self.context)
    }

    /// The wrapper widget to insert into the bar.
    pub fn widget(&self) -> gtk::Widget {
        self.outer.clone().upcast()
    }

    /// The inner label. Used by modules that attach their own controllers
    /// (a weak reference keeps the widget from leaking through the closure).
    pub fn label(&self) -> &gtk::Label {
        &self.label
    }

    /// The tooltip handle, when tooltips are enabled for this module.
    pub fn tooltip(&self) -> Option<&Tooltip> {
        self.tooltip.as_ref()
    }

    /// The module's option object.
    pub fn config(&self) -> &Value {
        &self.config
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

    /// Show or hide the module.
    pub fn set_visible(&self, visible: bool) {
        self.outer.set_visible(visible);
    }

    /// Add or remove a CSS class on the label.
    pub fn set_class(&self, class: &str, on: bool) {
        if on {
            self.label.add_css_class(class);
        } else {
            self.label.remove_css_class(class);
        }
    }

    /// Add or remove a CSS class on the wrapper box.
    pub fn set_box_class(&self, class: &str, on: bool) {
        if on {
            self.root.add_css_class(class);
        } else {
            self.root.remove_css_class(class);
        }
    }

    // --- option helpers -------------------------------------------------

    /// Read a string option, falling back to `default`.
    pub fn option_str(&self, key: &str, default: &str) -> String {
        self.config
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or(default)
            .to_string()
    }

    pub fn option_bool(&self, key: &str, default: bool) -> bool {
        self.config
            .get(key)
            .and_then(|v| v.as_bool())
            .unwrap_or(default)
    }

    pub fn option_f64(&self, key: &str, default: f64) -> f64 {
        self.config
            .get(key)
            .and_then(|v| v.as_f64())
            .unwrap_or(default)
    }

    /// The update interval in seconds, or `None` for `interval: "once"`.
    ///
    /// A non-positive numeric interval falls back to `default`, matching
    /// waybar's periodic modules.
    pub fn interval_secs(&self, default: u64) -> Option<u64> {
        parse_interval(&self.config, default)
    }

    // --- ALabel behaviour -----------------------------------------------

    /// Resolve the format string: `format-<state>`, then `format`, then the
    /// module-provided `default`.
    pub fn resolve_format(&self, default: &str, state: &str) -> String {
        if !state.is_empty()
            && let Some(s) = self
                .config
                .get(format!("format-{state}"))
                .and_then(|v| v.as_str())
        {
            return s.to_string();
        }
        if let Some(s) = self.config.get("format").and_then(|v| v.as_str()) {
            return s.to_string();
        }
        default.to_string()
    }

    /// Resolve the tooltip format: `tooltip-format-<state>`, then
    /// `tooltip-format`, then `default`.
    pub fn resolve_tooltip_format(&self, default: &str, state: &str) -> String {
        if !state.is_empty()
            && let Some(s) = self
                .config
                .get(format!("tooltip-format-{state}"))
                .and_then(|v| v.as_str())
        {
            return s.to_string();
        }
        if let Some(s) = self.config.get("tooltip-format").and_then(|v| v.as_str()) {
            return s.to_string();
        }
        default.to_string()
    }

    /// Apply the `states` thresholds and return the active state name.
    ///
    /// The active state's CSS class is added to the label and every other
    /// state class is removed, mirroring `ALabel::getState`. With `lesser`,
    /// lower values are "worse" (used by the battery module).
    pub fn get_state(&self, value: u8, lesser: bool) -> String {
        let plan = state_plan(&self.config, value, lesser);
        let mut active = String::new();
        for (name, is_active) in &plan {
            self.set_class(name, *is_active);
            if *is_active {
                active = name.clone();
            }
        }
        active
    }

    /// Look up an icon from `format-icons` (see [`icon_for`]).
    pub fn get_icon(&self, percentage: u16, alts: &[&str], max: u16) -> String {
        icon_for(&self.config, percentage, alts, max)
    }
}

/// The full module name for warnings: `name#id` when an `#id` is configured.
fn full_name(name: &str, id: &str) -> String {
    if id.is_empty() {
        name.to_string()
    } else {
        format!("{name}#{id}")
    }
}

/// Decide which `states` entries should be active for `value`.
///
/// Returns every state name paired with whether its class should be applied;
/// exactly one (the first match in sorted order) can be active. The sort order
/// follows `lesser`: ascending when lower is worse, descending otherwise.
pub(crate) fn state_plan(config: &Value, value: u8, lesser: bool) -> Vec<(String, bool)> {
    let Some(states) = config.get("states").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut pairs: Vec<(String, u8)> = states
        .iter()
        .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n.min(u8::MAX as u64) as u8)))
        .collect();
    pairs.sort_by(|a, b| if lesser { a.1.cmp(&b.1) } else { b.1.cmp(&a.1) });

    let mut taken = false;
    pairs
        .into_iter()
        .map(|(name, threshold)| {
            let matched = if lesser {
                value <= threshold
            } else {
                value >= threshold
            };
            let active = matched && !taken;
            if active {
                taken = true;
            }
            (name, active)
        })
        .collect()
}

/// Pick an icon from the `format-icons` option.
///
/// `alts` are tried in order as keys of the object form; otherwise the array
/// form is used, supporting both evenly-divided arrays and threshold objects
/// (`{"icon": "...", "max": N}`). `max` bounds the evenly-divided form
/// (0 means 100).
pub(crate) fn icon_for(config: &Value, percentage: u16, alts: &[&str], max: u16) -> String {
    let mut format_icons = config.get("format-icons").cloned().unwrap_or(Value::Null);

    if let Some(obj) = format_icons.as_object() {
        let mut chosen = "default";
        for alt in alts {
            if alt.is_empty() {
                continue;
            }
            if let Some(v) = obj.get(*alt)
                && (v.is_string() || v.is_array())
            {
                chosen = alt;
                break;
            }
        }
        format_icons = obj.get(chosen).cloned().unwrap_or(Value::Null);
    }

    if let Some(arr) = format_icons.as_array() {
        let size = arr.len();
        if size == 0 {
            return String::new();
        }
        if arr[0].is_object() {
            let mut last_icon = String::new();
            for threshold in arr {
                let (Some(icon), Some(bound)) = (
                    threshold.get("icon").and_then(|v| v.as_str()),
                    threshold.get("max").and_then(|v| v.as_u64()),
                ) else {
                    continue;
                };
                last_icon = icon.to_string();
                if u64::from(percentage) <= bound {
                    return last_icon;
                }
            }
            return last_icon;
        }
        let base = if max == 0 { 100u32 } else { u32::from(max) };
        let divisor = (base / size as u32).max(1);
        let idx = ((u32::from(percentage) / divisor) as usize).min(size - 1);
        return arr[idx].as_str().unwrap_or("").to_string();
    }

    if let Some(s) = format_icons.as_str() {
        return s.to_string();
    }
    String::new()
}

/// Parse the `interval` option: `"once"` disables the timer (`None`), a number
/// or a numeric string is rounded to whole seconds (at least 1), anything else
/// (including a non-positive value) falls back to `default`.
pub(crate) fn parse_interval(config: &Value, default: u64) -> Option<u64> {
    match config.get("interval") {
        Some(Value::String(s)) if s == "once" => None,
        Some(Value::String(s)) => {
            let secs = s.trim().parse::<f64>().unwrap_or(default as f64);
            Some(normalize_interval(secs, default))
        }
        Some(v) if v.is_number() => {
            let secs = v.as_f64().unwrap_or(default as f64);
            Some(normalize_interval(secs, default))
        }
        _ => Some(default),
    }
}

fn normalize_interval(secs: f64, default: u64) -> u64 {
    if secs <= 0.0 {
        default
    } else {
        secs.round().max(1.0) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn state_plan_picks_first_match_greater() {
        // CPU-style: higher is worse. critical(90) > warning(70).
        let cfg = json!({"states": {"warning": 70, "critical": 90}});
        let active = |v: u8| {
            state_plan(&cfg, v, false)
                .into_iter()
                .filter(|(_, a)| *a)
                .map(|(n, _)| n)
                .collect::<Vec<_>>()
        };
        assert_eq!(active(50), Vec::<String>::new());
        assert_eq!(active(80), vec!["warning"]);
        assert_eq!(active(95), vec!["critical"]);
    }

    #[test]
    fn state_plan_lesser_marks_low_values() {
        let cfg = json!({"states": {"critical": 15, "warning": 30, "good": 100}});
        let plan = state_plan(&cfg, 10, true);
        let active: Vec<&String> = plan.iter().filter(|(_, a)| *a).map(|(n, _)| n).collect();
        assert_eq!(active, vec!["critical"]);

        let plan = state_plan(&cfg, 50, true);
        let active: Vec<&String> = plan.iter().filter(|(_, a)| *a).map(|(n, _)| n).collect();
        assert_eq!(active, vec!["good"]);
    }

    #[test]
    fn icon_array_is_evenly_divided() {
        let cfg = json!({"format-icons": ["a", "b", "c", "d"]});
        assert_eq!(icon_for(&cfg, 0, &[], 0), "a");
        assert_eq!(icon_for(&cfg, 30, &[], 0), "b");
        assert_eq!(icon_for(&cfg, 60, &[], 0), "c");
        assert_eq!(icon_for(&cfg, 100, &[], 0), "d");
    }

    #[test]
    fn icon_object_uses_alt_then_default() {
        let cfg = json!({"format-icons": {"charging": ["c1", "c2"], "default": ["d1", "d2"]}});
        assert_eq!(icon_for(&cfg, 0, &["charging"], 0), "c1");
        assert_eq!(icon_for(&cfg, 0, &["missing", "charging"], 0), "c1");
        assert_eq!(icon_for(&cfg, 0, &[], 0), "d1");
        // object with a plain string value
        let cfg = json!({"format-icons": {"default": "x"}});
        assert_eq!(icon_for(&cfg, 0, &[], 0), "x");
    }

    #[test]
    fn icon_threshold_objects() {
        let cfg = json!({"format-icons": [
            {"icon": "low", "max": 20},
            {"icon": "mid", "max": 60},
            {"icon": "high", "max": 100}
        ]});
        assert_eq!(icon_for(&cfg, 5, &[], 0), "low");
        assert_eq!(icon_for(&cfg, 20, &[], 0), "low");
        assert_eq!(icon_for(&cfg, 21, &[], 0), "mid");
        assert_eq!(icon_for(&cfg, 100, &[], 0), "high");
        // above every threshold: last icon
        assert_eq!(icon_for(&cfg, 200, &[], 0), "high");
    }

    #[test]
    fn icon_missing_is_empty() {
        let cfg = json!({});
        assert_eq!(icon_for(&cfg, 50, &[], 0), "");
    }

    #[test]
    fn parses_interval_option() {
        assert_eq!(parse_interval(&json!({}), 60), Some(60));
        assert_eq!(parse_interval(&json!({"interval": "once"}), 60), None);
        assert_eq!(parse_interval(&json!({"interval": 5}), 60), Some(5));
        assert_eq!(parse_interval(&json!({"interval": 5.4}), 60), Some(5));
        assert_eq!(parse_interval(&json!({"interval": 0.4}), 60), Some(1));
        // numeric strings are accepted too
        assert_eq!(parse_interval(&json!({"interval": "5"}), 60), Some(5));
        assert_eq!(parse_interval(&json!({"interval": "1.5"}), 60), Some(2));
        // non-positive or unparsable values fall back to the default
        assert_eq!(parse_interval(&json!({"interval": 0}), 60), Some(60));
        assert_eq!(parse_interval(&json!({"interval": -3}), 60), Some(60));
        assert_eq!(parse_interval(&json!({"interval": "junk"}), 60), Some(60));
    }
}
