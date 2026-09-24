//! Format-string engine, a small replacement for C++ `fmt`.
//!
//! Waybar format strings look like `{capacity:03}%`, `{essid} {icon}` or
//! `{total:0.1f}`. Supported here:
//!
//! * `{name}` — plain substitution
//! * `{name:N}` / `{name:0N}` — width padding (the `0` flag zero-pads numbers
//!   after the sign, e.g. `{value:03}` with `value = -7` renders `-07`)
//! * `{name:.P}` / `{name:0.Pf}` — fixed precision for floats
//! * `{name:e}` / `{name:.Pe}` / `{name:E}` — scientific notation for floats
//! * `{name:<N}` / `{name:>N}` / `{name:^N}` — explicit alignment (an optional
//!   fill character may precede it, e.g. `{name:*<N}`)
//! * `{name:+N}` — always show a sign for numbers
//! * `{{` and `}}` — literal braces
//!
//! Unknown placeholders are replaced with an empty string, mirroring waybar's
//! tolerance of partially-defined format args. [`format_ctx`] reports them once
//! per `(module, placeholder, template)` under the caller's context (typically
//! the module name), so a typo is visible without flooding the log on every
//! refresh; [`format`] is the context-less wrapper. Unsupported numeric type
//! specifiers are ignored with a warning.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use tracing::warn;

/// A value that can be substituted into a format string.
#[derive(Debug, Clone)]
pub enum FmtArg {
    Text(String),
    Int(i64),
    Float(f64),
    /// An auto-scaling quantity (waybar's `pow_format`), e.g. network
    /// bandwidth. The format spec may carry scale/unit modifiers.
    Pow(Pow),
}

/// An auto-scaling numeric value with an SI unit suffix.
///
/// Mirrors waybar's `pow_format`: the value is divided by a power of the base
/// until it falls below it, then rendered with one decimal and a prefix such
/// as `k`/`M`/`G`. The `{name:...}` spec may force a scale (`k`/`M`/...), hide
/// or show the unit (`u`/`U`), force decimal/binary (`b`/`B`), force an
/// integer (`i`) and align the result.
#[derive(Debug, Clone)]
pub struct Pow {
    pub value: i64,
    pub unit: String,
    pub binary: bool,
    pub skip_decimal: bool,
    pub min_pow_for_decimal: i32,
}

impl Pow {
    pub fn new(value: i64, unit: &str) -> Self {
        Pow {
            value,
            unit: unit.to_string(),
            binary: false,
            skip_decimal: false,
            min_pow_for_decimal: 0,
        }
    }

    pub fn binary(mut self, binary: bool) -> Self {
        self.binary = binary;
        self
    }

    pub fn skip_decimal(mut self, skip: bool) -> Self {
        self.skip_decimal = skip;
        self
    }

    pub fn min_pow_for_decimal(mut self, min_pow: i32) -> Self {
        self.min_pow_for_decimal = min_pow;
        self
    }
}

