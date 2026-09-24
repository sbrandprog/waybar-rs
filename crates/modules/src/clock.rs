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
//! - the refresh interval defaults to 1 s when a format shows seconds and to
//!   60 s otherwise; an explicit `interval` always wins.
//! - ticks land on wall-clock boundaries (whole seconds/minutes), not on a
//!   monotonic phase.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use chrono::{DateTime, FixedOffset, Local, Locale, Utc};
use chrono_tz::Tz;
use gtk::prelude::*;
use serde_json::Value;
use tracing::warn;

use waybar_core::timer::{self, TimerHandle};
use waybar_core::widgets::tooltip_enabled;
use waybar_core::{Module, ModuleInit, Registry};

use crate::label::LabelModule;

/// strftime specifiers whose output contains seconds; their presence forces a
/// one-second refresh. `%X`/`%c`/`%r` are locale-dependent but commonly carry
/// seconds, so they are treated conservatively.
const SECONDS_SPECIFIERS: &[char] = &['S', 'T', 'X', 'r', 'c', 's', 'f'];

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
                render_error(&self.inner, err, &self.cache);
                return;
            }
        };

        render(&self.inner, cfg, self.alt.get(), &self.cache);

        // Left click toggles between `format` and `format-alt`; `attach_actions`
        // still runs any configured `on-click*` command independently (same
        // pattern as `idle_inhibitor`).
        if cfg.alt.is_some() {
            let inner = self.inner.clone();
            let cfg = cfg.clone();
            let alt = self.alt.clone();
            let cache = self.cache.clone();
            let gesture = gtk::GestureClick::new();
            gesture.set_button(1);
            gesture.connect_pressed(move |_, n_press, _, _| {
                if n_press != 1 {
                    return;
                }
                alt.set(!alt.get());
                render(&inner, &cfg, alt.get(), &cache);
            });
            self.inner.widget().add_controller(gesture);
        }

        let default_interval = if cfg.has_seconds { 1 } else { 60 };
        let Some(interval) = self.inner.interval_secs(default_interval) else {
            return; // `interval: "once"` — one render is enough.
        };

        let inner = self.inner.clone();
        let cfg = cfg.clone();
        let alt = self.alt.clone();
        let cache = self.cache.clone();
        let handle = timer::schedule_aligned(interval, move || {
            render(&inner, &cfg, alt.get(), &cache);
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
    /// Whether any format produces seconds (drives the default interval).
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

        let has_seconds = primary.as_ref().is_ok_and(|t| t.has_seconds)
            || alt
                .as_ref()
                .and_then(|r| r.as_ref().ok())
                .is_some_and(|t| t.has_seconds)
            || tooltip
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
    /// An already-validated strftime pattern.
    Time(String),
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
            validate_spec(pattern, locale)?;
            has_seconds |= pattern_has_seconds(pattern);
            segments.push(Segment::Time(pattern.to_string()));
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

    fn render(&self, now: &DateTime<FixedOffset>, locale: Option<Locale>) -> String {
        let mut out = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Literal(text) => out.push_str(text),
                Segment::Time(pattern) => match locale {
                    Some(locale) => {
                        out.push_str(&now.format_localized(pattern, locale).to_string())
                    }
                    None => out.push_str(&now.format(pattern).to_string()),
                },
            }
        }
        out
    }
}

/// Validate `pattern` with chrono.
///
/// `chrono`'s formatter panics on unknown specifiers (e.g. `%OI`, `%Od`) when
/// the result is rendered. This runs inside GTK callbacks, so an unguarded
/// panic would abort the whole bar; instead the format is rejected and the
/// module shows a short error text.
fn validate_spec(pattern: &str, locale: Option<Locale>) -> Result<(), ClockError> {
    let items = match locale {
        Some(locale) => chrono::format::StrftimeItems::new_with_locale(pattern, locale),
        None => chrono::format::StrftimeItems::new(pattern),
    };
    if items
        .into_iter()
        .any(|item| matches!(item, chrono::format::Item::Error))
    {
        Err(ClockError::InvalidSpecifier(pattern.to_string()))
    } else {
        Ok(())
    }
}

/// Whether a validated strftime pattern can output seconds.
fn pattern_has_seconds(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            i += 1;
            continue;
        }
        i += 1;
        // Flags and modifiers (`%-H`, `%_d`, `%0e`, `%:z`, `%OI`, ...).
        while i < bytes.len() && matches!(bytes[i], b'-' | b'_' | b'0' | b':' | b'O' | b'#' | b'^')
        {
            i += 1;
        }
        // Width, e.g. `%3f`.
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if SECONDS_SPECIFIERS.contains(&(bytes[i] as char)) {
            return true;
        }
        i += 1;
    }
    false
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

