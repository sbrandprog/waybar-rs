//! Configuration loading.
//!
//! Waybar configs are JSON-with-comments (JSONC). A config file is either a
//! single bar object or an array of bar objects (multi-bar setups). Module
//! options live in the same object, keyed by module name (`"clock"`,
//! `"battery#bat2"`, `"custom/foo"`, ...).

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;
use tracing::warn;

/// Screen edge the bar is anchored to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Top,
    Bottom,
    Left,
    Right,
}

impl std::str::FromStr for Position {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "top" => Ok(Position::Top),
            "bottom" => Ok(Position::Bottom),
            "left" => Ok(Position::Left),
            "right" => Ok(Position::Right),
            _ => Err(()),
        }
    }
}

impl Position {
    pub fn as_str(&self) -> &'static str {
        match self {
            Position::Top => "top",
            Position::Bottom => "bottom",
            Position::Left => "left",
            Position::Right => "right",
        }
    }

    /// A bar on the left/right edge is laid out vertically.
    pub fn is_vertical(&self) -> bool {
        matches!(self, Position::Left | Position::Right)
    }
}

/// The bar's `output` option, mirroring waybar's syntax.
///
/// Waybar accepts a string or an array of strings. An entry may be an exact
/// connector (`DP-1`) or output description (`Dell Inc. DELL U2419H`), a
/// `$ENV` expansion, a leading `!` exclusion, or — in the array form only — a
/// leading `*` wildcard that matches any output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum OutputFilter {
    /// No `output` key, an empty value or a malformed one: every output.
    #[default]
    All,
    /// The string form, e.g. `DP-1` or `!HDMI-1`.
    Single(String),
    /// The array form, evaluated in order.
    Array(Vec<String>),
}

impl OutputFilter {
    /// Parse the `output` key of a bar config object.
    pub fn from_value(v: &Value) -> Self {
        match v.get("output") {
            Some(Value::String(s)) if !s.is_empty() => Self::Single(s.clone()),
            Some(Value::Array(items)) => {
                let entries: Vec<String> = items
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect();
                if entries.is_empty() {
                    Self::All
                } else {
                    Self::Array(entries)
                }
            }
            _ => Self::All,
        }
    }

    /// Whether the filter selects the output with connector `name` and
    /// description `identifier`.
    pub fn matches(&self, name: &str, identifier: &str) -> bool {
        match self {
            Self::All => true,
            Self::Single(s) => match s.strip_prefix('!') {
                Some(rest) => !output_name_matches(rest, name, identifier),
                None => output_name_matches(s, name, identifier),
            },
            // An array is an ordered allow-list: `!` entries remove an output,
            // exact/`$ENV` entries select one, and a leading `*` selects any.
            Self::Array(entries) => {
                for entry in entries {
                    if let Some(rest) = entry.strip_prefix('!') {
                        if output_name_matches(rest, name, identifier) {
                            return false;
                        }
                        continue;
                    }
                    if output_name_matches(entry, name, identifier) || entry.starts_with('*') {
                        return true;
                    }
                }
                false
            }
        }
    }
}

/// Exact match against the output connector or description, expanding `$ENV`.
///
/// Mirrors waybar's `isOutputMatches`: an unset environment variable warns and
/// does not match.
fn output_name_matches(entry: &str, name: &str, identifier: &str) -> bool {
    let resolved = match entry.strip_prefix('$') {
        Some(var) => match std::env::var(var) {
            Ok(value) => value,
            Err(_) => {
                warn!("output: environment variable `{var}` is not set");
                return false;
            }
        },
        None => entry.to_string(),
    };
    resolved == name || resolved == identifier
}

/// Configuration for a single bar.
#[derive(Debug, Clone)]
pub struct BarConfig {
    pub position: Position,
    /// Layer-shell layer: `background` | `bottom` | `top` | `overlay`.
    /// Defaults to `bottom`, matching waybar.
    pub layer: String,
    pub height: i32,
    pub width: i32,
    pub spacing: i32,
    /// CSS/namespace name of the bar (`"waybar"` by default).
    pub name: String,
    /// Restrict the bar to specific output(s); defaults to every output.
    pub output: OutputFilter,
    /// Whether the bar reserves space (exclusive zone). Defaults to true.
    pub exclusive: bool,
    pub margin_top: i32,
    pub margin_right: i32,
    pub margin_bottom: i32,
    pub margin_left: i32,
    /// Reload the stylesheet when it (or one of its `@import`s) changes.
    pub reload_style_on_change: bool,
    /// The raw JSON object this bar was parsed from. Used to look up the
    /// per-module option objects.
    pub raw: Value,
}

