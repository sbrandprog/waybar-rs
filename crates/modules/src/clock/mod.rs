use std::cell::{Cell, RefCell};
use std::fmt::{Display, Write as _};
use std::rc::Rc;

use chrono::format::{Fixed, Item, Numeric, StrftimeItems};
use chrono::{DateTime, Local, Locale, TimeZone, Utc};
use chrono_tz::Tz;
use gtk::prelude::*;
use serde_json::Value;
use tracing::warn;

use waybar_core::format::warn_unknown_placeholder;
use waybar_core::timer::{self, TimerHandle};
use waybar_core::widgets::{attach_single_click, has_left_click_command, tooltip_enabled};
use waybar_core::{Module, ModuleInit, Registry};

use crate::label::LabelModule;

mod calendar;

use calendar::{CalendarConfig, CalendarView};

struct Clock {
    inner: LabelModule,
    cfg: Result<Rc<ClockConfig>, ClockError>,
    alt: Rc<Cell<bool>>,
    cache: Rc<RefCell<Cache>>,
    timer: RefCell<Option<TimerHandle>>,
    calendar: Option<Rc<CalendarView>>,
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

        if let Some(tooltip) = self.inner.tooltip().cloned() {
            let cfg = cfg.clone();
            let cache = self.cache.clone();
            tooltip.set_provider(move || render_tooltip(&cfg, &cache));
        }

        if cfg.alt.is_some() && !has_left_click_command(self.inner.config()) {
            let label = self.inner.label().downgrade();
            let cfg = cfg.clone();
            let alt = self.alt.clone();
            let cache = self.cache.clone();
            attach_single_click(&self.inner.widget(), self.inner.config(), 1, move |_| {
                let Some(label) = label.upgrade() else {
                    return;
                };
                alt.set(!alt.get());
                render(&label, &cfg, alt.get(), &cache);
            });
        }

        if let Some(calendar) = &self.calendar {
            let toggle = calendar.clone();
            attach_single_click(&self.inner.widget(), self.inner.config(), 3, move |_| {
                toggle.toggle();
            });
        }

        let default_interval = if cfg.has_seconds { 1 } else { 60 };
        let Some(interval) = self.inner.interval_secs(default_interval) else {
            return;
        };

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

struct ClockConfig {
    primary: ParsedTemplate,
    alt: Option<ParsedTemplate>,
    tooltip: Option<ParsedTemplate>,
    locale: Option<Locale>,
    tz: TzSpec,
    has_seconds: bool,
    calendar: Option<Rc<CalendarConfig>>,
}

#[derive(Clone, Copy)]
enum TzSpec {
    Local,
    Named(Tz),
}

type ParsedTemplate = Result<Template, ClockError>;

impl ClockConfig {
    fn parse(config: &Value, ctx: &str) -> Result<Rc<Self>, ClockError> {
        let locale = parse_locale(config.get("locale").and_then(Value::as_str))?;
        let tz = parse_tz(config.get("timezone").and_then(Value::as_str))?;
        let calendar = CalendarConfig::parse(config.get("calendar"), tz, ctx);

        let primary_fmt = config
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("{:%H:%M}");
        let primary = Template::parse(primary_fmt, locale);
        warn_unknown_in(&primary, primary_fmt, ctx);
        let alt = config.get("format-alt").and_then(Value::as_str).map(|s| {
            let parsed = Template::parse(s, locale);
            warn_unknown_in(&parsed, s, ctx);
            parsed
        });
        let tooltip = if tooltip_enabled(config) {
            match config.get("tooltip-format").and_then(Value::as_str) {
                Some(s) => {
                    let parsed = Template::parse(s, locale);
                    warn_unknown_in(&parsed, s, ctx);
                    Some(parsed)
                }
                None if primary.is_ok() => Some(primary.clone()),
                None => None,
            }
        } else {
            None
        };

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
            calendar,
        }))
    }
}

enum ClockTime {
    Local(DateTime<Local>),
    Named(DateTime<Tz>),
}

#[derive(Clone)]
struct Template {
    segments: Vec<Segment>,
    has_seconds: bool,
    unknown: Vec<String>,
}