impl From<Pow> for FmtArg {
    fn from(pow: Pow) -> Self {
        FmtArg::Pow(pow)
    }
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

/// Key used for the bare `{}` placeholder (waybar's automatic argument 0).
pub const DEFAULT: &str = "";

/// Insert the value substituted for a bare `{}` placeholder.
pub fn insert_default(args: &mut Args, value: impl Into<FmtArg>) {
    args.insert(DEFAULT.to_string(), value.into());
}

/// Render `template` substituting values from `args`.
///
/// A thin wrapper over [`format_ctx`] using the generic `format` context.
pub fn format(template: &str, args: &Args) -> String {
    format_ctx(template, args, "format")
}

/// Render `template` substituting values from `args`, attributing any unknown
/// placeholder warning to `ctx` (typically the module name).
///
/// An unknown placeholder is replaced with an empty string and warned about
/// once per `(ctx, placeholder, template)`, so a format that misses an argument
/// does not flood the log on every refresh.
pub fn format_ctx(template: &str, args: &Args, ctx: &str) -> String {
    let mut out = String::with_capacity(template.len());
    for piece in Pieces::new(template) {
        match piece {
            Piece::Text(text) => out.push_str(text),
            Piece::Brace(brace) => out.push(brace),
            Piece::Placeholder(token) => out.push_str(&resolve(token, args, ctx, template)),
        }
    }
    out
}

/// Collect the unknown placeholder tokens in `template`, in order of appearance.
pub fn unknown_placeholders(template: &str, args: &Args) -> Vec<String> {
    Pieces::new(template)
        .filter_map(|piece| match piece {
            Piece::Placeholder(token) if !args.contains_key(token_key(token)) => {
                Some(token.to_string())
            }
            _ => None,
        })
        .collect()
}

thread_local! {
    /// Unknown placeholders already reported, keyed by `ctx\0token\0template`.
    static WARNED_PLACEHOLDERS: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// Warn once about an unknown placeholder.
///
/// The key includes the template, so two different formats with the same typo
/// are both reported; `ctx` names the module (or the engine itself).
pub fn warn_unknown_placeholder(ctx: &str, token: &str, template: &str) {
    let key = format!("{ctx}\u{0}{token}\u{0}{template}");
    WARNED_PLACEHOLDERS.with(|warned| {
        if warned.borrow_mut().insert(key) {
            warn!("{ctx}: unknown placeholder `{{{token}}}` in `{template}`");
        }
    });
}

/// One piece of a format template.
enum Piece<'a> {
    /// Literal text that needs no unescaping.
    Text(&'a str),
    /// A literal brace from `{{`, `}}` or an unmatched brace.
    Brace(char),
    /// The contents between `{` and `}`, without the braces.
    Placeholder(&'a str),
}

/// Iterator over the pieces of a format template.
struct Pieces<'a> {
    rest: &'a str,
}

impl<'a> Pieces<'a> {
    fn new(template: &'a str) -> Self {
        Pieces { rest: template }
    }
}

impl<'a> Iterator for Pieces<'a> {
    type Item = Piece<'a>;

    fn next(&mut self) -> Option<Piece<'a>> {
        if self.rest.is_empty() {
            return None;
        }
        if let Some(rest) = self.rest.strip_prefix("{{") {
            self.rest = rest;
            return Some(Piece::Brace('{'));
        }
        if let Some(rest) = self.rest.strip_prefix("}}") {
            self.rest = rest;
            return Some(Piece::Brace('}'));
        }
        if let Some(rest) = self.rest.strip_prefix('{') {
            if let Some(end) = rest.find('}') {
                let token = &rest[..end];
                self.rest = &rest[end + 1..];
                return Some(Piece::Placeholder(token));
            }
            self.rest = rest;
            return Some(Piece::Brace('{'));
        }
        if let Some(rest) = self.rest.strip_prefix('}') {
            self.rest = rest;
            return Some(Piece::Brace('}'));
        }
        let end = self.rest.find(['{', '}']).unwrap_or(self.rest.len());
        let text = &self.rest[..end];
        self.rest = &self.rest[end..];
        Some(Piece::Text(text))
    }
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

fn resolve(token: &str, args: &Args, ctx: &str, template: &str) -> String {
    let (name, spec) = match token.find(':') {
        Some(p) => (&token[..p], Some(&token[p + 1..])),
        None => (token, None),
    };
    // A bare `{}` (waybar's automatic argument 0) is stored under [`DEFAULT`].
    let key = if name.is_empty() { DEFAULT } else { name };
    match args.get(key) {
        Some(value) => apply_spec(key, value, spec),
        None => {
            warn_unknown_placeholder(ctx, token, template);
            String::new()
        }
    }
}

/// The `args` key a placeholder token resolves to (a bare `{}` uses [`DEFAULT`]).
fn token_key(token: &str) -> &str {
    let name = match token.find(':') {
        Some(pos) => &token[..pos],
        None => token,
    };
    if name.is_empty() { DEFAULT } else { name }
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
    /// The optional trailing type letter (`f`, `e`, `d`, ...).
    type_letter: Option<char>,
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
            type_letter: None,
        }
    }
}

fn parse_spec(spec: Option<&str>) -> Spec {
    let Some(spec) = spec else {
        return Spec::default();
    };
    let chars: Vec<char> = spec.chars().collect();
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

    // [type]
    out.type_letter = chars.get(i).copied();

    out
}

