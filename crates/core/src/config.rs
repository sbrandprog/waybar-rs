//! Configuration loading.
//!
//! Waybar configs are JSON-with-comments (JSONC). A config file is either a
//! single bar object or an array of bar objects (multi-bar setups). Module
//! options live in the same object, keyed by module name (`"clock"`,
//! `"battery#bat2"`, `"custom/foo"`, ...).

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

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

/// Configuration for a single bar.
#[derive(Debug, Clone)]
pub struct BarConfig {
    pub position: Position,
    /// Layer-shell layer: `background` | `bottom` | `top` | `overlay`.
    pub layer: String,
    pub height: i32,
    pub width: i32,
    pub spacing: i32,
    /// CSS/namespace name of the bar (`"waybar"` by default).
    pub name: String,
    /// Restrict the bar to a single output, if set.
    pub output: Option<String>,
    /// Whether the bar reserves space (exclusive zone). Defaults to true.
    pub exclusive: bool,
    pub margin_top: i32,
    pub margin_right: i32,
    pub margin_bottom: i32,
    pub margin_left: i32,
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
            layer: get_str("layer").unwrap_or_else(|| "top".to_string()),
            height: get_i32("height").unwrap_or(0),
            width: get_i32("width").unwrap_or(0),
            spacing: get_i32("spacing").unwrap_or(0),
            name: get_str("name").unwrap_or_else(|| "waybar".to_string()),
            output: get_str("output"),
            exclusive: v.get("exclusive").and_then(|x| x.as_bool()).unwrap_or(true),
            margin_top,
            margin_right,
            margin_bottom,
            margin_left,
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
}
