# Vendored `system-tray` — local patches

This directory is a copy of the [`system-tray`](https://github.com/jakestanger/system-tray)
crate, version **0.8.9** (upstream commit `1fbc4877d188ce9b6be881f5c9887736d8bec7fc`,
published 2026-08-29), vendored so we can fix the log burst and a shutdown race
that the crate handles poorly. It is wired in via `[patch.crates-io]` in the
workspace `Cargo.toml`.

## Why

When a StatusNotifierItem application exits, its final `NewIcon`/menu signals
race the disappearance of its D-Bus name. The crate reacts by reading properties
and fetching menu layouts from the (already gone) name and logging the resulting
errors at `WARN`/`ERROR`. It also leaves the per-item menu watcher running after
the property watcher has stopped, producing one more doomed `GetLayout`. The
result is a burst of warnings/errors in the bar log every time a tray app closes
(easily reproduced with KeePassXC).

## Patches

All local edits are marked with a `// waybar-rs patch:` comment.

`src/client.rs`:

1. `handle_item` creates a `tokio::sync::watch` channel and passes the receiver
   to `watch_menu`, the sender to `watch_item_properties`.
2. `watch_item_properties` sets the shutdown flag on the `NameOwnerChanged`
   disconnect branch, and the `UnregisterStatusNotifierItem` failure — expected
   when the item is already gone or a *foreign* watcher owns
   `org.kde.StatusNotifierWatcher` and lacks the method — is logged at `debug`.
3. `watch_menu` takes the shutdown receiver and exits from its `select!` when
   signalled, instead of doing a final `GetLayout` against the dead name.
4. The layout-debounce task now ignores a failed `layout_tx.send` (the watcher
   may have stopped while the debounce was pending) instead of panicking.
5. Downgraded to `debug` (expected during/after disconnect): `Error parsing
   update properties`, `Error getting IconName`, `Error getting IconPixmap`,
   the `InvalidArgs` branch of the property read macro, `error fetching layout`,
   `Timeout getting layout`, and `could not find item in state`.

`src/data.rs`:

6. The two `could not find item in state` error logs became `debug` (and the
   `error` import was swapped for `debug`).

Genuine failures (`failed to initialize tray item`, `failed to parse tray item
registration`, `failed to wait for service exit`, `failed to signal Watcher`)
keep their original levels.

## Updating

When bumping the crate:

1. Replace this directory with the new upstream sources (keep a `LICENSE`; the
   published tarball does not ship one).
2. Re-apply the edits above (grep for `waybar-rs patch` in the old tree first).
3. Keep `name`/`version` in `Cargo.toml` in sync with the `[patch]` expectation.
4. Re-run `cargo test -p system-tray`, then the tray repro (start/close a tray
   app) at the default log level and confirm there is no `WARN`/`ERROR` burst.
