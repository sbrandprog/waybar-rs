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
    format: String,
}

impl Module for Clock {
    fn widget(&self) -> gtk::Widget {
        self.inner.widget()
    }

    fn start(&self) {
        self.inner
            .set_markup(&format_time(&self.format, &chrono::Local::now()));

        let inner = self.inner.clone();
        let format = self.format.clone();
        glib::timeout_add_seconds_local(1, move || {
            inner.set_markup(&format_time(&format, &chrono::Local::now()));
            glib::ControlFlow::Continue
        });
    }
}

/// Expand `{:<strftime>}` placeholders in a waybar clock format string.
fn format_time(template: &str, now: &chrono::DateTime<chrono::Local>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{:") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                out.push_str(&format_strftime(&after[..end], now));
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Format `pattern` with chrono, falling back to `%H:%M` when it contains a
/// specifier chrono does not understand.
///
/// `chrono`'s formatter panics on unknown specifiers (e.g. `%OI`, `%Od`) when
/// the result is rendered. This runs inside GTK callbacks, so an unguarded
/// panic would abort the whole bar; waybar instead logs and falls back.
fn format_strftime(pattern: &str, now: &chrono::DateTime<chrono::Local>) -> String {
    if chrono::format::StrftimeItems::new(pattern)
        .any(|item| matches!(item, chrono::format::Item::Error))
    {
        warn!("clock: invalid format specifier in `{{{pattern}}}`; using `{{:%H:%M}}`");
        return now.format("%H:%M").to_string();
    }
    now.format(pattern).to_string()
}

pub fn register(reg: &mut Registry) {
    reg.register("clock", |init: &ModuleInit| {
        Ok(Box::new(Clock {
            inner: LabelModule::new("clock", &init.id, &init.config),
            format: init
                .config
                .get("format")
                .and_then(|v| v.as_str())
                .unwrap_or("{:%H:%M}")
                .to_string(),
        }))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_strftime_placeholders() {
        let now = chrono::Local::now();
        assert_eq!(format_time("{:%H}", &now), now.format("%H").to_string());
        assert_eq!(
            format_time("it is {:%H} now", &now),
            format!("it is {} now", now.format("%H"))
        );
    }

    #[test]
    fn invalid_specifier_falls_back_instead_of_panicking() {
        let now = chrono::Local::now();
        let fallback = now.format("%H:%M").to_string();
        assert_eq!(format_time("{:%OI}", &now), fallback);
        assert_eq!(format_time("{:%Q}", &now), fallback);
    }
}
