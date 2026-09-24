//! Clock module.
//!
//! Supports waybar's chrono-style placeholders, e.g. `{:%H:%M}` renders the
//! current time using the given `strftime` pattern.
//!
//! Contract (see `AGENTS.md`):
//! - `format`/`format-alt`/`tooltip-format` are Pango markup; literal `&` or
//!   `<` must be escaped by the user.
//! - an invalid format/timezone is reported with a short error text in the
//!   module itself plus a `warn!`, rather than silently falling back.
//! - the refresh interval defaults to 1 s when a *label* format shows seconds
//!   and to 60 s otherwise; an explicit `interval` always wins. The tooltip is
//!   rendered on demand (see below), so it does not affect the interval.
//! - ticks land on wall-clock boundaries (whole seconds/minutes), not on a
//!   monotonic phase.
//! - the tooltip is only built when it is about to be shown, so it stays fresh
//!   even when the label refreshes once a minute.
//! - `{:%Z}` yields the zone abbreviation for a named `timezone` (`MSK`, `CET`)
//!   and the numeric offset for the local zone (a `chrono::Local` limitation).

use std::cell::{Cell, RefCell};
use std::fmt::{Display, Write as _};
use std::rc::Rc;

use chrono::format::{Fixed, Item, Numeric, StrftimeItems};
use chrono::{DateTime, Local, Locale, TimeZone, Utc};
use chrono_tz::Tz;
use gtk::prelude::*;
use serde_json::Value;
use tracing::warn;

use waybar_core::timer::{self, TimerHandle};
use waybar_core::widgets::{has_left_click_command, tooltip_enabled};
use waybar_core::{Module, ModuleInit, Registry};

use crate::label::LabelModule;

struct Clock {
    inner: LabelModule,
    cfg: Result<Rc<ClockConfig>, ClockError>,
    alt: Rc<Cell<bool>>,
    cache: Rc<RefCell<Cache>>,
    timer: RefCell<Option<TimerHandle>>,
}

impl Module for Clock {
    fn widget(&self) -> gtk::Widget {
        self.inner.widget()
    }

    fn start(&self) {
        let cfg = match &self.cfg {
            Ok(cfg) => cfg,
            Err(err) => {
                render_error(self.inner.label(), err, &self.cache);
                return;
            }
        };

        render(self.inner.label(), cfg, self.alt.get(), &self.cache);

        // Render the tooltip only when it is about to be shown, so it does not
        // have to be refreshed on every label tick (and the label interval can
        // stay at a minute even if the tooltip shows seconds).
        if let Some(tooltip) = self.inner.tooltip().cloned() {
            let cfg = cfg.clone();
            let cache = self.cache.clone();
            tooltip.set_provider(move || render_tooltip(&cfg, &cache));
        }

        // Left click toggles between `format` and `format-alt`, unless the user
        // configured a left-click command (`on-click`/`on-double-click`/
        // `on-triple-click`) — then the command wins and the toggle is not
        // installed, so a single click does not both run the command and flip
        // the format.
        if cfg.alt.is_some() && !has_left_click_command(self.inner.config()) {
            // The label is held weakly: a strong reference would form a cycle
            // (widget -> controller -> closure -> widget) that leaks the module
            // when the bar is destroyed on monitor hotplug.
            let label = self.inner.label().downgrade();
            let cfg = cfg.clone();
            let alt = self.alt.clone();
            let cache = self.cache.clone();
            let gesture = gtk::GestureClick::new();
            gesture.set_button(1);
            gesture.connect_pressed(move |_, n_press, _, _| {
                if n_press != 1 {
                    return;
                }
                let Some(label) = label.upgrade() else {
                    return;
                };
                alt.set(!alt.get());
                render(&label, &cfg, alt.get(), &cache);
            });
            self.inner.widget().add_controller(gesture);
        }

        let default_interval = if cfg.has_seconds { 1 } else { 60 };
        let Some(interval) = self.inner.interval_secs(default_interval) else {
            return; // `interval: "once"` — one render is enough.
        };

        // A strong label reference is fine here: `TimerHandle::drop` removes the
        // GLib source (and with it this closure) when the bar goes away.
        let label = self.inner.label().clone();
        let cfg = cfg.clone();
        let alt = self.alt.clone();
        let cache = self.cache.clone();
        let handle = timer::schedule_aligned(interval, move || {
            render(&label, &cfg, alt.get(), &cache);
        });
        *self.timer.borrow_mut() = Some(handle);
    }
}

