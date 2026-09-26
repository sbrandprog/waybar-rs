//! CPU usage module (`/proc/stat`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::glib;
use waybar_core::format::{Args, FmtArg};
use waybar_core::{Module, ModuleInit, Registry, timer};

use crate::label::LabelModule;
use crate::sysinfo::cpu::{CpuTimes, cpu_usages, parse_cpu_times, parse_load};

struct Cpu {
    inner: LabelModule,
    /// Kept so the timer can be stopped when the module is dropped. Without
    /// this the GLib source would keep firing (and hold the widgets alive)
    /// after the bar is recreated on a monitor hotplug.
    timer: RefCell<Option<glib::SourceId>>,
    /// Guards the one-time `/proc/stat` read warning.
    warned: Rc<Cell<bool>>,
}

impl Drop for Cpu {
    fn drop(&mut self) {
        if let Some(id) = self.timer.borrow_mut().take() {
            id.remove();
        }
    }
}

impl Module for Cpu {
    fn widget(&self) -> gtk::Widget {
        self.inner.widget()
    }

    fn start(&self) {
        let inner = self.inner.clone();
        // Seed `prev` with zeros of the right length so the first (immediate)
        // update reports the average usage since boot. This avoids blocking the
        // main loop for a short "second sample".
        let sample = parse_cpu_times(&read_proc_stat(&self.warned).unwrap_or_default());
        let prev: Rc<RefCell<Vec<CpuTimes>>> = Rc::new(RefCell::new(vec![(0, 0); sample.len()]));

        let update = {
            let inner = inner.clone();
            let prev = prev.clone();
            let warned = self.warned.clone();
            move || update(&inner, &prev, &warned)
        };

        match inner.interval_secs(10) {
            Some(secs) => {
                let id = timer::every_seconds(secs, update);
                *self.timer.borrow_mut() = Some(id);
            }
            None => update(),
        }
    }
}

/// Read `/proc/stat`, warning once if it cannot be read.
fn read_proc_stat(warned: &Cell<bool>) -> Option<String> {
    match std::fs::read_to_string("/proc/stat") {
        Ok(text) => Some(text),
        Err(err) => {
            if !warned.get() {
                tracing::warn!("cpu: cannot read /proc/stat: {err}");
                warned.set(true);
            }
            None
        }
    }
}

fn update(inner: &LabelModule, prev: &Rc<RefCell<Vec<CpuTimes>>>, warned: &Rc<Cell<bool>>) {
    let Some(text) = read_proc_stat(warned) else {
        return; // keep the previous reading rather than showing a bogus 0%
    };
    let current = parse_cpu_times(&text);
    if current.is_empty() {
        return;
    }

    let (usages, tooltip_lines) = {
        let previous = prev.borrow();
        cpu_usages(previous.as_slice(), current.as_slice())
    };
    *prev.borrow_mut() = current;

    let total = usages.first().copied().unwrap_or(0);
    let state = inner.get_state(total as u8, false);
    let format = inner.resolve_format("{usage}%", &state);
    if format.is_empty() {
        inner.set_visible(false);
        return;
    }
    inner.set_visible(true);

    let mut args = Args::new();
    args.insert("usage".into(), FmtArg::Int(i64::from(total)));
    args.insert(
        "icon".into(),
        FmtArg::from(inner.get_icon(total, &[state.as_str()], 0)),
    );

    let mut all_icons = String::new();
    for (i, usage) in usages.iter().enumerate().skip(1) {
        let icon = inner.get_icon(*usage, &[state.as_str()], 0);
        args.insert(format!("usage{}", i - 1), FmtArg::Int(i64::from(*usage)));
        args.insert(format!("icon{}", i - 1), FmtArg::from(icon.clone()));
        all_icons.push_str(&icon);
    }
    args.insert("icons".into(), FmtArg::from(all_icons));

    let (load1, load5, load15) =
        parse_load(&std::fs::read_to_string("/proc/loadavg").unwrap_or_default());
    args.insert("load".into(), FmtArg::Float(load1));
    args.insert("load1".into(), FmtArg::Float(load1));
    args.insert("load5".into(), FmtArg::Float(load5));
    args.insert("load15".into(), FmtArg::Float(load15));

    inner.set_markup(&inner.render(&format, &args));

    let tooltip_default = tooltip_lines.join("\n");
    let tooltip_format = inner.resolve_tooltip_format(&tooltip_default, &state);
    inner.set_tooltip(&inner.render(&tooltip_format, &args));
}

pub fn register(reg: &mut Registry) {
    reg.register("cpu", |init: &ModuleInit| {
        Ok(Box::new(Cpu {
            inner: LabelModule::new("cpu", &init.id, &init.config).with_context(&init.name),
            timer: RefCell::new(None),
            warned: Rc::new(Cell::new(false)),
        }))
    });
}
