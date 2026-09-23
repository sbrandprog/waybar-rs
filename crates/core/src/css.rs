//! Loading and (optionally) hot-reloading of the user stylesheet.
//!
//! Waybar only watches the stylesheet when `reload_style_on_change` is set.
//! When it is, the file and every recursively `@import`ed file are monitored
//! and the provider is reloaded on change, mirroring `CssReloadHelper`.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::gio::prelude::*;
use tracing::{debug, info, warn};

/// A loaded stylesheet.
///
/// Keeps the provider (and, once [`Css::watch`] has run, the file monitors)
/// alive for as long as the handle is held.
#[derive(Clone)]
pub struct Css {
    inner: Rc<CssInner>,
}

struct CssInner {
    path: PathBuf,
    provider: gtk::CssProvider,
    monitors: RefCell<Vec<gtk::gio::FileMonitor>>,
}

impl Css {
    /// Load `path` into the default display.
    ///
    /// A missing file is not an error (waybar happily runs unstyled). The
    /// provider also supports `@import`, so user stylesheets that pull in extra
    /// files work.
    pub fn load(path: &Path) -> Self {
        let provider = gtk::CssProvider::new();
        provider.connect_parsing_error(|_, section, error| {
            // The section points at the offending rule; its first line is
            // enough to locate GTK3-only properties in a ported stylesheet.
            let location = section.to_string();
            let location = location.lines().next().unwrap_or("").to_string();
            warn!("stylesheet parse error at {location}: {error}");
        });

        let css = Css {
            inner: Rc::new(CssInner {
                path: path.to_path_buf(),
                provider,
                monitors: RefCell::new(Vec::new()),
            }),
        };

        if path.exists() {
            css.inner.provider.load_from_path(path);
        } else {
            warn!("stylesheet not found: {}", path.display());
        }

        match gtk::gdk::Display::default() {
            Some(display) => {
                gtk::style_context_add_provider_for_display(
                    &display,
                    &css.inner.provider,
                    gtk::STYLE_PROVIDER_PRIORITY_USER,
                );
                info!("loaded stylesheet: {}", path.display());
            }
            None => warn!("no display available, stylesheet not applied"),
        }

        css
    }

    /// Re-parse the stylesheet. GTK re-applies the provider's rules itself.
    pub fn reload(&self) {
        if !self.inner.path.exists() {
            warn!("stylesheet disappeared: {}", self.inner.path.display());
            return;
        }
        self.inner.provider.load_from_path(&self.inner.path);
        info!("reloaded stylesheet: {}", self.inner.path.display());
    }

    /// Watch the stylesheet and its `@import`ed files, reloading on change.
    ///
    /// Call once; repeated calls add duplicate monitors.
    pub fn watch(&self) {
        for file in imported_files(&self.inner.path) {
            let gio_file = gtk::gio::File::for_path(&file);
            let monitor = match gio_file.monitor_file(
                gtk::gio::FileMonitorFlags::NONE,
                gtk::gio::Cancellable::NONE,
            ) {
                Ok(monitor) => monitor,
                Err(err) => {
                    warn!("cannot watch {}: {err}", file.display());
                    continue;
                }
            };

            let css = self.clone();
            monitor.connect_changed(move |_, changed, _, event| {
                // A single save fires several events; reload once the write is
                // complete, like upstream.
                if event == gtk::gio::FileMonitorEvent::ChangesDoneHint {
                    debug!("stylesheet changed: {}", changed.path().unwrap_or_default().display());
                    css.reload();
                }
            });
            self.inner.monitors.borrow_mut().push(monitor);
        }
    }
}

/// The stylesheet plus every file reachable through `@import`, symlinks
/// resolved (Gio's file monitor does not follow symlinks).
fn imported_files(path: &Path) -> Vec<PathBuf> {
    let mut seen = Vec::new();
    let mut queue = vec![canonical(path)];

    while let Some(file) = queue.pop() {
        if seen.contains(&file) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        seen.push(file.clone());
        for import in parse_imports(&text) {
            let resolved = resolve_import(&file, &import);
            if !seen.contains(&resolved) {
                queue.push(resolved);
            }
        }
    }

    seen
}

/// Resolve an `@import` target relative to the importing file, then as-is.
fn resolve_import(importer: &Path, import: &str) -> PathBuf {
    let import_path = Path::new(import);
    if import_path.is_absolute() && import_path.exists() {
        return canonical(import_path);
    }
    if let Some(dir) = importer.parent() {
        let candidate = dir.join(import);
        if candidate.exists() {
            return canonical(&candidate);
        }
    }
    canonical(import_path)
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Extract the targets of every `@import` rule in `text`.
///
/// Supports the forms CSS allows: `@import "x"`, `@import 'x'`,
/// `@import url("x")`, `@import url('x')` and `@import url(x)`.
pub fn parse_imports(text: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut rest = text;

    while let Some(pos) = rest.find("@import") {
        rest = &rest[pos + "@import".len()..];
        let trimmed = rest.trim_start();

        // `url(...)` is optional; without it the target must be quoted.
        let (cursor, requires_quotes) = match trimmed.strip_prefix("url") {
            Some(after) => {
                let after = after.trim_start();
                match after.strip_prefix('(') {
                    Some(inner) => (inner.trim_start(), false),
                    None => continue,
                }
            }
            None => (trimmed, true),
        };

        let Some(first) = cursor.chars().next() else {
            continue;
        };
        let url = match first {
            '"' | '\'' => {
                let body = &cursor[first.len_utf8()..];
                match body.find(first) {
                    Some(end) => body[..end].to_string(),
                    None => continue,
                }
            }
            _ if requires_quotes => continue,
            _ => match cursor.find(')') {
                Some(end) => cursor[..end].trim().to_string(),
                None => continue,
            },
        };

        if !url.is_empty() {
            result.push(url);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_import_forms() {
        let css = r#"
            @import "plain.css";
            @import 'single.css';
            @import url("quoted.css");
            @import url('url-single.css');
            @import url(bare.css);
            @import url(  spaced.css  );
            @import "no-semicolon.css"
        "#;
        assert_eq!(
            parse_imports(css),
            vec![
                "plain.css",
                "single.css",
                "quoted.css",
                "url-single.css",
                "bare.css",
                "spaced.css",
                "no-semicolon.css",
            ]
        );
    }

    #[test]
    fn ignores_non_imports_and_malformed() {
        assert!(parse_imports("body { color: red; }").is_empty());
        // `url` without parentheses is not an import target.
        assert!(parse_imports("@import urlx.css;").is_empty());
        // Unterminated quote/url is skipped without panicking.
        assert!(parse_imports("@import \"broken.css").is_empty());
        assert!(parse_imports("@import url(broken.css").is_empty());
    }
}
