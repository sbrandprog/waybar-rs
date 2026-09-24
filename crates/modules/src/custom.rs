//! `custom/*` — script-driven labels.
//!
//! Supports waybar's three execution modes (continuous line stream, interval,
//! signal) and both `return-type` formats (`raw` and `json`).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tracing::{error, warn};

use waybar_core::bridge;
use waybar_core::format::{self as fmt, Args, FmtArg};
use waybar_core::{Module, ModuleInit, Registry};

use crate::label::LabelModule;

/// A raw command result, copied for delivery to the main thread.
#[derive(Clone, Debug, PartialEq)]
struct Output {
    exit_code: i32,
    out: String,
}

/// The parsed output the label is rendered from.
#[derive(Clone, Debug, Default, PartialEq)]
struct Parsed {
    text: String,
    alt: String,
    tooltip: String,
    classes: Vec<String>,
    percentage: i64,
}

/// How the script should be driven.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Continuous,
    /// `once` runs a single time and then waits for refresh events.
    Interval {
        once: bool,
    },
    Signal {
        signal: i32,
    },
    None,
}

struct Custom {
    inner: LabelModule,
    script_name: String,
    output_name: String,
    refresh_tx: async_channel::Sender<()>,
    refresh_rx: RefCell<Option<async_channel::Receiver<()>>>,
}

impl Module for Custom {
    fn widget(&self) -> gtk::Widget {
        self.inner.widget()
    }

    fn start(&self) {
        let config = self.inner.config().clone();
        let (tx, rx) = bridge::channel::<Output>();
        let refresh_rx = self.refresh_rx.borrow_mut().take();

        {
            let config = config.clone();
            let output_name = self.output_name.clone();
            let script_name = self.script_name.clone();
            bridge::spawn(async move {
                worker(config, output_name, script_name, tx, refresh_rx).await;
            });
        }

        let applied: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let update_config = config.clone();
        let inner = self.inner.clone();
        bridge::on_main(rx, move |output| {
            update(&inner, &update_config, &applied, output)
        });

        // `exec-on-event` (default true): re-run on click/scroll.
        if config
            .get("exec-on-event")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
        {
            let widget = self.inner.widget();
            let tx = self.refresh_tx.clone();
            let gesture = gtk::GestureClick::new();
            gesture.set_button(0);
            gesture.connect_pressed(move |_, _, _, _| {
                let _ = tx.try_send(());
            });
            widget.add_controller(gesture);

            let tx = self.refresh_tx.clone();
            let scroll =
                gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
            scroll.connect_scroll(move |_, _, _| {
                let _ = tx.try_send(());
                gtk::glib::Propagation::Proceed
            });
            widget.add_controller(scroll);
        }
    }
}

// --- workers ------------------------------------------------------------

async fn worker(
    config: Value,
    output_name: String,
    script_name: String,
    tx: async_channel::Sender<Output>,
    refresh: Option<async_channel::Receiver<()>>,
) {
    let mut refresh = refresh;
    match detect_mode(&config) {
        Mode::Continuous => loop {
            run_continuous(&config, &output_name, &script_name, &tx).await;
            let restart = config
                .get("restart-interval")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            if restart <= 0.0 {
                break;
            }
            tokio::time::sleep(Duration::from_secs_f64(restart)).await;
        },
        Mode::Interval { once } => loop {
            let output = run_once(&config, &output_name).await;
            let _ = tx.send(output).await;
            match (&mut refresh, once) {
                (Some(rx), true) => {
                    let _ = rx.recv().await;
                }
                (Some(rx), false) => {
                    let secs = interval_secs(&config);
                    let sleep = tokio::time::sleep(Duration::from_secs_f64(secs));
                    tokio::pin!(sleep);
                    tokio::select! {
                        _ = sleep => {}
                        _ = rx.recv() => {}
                    }
                }
                (None, true) => break,
                (None, false) => {
                    tokio::time::sleep(Duration::from_secs_f64(interval_secs(&config))).await
                }
            }
        },
        Mode::Signal { signal } => {
            let mut stream = signal_stream(signal);
            loop {
                match (&mut stream, &mut refresh) {
                    (Some(stream), Some(rx)) => {
                        tokio::select! {
                            _ = stream.recv() => {}
                            _ = rx.recv() => {}
                        }
                    }
                    (Some(stream), None) => {
                        stream.recv().await;
                    }
                    (None, Some(rx)) => {
                        let _ = rx.recv().await;
                    }
                    (None, None) => break,
                }
                let output = run_once(&config, &output_name).await;
                let _ = tx.send(output).await;
            }
        }
        Mode::None => {
            // Nothing to run: hide the module.
            let _ = tx
                .send(Output {
                    exit_code: 1,
                    out: String::new(),
                })
                .await;
        }
    }
}

