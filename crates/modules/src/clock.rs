//! Clock module.
//!
//! Supports waybar's chrono-style placeholders, e.g. `{:%H:%M}` renders the
//! current time using the given `strftime` pattern.

use gtk::glib;

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
                out.push_str(&now.format(&after[..end]).to_string());
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