/// Everything parsed from the module config, validated once at construction.
struct ClockConfig {
    primary: ParsedTemplate,
    /// `None` when `format-alt` is not configured.
    alt: Option<ParsedTemplate>,
    /// `None` when tooltips are disabled.
    tooltip: Option<ParsedTemplate>,
    locale: Option<Locale>,
    tz: TzSpec,
    /// Whether a label format produces seconds (drives the default interval).
    has_seconds: bool,
}

#[derive(Clone, Copy)]
enum TzSpec {
    Local,
    Named(Tz),
}

type ParsedTemplate = Result<Template, ClockError>;

impl ClockConfig {
    fn parse(config: &Value) -> Result<Rc<Self>, ClockError> {
        let locale = parse_locale(config.get("locale").and_then(Value::as_str));
        let tz = parse_tz(config.get("timezone").and_then(Value::as_str))?;

        let primary = Template::parse(
            config
                .get("format")
                .and_then(Value::as_str)
                .unwrap_or("{:%H:%M}"),
            locale,
        );
        let alt = config
            .get("format-alt")
            .and_then(Value::as_str)
            .map(|s| Template::parse(s, locale));
        let tooltip = if tooltip_enabled(config) {
            Some(
                config
                    .get("tooltip-format")
                    .and_then(Value::as_str)
                    .map(|s| Template::parse(s, locale))
                    .unwrap_or_else(|| primary.clone()),
            )
        } else {
            None
        };

        // Only label formats drive the interval; the tooltip is rendered on
        // demand and is always current when shown.
        let has_seconds = primary.as_ref().is_ok_and(|t| t.has_seconds)
            || alt
                .as_ref()
                .and_then(|r| r.as_ref().ok())
                .is_some_and(|t| t.has_seconds);

        Ok(Rc::new(ClockConfig {
            primary,
            alt,
            tooltip,
            locale,
            tz,
            has_seconds,
        }))
    }
}

/// The current instant, keeping the timezone type so `%Z` can yield an
/// abbreviation for named zones.
enum ClockTime {
    Local(DateTime<Local>),
    Named(DateTime<Tz>),
}

/// A clock format string parsed into literal text and validated strftime
/// patterns, so validation happens once at construction rather than every tick.
#[derive(Clone)]
struct Template {
    segments: Vec<Segment>,
    has_seconds: bool,
}

#[derive(Clone)]
enum Segment {
    Literal(String),
    /// An already-validated strftime pattern, pre-parsed into owned items.
    Time(Vec<Item<'static>>),
}

impl Template {
    /// Expand `{:<strftime>}` placeholders in a waybar clock format string.
    fn parse(template: &str, locale: Option<Locale>) -> ParsedTemplate {
        let mut segments = Vec::new();
        let mut has_seconds = false;
        let mut rest = template;
        while let Some(start) = rest.find("{:") {
            if start > 0 {
                segments.push(Segment::Literal(rest[..start].to_string()));
            }
            let after = &rest[start + 2..];
            let Some(end) = after.find('}') else {
                return Err(ClockError::Unterminated);
            };
            let pattern = &after[..end];
            let items = parse_items(pattern, locale)?;
            has_seconds |= items_have_seconds(&items);
            segments.push(Segment::Time(items));
            rest = &after[end + 1..];
        }
        if !rest.is_empty() {
            segments.push(Segment::Literal(rest.to_string()));
        }
        Ok(Template {
            segments,
            has_seconds,
        })
    }

    fn render(&self, now: &ClockTime, locale: Option<Locale>) -> String {
        match now {
            ClockTime::Local(dt) => self.render_dt(dt, locale),
            ClockTime::Named(dt) => self.render_dt(dt, locale),
        }
    }

