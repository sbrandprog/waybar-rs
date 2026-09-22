//! Format-string engine, a small replacement for C++ `fmt`.
//!
//! Waybar format strings look like `{capacity:03}%`, `{essid} {icon}` or
//! `{total:0.1f}`. Supported here:
//!
//! * `{name}` — plain substitution
//! * `{name:N}` / `{name:0N}` — width padding (the `0` flag zero-pads numbers
//!   after the sign, e.g. `{-7:03}` renders `-07`)
//! * `{name:.P}` / `{name:0.Pf}` — fixed precision for floats
//! * `{name:<N}` / `{name:>N}` / `{name:^N}` — explicit alignment (an optional
//!   fill character may precede it, e.g. `{name:*<N}`)
//! * `{name:+N}` — always show a sign for numbers
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Right,
    Center,
}

/// A parsed `{name:spec}` format spec.
struct Spec {
    fill: char,
    align: Option<Align>,
    sign_plus: bool,
    zero: bool,
    width: Option<usize>,
    precision: Option<usize>,
}

impl Default for Spec {
    fn default() -> Self {
        Spec {
            fill: ' ',
            align: None,
            sign_plus: false,
            zero: false,
            width: None,
            precision: None,
        }
    }
}

fn parse_spec(spec: Option<&str>) -> Spec {
    let Some(spec) = spec else { return Spec::default() };
    // Strip a trailing C-style type letter (`f`, `e`, `d`, ...).
    let s = spec.trim_end_matches(['f', 'F', 'e', 'E', 'd']);
    let chars: Vec<char> = s.chars().collect();
    let mut out = Spec::default();

    let align_at = |c: char| match c {
        '<' => Some(Align::Left),
        '>' => Some(Align::Right),
        '^' => Some(Align::Center),
        _ => None,
    };

    // [[fill]align]
    let mut i = 0;
    if let (Some(&first), Some(&second)) = (chars.first(), chars.get(1))
        && let Some(align) = align_at(second)
    {
        out.fill = first;
        out.align = Some(align);
        i = 2;
    }
    if out.align.is_none()
        && let Some(&c) = chars.get(i)
        && let Some(align) = align_at(c)
    {
        out.align = Some(align);
        i += 1;
    }

    // [sign]
    match chars.get(i) {
        Some('+') => {
            out.sign_plus = true;
            i += 1;
        }
        Some('-') => i += 1,
        _ => {}
    }

    // [#] then [0]
    if chars.get(i) == Some(&'#') {
        i += 1;
    }
    if chars.get(i) == Some(&'0') {
        out.zero = true;
        i += 1;
    }

    // [width]
    let start = i;
    while matches!(chars.get(i), Some(c) if c.is_ascii_digit()) {
        i += 1;
    }
    if i > start {
        let width: String = chars[start..i].iter().collect();
        out.width = width.parse::<usize>().ok().filter(|w| *w > 0);
    }

    // [.precision]
    if chars.get(i) == Some(&'.') {
        i += 1;
        let start = i;
        while matches!(chars.get(i), Some(c) if c.is_ascii_digit()) {
            i += 1;
        }
        if i > start {
            let precision: String = chars[start..i].iter().collect();
            out.precision = precision.parse::<usize>().ok();
        }
    }

    out
}

fn apply_spec(value: &FmtArg, spec: Option<&str>) -> String {
    let spec = parse_spec(spec);
    match value {
        FmtArg::Int(n) => {
            let mut s = n.to_string();
            if spec.sign_plus && *n >= 0 {
                s.insert(0, '+');
            }
            pad(&s, &spec, Align::Right, true)
        }
        FmtArg::Float(f) => {
            let mut s = match spec.precision {
                Some(p) => format!("{f:.p$}"),
                None => format!("{f}"),
            };
            if spec.sign_plus && !s.starts_with('-') {
                s.insert(0, '+');
            }
            pad(&s, &spec, Align::Right, true)
        }
        FmtArg::Text(t) => {
            let mut s = t.clone();
            if let Some(p) = spec.precision {
                s = s.chars().take(p).collect();
            }
            pad(&s, &spec, Align::Left, false)
        }
    }
}

/// Apply width/alignment, keeping a leading sign in front of zero padding.
fn pad(s: &str, spec: &Spec, default_align: Align, numeric: bool) -> String {
    let Some(width) = spec.width else {
        return s.to_string();
    };
    let len = s.chars().count();
    if len >= width {
        return s.to_string();
    }
    let total = width - len;

    // The `0` flag zero-pads numbers *after* the sign. It only applies when no
    // explicit alignment is given, matching Rust/C++ `fmt` semantics.
    if numeric && spec.zero && spec.align.is_none() {
        let (sign, body) = match s.strip_prefix(['+', '-']) {
            Some(rest) => (&s[..1], rest),
            None => ("", s),
        };
        let mut out = String::with_capacity(width);
        out.push_str(sign);
        out.extend(std::iter::repeat_n('0', total));
        out.push_str(body);
        return out;
    }

    let fill = spec.fill;
    let mut out = String::with_capacity(width);
    match spec.align.unwrap_or(default_align) {
        Align::Left => {
            out.push_str(s);
            out.extend(std::iter::repeat_n(fill, total));
        }
        Align::Right => {
            out.extend(std::iter::repeat_n(fill, total));
            out.push_str(s);
        }
        Align::Center => {
            let left = total / 2;
            out.extend(std::iter::repeat_n(fill, left));
            out.push_str(s);
            out.extend(std::iter::repeat_n(fill, total - left));
        }
    }
    out
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

    #[test]
    fn zero_flag_pads_after_the_sign() {
        let a = args_from([("value", FmtArg::Int(-7))]);
        assert_eq!(format("{value:03}", &a), "-07");
        // An explicit fill is not sign-aware, matching `fmt`.
        assert_eq!(format("{value:0>4}", &a), "00-7");
    }

    #[test]
    fn supports_sign_and_alignment() {
        let a = args_from([("value", FmtArg::Int(7)), ("neg", FmtArg::Int(-7))]);
        assert_eq!(format("{value:+03}", &a), "+07");
        assert_eq!(format("{neg:+03}", &a), "-07");
        assert_eq!(format("{value:>4}", &a), "   7");
        assert_eq!(format("{value:<4}", &a), "7   ");
        assert_eq!(format("{value:^4}", &a), " 7  ");
        assert_eq!(format("{value:*<4}", &a), "7***");
    }

    #[test]
    fn text_is_left_aligned_by_default() {
        let a = args_from([("icon", FmtArg::from("ab"))]);
        assert_eq!(format("{icon:4}", &a), "ab  ");
        assert_eq!(format("{icon:>4}", &a), "  ab");
    }
}