fn normalize_locale(raw: &str) -> String {
    let base = raw.split('.').next().unwrap_or(raw);
    if base == "C" {
        return "POSIX".to_string();
    }
    match base.split_once('@') {
        Some((lang, modifier)) => format!("{lang}_{modifier}"),
        None => base.to_string(),
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

fn now_in(tz: TzSpec) -> DateTime<FixedOffset> {
    match tz {
        TzSpec::Local => Local::now().fixed_offset(),
        TzSpec::Named(tz) => Utc::now().with_timezone(&tz).fixed_offset(),
    }
}

/// Per-module render cache: avoids redundant relayouts and repeated warnings.
#[derive(Default)]
struct Cache {
    label: String,
    tooltip: String,
    warned_format: bool,
    warned_tooltip: bool,
}

fn render(inner: &LabelModule, cfg: &ClockConfig, alt: bool, cache: &RefCell<Cache>) {
    let now = now_in(cfg.tz);

    let template = match (alt, &cfg.alt) {
        (true, Some(alt)) => alt,
        _ => &cfg.primary,
    };
    let text = match template {
        Ok(template) => template.render(&now, cfg.locale),
        Err(err) => {
            warn_format(cache, err);
            err.label().to_string()
        }
    };
    let text = if check_markup(&text).is_err() {
        warn_format(cache, &ClockError::Markup);
        ClockError::Markup.label().to_string()
    } else {
        text
    };
    set_label(inner, &text, cache);

    let Some(tooltip) = &cfg.tooltip else {
        return;
    };
    match tooltip {
        Ok(template) => {
            let text = template.render(&now, cfg.locale);
            if check_markup(&text).is_ok() {
                set_tooltip(inner, &text, cache);
            } else {
                warn_tooltip(cache, &ClockError::Markup);
            }
        }
        Err(err) => warn_tooltip(cache, err),
    }
}

/// Render a fatal configuration error (unknown timezone) in the module.
fn render_error(inner: &LabelModule, err: &ClockError, cache: &RefCell<Cache>) {
    warn_format(cache, err);
    set_label(inner, &err.label(), cache);
}

fn set_label(inner: &LabelModule, text: &str, cache: &RefCell<Cache>) {
    let mut cache = cache.borrow_mut();
    if cache.label != text {
        inner.set_markup(text);
        cache.label = text.to_string();
    }
}

fn set_tooltip(inner: &LabelModule, text: &str, cache: &RefCell<Cache>) {
    let mut cache = cache.borrow_mut();
    if cache.tooltip != text {
        inner.set_tooltip(text);
        cache.tooltip = text.to_string();
    }
}

fn warn_format(cache: &RefCell<Cache>, err: &ClockError) {
    let mut cache = cache.borrow_mut();
    if !cache.warned_format {
        cache.warned_format = true;
        warn!("clock: {err}");
    }
}

fn warn_tooltip(cache: &RefCell<Cache>, err: &ClockError) {
    let mut cache = cache.borrow_mut();
    if !cache.warned_tooltip {
        cache.warned_tooltip = true;
        warn!("clock: tooltip-format: {err}");
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
    use chrono::TimeZone;
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
            Template::parse("{:%H}", None).unwrap().render(&now, None),
            "14"
        );
        assert_eq!(
            Template::parse("it is {:%H} now", None)
                .unwrap()
                .render(&now, None),
            "it is 14 now"
        );
        assert_eq!(
            Template::parse("{:%H}:{:%M}:{:%S}", None)
                .unwrap()
                .render(&now, None),
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
        assert!(!pattern_has_seconds("%H:%M"));
        assert!(pattern_has_seconds("%H:%M:%S"));
        assert!(pattern_has_seconds("%T"));
        assert!(pattern_has_seconds("%3f"));
        // `%%S` is a literal percent followed by `S`, not a specifier.
        assert!(!pattern_has_seconds("%%S"));
        // `%OI` is not a seconds specifier.
        assert!(!pattern_has_seconds("%OI"));
        assert!(!Template::parse("{:%H:%M}", None).unwrap().has_seconds);
        assert!(Template::parse("{:%H:%M:%S}", None).unwrap().has_seconds);
    }

    #[test]
    fn normalizes_posix_locales() {
        assert_eq!(normalize_locale("ru_RU.UTF-8"), "ru_RU");
        assert_eq!(normalize_locale("ru_RU"), "ru_RU");
        assert_eq!(normalize_locale("C"), "POSIX");
        assert_eq!(normalize_locale("sr_RS@latin"), "sr_RS_latin");
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
    fn locale_changes_month_names() {
        let now = fixed_now();
        let template = Template::parse("{:%B}", None).unwrap();
        assert_eq!(template.render(&now, None), "March");
        let ru = parse_locale(Some("ru_RU")).unwrap();
        let localized = template.render(&now, Some(ru));
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
    }
}