    fn render_dt<Tz: TimeZone>(&self, now: &DateTime<Tz>, locale: Option<Locale>) -> String
    where
        Tz::Offset: Display,
    {
        let mut out = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Literal(text) => out.push_str(text),
                Segment::Time(items) => match locale {
                    Some(locale) => {
                        let _ = write!(
                            out,
                            "{}",
                            now.format_localized_with_items(items.iter(), locale)
                        );
                    }
                    None => {
                        let _ = write!(out, "{}", now.format_with_items(items.iter()));
                    }
                },
            }
        }
        out
    }
}

/// Parse and validate a strftime pattern once, into owned format items.
///
/// `chrono`'s formatter panics on unknown specifiers (e.g. `%OI`, `%Od`) when
/// the result is rendered. This runs inside GTK callbacks, so an unguarded
/// panic would abort the whole bar; instead the format is rejected and the
/// module shows a short error text. Pre-parsing also avoids re-tokenising the
/// pattern on every tick.
fn parse_items(pattern: &str, locale: Option<Locale>) -> Result<Vec<Item<'static>>, ClockError> {
    let items = match locale {
        Some(locale) => StrftimeItems::new_with_locale(pattern, locale).parse_to_owned(),
        None => StrftimeItems::new(pattern).parse_to_owned(),
    };
    items.map_err(|_| ClockError::InvalidSpecifier(pattern.to_string()))
}

/// Whether pre-parsed strftime items can output seconds.
fn items_have_seconds(items: &[Item<'_>]) -> bool {
    items.iter().any(|item| match item {
        Item::Numeric(Numeric::Second | Numeric::Nanosecond | Numeric::Timestamp, _) => true,
        Item::Fixed(
            Fixed::Nanosecond
            | Fixed::Nanosecond3
            | Fixed::Nanosecond6
            | Fixed::Nanosecond9
            | Fixed::RFC3339
            | Fixed::RFC2822
            // `%3f`/`%6f`/`%9f` are the internal no-dot nanosecond forms. The
            // only other internal fixed item is the permissive `%#z` offset;
            // treating it as seconds-bearing merely refreshes once a second.
            | Fixed::Internal(_),
        ) => true,
        _ => false,
    })
}

/// Parse the `locale` option into a chrono locale.
///
/// A POSIX locale string such as `ru_RU.UTF-8` is normalised to the chrono
/// variant name (`ru_RU`); `C` maps to `POSIX`. An unknown locale is not fatal:
/// it is logged and the system default (English) is used.
fn parse_locale(raw: Option<&str>) -> Option<Locale> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let normalized = normalize_locale(raw);
    match normalized.parse::<Locale>() {
        Ok(locale) => Some(locale),
        Err(_) => {
            warn!("clock: unknown locale `{raw}`; using the default locale");
            None
        }
    }
}

/// Normalise a POSIX locale string (`lang[_territory][.codeset][@modifier]`) to
/// the chrono variant name: strip the codeset, turn `-` into `_`, map `C` to
/// `POSIX`, and append the modifier with `_` (`sr_RS@latin` → `sr_RS_latin`).
fn normalize_locale(raw: &str) -> String {
    let (base, modifier) = match raw.split_once('@') {
        Some((base, modifier)) => (base, Some(modifier)),
        None => (raw, None),
    };
    let base = base.split('.').next().unwrap_or(base);
    if base == "C" || base == "POSIX" {
        return "POSIX".to_string();
    }
    let base = base.replace('-', "_");
    match modifier {
        Some(modifier) if !modifier.is_empty() => format!("{base}_{modifier}"),
        _ => base,
    }
}

/// Parse the `timezone` option: empty/`local` means the system timezone.
fn parse_tz(raw: Option<&str>) -> Result<TzSpec, ClockError> {
    let Some(raw) = raw else {
        return Ok(TzSpec::Local);
    };
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("local") {
        return Ok(TzSpec::Local);
    }
    raw.parse::<Tz>()
        .map(TzSpec::Named)
        .map_err(|_| ClockError::UnknownTimezone(raw.to_string()))
}

