//! Format-string engine, a small replacement for C++ `fmt`.
//!
//! Waybar format strings look like `{capacity:03}%`, `{essid} {icon}` or
//! `{total:0.1f}`. Supported here:
//!
//! * `{name}` — plain substitution
//! * `{name:N}` / `{name:0N}` — width padding (zero-padded for integers)
//! * `{name:.P}` / `{name:0.Pf}` — fixed precision for floats
//! * `{{` and `}}` — literal braces
//!
//! Unknown placeholders are replaced with an empty string and a warning is
//! logged, mirroring waybar's tolerance of partially-defined format args.

use std::collections::HashMap;

use tracing::warn;

/// A value that can be substituted into a format string.
#[derive(Debug, Clone)]
pub enum FmtArg {
    Text(String),
    Int(i64),
    Float(f64),
}

impl From<&str> for FmtArg {
    fn from(s: &str) -> Self {
        FmtArg::Text(s.to_string())
    }
}

impl From<String> for FmtArg {
    fn from(s: String) -> Self {
        FmtArg::Text(s)
    }
}

impl From<i64> for FmtArg {
    fn from(v: i64) -> Self {
        FmtArg::Int(v)
    }
}

impl From<i32> for FmtArg {
    fn from(v: i32) -> Self {
        FmtArg::Int(v as i64)
    }
}

impl From<u64> for FmtArg {
    fn from(v: u64) -> Self {
        FmtArg::Int(v as i64)
    }
}

impl From<u32> for FmtArg {
    fn from(v: u32) -> Self {
        FmtArg::Int(v as i64)
    }
}

impl From<usize> for FmtArg {
    fn from(v: usize) -> Self {
        FmtArg::Int(v as i64)
    }
}

impl From<f64> for FmtArg {
    fn from(v: f64) -> Self {
        FmtArg::Float(v)
    }
}

impl From<f32> for FmtArg {
    fn from(v: f32) -> Self {
        FmtArg::Float(v as f64)
    }
}

pub type Args = HashMap<String, FmtArg>;

/// Render `template` substituting values from `args`.
pub fn format(template: &str, args: &Args) -> String {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                    out.push('{');
                    i += 2;
                    continue;
                }
                if let Some(rel) = template[i + 1..].find('}') {
                    let token = &template[i + 1..i + 1 + rel];
                    out.push_str(&resolve(token, args));
                    i = i + 1 + rel + 1;
                } else {
                    out.push('{');
                    i += 1;
                }
            }
            b'}' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'}' {
                    out.push('}');
                    i += 2;
                } else {
                    out.push('}');
                    i += 1;
                }
            }
            _ => {
                let ch = template[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// Convenience: build args from an iterator of `(name, value)` pairs.
pub fn args_from<I, K, V>(items: I) -> Args
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<FmtArg>,
{
    items
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect()
}

fn resolve(token: &str, args: &Args) -> String {
    let (name, spec) = match token.find(':') {
        Some(p) => (&token[..p], Some(&token[p + 1..])),
        None => (token, None),
    };
    match args.get(name) {
        Some(value) => apply_spec(value, spec),
        None => {
            warn!("format: unknown placeholder `{{{}}}`", token);
            String::new()
        }
    }
}

#[derive(Default)]
struct Spec {
    zero: bool,
    width: Option<usize>,
    precision: Option<usize>,
}

fn parse_spec(spec: Option<&str>) -> Spec {
    let Some(spec) = spec else { return Spec::default() };
    // Strip a trailing C-style type letter (`f`, `e`, `d`, ...).
    let s = spec.trim_end_matches(['f', 'F', 'e', 'E', 'd']);
    let zero = s.starts_with('0') && s.len() > 1;

    let (width, precision) = match s.find('.') {
        Some(dot) => {
            let w = &s[..dot];
            let p = &s[dot + 1..];
            (w.parse::<usize>().ok().filter(|w| *w > 0), p.parse().ok())
        }
        None => (s.parse::<usize>().ok().filter(|w| *w > 0), None),
    };

    Spec {
        zero,
        width,
        precision,
    }
}

fn apply_spec(value: &FmtArg, spec: Option<&str>) -> String {
    let spec = parse_spec(spec);
    match value {
        FmtArg::Int(n) => {
            let s = n.to_string();
            match spec.width {
                Some(w) if spec.zero => format!("{s:0>w$}"),
                Some(w) => format!("{s:>w$}"),
                None => s,
            }
        }
        FmtArg::Float(f) => {
            let s = match spec.precision {
                Some(p) => format!("{f:.p$}"),
                None => format!("{f}"),
            };
            match spec.width {
                Some(w) if spec.zero => format!("{s:0>w$}"),
                Some(w) => format!("{s:>w$}"),
                None => s,
            }
        }
        FmtArg::Text(t) => {
            let mut s = t.clone();
            if let Some(p) = spec.precision {
                s = s.chars().take(p).collect();
            }
            match spec.width {
                Some(w) => format!("{s:>w$}"),
                None => s,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Args {
        args_from([
            ("capacity", FmtArg::Int(7)),
            ("usage", FmtArg::Int(42)),
            ("health", FmtArg::Int(98)),
            ("total", FmtArg::Float(15.937)),
            ("essid", FmtArg::from("\"wifi\"")),
        ])
    }

    #[test]
    fn zero_pads_integers() {
        assert_eq!(format("{capacity:03}", &args()), "007");
        assert_eq!(format("{usage:03}", &args()), "042");
    }

    #[test]
    fn space_pads_integers() {
        assert_eq!(format("{health:3}", &args()), " 98");
    }

    #[test]
    fn formats_floats() {
        assert_eq!(format("{total:0.1f}", &args()), "15.9");
        assert_eq!(format("{total:.2}", &args()), "15.94");
    }

    #[test]
    fn plain_and_escapes() {
        assert_eq!(format("{essid}!", &args()), "\"wifi\"!");
        assert_eq!(format("{{literal}}", &args()), "{literal}");
    }

    #[test]
    fn mixed_template() {
        let a = args_from([
            ("capacity", FmtArg::Int(80)),
            ("icon", FmtArg::from("ICON")),
        ]);
        assert_eq!(format("{capacity:03} {icon}", &a), "080 ICON");
    }

    #[test]
    fn unknown_placeholder_is_empty() {
        assert_eq!(format("x{missing}y", &args()), "xy");
    }
}
