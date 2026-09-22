//! Clock module.
//!
//! Supports waybar's chrono-style placeholders, e.g. `{:%H:%M}` renders the
//! current time using the given `strftime` pattern.

use gtk::glib;
use tracing::warn;

use waybar_core::{Module, ModuleInit, Registry};

use crate::label::LabelModule;

struct Clock {
    inner: LabelModule,
    template: ClockTemplate,
}

impl Module for Clock {
    fn widget(&self) -> gtk::Widget {
        self.inner.widget()
    }

    fn start(&self) {
        let now = chrono::Local::now();
        self.inner.set_markup(&self.template.render(&now));

        let inner = self.inner.clone();
        let template = self.template.clone();
        glib::timeout_add_seconds_local(1, move || {
            inner.set_markup(&template.render(&chrono::Local::now()));
            glib::ControlFlow::Continue
        });
    }
}

/// A clock format string parsed into literal text and validated strftime
/// patterns, so validation happens once at construction rather than every tick.
#[derive(Clone)]
struct ClockTemplate {
    segments: Vec<Segment>,
}

#[derive(Clone)]
enum Segment {
    Literal(String),
    /// An already-validated strftime pattern.
    Time(String),
}

impl ClockTemplate {
    /// Expand `{:<strftime>}` placeholders in a waybar clock format string.
    fn parse(template: &str) -> Self {
        let mut segments = Vec::new();
        let mut rest = template;
        while let Some(start) = rest.find("{:") {
            if start > 0 {
                segments.push(Segment::Literal(rest[..start].to_string()));
            }
            let after = &rest[start + 2..];
            match after.find('}') {
                Some(end) => {
                    segments.push(Segment::Time(validate_strftime(&after[..end])));
                    rest = &after[end + 1..];
                }
                None => {
                    // Unterminated placeholder: keep the remainder literal.
                    segments.push(Segment::Literal(rest[start..].to_string()));
                    return Self { segments };
                }
            }
        }
        if !rest.is_empty() {
            segments.push(Segment::Literal(rest.to_string()));
        }
        Self { segments }
    }

    fn render(&self, now: &chrono::DateTime<chrono::Local>) -> String {
        let mut out = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Literal(text) => out.push_str(text),
                Segment::Time(pattern) => out.push_str(&now.format(pattern).to_string()),
            }
        }
        out
    }
}

/// Validate `pattern` with chrono, falling back to `%H:%M` when it contains a
/// specifier chrono does not understand.
///
/// `chrono`'s formatter panics on unknown specifiers (e.g. `%OI`, `%Od`) when
/// the result is rendered. This runs inside GTK callbacks, so an unguarded
/// panic would abort the whole bar; waybar instead logs and falls back.
fn validate_strftime(pattern: &str) -> String {
    if chrono::format::StrftimeItems::new(pattern)
        .any(|item| matches!(item, chrono::format::Item::Error))
    {
        warn!("clock: invalid format specifier in `{{:{pattern}}}`; using `{{:%H:%M}}`");
        "%H:%M".to_string()
    } else {
        pattern.to_string()
    }
}

pub fn register(reg: &mut Registry) {
    reg.register("clock", |init: &ModuleInit| {
        Ok(Box::new(Clock {
            inner: LabelModule::new("clock", &init.id, &init.config),
            template: ClockTemplate::parse(
                init.config
                    .get("format")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{:%H:%M}"),
            ),
        }))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_strftime_placeholders() {
        let now = chrono::Local::now();
        assert_eq!(
            ClockTemplate::parse("{:%H}").render(&now),
            now.format("%H").to_string()
        );
        assert_eq!(
            ClockTemplate::parse("it is {:%H} now").render(&now),
            format!("it is {} now", now.format("%H"))
        );
    }

    #[test]
    fn invalid_specifier_falls_back_instead_of_panicking() {
        let now = chrono::Local::now();
        let fallback = now.format("%H:%M").to_string();
        assert_eq!(ClockTemplate::parse("{:%OI}").render(&now), fallback);
        assert_eq!(ClockTemplate::parse("{:%Q}").render(&now), fallback);
    }
}