fn apply_spec(name: &str, value: &FmtArg, spec: Option<&str>) -> String {
    if let FmtArg::Pow(pow) = value {
        return apply_pow(pow, spec.unwrap_or(""));
    }
    let spec = parse_spec(spec);
    match value {
        FmtArg::Int(n) => {
            if let Some(letter) = spec.type_letter
                && letter != 'd'
            {
                warn!("format: unsupported type `{letter}` for integer `{{{name}}}`; ignoring");
            }
            let mut s = n.to_string();
            if spec.sign_plus && *n >= 0 {
                s.insert(0, '+');
            }
            pad(&s, &spec, Align::Right, true)
        }
        FmtArg::Float(f) => {
            let mut s = match spec.type_letter {
                Some('e') => format_exp(*f, spec.precision, false),
                Some('E') => format_exp(*f, spec.precision, true),
                Some(letter) if letter != 'f' && letter != 'F' => {
                    warn!(
                        "format: unsupported type `{letter}` for float `{{{name}}}`; using fixed"
                    );
                    format_fixed(*f, spec.precision)
                }
                _ => format_fixed(*f, spec.precision),
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
        FmtArg::Pow(_) => String::new(),
    }
}

/// Render an auto-scaling [`Pow`] value, mirroring waybar's `pow_format`.
fn apply_pow(p: &Pow, spec: &str) -> String {
    let mut align: Option<char> = None;
    let mut width: usize = 0;
    let mut scale: Option<char> = None;
    let mut unit_pref: Option<char> = None;
    let mut base_pref: Option<char> = None;
    let mut force_int = false;

    let mut chars = spec.chars().peekable();
    if let Some(&c) = chars.peek()
        && matches!(c, '>' | '<' | '=')
    {
        align = Some(c);
        chars.next();
    }
    while let Some(&c) = chars.peek() {
        match c {
            '#' | 'k' | 'M' | 'G' | 'T' | 'P' => {
                scale = Some(c);
                chars.next();
            }
            'u' | 'U' => {
                unit_pref = Some(c);
                chars.next();
            }
            'b' | 'B' => {
                base_pref = Some(c);
                chars.next();
            }
            'i' => {
                force_int = true;
                chars.next();
            }
            '0'..='9' => {
                let mut value = 0usize;
                while let Some(&d) = chars.peek() {
                    if let Some(digit) = d.to_digit(10) {
                        value = value * 10 + digit as usize;
                        chars.next();
                    } else {
                        break;
                    }
                }
                width = value;
            }
            _ => break,
        }
    }

    let units = ["", "k", "M", "G", "T", "P"];
    let max_pow = units.len() - 1;

    let binary = match base_pref {
        Some('B') => true,
        Some('b') => false,
        _ => p.binary,
    };
    let base: i64 = if binary { 1024 } else { 1000 };
    let mut div: i64 = 1;
    let mut fraction = p.value as f64;

    let pow = match scale {
        Some(s) => {
            let pow = match s {
                'k' => 1,
                'M' => 2,
                'G' => 3,
                'T' => 4,
                'P' => 5,
                _ => 0,
            }
            .min(max_pow);
            for _ in 0..pow {
                div *= base;
            }
            fraction /= div as f64;
            pow
        }
        None => {
            let mut pow = 0;
            while pow < max_pow && fraction / base as f64 >= 1.0 {
                fraction /= base as f64;
                div *= base;
                pow += 1;
            }
            pow
        }
    };

    let mut precision = if force_int || (p.skip_decimal && p.value % div == 0) {
        0
    } else {
        1
    };
    if !force_int && scale.is_none() && (pow as i32) < p.min_pow_for_decimal {
        precision = 0;
    }

    let hide_unit = unit_pref == Some('u') || (unit_pref.is_none() && scale.is_some());
    let prefix = if scale.is_some() {
        String::new()
    } else {
        format!("{}{}", units[pow], if binary && pow > 0 { "i" } else { "" })
    };
    let unit = if hide_unit {
        String::new()
    } else {
        p.unit.clone()
    };

    let number_width = 3 + precision + usize::from(precision != 0) + usize::from(binary);
    let prefix_col = if scale.is_some() {
        0
    } else {
        1 + usize::from(binary)
    };
    let max_width = number_width + prefix_col + unit.chars().count();

    let fixed_num = scale.is_some() && width > 0;
    let mut number = format!("{fraction:.precision$}");
    if fixed_num && number.chars().count() > width {
        number = "#".repeat(width);
    }

    let padding = if scale.is_none() && pow == 0 {
        if binary { "  " } else { " " }
    } else {
        ""
    };

    let body = |number: String| format!("{number}{padding}{prefix}{unit}");
    match align {
        Some('=') => {
            let field = if fixed_num { width } else { number_width };
            format!("{number:<field$}{padding}{prefix}{unit}")
        }
        Some('>') => {
            let number = if fixed_num {
                format!("{number:>width$}")
            } else {
                number
            };
            format!("{:>max_width$}", body(number))
        }
        Some('<') => {
            let number = if fixed_num {
                format!("{number:>width$}")
            } else {
                number
            };
            format!("{:<max_width$}", body(number))
        }
        _ => {
            let number = if fixed_num {
                format!("{number:>width$}")
            } else {
                number
            };
            body(number)
        }
    }
}

fn format_fixed(f: f64, precision: Option<usize>) -> String {
    match precision {
        Some(p) => format!("{f:.p$}"),
        None => format!("{f}"),
    }
}

fn format_exp(f: f64, precision: Option<usize>, upper: bool) -> String {
    match (precision, upper) {
        (Some(p), false) => format!("{f:.p$e}"),
        (Some(p), true) => format!("{f:.p$E}"),
        (None, false) => format!("{f:e}"),
        (None, true) => format!("{f:E}"),
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
    fn formats_scientific_floats() {
        let a = args_from([("value", FmtArg::Float(2.0)), ("neg", FmtArg::Float(-2.0))]);
        assert_eq!(format("{value:e}", &a), "2e0");
        assert_eq!(format("{value:.3e}", &a), "2.000e0");
        assert_eq!(format("{value:E}", &a), "2E0");
        assert_eq!(format("{neg:+e}", &a), "-2e0");
    }

    #[test]
    fn plain_and_escapes() {
        assert_eq!(format("{essid}!", &args()), "\"wifi\"!");
        assert_eq!(format("{{literal}}", &args()), "{literal}");
    }

    #[test]
    fn bare_placeholder_uses_default_arg() {
        let mut a = Args::new();
        insert_default(&mut a, FmtArg::from("mode"));
        assert_eq!(format("{}", &a), "mode");
        assert_eq!(format("<i>{}</i>", &a), "<i>mode</i>");

        let mut n = Args::new();
        insert_default(&mut n, FmtArg::Int(7));
        assert_eq!(format("{:03}", &n), "007");
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
    fn unknown_placeholders_are_collected() {
        let a = args_from([("capacity", FmtArg::Int(80))]);
        assert_eq!(
            unknown_placeholders("{capacity}%", &a),
            Vec::<String>::new()
        );
        assert_eq!(
            unknown_placeholders("{capacity} {missing:03}", &a),
            vec!["missing:03"]
        );
        // Escaped braces are literal, not placeholders.
        assert_eq!(
            unknown_placeholders("{{literal}}", &a),
            Vec::<String>::new()
        );
        // A bare `{}` without a default argument is unknown.
        assert_eq!(unknown_placeholders("{}", &a), vec![""]);
    }

    #[test]
    fn format_ctx_renders_like_format() {
        let a = args_from([
            ("capacity", FmtArg::Int(80)),
            ("icon", FmtArg::from("ICON")),
        ]);
        assert_eq!(
            format_ctx("{capacity:03} {icon}", &a, "test"),
            format("{capacity:03} {icon}", &a)
        );
        // A different context does not change the rendered output.
        assert_eq!(format_ctx("x{missing}y", &a, "test"), "xy");
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

    fn pow_args(value: i64) -> Args {
        args_from([("bw", FmtArg::Pow(Pow::new(value, "b/s")))])
    }

    #[test]
    fn pow_auto_scales() {
        assert_eq!(format("{bw}", &pow_args(1500)), "1.5kb/s");
        assert_eq!(format("{bw}", &pow_args(0)), "0.0 b/s");
        assert_eq!(format("{bw}", &pow_args(2_500_000)), "2.5Mb/s");
        assert_eq!(format("{bw}", &pow_args(999)), "999.0 b/s");
    }

    #[test]
    fn pow_forced_scale_and_unit_modifiers() {
        assert_eq!(format("{bw:M}", &pow_args(1500)), "0.0");
        assert_eq!(format("{bw:MU}", &pow_args(1500)), "0.0b/s");
        assert_eq!(format("{bw:#}", &pow_args(1500)), "1500.0");
        assert_eq!(format("{bw:i}", &pow_args(1500)), "2kb/s");
    }

    #[test]
    fn pow_overflow_uses_hashes() {
        assert_eq!(format("{bw:=3#}", &pow_args(1234)), "###");
        assert_eq!(format("{bw:=4#}", &pow_args(1234)), "####");
        assert_eq!(format("{bw:=4#i}", &pow_args(1234)), "1234");
    }

    #[test]
    fn pow_binary_and_skip_decimal() {
        let a = args_from([("bw", FmtArg::Pow(Pow::new(1_048_576, "B/s").binary(true)))]);
        assert_eq!(format("{bw}", &a), "1.0MiB/s");

        let a = args_from([(
            "speed",
            FmtArg::Pow(Pow::new(1_000_000_000, "b/s").skip_decimal(true)),
        )]);
        assert_eq!(format("{speed}", &a), "1Gb/s");
    }
}
