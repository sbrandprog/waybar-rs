//! CPU usage module (`/proc/stat`).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use waybar_core::format::{self as fmt, Args, FmtArg};
use waybar_core::{timer, Module, ModuleInit, Registry};

use crate::label::LabelModule;
use crate::sysinfo::cpu::{parse_cpu_times, parse_load, usage_percent, CpuTimes};

struct Cpu {
    inner: LabelModule,
}

impl Module for Cpu {
    fn widget(&self) -> gtk::Widget {
        self.inner.widget()
    }

    fn start(&self) {
        let inner = self.inner.clone();
        let prev: Rc<RefCell<Vec<CpuTimes>>> = Rc::new(RefCell::new(parse_cpu_times(
            &std::fs::read_to_string("/proc/stat").unwrap_or_default(),
        )));
        // waybar takes a short second sample so the first reading is accurate.
        std::thread::sleep(Duration::from_millis(100));

        let update = {
            let inner = inner.clone();
            let prev = prev.clone();
            move || update(&inner, &prev)
        };

        match inner.interval_secs(10) {
            Some(secs) => {
                timer::every_seconds(secs, update);
            }
            None => update(),
        }
    }
}

fn update(inner: &LabelModule, prev: &Rc<RefCell<Vec<CpuTimes>>>) {
    let current = parse_cpu_times(&std::fs::read_to_string("/proc/stat").unwrap_or_default());

    let (usages, tooltip_lines) = {
        let previous = prev.borrow();
        let mut usages = Vec::new();
        let mut lines = Vec::new();
        if previous.len() == current.len() && !current.is_empty() {
            for i in 0..current.len() {
                if i > 0 && (current[i].1 == 0 || previous[i].1 == 0) {
                    lines.push(format!("Core{}: offline", i - 1));
                    usages.push(0u16);
                    continue;
                }
                let usage = usage_percent(previous[i], current[i]);
                if i == 0 {
                    lines.push(format!("Total: {usage}%"));
                } else {
                    lines.push(format!("Core{}: {usage}%", i - 1));
                }
                usages.push(usage);
            }
        } else if !current.is_empty() && !previous.is_empty() {
            let usage = usage_percent(previous[0], current[0]);
            lines.push(format!("Total: {usage}%"));
            lines.push("Cores: (pending)".to_string());
            usages.push(usage);
        }
        (usages, lines)
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

    let (load1, load5, load15) = parse_load(&std::fs::read_to_string("/proc/loadavg").unwrap_or_default());
    args.insert("load".into(), FmtArg::Float(load1));
    args.insert("load1".into(), FmtArg::Float(load1));
    args.insert("load5".into(), FmtArg::Float(load5));
    args.insert("load15".into(), FmtArg::Float(load15));

    inner.set_markup(&fmt::format(&format, &args));

    let tooltip_default = tooltip_lines.join("\n");
    let tooltip_format = inner.resolve_tooltip_format(&tooltip_default, &state);
    inner.set_tooltip(&fmt::format(&tooltip_format, &args));
}

pub fn register(reg: &mut Registry) {
    reg.register("cpu", |init: &ModuleInit| {
        Ok(Box::new(Cpu {
            inner: LabelModule::new("cpu", &init.id, &init.config),
        }))
    });
}
