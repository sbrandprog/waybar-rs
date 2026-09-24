//! `sway/language` — the active keyboard layout.
//!
//! Reads the layout list from `GET_INPUTS`, maps full names to short ones with
//! libxkbregistry and follows `input` events for live layout switches.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use futures_lite::StreamExt;
use serde_json::Value;
use swayipc_async::{Connection, Event, EventType, Input, InputChange};
use tracing::{error, warn};

use waybar_core::bridge;
use waybar_core::format::{self as fmt, Args};
use waybar_core::{Module, ModuleInit, Registry};

use crate::label::LabelModule;

use super::xkb::{Context, Layout};

/// The subset of a layout the UI needs, copied out for the main thread.
#[derive(Clone, Debug, PartialEq)]
struct LanguageState {
    short: String,
    short_description: String,
    long: String,
    variant: String,
    flag: String,
    layout_count: usize,
}

struct Language {
    inner: LabelModule,
}

impl Module for Language {
    fn widget(&self) -> gtk::Widget {
        self.inner.widget()
    }

    fn start(&self) {
        let inner = self.inner.clone();
        let config = inner.config().clone();

        let (tx, rx) = bridge::channel::<Option<LanguageState>>();
        {
            let config = config.clone();
            bridge::spawn(async move { worker(config, tx).await });
        }

        let applied: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        bridge::on_main(rx, move |state| update(&inner, &config, &applied, state));
    }
}

async fn worker(config: Value, tx: async_channel::Sender<Option<LanguageState>>) {
    let format = config
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("{}")
        .to_string();
    let is_variant_displayed = format.contains("{variant}");
    let displayed_short = format.contains("{}")
        || format.contains("{short}")
        || format.contains("{shortDescription}");

    let context = Context::new();
    let all_layouts = context.as_ref().map(Context::layouts).unwrap_or_default();
    if context.is_none() {
        warn!("sway/language: libxkbregistry unavailable; layout names will not resolve");
    }

    let event_conn = match Connection::new().await {
        Ok(conn) => conn,
        Err(err) => {
            error!("sway/language: cannot connect to sway: {err}");
            return;
        }
    };
    let mut events = match event_conn.subscribe([EventType::Input]).await {
        Ok(events) => events,
        Err(err) => {
            error!("sway/language: cannot subscribe: {err}");
            return;
        }
    };
    let mut cmd_conn = match Connection::new().await {
        Ok(conn) => conn,
        Err(err) => {
            error!("sway/language: cannot open command connection: {err}");
            return;
        }
    };

    let mut map: HashMap<String, Layout> = HashMap::new();
    let mut current_key = String::new();

    if let Ok(inputs) = cmd_conn.get_inputs().await {
        let (used, active) = collect_used_layouts(&inputs);
        map = build_layouts_map(&all_layouts, &used, is_variant_displayed, displayed_short);
        if let Some(active) = active {
            current_key = active;
        }
        let _ = tx.send(state_for(&map, &current_key)).await;
    }

    while let Some(event) = events.next().await {
        let event = match event {
            Ok(event) => event,
            Err(err) => {
                warn!("sway/language: event error: {err}");
                continue;
            }
        };
        let Event::Input(event) = event else {
            continue;
        };
        if event.input.input_type != "keyboard" {
            continue;
        }

        if let Some(active) = &event.input.xkb_active_layout_name
            && map.contains_key(active)
        {
            current_key = active.clone();
        }

        // A new device or a changed keymap can add layouts, so refresh the map.
        if matches!(event.change, InputChange::Added | InputChange::XkbKeymap)
            && let Ok(inputs) = cmd_conn.get_inputs().await
        {
            let (used, _) = collect_used_layouts(&inputs);
            map = build_layouts_map(&all_layouts, &used, is_variant_displayed, displayed_short);
        }

        let _ = tx.send(state_for(&map, &current_key)).await;
    }
}

/// Union of every keyboard's layout names, in first-seen order, plus the name
/// of the active layout on the device with the most layouts.
fn collect_used_layouts(inputs: &[Input]) -> (Vec<String>, Option<String>) {
    let mut used = Vec::new();
    let mut max = 0usize;
    let mut active = None;
    for input in inputs {
        if input.input_type != "keyboard" {
            continue;
        }
        if input.xkb_layout_names.len() > max {
            max = input.xkb_layout_names.len();
            active = input.xkb_active_layout_name.clone();
        }
        for name in &input.xkb_layout_names {
            if !used.contains(name) {
                used.push(name.clone());
            }
        }
    }
    (used, active)
}

/// Map every used full layout name to its xkbregistry entry.
///
/// When the format doesn't display the variant, layouts sharing a short name
/// get a numeric suffix so they stay distinguishable.
fn build_layouts_map(
    all: &[Layout],
    used: &[String],
    is_variant_displayed: bool,
    displayed_short: bool,
) -> HashMap<String, Layout> {
    let mut map = HashMap::new();
    let mut found_by_short: HashMap<String, usize> = HashMap::new();

    for layout in all {
        if !used.iter().any(|name| name == &layout.description) {
            continue;
        }
        if !is_variant_displayed {
            *found_by_short.entry(layout.short_name.clone()).or_insert(0) += 1;
        }
        map.insert(layout.description.clone(), layout.clone());
    }

    if is_variant_displayed || found_by_short.is_empty() {
        return map;
    }

    let mut counters: HashMap<String, u32> = HashMap::new();
    for used_name in used {
        let Some(layout) = map.get(used_name) else {
            continue;
        };
        let short = layout.short_name.clone();
        if found_by_short.get(&short).copied().unwrap_or(0) < 2 {
            continue;
        }
        let number = counters.entry(short.clone()).or_insert(1);
        if displayed_short {
            let mut suffixed = layout.clone();
            suffixed.short_name = format!("{short}{number}");
            suffixed.short_description = format!("{}{number}", layout.short_description);
            *number += 1;
            map.insert(used_name.clone(), suffixed);
        }
    }

    map
}

