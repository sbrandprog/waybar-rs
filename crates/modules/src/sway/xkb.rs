//! Minimal FFI to `libxkbregistry`, used to map sway's full layout names
//! (`"English (US)"`) to their short names (`"us"`), matching upstream.
//!
//! Only the handful of `rxkb_*` calls waybar uses are declared. The strings
//! returned by the library are copied immediately, before the next
//! `rxkb_layout_next`, so no borrowed lifetimes escape this module.

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_int};

#[repr(C)]
struct RxkbContext {
    _private: [u8; 0],
}

#[repr(C)]
struct RxkbLayout {
    _private: [u8; 0],
}

#[link(name = "xkbregistry")]
unsafe extern "C" {
    fn rxkb_context_new(flags: u32) -> *mut RxkbContext;
    fn rxkb_context_parse_default_ruleset(ctx: *mut RxkbContext) -> c_int;
    fn rxkb_context_unref(ctx: *mut RxkbContext);
    fn rxkb_layout_first(ctx: *mut RxkbContext) -> *mut RxkbLayout;
    fn rxkb_layout_next(layout: *mut RxkbLayout) -> *mut RxkbLayout;
    fn rxkb_layout_get_name(layout: *mut RxkbLayout) -> *const c_char;
    fn rxkb_layout_get_description(layout: *mut RxkbLayout) -> *const c_char;
    fn rxkb_layout_get_brief(layout: *mut RxkbLayout) -> *const c_char;
    fn rxkb_layout_get_variant(layout: *mut RxkbLayout) -> *const c_char;
}

/// `RXKB_CONTEXT_LOAD_EXOTIC_RULES` from `xkbregistry.h`.
const LOAD_EXOTIC_RULES: u32 = 1 << 1;

/// One keyboard layout as exposed by xkbregistry.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    /// Short layout name, e.g. `us`.
    pub short_name: String,
    /// Full description, e.g. `English (US)`.
    pub description: String,
    /// Variant, or empty.
    pub variant: String,
    /// Brief description, e.g. `en`; falls back to the base layout's brief.
    pub short_description: String,
}

/// A parsed xkbregistry context.
pub struct Context {
    ctx: *mut RxkbContext,
}

// The context is only used from the language worker task and never shared.
unsafe impl Send for Context {}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe { rxkb_context_unref(self.ctx) };
    }
}

impl Context {
    /// Open the default ruleset, or `None` when xkbregistry is unavailable.
    pub fn new() -> Option<Self> {
        unsafe {
            let ctx = rxkb_context_new(LOAD_EXOTIC_RULES);
            if ctx.is_null() {
                return None;
            }
            if rxkb_context_parse_default_ruleset(ctx) == 0 {
                rxkb_context_unref(ctx);
                return None;
            }
            Some(Context { ctx })
        }
    }

    /// Every layout in the default ruleset.
    pub fn layouts(&self) -> Vec<Layout> {
        let mut out = Vec::new();
        let mut base_briefs: HashMap<String, String> = HashMap::new();
        unsafe {
            let mut layout = rxkb_layout_first(self.ctx);
            while !layout.is_null() {
                let name = cstr(rxkb_layout_get_name(layout));
                let description = cstr(rxkb_layout_get_description(layout));
                let variant = cstr(rxkb_layout_get_variant(layout));
                let brief = rxkb_layout_get_brief(layout);
                let short_description = if brief.is_null() {
                    base_briefs.get(&name).cloned().unwrap_or_default()
                } else {
                    let brief = cstr(brief);
                    base_briefs.insert(name.clone(), brief.clone());
                    brief
                };
                out.push(Layout {
                    short_name: name,
                    description,
                    variant,
                    short_description,
                });
                layout = rxkb_layout_next(layout);
            }
        }
        out
    }
}

/// Copy a C string, mapping null to an empty `String`.
unsafe fn cstr(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}
