//! `waybar-rs` — a Rust reimplementation of Waybar.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use anyhow::Result;
use gtk::prelude::*;
use tracing_subscriber::EnvFilter;

use waybar_core::css::Css;
use waybar_core::{Bar, Config, Registry};

thread_local! {
    /// The application's live state: one bar per matching output, the loaded
    /// config/stylesheet and the module registry. `GApplication` can emit
    /// `activate` more than once — a second launch activates the already-running
    /// primary instance — so we re-present the existing bars instead of
    /// building a duplicate set.
    static STATE: RefCell<Option<AppState>> = const { RefCell::new(None) };
}

struct AppState {
    app: gtk::Application,
    config: Config,
    registry: Registry,
    /// Held to keep the CSS provider and its file monitors alive.
    #[allow(dead_code)]
    css: Css,
    bars: Vec<Bar>,
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
    if STATE.with(|slot| slot.borrow().is_some()) {
        present_all();
        return Ok(());
    }

    let config = Config::load(config_path)?;
    let css = Css::load(style_path);
    waybar_core::icons::install();

    let mut registry = Registry::new();
    waybar_modules::register(&mut registry);

    if config.reload_style_on_change() {
        css.watch();
    }

    STATE.with(|slot| {
        *slot.borrow_mut() = Some(AppState {
            app: app.clone(),
            config,
            registry,
            css,
            bars: Vec::new(),
        });
    });

    // GDK4 exposes the monitor list as a `GListModel` (there are no
    // `monitor-added`/`monitor-removed` signals), so hotplug is handled by
    // reconciling on `items-changed`.
    if let Some(display) = gtk::gdk::Display::default() {
        display
            .monitors()
            .connect_items_changed(|_, _, _, _| reconcile());
    }

    reconcile();
    Ok(())
}

/// Bring the set of bars in line with the currently connected monitors: create
/// a bar for every monitor that has none yet, close bars whose monitor is gone.
fn reconcile() {
    let mut removed: Vec<gtk::ApplicationWindow> = Vec::new();

    STATE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return;
        };

        let monitors = waybar_core::bar::monitors();

        // Drop bars whose monitor disappeared.
        let mut kept = Vec::new();
        for bar in state.bars.drain(..) {
            if monitors.iter().any(|info| bar.monitor() == &info.monitor) {
                kept.push(bar);
            } else {
                removed.push(bar.into_window());
            }
        }
        state.bars = kept;

        // Create bars for monitors that don't have any yet.
        for info in &monitors {
            if state.bars.iter().any(|bar| bar.monitor() == &info.monitor) {
                continue;
            }
            let configs: Vec<_> = state
                .config
                .configs_for_output(&info.name, &info.identifier)
                .into_iter()
                .cloned()
                .collect();
            for cfg in configs {
                match Bar::new(&state.app, &cfg, &state.registry, &info.monitor) {
                    Ok(bar) => {
                        bar.present();
                        state.bars.push(bar);
                    }
                    Err(err) => {
                        tracing::warn!("failed to create bar for {}: {err:#}", info.name);
                    }
                }
            }
        }
    });

    // Destroying windows during the `items-changed` dispatch is unsafe; defer
    // to the next idle, like waybar's deferred monitor removal.
    for window in removed {
        gtk::glib::idle_add_local_once(move || window.close());
    }
}

fn present_all() {
    STATE.with(|slot| {
        if let Some(state) = slot.borrow().as_ref() {
            for bar in &state.bars {
                bar.present();
            }
        }
    });
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