fn state_for(map: &HashMap<String, Layout>, key: &str) -> Option<LanguageState> {
    let layout = map.get(key)?;
    Some(LanguageState {
        short: layout.short_name.clone(),
        short_description: layout.short_description.clone(),
        long: layout.description.clone(),
        variant: layout.variant.clone(),
        flag: country_flag(&layout.short_name),
        layout_count: map.len(),
    })
}

/// Build the two-letter regional-indicator emoji for a two-letter layout code.
fn country_flag(short: &str) -> String {
    if short.chars().count() != 2 {
        return String::new();
    }
    let bytes = short.as_bytes();
    let mut result = [0xf0, 0x9f, 0x87, 0x00, 0xf0, 0x9f, 0x87, 0x00];
    result[3] = bytes[0].wrapping_add(0x45);
    result[7] = bytes[1].wrapping_add(0x45);
    if !(0xa6..=0xbf).contains(&result[3]) || !(0xa6..=0xbf).contains(&result[7]) {
        return String::new();
    }
    String::from_utf8(result.to_vec()).unwrap_or_default()
}

fn update(
    inner: &LabelModule,
    config: &Value,
    applied: &Rc<RefCell<String>>,
    state: Option<LanguageState>,
) {
    let hide_single = config
        .get("hide-single-layout")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let Some(state) = state else {
        inner.set_visible(false);
        return;
    };
    if hide_single && state.layout_count <= 1 {
        inner.set_visible(false);
        return;
    }

    // Swap the short-name class on the label.
    {
        let mut applied = applied.borrow_mut();
        if *applied != state.short {
            if !applied.is_empty() {
                inner.set_class(&applied, false);
            }
            if !state.short.is_empty() {
                inner.set_class(&state.short, true);
            }
            *applied = state.short.clone();
        }
    }

    let mut args = Args::new();
    fmt::insert_default(&mut args, state.short.clone());
    args.insert("short".into(), state.short.clone().into());
    args.insert(
        "shortDescription".into(),
        state.short_description.clone().into(),
    );
    args.insert("long".into(), state.long.clone().into());
    args.insert("variant".into(), state.variant.clone().into());
    args.insert("flag".into(), state.flag.clone().into());

    let format = inner.resolve_format("{}", "");
    let display = inner.render(&format, &args);
    tracing::debug!("sway/language: {} ({})", state.short, state.long);
    inner.set_markup(display.trim());

    let tooltip_format = config
        .get("tooltip-format")
        .and_then(|v| v.as_str())
        .unwrap_or(&format);
    inner.set_tooltip(inner.render(tooltip_format, &args).trim());

    inner.set_visible(true);
}

pub fn register(reg: &mut Registry) {
    reg.register("sway/language", |init: &ModuleInit| {
        Ok(Box::new(Language {
            inner: LabelModule::new("language", &init.id, &init.config).with_context(&init.name),
        }))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(short: &str, description: &str, brief: &str) -> Layout {
        Layout {
            short_name: short.to_string(),
            description: description.to_string(),
            variant: String::new(),
            short_description: brief.to_string(),
        }
    }

    #[test]
    fn maps_used_layouts_only() {
        let all = vec![
            layout("us", "English (US)", "en"),
            layout("ru", "Russian", "ru"),
            layout("de", "German", "de"),
        ];
        let used = vec!["English (US)".to_string(), "Russian".to_string()];
        let map = build_layouts_map(&all, &used, false, false);
        assert_eq!(map.len(), 2);
        assert_eq!(map["Russian"].short_name, "ru");
        assert!(!map.contains_key("German"));
    }

    #[test]
    fn suffixes_duplicate_short_names() {
        let all = vec![
            layout("us", "English (US)", "en"),
            layout("us", "English (US, intl)", "en"),
        ];
        let used = vec![
            "English (US)".to_string(),
            "English (US, intl)".to_string(),
        ];
        let map = build_layouts_map(&all, &used, false, true);
        assert_eq!(map["English (US)"].short_name, "us1");
        assert_eq!(map["English (US, intl)"].short_name, "us2");
    }

    #[test]
    fn no_suffix_without_displayed_short() {
        let all = vec![
            layout("us", "English (US)", "en"),
            layout("us", "English (US, intl)", "en"),
        ];
        let used = vec![
            "English (US)".to_string(),
            "English (US, intl)".to_string(),
        ];
        let map = build_layouts_map(&all, &used, false, false);
        assert_eq!(map["English (US)"].short_name, "us");
        assert_eq!(map["English (US, intl)"].short_name, "us");
    }

    #[test]
    fn flags_from_two_letter_codes() {
        assert_eq!(country_flag("us"), "🇺🇸");
        assert_eq!(country_flag("ru"), "🇷🇺");
        assert_eq!(country_flag("eng"), "");
        assert_eq!(country_flag("1"), "");
    }
}