impl BarConfig {
    pub fn from_value(v: &Value) -> Self {
        let get_str = |key: &str| v.get(key).and_then(|x| x.as_str()).map(str::to_string);
        let get_i32 = |key: &str| v.get(key).and_then(|x| x.as_i64()).map(|x| x as i32);

        let position = get_str("position")
            .and_then(|s| s.parse::<Position>().ok())
            .unwrap_or(Position::Top);

        let (margin_top, margin_right, margin_bottom, margin_left) = parse_margins(v);

        BarConfig {
            position,
            layer: get_str("layer").unwrap_or_else(|| "bottom".to_string()),
            height: get_i32("height").unwrap_or(0),
            width: get_i32("width").unwrap_or(0),
            spacing: get_i32("spacing").unwrap_or(0),
            name: get_str("name").unwrap_or_else(|| "waybar".to_string()),
            output: OutputFilter::from_value(v),
            exclusive: v.get("exclusive").and_then(|x| x.as_bool()).unwrap_or(true),
            margin_top,
            margin_right,
            margin_bottom,
            margin_left,
            reload_style_on_change: v
                .get("reload_style_on_change")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            raw: v.clone(),
        }
    }

    /// The option object for `name` (the full key, including any `#id`).
    pub fn module_config(&self, name: &str) -> Value {
        self.raw.get(name).cloned().unwrap_or(Value::Null)
    }