/// Decide how the script is driven, mirroring upstream's constructor.
fn detect_mode(config: &Value) -> Mode {
    let has_interval = config.get("interval").is_some_and(|v| !v.is_null());
    let has_restart = config.get("restart-interval").is_some_and(|v| !v.is_null());
    if let Some(signal) = config.get("signal").and_then(|v| v.as_i64())
        && !has_interval
        && !has_restart
    {
        return Mode::Signal {
            signal: signal as i32,
        };
    }
    if let Some(interval) = config.get("interval") {
        if interval.as_str() == Some("once") {
            return Mode::Interval { once: true };
        }
        if interval.as_f64().is_some_and(|secs| secs > 0.0) {
            return Mode::Interval { once: false };
        }
    }
    if config.get("exec").is_some_and(|v| v.is_string()) {
        return Mode::Continuous;
    }
    Mode::None
}

fn interval_secs(config: &Value) -> f64 {
    config
        .get("interval")
        .and_then(|v| v.as_f64())
        .filter(|secs| *secs > 0.0)
        .unwrap_or(1.0)
}

fn signal_stream(signal: i32) -> Option<tokio::signal::unix::Signal> {
    // glibc's SIGRTMIN is 34; waybar uses SIGRTMIN + `signal`.
    let raw = 34 + signal;
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(raw)) {
        Ok(stream) => Some(stream),
        Err(err) => {
            warn!("custom: cannot listen for signal {raw}: {err}");
            None
        }
    }
}

/// Build a `/bin/sh -c` command with waybar's child environment.
fn shell(command: &str, output_name: &str) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(command).kill_on_drop(true);
    if !output_name.is_empty() {
        cmd.env("WAYBAR_OUTPUT_NAME", output_name);
    }
    cmd
}

async fn run_once(config: &Value, output_name: &str) -> Output {
    if let Some(exec_if) = config.get("exec-if").and_then(|v| v.as_str()) {
        match shell(exec_if, output_name).status().await {
            Ok(status) if status.success() => {}
            Ok(status) => {
                return Output {
                    exit_code: status.code().unwrap_or(1),
                    out: String::new(),
                };
            }
            Err(err) => {
                warn!("custom: `exec-if` failed: {err}");
                return Output {
                    exit_code: 1,
                    out: String::new(),
                };
            }
        }
    }

    let Some(exec) = config.get("exec").and_then(|v| v.as_str()) else {
        return Output {
            exit_code: 0,
            out: String::new(),
        };
    };
    match shell(exec, output_name).output().await {
        Ok(output) => Output {
            exit_code: output.status.code().unwrap_or(1),
            out: String::from_utf8_lossy(&output.stdout).into_owned(),
        },
        Err(err) => {
            error!("custom: cannot run `{exec}`: {err}");
            Output {
                exit_code: 1,
                out: String::new(),
            }
        }
    }
}

