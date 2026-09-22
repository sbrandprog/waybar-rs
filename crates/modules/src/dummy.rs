//! A placeholder module, useful as a smoke test.
//!
//! Renders `format` (default `waybar-rs`) as a static label.

use waybar_core::{Module, ModuleInit, Registry};

use crate::label::LabelModule;

struct Dummy {
    inner: LabelModule,
}

impl Module for Dummy {
    fn widget(&self) -> gtk::Widget {
        self.inner.widget()
    }

    fn start(&self) {
        let text = self.inner.option_str("format", "waybar-rs");
        self.inner.set_markup(&text);
    }
}

pub fn register(reg: &mut Registry) {
    reg.register("dummy", |init: &ModuleInit| {
        Ok(Box::new(Dummy {
            inner: LabelModule::new(&init.kind, &init.id, &init.config),
        }))
    });
}