    /// The list of module names in the given section (`modules-left`, ...).
    pub fn module_list(&self, section: &str) -> Vec<String> {
        self.raw
            .get(section)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn parse_margins(v: &Value) -> (i32, i32, i32, i32) {
    let named = (
        v.get("margin-top").and_then(|x| x.as_i64()),
        v.get("margin-right").and_then(|x| x.as_i64()),
        v.get("margin-bottom").and_then(|x| x.as_i64()),
        v.get("margin-left").and_then(|x| x.as_i64()),
    );
    if named.0.is_some() || named.1.is_some() || named.2.is_some() || named.3.is_some() {
        return (
            named.0.unwrap_or(0) as i32,
            named.1.unwrap_or(0) as i32,
            named.2.unwrap_or(0) as i32,
            named.3.unwrap_or(0) as i32,
        );
    }

    match v.get("margin") {
        Some(Value::Number(n)) => {
            let g = n.as_i64().unwrap_or(0) as i32;
            (g, g, g, g)
        }
        Some(Value::String(s)) => {
            let parts: Vec<i32> = s
                .split_whitespace()
                .filter_map(|p| p.parse::<i32>().ok())
                .collect();
            match parts.as_slice() {
                [g] => (*g, *g, *g, *g),
                [v_, h] => (*v_, *h, *v_, *h),
                [t, h, b] => (*t, *h, *b, *h),
                [t, r, b, l] => (*t, *r, *b, *l),
                _ => (0, 0, 0, 0),
            }
        }
        _ => (0, 0, 0, 0),
    }
}

/// All bars described by a config file.
#[derive(Debug, Clone)]
pub struct Config {
    pub bars: Vec<BarConfig>,
}

impl Config {
    /// Every bar config whose `output` filter matches the output with connector
    /// `name` and description `identifier`, in file order.
    ///
    /// A bar without an `output` key matches every output, so it produces one
    /// bar per monitor — matching waybar's `getOutputConfigs`.
    pub fn configs_for_output(&self, name: &str, identifier: &str) -> Vec<&BarConfig> {
        self.bars
            .iter()
            .filter(|bar| bar.output.matches(name, identifier))
            .collect()
    }

    /// Whether any bar asks for the stylesheet to be reloaded on change.
    pub fn reload_style_on_change(&self) -> bool {
        self.bars.iter().any(|bar| bar.reload_style_on_change)
    }

    /// Load and parse a JSONC config file.
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config file {}", path.display()))?;
        Self::parse(&text)
    }

    /// Parse a JSONC config from a string.
    pub fn parse(text: &str) -> Result<Config> {
        let value: Value =
            json5::from_str(text).context("failed to parse config (JSONC/JSON5 syntax error)")?;
        let bars = match value {
            Value::Array(items) => items.iter().map(BarConfig::from_value).collect(),
            Value::Object(_) => vec![BarConfig::from_value(&value)],
            _ => anyhow::bail!("config root must be an object or an array of objects"),
        };
        Ok(Config { bars })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_bar_with_comments() {
        let cfg = Config::parse(
            r#"{
                // a comment
                "position": "bottom",
                "height": 28,
                "modules-left": ["clock", "cpu#1"],
            }"#,
        )
        .unwrap();
        assert_eq!(cfg.bars.len(), 1);
        let bar = &cfg.bars[0];
        assert_eq!(bar.position, Position::Bottom);
        assert_eq!(bar.height, 28);
        assert_eq!(bar.module_list("modules-left"), vec!["clock", "cpu#1"]);
    }

    #[test]
    fn parses_multi_bar_array() {
        let cfg = Config::parse(r#"[{"height": 30}, {"height": 40}]"#).unwrap();
        assert_eq!(cfg.bars.len(), 2);
        assert_eq!(cfg.bars[1].height, 40);
    }

    #[test]
    fn margin_string_forms() {
        let bar = BarConfig::from_value(&serde_json::json!({"margin": "1 2 3 4"}));
        assert_eq!(
            (bar.margin_top, bar.margin_right, bar.margin_bottom, bar.margin_left),
            (1, 2, 3, 4)
        );
        let bar = BarConfig::from_value(&serde_json::json!({"margin": 5}));
        assert_eq!(
            (bar.margin_top, bar.margin_right, bar.margin_bottom, bar.margin_left),
            (5, 5, 5, 5)
        );
    }

    #[test]
    fn output_filter_absent_or_empty_is_all() {
        assert_eq!(OutputFilter::from_value(&serde_json::json!({})), OutputFilter::All);
        assert_eq!(
            OutputFilter::from_value(&serde_json::json!({"output": ""})),
            OutputFilter::All
        );
        assert_eq!(
            OutputFilter::from_value(&serde_json::json!({"output": []})),
            OutputFilter::All
        );
    }

    #[test]
    fn output_filter_string_and_negation() {
        let exact = OutputFilter::from_value(&serde_json::json!({"output": "DP-1"}));
        assert!(exact.matches("DP-1", "Dell Inc. DELL U2419H"));
        assert!(exact.matches("DP-1", ""));
        assert!(!exact.matches("HDMI-1", ""));

        let excluded = OutputFilter::from_value(&serde_json::json!({"output": "!DP-1"}));
        assert!(!excluded.matches("DP-1", ""));
        assert!(excluded.matches("HDMI-1", ""));
    }

    #[test]
    fn output_filter_array_with_wildcard() {
        let filter = OutputFilter::from_value(&serde_json::json!({"output": ["DP-1", "HDMI-1"]}));
        assert!(filter.matches("DP-1", ""));
        assert!(filter.matches("HDMI-1", ""));
        assert!(!filter.matches("eDP-1", ""));

        // `!` removes an output even when a later `*` would select it.
        let wildcard = OutputFilter::from_value(&serde_json::json!({"output": ["!DP-1", "*"]}));
        assert!(!wildcard.matches("DP-1", ""));
        assert!(wildcard.matches("HDMI-1", ""));
    }

    #[test]
    fn output_filter_matches_description() {
        let filter = OutputFilter::from_value(&serde_json::json!({"output": "DELL U2419H"}));
        assert!(filter.matches("DP-1", "DELL U2419H"));
    }

    #[test]
    fn configs_for_output_selects_in_file_order() {
        let cfg = Config::parse(
            r#"[
                {"name": "all"},
                {"name": "internal", "output": "eDP-1"},
                {"name": "external", "output": ["!eDP-1", "*"]}
            ]"#,
        )
        .unwrap();

        // An output without a filter matches every monitor, so every bar
        // without an `output` key is created once per monitor.
        let internal: Vec<&str> = cfg
            .configs_for_output("eDP-1", "desc")
            .iter()
            .map(|b| b.name.as_str())
            .collect();
        assert_eq!(internal, vec!["all", "internal"]);

        let external: Vec<&str> = cfg
            .configs_for_output("DP-1", "desc")
            .iter()
            .map(|b| b.name.as_str())
            .collect();
        assert_eq!(external, vec!["all", "external"]);
    }

    #[test]
    fn reload_style_on_change_is_opt_in() {
        let cfg = Config::parse(r#"[{"name": "a"}, {"name": "b"}]"#).unwrap();
        assert!(!cfg.reload_style_on_change());

        let cfg = Config::parse(r#"[{"name": "a"}, {"reload_style_on_change": true}]"#).unwrap();
        assert!(cfg.reload_style_on_change());
    }
}