#[derive(Clone)]
enum Segment {
    Literal(String),
    Time(Vec<Item<'static>>),
}

impl Template {
    fn parse(template: &str, locale: Option<Locale>) -> ParsedTemplate {
        let mut segments = Vec::new();
        let mut has_seconds = false;
        let mut unknown = Vec::new();
        let mut rest = template;
        while let Some(open) = rest.find('{') {
            push_literal(&mut segments, &rest[..open]);
            let inner = &rest[open + 1..];
            if let Some(after_colon) = inner.strip_prefix(':') {
                let Some(end) = after_colon.find('}') else {
                    return Err(ClockError::Unterminated);
                };
                let items = parse_items(&after_colon[..end], locale)?;
                has_seconds |= items_have_seconds(&items);
                segments.push(Segment::Time(items));
                rest = &after_colon[end + 1..];
            } else if let Some(end) = inner
                .find('}')
                .filter(|&end| is_placeholder_token(&inner[..end]))
            {
                unknown.push(inner[..end].to_string());
                push_literal(&mut segments, &rest[open..open + end + 2]);
                rest = &inner[end + 1..];
            } else {
                push_literal(&mut segments, "{");
                rest = inner;
            }
        }
        push_literal(&mut segments, rest);
        Ok(Template {
            segments,
            has_seconds,
            unknown,
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

fn push_literal(segments: &mut Vec<Segment>, text: &str) {
    if text.is_empty() {
        return;
    }
    match segments.last_mut() {
        Some(Segment::Literal(last)) => last.push_str(text),
        _ => segments.push(Segment::Literal(text.to_string())),
    }
}

fn parse_items(pattern: &str, locale: Option<Locale>) -> Result<Vec<Item<'static>>, ClockError> {
    let items = match locale {
        Some(locale) => StrftimeItems::new_with_locale(pattern, locale).parse_to_owned(),
        None => StrftimeItems::new(pattern).parse_to_owned(),
    };
    items.map_err(|_| ClockError::InvalidSpecifier(pattern.to_string()))
}

fn items_have_seconds(items: &[Item<'_>]) -> bool {
    items.iter().any(|item| {
        matches!(
            item,
            Item::Numeric(
                Numeric::Second | Numeric::Nanosecond | Numeric::Timestamp,
                _
            ) | Item::Fixed(
                Fixed::Nanosecond
                    | Fixed::Nanosecond3
                    | Fixed::Nanosecond6
                    | Fixed::Nanosecond9
                    | Fixed::RFC3339
                    | Fixed::RFC2822
                    | Fixed::Internal(_),
            )
        )
    })
}

fn parse_locale(raw: Option<&str>) -> Result<Option<Locale>, ClockError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let normalized = normalize_locale(raw);
    normalized
        .parse::<Locale>()
        .map(Some)
        .map_err(|_| ClockError::UnknownLocale(raw.to_string()))
}

fn warn_unknown_in(parsed: &ParsedTemplate, source: &str, ctx: &str) {
    let Ok(template) = parsed else {
        return;
    };
    for token in &template.unknown {
        warn_unknown_placeholder(ctx, token, source);
    }
}

fn is_placeholder_token(token: &str) -> bool {
    !token.is_empty()
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

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
        Some(modifier) if !modifier.is_empty() => format!("{base}@{modifier}"),
        _ => base,
    }
}

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

#[derive(Default)]
struct Cache {
    label: String,
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

fn render_error(label: &gtk::Label, err: &ClockError, cache: &RefCell<Cache>) {
    warn_format(&mut cache.borrow_mut(), err);
    set_label(label, &err.label(), cache);
}

fn set_label(label: &gtk::Label, text: &str, cache: &RefCell<Cache>) {
    let mut cache = cache.borrow_mut();
    if cache.label == text {
        return;
    }
    cache.label = text.to_string();
    if apply_markup(label, text) {
        return;
    }
    warn_format(&mut cache, &ClockError::Markup);
    apply_markup(label, &ClockError::Markup.label());
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
    UnknownLocale(String),
    Markup,
}

impl ClockError {
    fn label(&self) -> String {
        match self {
            ClockError::UnknownTimezone(_) => "clock: unknown timezone".to_string(),
            ClockError::UnknownLocale(_) => "clock: unknown locale".to_string(),
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
            ClockError::UnknownLocale(locale) => write!(f, "unknown locale `{locale}`"),
            ClockError::Markup => write!(f, "invalid Pango markup"),
        }
    }
}

pub fn register(reg: &mut Registry) {
    reg.register("clock", |init: &ModuleInit| {
        let cfg = ClockConfig::parse(&init.config, &init.name);

        let mut module_config = init.config.clone();
        let calendar_enabled = cfg.as_ref().is_ok_and(|cfg| cfg.calendar.is_some());
        if calendar_enabled
            && let Some(object) = module_config.as_object_mut()
            && object.remove("on-click-right").is_some()
        {
            warn!(
                "{}: `on-click-right` is ignored while the calendar is enabled",
                init.name
            );
        }

        let inner = LabelModule::new("clock", &init.id, &module_config).with_context(&init.name);
        let calendar = cfg.as_ref().ok().and_then(|cfg| {
            cfg.calendar
                .as_ref()
                .map(|cal| CalendarView::new(&inner.widget(), cal.clone(), cfg.locale))
        });

        Ok(Box::new(Clock {
            inner,
            cfg,
            alt: Rc::new(Cell::new(false)),
            cache: Rc::new(RefCell::new(Cache::default())),
            timer: RefCell::new(None),
            calendar,
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
    fn unknown_placeholder_stays_literal() {
        let now = fixed_now();
        let template = Template::parse("{:%H} {timezone}", None).unwrap();
        assert_eq!(template.render_dt(&now, None), "14 {timezone}");
    }

    #[test]
    fn unknown_placeholders_are_collected() {
        let now = fixed_now();
        let template = Template::parse("{:%H} {timezone}", None).unwrap();
        assert_eq!(template.unknown, vec!["timezone".to_string()]);
        assert_eq!(template.render_dt(&now, None), "14 {timezone}");

        assert!(Template::parse("{:%H}", None).unwrap().unknown.is_empty());
        assert!(
            Template::parse("{} {a b} {%H}", None)
                .unwrap()
                .unknown
                .is_empty()
        );
    }

    #[test]
    fn literal_braces_are_preserved() {
        let now = fixed_now();
        let rendered = |src: &str| Template::parse(src, None).unwrap().render_dt(&now, None);
        assert_eq!(rendered("{%H}"), "{%H}");
        assert_eq!(rendered("{a b}"), "{a b}");
        assert_eq!(rendered("{}"), "{}");
        assert_eq!(rendered("{"), "{");
        assert_eq!(rendered("{foo:%H}"), "{foo:%H}");
        let escaped = Template::parse("{{literal}}", None).unwrap();
        assert_eq!(escaped.unknown, vec!["literal".to_string()]);
        assert_eq!(escaped.render_dt(&now, None), "{{literal}}");
        assert_eq!(rendered("{{:%H}}"), "{14}");
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
        assert!(has("%+"));
        assert!(!has("%%S"));
        assert!(items_have_seconds(
            &parse_items("%X", Some(Locale::en_US)).unwrap()
        ));
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
        assert_eq!(normalize_locale("sr_RS@latin"), "sr_RS@latin");
        assert_eq!(normalize_locale("eu_ES.UTF-8@euro"), "eu_ES@euro");
        assert_eq!(normalize_locale("ru-RU"), "ru_RU");
    }

    #[test]
    fn parses_modifier_locales() {
        assert!(parse_locale(Some("eu_ES@euro")).unwrap().is_some());
        assert!(parse_locale(Some("sr_RS@latin")).unwrap().is_some());
        assert!(parse_locale(Some("de_DE.UTF-8@euro")).unwrap().is_some());
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
        let ru = parse_locale(Some("ru_RU")).unwrap().unwrap();
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
        warn_format(&mut cache, &ClockError::InvalidSpecifier("%Q".to_string()));
        assert_eq!(
            cache.last_format_warn.as_deref(),
            Some("invalid strftime specifier in `{:%Q}`")
        );
    }

    #[test]
    fn tooltip_is_rendered_on_demand() {
        let cfg = ClockConfig::parse(
            &json!({
                "format": "{:%H:%M}",
                "tooltip-format": "now {:%H:%M:%S}",
            }),
            "clock",
        )
        .unwrap();
        let cache = RefCell::new(Cache::default());
        let text = render_tooltip(&cfg, &cache);
        assert!(text.starts_with("now "), "got `{text}`");
        assert_eq!(text.len(), "now HH:MM:SS".len());

        let cfg = ClockConfig::parse(
            &json!({
                "format": "{:%H:%M}",
                "tooltip-format": "a & b",
            }),
            "clock",
        )
        .unwrap();
        assert_eq!(render_tooltip(&cfg, &RefCell::new(Cache::default())), "");
    }

    #[test]
    fn config_defaults_and_derived_interval() {
        let cfg = ClockConfig::parse(&json!({}), "clock").unwrap();
        assert!(!cfg.has_seconds);
        assert!(cfg.alt.is_none());
        assert!(cfg.tooltip.is_some());

        let cfg = ClockConfig::parse(
            &json!({
                "format": "{:%H:%M:%S}",
                "format-alt": "{:%H}",
                "tooltip": false,
            }),
            "clock",
        )
        .unwrap();
        assert!(cfg.has_seconds);
        assert!(cfg.alt.is_some());
        assert!(cfg.tooltip.is_none());

        let cfg = ClockConfig::parse(
            &json!({
                "format": "{:%H:%M}",
                "tooltip-format": "{:%H:%M:%S}",
            }),
            "clock",
        )
        .unwrap();
        assert!(!cfg.has_seconds);
        assert!(cfg.tooltip.is_some());
    }

    #[test]
    fn broken_format_does_not_leak_into_tooltip() {
        let cfg = ClockConfig::parse(&json!({ "format": "{:%Q}" }), "clock").unwrap();
        assert!(cfg.primary.is_err());
        assert!(cfg.tooltip.is_none());

        let cfg = ClockConfig::parse(
            &json!({ "format": "{:%H:%M}", "tooltip-format": "{:%Q}" }),
            "clock",
        )
        .unwrap();
        assert!(cfg.tooltip.as_ref().unwrap().is_err());
    }

    #[test]
    fn unknown_locale_is_fatal() {
        assert!(matches!(
            ClockConfig::parse(&json!({ "locale": "not_a_locale" }), "clock"),
            Err(ClockError::UnknownLocale(_))
        ));
        assert!(ClockConfig::parse(&json!({ "locale": "" }), "clock").is_ok());
        assert!(ClockConfig::parse(&json!({}), "clock").is_ok());
    }

    #[test]
    fn placeholder_token_detection() {
        assert!(is_placeholder_token("timezone"));
        assert!(is_placeholder_token("my-zone_2"));
        assert!(!is_placeholder_token(""));
        assert!(!is_placeholder_token("a b"));
        assert!(!is_placeholder_token("0:%H"));
    }
}
