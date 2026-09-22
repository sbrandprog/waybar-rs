//! `waybar-rs` — a Rust reimplementation of Waybar.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use anyhow::Result;
use gtk::prelude::*;
use tracing_subscriber::EnvFilter;

use waybar_core::{Bar, Config, Registry};

thread_local! {
    /// Bars are owned here for the lifetime of the process so their modules
    /// (which may hold background state) stay alive. `GApplication` can emit
    /// `activate` more than once — a second launch activates the already-running
    /// primary instance — so we present the existing bars instead of building a
    /// duplicate set.
    static BARS: RefCell<Vec<Bar>> = const { RefCell::new(Vec::new()) };
}

fn main() -> Result<()> {
    init_logging();
    let args = Args::parse();

    let application = gtk::Application::builder()
        .application_id("dev.waybar.rust")
        .build();

    let config = args.config.clone();
    let style = args.style.clone();
    application.connect_activate(move |app| {
        if let Err(err) = activate(app, &config, &style) {
            tracing::error!("{err:#}");
            app.quit();
        }
    });

    application.run_with_args::<&str>(&["waybar-rs"]);
    Ok(())
}

fn activate(app: &gtk::Application, config_path: &Path, style_path: &Path) -> Result<()> {
    // A re-activation (e.g. a second `waybar-rs` launch) should re-present the
    // existing bars, not create another copy of every module.
    if BARS.with(|bars| !bars.borrow().is_empty()) {
        BARS.with(|bars| {
            for bar in bars.borrow().iter() {
                bar.present();
            }
        });
        return Ok(());
    }

    let config = Config::load(config_path)?;
    waybar_core::css::load(style_path);

    let mut registry = Registry::new();
    waybar_modules::register(&mut registry);

    let mut bars = Vec::new();
    for bar_config in &config.bars {
        let bar = Bar::new(app, bar_config, &registry)?;
        bar.present();
        bars.push(bar);
    }

    BARS.with(|slot| *slot.borrow_mut() = bars);
    Ok(())
}

fn init_logging() {
    let filter =
        EnvFilter::try_from_env("WAYBAR_RS_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

struct Args {
    config: PathBuf,
    style: PathBuf,
}

impl Args {
    fn parse() -> Self {
        let mut config = None;
        let mut style = None;

        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "-c" | "--config" => config = iter.next().map(PathBuf::from),
                "-s" | "--style" => style = iter.next().map(PathBuf::from),
                "-h" | "--help" => {
                    println!(
                        "waybar-rs\n\n\
                         Usage: waybar-rs [OPTIONS]\n\n\
                         Options:\n  \
                         -c, --config <FILE>  Path to the config file (default: $XDG_CONFIG_HOME/waybar/config.jsonc)\n  \
                         -s, --style  <FILE>  Path to the style file  (default: $XDG_CONFIG_HOME/waybar/style.css)\n  \
                         -h, --help           Print help"
                    );
                    std::process::exit(0);
                }
                other => tracing::warn!("ignoring unknown argument `{other}`"),
            }
        }

        let dir = config_dir();
        Args {
            config: config.unwrap_or_else(|| first_existing(&dir, &["config.jsonc", "config"])),
            style: style.unwrap_or_else(|| dir.join("style.css")),
        }
    }
}

fn config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("waybar");
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/waybar")
}

fn first_existing(dir: &Path, names: &[&str]) -> PathBuf {
    for name in names {
        let candidate = dir.join(name);
        if candidate.exists() {
            return candidate;
        }
    }
    dir.join(names[0])
}