async fn run_continuous(
    config: &Value,
    output_name: &str,
    script_name: &str,
    tx: &async_channel::Sender<Output>,
) {
    let Some(exec) = config.get("exec").and_then(|v| v.as_str()) else {
        return;
    };
    let mut child: Child = match shell(exec, output_name)
        .stdout(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            error!("custom: cannot start `{script_name}`: {err}");
            let _ = tx
                .send(Output {
                    exit_code: 1,
                    out: String::new(),
                })
                .await;
            return;
        }
    };

    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let _ = tx
                        .send(Output {
                            exit_code: 0,
                            out: line,
                        })
                        .await;
                }
                Ok(None) => break,
                Err(err) => {
                    warn!("custom: reading `{script_name}` stdout failed: {err}");
                    break;
                }
            }
        }
    }

    match child.wait().await {
        Ok(status) => {
            let code = status.code().unwrap_or(1);
            if code != 0 {
                error!("custom: `{script_name}` stopped unexpectedly, is it endless?");
                let _ = tx
                    .send(Output {
                        exit_code: code,
                        out: String::new(),
                    })
                    .await;
            }
        }
        Err(err) => warn!("custom: waiting for `{script_name}` failed: {err}"),
    }
}

// --- parsing (unit-tested) ----------------------------------------------

fn escape_markup(text: &str) -> String {
    gtk::glib::markup_escape_text(text).to_string()
}

/// Parse a `return-type: raw` output (line 1 text, line 2 tooltip, line 3 class).
fn parse_raw(output: &str, escape: bool) -> Parsed {
    let mut parsed = Parsed::default();
    for (index, line) in output.lines().enumerate() {
        match index {
            0 => {
                if escape {
                    parsed.text = escape_markup(line);
                    parsed.tooltip = escape_markup(line);
                } else {
                    parsed.text = line.to_string();
                    parsed.tooltip = line.to_string();
                }
                // Upstream overwrites the tooltip with the raw line here.
                parsed.tooltip = line.to_string();
                parsed.classes.clear();
            }
            1 => {
                parsed.tooltip = if escape {
                    escape_markup(line)
                } else {
                    line.to_string()
                };
            }
            2 => parsed.classes.push(line.to_string()),
            _ => break,
        }
    }
    parsed
}

/// Parse the first line of a `return-type: json` output.
fn parse_json(output: &str, escape: bool) -> Parsed {
    let mut parsed = Parsed::default();
    let Some(line) = output.lines().next() else {
        return parsed;
    };
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return parsed;
    };
    let field = |key: &str| value.get(key).and_then(|v| v.as_str()).unwrap_or("");
    let escape_field = |text: &str| {
        if escape {
            escape_markup(text)
        } else {
            text.to_string()
        }
    };
    parsed.text = escape_field(field("text"));
    parsed.alt = escape_field(field("alt"));
    parsed.tooltip = escape_field(field("tooltip"));

    if let Some(class) = value.get("class") {
        if let Some(class) = class.as_str() {
            parsed.classes.push(class.to_string());
        } else if let Some(classes) = class.as_array() {
            parsed.classes.extend(
                classes
                    .iter()
                    .filter_map(|c| c.as_str().map(str::to_string)),
            );
        }
    }

    if let Some(percentage) = value.get("percentage")
        && percentage.is_number()
    {
        parsed.percentage = percentage.as_f64().unwrap_or(0.0).round() as i64;
    }
    parsed
}

// --- main-thread update -------------------------------------------------

