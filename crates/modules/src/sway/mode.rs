//! `sway/mode` — the currently active sway binding mode.
//!
//! Subscribes to sway's `mode` event and renders the mode name; an empty or
//! `default` mode hides the module, mirroring upstream.

use futures_lite::StreamExt;
use swayipc_async::{Connection, Event, EventType};
use tracing::{error, warn};

use waybar_core::bridge;
use waybar_core::format::{self as fmt, Args};
use waybar_core::{Module, ModuleInit, Registry};

use crate::label::LabelModule;

struct Mode {
    inner: LabelModule,
}

impl Module for Mode {
    fn widget(&self) -> gtk::Widget {
        self.inner.widget()
    }

    fn start(&self) {
        let inner = self.inner.clone();
        let (tx, rx) = bridge::channel::<Option<String>>();

        bridge::spawn(async move {
            let conn = match Connection::new().await {
                Ok(conn) => conn,
                Err(err) => {
                    error!("sway/mode: cannot connect to sway: {err}");
                    return;
                }
            };
            let mut events = match conn.subscribe([EventType::Mode]).await {
                Ok(events) => events,
                Err(err) => {
                    error!("sway/mode: cannot subscribe to mode events: {err}");
                    return;
                }
            };
            while let Some(event) = events.next().await {
                match event {
                    Ok(Event::Mode(event)) => {
                        let mode = if event.change == "default" {
                            None
                        } else if event.pango_markup {
                            Some(event.change)
                        } else {
                            Some(gtk::glib::markup_escape_text(&event.change).to_string())
                        };
                        let _ = tx.send(mode).await;
                    }
                    Ok(_) => {}
                    Err(err) => warn!("sway/mode: event error: {err}"),
                }
            }
        });

        bridge::on_main(rx, move |mode| update(&inner, mode.as_deref()));
    }
}

fn update(inner: &LabelModule, mode: Option<&str>) {
    match mode {
        Some(mode) if !mode.is_empty() => {
            let format = inner.resolve_format("{}", "");
            let mut args = Args::new();
            fmt::insert_default(&mut args, mode);
            inner.set_markup(&fmt::format(&format, &args));
            inner.set_tooltip(mode);
            inner.set_visible(true);
        }
        _ => inner.set_visible(false),
    }
}

pub fn register(reg: &mut Registry) {
    reg.register("sway/mode", |init: &ModuleInit| {
        Ok(Box::new(Mode {
            inner: LabelModule::new("mode", &init.id, &init.config),
        }))
    });
}