fn now_in(tz: TzSpec) -> ClockTime {
    match tz {
        TzSpec::Local => ClockTime::Local(Local::now()),
        TzSpec::Named(tz) => ClockTime::Named(Utc::now().with_timezone(&tz)),
    }
}

/// Per-module render cache: avoids redundant relayouts and repeated warnings.
#[derive(Default)]
struct Cache {
    label: String,
    /// Last warning emitted for the label and the tooltip, so an identical
    /// warning is not repeated every tick while a *different* one still is.
    last_format_warn: Option<String>,
    last_tooltip_warn: Option<String>,
}

fn render(label: &gtk::Label, cfg: &ClockConfig, alt: bool, cache: &RefCell<Cache>) {
    let now = now_in(cfg.tz);

    let template = match (alt, &cfg.alt) {
        (true, Some(alt)) => alt,
        _ => &cfg.primary,
    };
    let text = match template {
        Ok(template) => template.render(&now, cfg.locale),
        Err(err) => {
            warn_format(&mut cache.borrow_mut(), err);
            err.label().to_string()
        }
    };
    set_label(label, &text, cache);
}

/// Build the tooltip markup on demand (called from the `query-tooltip`
/// handler). Returns an empty string to show no tooltip.
fn render_tooltip(cfg: &ClockConfig, cache: &RefCell<Cache>) -> String {
    let Some(template) = &cfg.tooltip else {
        return String::new();
    };
    match template {
        Ok(template) => {
            let text = template.render(&now_in(cfg.tz), cfg.locale);
            if check_markup(&text).is_ok() {
                text
            } else {
                warn_tooltip(&mut cache.borrow_mut(), &ClockError::Markup);
                String::new()
            }
        }
        Err(err) => {
            warn_tooltip(&mut cache.borrow_mut(), err);
            String::new()
        }
    }
}

/// Render a fatal configuration error (unknown timezone) in the module.
fn render_error(label: &gtk::Label, err: &ClockError, cache: &RefCell<Cache>) {
    warn_format(&mut cache.borrow_mut(), err);
    set_label(label, &err.label(), cache);
}

/// Set the label, applying Pango markup only when the text actually changed.
fn set_label(label: &gtk::Label, text: &str, cache: &RefCell<Cache>) {
    let mut cache = cache.borrow_mut();
    if cache.label == text {
        return;
    }
    if apply_markup(label, text) {
        cache.label = text.to_string();
        return;
    }
    warn_format(&mut cache, &ClockError::Markup);
    let err = ClockError::Markup.label();
    if cache.label != err {
        apply_markup(label, &err);
        cache.label = err;
    }
}

fn warn_format(cache: &mut Cache, err: &ClockError) {
    let message = err.to_string();
    if cache.last_format_warn.as_deref() != Some(message.as_str()) {
        cache.last_format_warn = Some(message);
        warn!("clock: {err}");
    }
}

fn warn_tooltip(cache: &mut Cache, err: &ClockError) {
    let message = err.to_string();
    if cache.last_tooltip_warn.as_deref() != Some(message.as_str()) {
        cache.last_tooltip_warn = Some(message);
        warn!("clock: tooltip-format: {err}");
    }
}

/// Parse Pango markup once and apply the resulting text and attributes.
///
/// `gtk::Label::set_markup` would parse the same markup a second time; feeding
/// it the already-parsed attribute list avoids that. Returns `false` when the
/// markup is invalid, so the caller can substitute the error text instead of
/// letting GTK log a warning and drop the text.
fn apply_markup(label: &gtk::Label, markup: &str) -> bool {
    match gtk::pango::parse_markup(markup, '\0') {
        Ok((attrs, text, _)) => {
            label.set_text(&text);
            label.set_attributes(Some(&attrs));
            true
        }
        Err(_) => false,
    }
}

/// Reject markup Pango cannot parse; GTK would otherwise log and drop the text.
fn check_markup(text: &str) -> Result<(), ()> {
    gtk::pango::parse_markup(text, '\0')
        .map(|_| ())
        .map_err(|_| ())
}

#[derive(Debug, Clone)]
enum ClockError {
    Unterminated,
    InvalidSpecifier(String),
    UnknownTimezone(String),
    Markup,
}