fn update(inner: &LabelModule, config: &Value, applied: &Rc<RefCell<Vec<String>>>, output: Output) {
    let has_exec = config.get("exec").is_some_and(|v| v.is_string())
        || config.get("exec-if").is_some_and(|v| v.is_string());
    if has_exec && (output.out.is_empty() || output.exit_code != 0) {
        inner.set_visible(false);
        return;
    }

    let escape = config
        .get("escape")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let parsed = if config.get("return-type").and_then(|v| v.as_str()) == Some("json") {
        parse_json(&output.out, escape)
    } else {
        parse_raw(&output.out, escape)
    };

    let icon = inner.get_icon(
        parsed.percentage.clamp(0, u16::MAX as i64) as u16,
        &[&parsed.alt],
        0,
    );
    let mut args = Args::new();
    fmt::insert_default(&mut args, parsed.text.clone());
    args.insert("text".into(), FmtArg::from(parsed.text.clone()));
    args.insert("alt".into(), FmtArg::from(parsed.alt.clone()));
    args.insert("icon".into(), FmtArg::from(icon));
    args.insert("percentage".into(), FmtArg::Int(parsed.percentage));
    args.insert("tooltip".into(), FmtArg::from(parsed.tooltip.clone()));

    let format = inner.resolve_format("{}", "");
    let rendered = inner.render(&format, &args);

    let hide_empty = config
        .get("hide-empty-text")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if (hide_empty && parsed.text.is_empty()) || rendered.is_empty() {
        inner.set_visible(false);
        return;
    }

    inner.set_markup(&rendered);
    tracing::trace!("custom: rendered `{}`", parsed.text);

    let tooltip = match config.get("tooltip-format").and_then(|v| v.as_str()) {
        Some(tooltip_format) => inner.render(tooltip_format, &args),
        None if parsed.text == parsed.tooltip => rendered.clone(),
        None => parsed.tooltip.clone(),
    };
    inner.set_tooltip(&tooltip);

    // Dynamic classes go on both the label and the wrapper box.
    let mut applied = applied.borrow_mut();
    for class in applied.iter() {
        inner.set_class(class, false);
        inner.set_box_class(class, false);
    }
    for class in &parsed.classes {
        inner.set_class(class, true);
        inner.set_box_class(class, true);
    }
    *applied = parsed.classes;
    inner.set_class("flat", true);
    inner.set_class("text-button", true);
    inner.set_class("module", true);

    inner.set_visible(true);
}

pub fn register(reg: &mut Registry) {
    reg.register("custom", |init: &ModuleInit| {
        let script_name = init
            .kind
            .strip_prefix("custom/")
            .unwrap_or(&init.kind)
            .to_string();
        let (refresh_tx, refresh_rx) = bridge::channel::<()>();

        Ok(Box::new(Custom {
            inner: LabelModule::new_icon_label(
                &format!("custom-{script_name}"),
                &init.id,
                &init.config,
            )
            .with_context(&init.name),
            script_name,
            output_name: init.output_name.clone(),
            refresh_tx,
            refresh_rx: RefCell::new(Some(refresh_rx)),
        }))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_continuous_mode() {
        assert_eq!(detect_mode(&json!({"exec": "echo hi"})), Mode::Continuous);
    }

    #[test]
    fn detects_interval_and_signal() {
        assert_eq!(
            detect_mode(&json!({"exec": "x", "interval": 5})),
            Mode::Interval { once: false }
        );
        assert_eq!(
            detect_mode(&json!({"exec": "x", "interval": "once"})),
            Mode::Interval { once: true }
        );
        assert_eq!(
            detect_mode(&json!({"exec": "x", "signal": 5})),
            Mode::Signal { signal: 5 }
        );
        // An interval wins over a signal, like upstream.
        assert_eq!(
            detect_mode(&json!({"exec": "x", "signal": 5, "interval": 2})),
            Mode::Interval { once: false }
        );
    }

    #[test]
    fn parses_raw_lines() {
        let parsed = parse_raw("hello\nworld\nurgent\n", false);
        assert_eq!(parsed.text, "hello");
        assert_eq!(parsed.tooltip, "world");
        assert_eq!(parsed.classes, vec!["urgent"]);
    }

    #[test]
    fn raw_single_line_uses_text_for_tooltip() {
        let parsed = parse_raw("webcam\n", false);
        assert_eq!(parsed.text, "webcam");
        assert_eq!(parsed.tooltip, "webcam");
        assert!(parsed.classes.is_empty());
    }

    #[test]
    fn parses_json_line() {
        let parsed = parse_json(
            r#"{"text":"T","alt":"A","tooltip":"TT","class":["a","b"],"percentage":42.6}"#,
            false,
        );
        assert_eq!(parsed.text, "T");
        assert_eq!(parsed.alt, "A");
        assert_eq!(parsed.tooltip, "TT");
        assert_eq!(parsed.classes, vec!["a", "b"]);
        assert_eq!(parsed.percentage, 43);
    }

    #[test]
    fn escape_applies_to_text() {
        let parsed = parse_raw("<b>", true);
        assert_eq!(parsed.text, "&lt;b&gt;");
    }
}
