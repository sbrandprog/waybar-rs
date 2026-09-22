//! Memory usage module (`/proc/meminfo`).

use waybar_core::format::{self as fmt, Args, FmtArg};
use waybar_core::{timer, Module, ModuleInit, Registry};

use crate::label::LabelModule;
use crate::sysinfo::memory::{compute, parse_meminfo, parse_zfs_size, to_unit, unit_divisor};

struct Memory {
    inner: LabelModule,
}

impl Module for Memory {
    fn widget(&self) -> gtk::Widget {
        self.inner.widget()
    }

    fn start(&self) {
        let inner = self.inner.clone();
        let unit = inner.option_str("unit", "GiB");
        let update = {
            let inner = inner.clone();
            move || update(&inner, &unit)
        };

        match inner.interval_secs(30) {
            Some(secs) => {
                timer::every_seconds(secs, update);
            }
            None => update(),
        }
    }
}

fn update(inner: &LabelModule, unit: &str) {
    let meminfo = parse_meminfo(&std::fs::read_to_string("/proc/meminfo").unwrap_or_default());
    let zfs = std::fs::read_to_string("/proc/spl/kstat/zfs/arcstats")
        .map(|t| parse_zfs_size(&t))
        .unwrap_or(0);

    let Some(stats) = compute(&meminfo, zfs) else {
        inner.set_visible(false);
        return;
    };

    let state = inner.get_state(stats.percentage, false);
    let format = inner.resolve_format("{percentage}%", &state);
    if format.is_empty() {
        inner.set_visible(false);
        return;
    }
    inner.set_visible(true);

    let divisor = unit_divisor(unit);
    let mut args = Args::new();
    args.insert("percentage".into(), FmtArg::Int(i64::from(stats.percentage)));
    args.insert(
        "swapPercentage".into(),
        FmtArg::Int(i64::from(stats.swap_percentage)),
    );
    args.insert("total".into(), FmtArg::Float(to_unit(stats.total, divisor)));
    args.insert("used".into(), FmtArg::Float(to_unit(stats.used, divisor)));
    args.insert("avail".into(), FmtArg::Float(to_unit(stats.avail, divisor)));
    args.insert(
        "swapTotal".into(),
        FmtArg::Float(to_unit(stats.swap_total, divisor)),
    );
    args.insert(
        "swapUsed".into(),
        FmtArg::Float(to_unit(stats.swap_used, divisor)),
    );
    args.insert(
        "swapAvail".into(),
        FmtArg::Float(to_unit(stats.swap_avail, divisor)),
    );
    args.insert(
        "swapState".into(),
        FmtArg::from(if stats.swap_on() { "On" } else { "Off" }),
    );
    args.insert(
        "icon".into(),
        FmtArg::from(inner.get_icon(u16::from(stats.percentage), &[state.as_str()], 0)),
    );

    inner.set_markup(&fmt::format(&format, &args));

    let tooltip_default = format!("{:.1}{} used", to_unit(stats.used, divisor), unit);
    let tooltip_format = inner.resolve_tooltip_format(&tooltip_default, &state);
    inner.set_tooltip(&fmt::format(&tooltip_format, &args));
}

pub fn register(reg: &mut Registry) {
    reg.register("memory", |init: &ModuleInit| {
        Ok(Box::new(Memory {
            inner: LabelModule::new("memory", &init.id, &init.config),
        }))
    });
}