impl ClockError {
    /// The short text shown in the module itself.
    fn label(&self) -> String {
        match self {
            ClockError::UnknownTimezone(_) => "clock: unknown timezone".to_string(),
            ClockError::Markup => "clock: invalid markup".to_string(),
            _ => "clock: invalid format".to_string(),
        }
    }
}

impl std::fmt::Display for ClockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClockError::Unterminated => write!(f, "unterminated `{{:` placeholder"),
            ClockError::InvalidSpecifier(pattern) => {
                write!(f, "invalid strftime specifier in `{{:{pattern}}}`")
            }
            ClockError::UnknownTimezone(tz) => write!(f, "unknown timezone `{tz}`"),
            ClockError::Markup => write!(f, "invalid Pango markup"),
        }
    }
}

pub fn register(reg: &mut Registry) {
    reg.register("clock", |init: &ModuleInit| {
        Ok(Box::new(Clock {
            inner: LabelModule::new("clock", &init.id, &init.config),
            cfg: ClockConfig::parse(&init.config),
            alt: Rc::new(Cell::new(false)),
            cache: Rc::new(RefCell::new(Cache::default())),
            timer: RefCell::new(None),
        }))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone};
    use serde_json::json;

    fn fixed_now() -> DateTime<FixedOffset> {
        FixedOffset::east_opt(3 * 3600)
            .unwrap()
            .with_ymd_and_hms(2024, 3, 5, 14, 7, 9)
            .unwrap()
    }

    #[test]
    fn expands_strftime_placeholders() {
        let now = fixed_now();
        assert_eq!(
            Template::parse("{:%H}", None)
                .unwrap()
                .render_dt(&now, None),
            "14"
        );
        assert_eq!(
            Template::parse("it is {:%H} now", None)
                .unwrap()
                .render_dt(&now, None),
            "it is 14 now"
        );
        assert_eq!(
            Template::parse("{:%H}:{:%M}:{:%S}", None)
                .unwrap()
                .render_dt(&now, None),
            "14:07:09"
        );
    }

    #[test]
    fn invalid_specifier_is_rejected() {
        assert!(matches!(
            Template::parse("{:%OI}", None),
            Err(ClockError::InvalidSpecifier(_))
        ));
        assert!(matches!(
            Template::parse("{:%Q}", None),
            Err(ClockError::InvalidSpecifier(_))
        ));
    }

    #[test]
    fn unterminated_placeholder_is_rejected() {
        assert!(matches!(
            Template::parse("{:%H", None),
            Err(ClockError::Unterminated)
        ));
    }

    #[test]
    fn detects_seconds_specifiers() {
        let has = |pattern: &str| items_have_seconds(&parse_items(pattern, None).unwrap());
        assert!(!has("%H:%M"));
        assert!(has("%H:%M:%S"));
        assert!(has("%T"));
        assert!(has("%3f"));
        assert!(has("%s"));
        // `%+` (RFC 3339) contains seconds; the old hand-written scanner missed it.
        assert!(has("%+"));
        // `%%S` is a literal percent followed by `S`, not a specifier.
        assert!(!has("%%S"));
        // Locale time formats (`%X`, `%c`) expand to items that include seconds.
        assert!(items_have_seconds(
            &parse_items("%X", Some(Locale::en_US)).unwrap()
        ));
        // `%OI` is rejected outright, not merely "not seconds".
        assert!(parse_items("%OI", None).is_err());
        assert!(!Template::parse("{:%H:%M}", None).unwrap().has_seconds);
        assert!(Template::parse("{:%H:%M:%S}", None).unwrap().has_seconds);
    }

    #[test]
    fn normalizes_posix_locales() {
        assert_eq!(normalize_locale("ru_RU.UTF-8"), "ru_RU");
        assert_eq!(normalize_locale("ru_RU"), "ru_RU");
        assert_eq!(normalize_locale("C"), "POSIX");
        assert_eq!(normalize_locale("C.UTF-8"), "POSIX");
        assert_eq!(normalize_locale("sr_RS@latin"), "sr_RS_latin");
        // The modifier survives the codeset, and `-` is normalised to `_`.
        assert_eq!(normalize_locale("eu_ES.UTF-8@euro"), "eu_ES_euro");
        assert_eq!(normalize_locale("ru-RU"), "ru_RU");
    }

    #[test]
    fn parses_timezones() {
        assert!(matches!(parse_tz(None), Ok(TzSpec::Local)));
        assert!(matches!(parse_tz(Some("")), Ok(TzSpec::Local)));
        assert!(matches!(parse_tz(Some("local")), Ok(TzSpec::Local)));
        assert!(matches!(
            parse_tz(Some("Europe/Moscow")),
            Ok(TzSpec::Named(_))
        ));
        assert!(matches!(
            parse_tz(Some("Mars/Olympus")),
            Err(ClockError::UnknownTimezone(_))
        ));
    }

    #[test]
    fn named_timezone_yields_abbreviation() {
        let tz = "Europe/Moscow".parse::<Tz>().unwrap();
        let now = now_in(TzSpec::Named(tz));
        let template = Template::parse("{:%Z}", None).unwrap();
        assert_eq!(template.render(&now, None), "MSK");
    }

    #[test]
    fn locale_changes_month_names() {
        let now = fixed_now();
        let template = Template::parse("{:%B}", None).unwrap();
        assert_eq!(template.render_dt(&now, None), "March");
        let ru = parse_locale(Some("ru_RU")).unwrap();
        let localized = template.render_dt(&now, Some(ru));
        assert!(
            localized.to_lowercase().contains("март"),
            "expected a Russian month name, got `{localized}`"
        );
    }

    #[test]
    fn markup_is_validated() {
        assert!(check_markup("<b>hi</b>").is_ok());
        assert!(check_markup("a & b").is_err());
    }

    #[test]
    fn warnings_are_deduplicated_per_message() {
        let mut cache = Cache::default();
        warn_format(&mut cache, &ClockError::Unterminated);
        assert_eq!(
            cache.last_format_warn.as_deref(),
            Some("unterminated `{:` placeholder")
        );
        // A different error still updates the state (and would be logged).
        warn_format(&mut cache, &ClockError::InvalidSpecifier("%Q".to_string()));
        assert_eq!(
            cache.last_format_warn.as_deref(),
            Some("invalid strftime specifier in `{:%Q}`")
        );
    }

    #[test]
    fn tooltip_is_rendered_on_demand() {
        let cfg = ClockConfig::parse(&json!({
            "format": "{:%H:%M}",
            "tooltip-format": "now {:%H:%M:%S}",
        }))
        .unwrap();
        let cache = RefCell::new(Cache::default());
        let text = render_tooltip(&cfg, &cache);
        assert!(text.starts_with("now "), "got `{text}`");
        assert_eq!(text.len(), "now HH:MM:SS".len());

        // Invalid markup shows no tooltip instead of leaking GTK warnings.
        let cfg = ClockConfig::parse(&json!({
            "format": "{:%H:%M}",
            "tooltip-format": "a & b",
        }))
        .unwrap();
        assert_eq!(render_tooltip(&cfg, &RefCell::new(Cache::default())), "");
    }

    #[test]
    fn config_defaults_and_derived_interval() {
        let cfg = ClockConfig::parse(&json!({})).unwrap();
        assert!(!cfg.has_seconds); // default `{:%H:%M}`
        assert!(cfg.alt.is_none());
        assert!(cfg.tooltip.is_some());

        let cfg = ClockConfig::parse(&json!({
            "format": "{:%H:%M:%S}",
            "format-alt": "{:%H}",
            "tooltip": false,
        }))
        .unwrap();
        assert!(cfg.has_seconds);
        assert!(cfg.alt.is_some());
        assert!(cfg.tooltip.is_none());

        // A tooltip with seconds is rendered on demand and must not force the
        // label to refresh every second.
        let cfg = ClockConfig::parse(&json!({
            "format": "{:%H:%M}",
            "tooltip-format": "{:%H:%M:%S}",
        }))
        .unwrap();
        assert!(!cfg.has_seconds);
        assert!(cfg.tooltip.is_some());
    }
}
