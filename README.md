# waybar-rs

A Rust reimplementation of [Waybar](https://github.com/Alexays/Waybar), built
against **GTK4** and `gtk4-layer-shell`.

## Status

Early scaffolding. The bar window, JSONC config loading, the module registry,
the format-string engine and a small async bridge are in place, together with
the `clock` and a placeholder `dummy` module.

See the project plan for the full roadmap and per-module dependency mapping.

## Build & run

```sh
cargo build
cargo run -p waybar-rs -- -c examples/config.jsonc -s examples/style.css
```

The binary reads `$XDG_CONFIG_HOME/waybar/config.jsonc` and `style.css` by
default, so it can act as a drop-in for the real waybar.

Logging is controlled with `WAYBAR_RS_LOG` (tracing `EnvFilter` syntax),
e.g. `WAYBAR_RS_LOG=debug`.

## Layout

```
crates/core      config, bar window, module registry, format engine, bridge, css
crates/modules   module implementations (feature-gated per module)
app              the `waybar-rs` binary
upstream/        symlink to the original Waybar sources (reference)
```

## Adding a module

1. Implement [`waybar_core::Module`] in `crates/modules/src/<name>.rs`.
2. Add a `register` function and call it from `waybar_modules::register`.
3. Gate it behind a Cargo feature mirroring waybar's `HAVE_*` flags.
