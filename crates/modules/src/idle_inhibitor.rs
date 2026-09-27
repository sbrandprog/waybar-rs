//! `idle_inhibitor` — presentation mode via the Wayland idle-inhibit protocol.
//!
//! Upstream Waybar creates a `zwp_idle_inhibitor_v1` object on the bar surface.
//! We cannot reach GDK's surface from `gtk4-layer-shell` without pulling in the
//! whole `gdk4-wayland` stack, so the module opens its own Wayland connection,
//! binds `zwp_idle_inhibit_manager_v1`, and keeps an inhibitor alive on a
//! private (never-mapped) `wl_surface`. wlroots/sway only track the inhibitor's
//! presence for the surface's lifetime, so this inhibits `swayidle` exactly like
//! the upstream module does.
//!
//! Only the pieces the config actually uses are implemented: the click toggle
//! and the optional `timeout` countdown. `dynamic-timeouts`, `signal` and
//! `wait-for-activity` are intentionally not ported yet.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tracing::{debug, warn};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_compositor::{self, WlCompositor};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_surface::{self, WlSurface};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::wp::idle_inhibit::zv1::client::zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1;
use wayland_protocols::wp::idle_inhibit::zv1::client::zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1;

use waybar_core::format::{Args, FmtArg};
use waybar_core::widgets::attach_single_click;
use waybar_core::{Module, ModuleInit, Registry, timer};

use crate::label::LabelModule;

/// Inhibitor state shared by every `idle_inhibitor` module, mirroring
/// upstream's process-wide `IdleInhibitor::status` static.
static STATUS: AtomicBool = AtomicBool::new(false);

/// The dispatch state for the module's private Wayland connection.
struct WaylandState;

impl Dispatch<WlRegistry, GlobalListContents> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(WaylandState: wl_compositor::WlCompositor);
wayland_client::delegate_noop!(WaylandState: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(WaylandState: ZwpIdleInhibitManagerV1);
wayland_client::delegate_noop!(WaylandState: ZwpIdleInhibitorV1);

/// The private Wayland connection plus the objects needed to (de)activate.
struct Wayland {
    conn: Connection,
    queue: EventQueue<WaylandState>,
    qh: QueueHandle<WaylandState>,
    manager: ZwpIdleInhibitManagerV1,
    surface: WlSurface,
    inhibitor: Option<ZwpIdleInhibitorV1>,
}

impl Wayland {
    fn connect() -> Option<Self> {
        let conn = match Connection::connect_to_env() {
            Ok(conn) => conn,
            Err(err) => {
                warn!("idle_inhibitor: cannot connect to Wayland: {err}");
                return None;
            }
        };
        let (globals, queue) = match registry_queue_init::<WaylandState>(&conn) {
            Ok(pair) => pair,
            Err(err) => {
                warn!("idle_inhibitor: cannot query Wayland globals: {err}");
                return None;
            }
        };
        let qh = queue.handle();
        let compositor: WlCompositor = match globals.bind(&qh, 1..=6, ()) {
            Ok(compositor) => compositor,
            Err(err) => {
                warn!("idle_inhibitor: wl_compositor unavailable: {err}");
                return None;
            }
        };
        let manager: ZwpIdleInhibitManagerV1 = match globals.bind(&qh, 1..=1, ()) {
            Ok(manager) => manager,
            Err(err) => {
                warn!("idle_inhibitor: idle-inhibit protocol unavailable: {err}");
                return None;
            }
        };
        let surface = compositor.create_surface(&qh, ());
        let _ = conn.flush();
        Some(Wayland {
            conn,
            queue,
            qh,
            manager,
            surface,
            inhibitor: None,
        })
    }

    fn set(&mut self, active: bool) {
        match (active, self.inhibitor.is_some()) {
            (true, false) => {
                let inhibitor = self.manager.create_inhibitor(&self.surface, &self.qh, ());
                self.inhibitor = Some(inhibitor);
            }
            (false, true) => {
                if let Some(inhibitor) = self.inhibitor.take() {
                    inhibitor.destroy();
                }
            }
            _ => return,
        }
        let _ = self.conn.flush();
        // Drain any `delete_id`/error events so the socket does not fill up.
        let _ = self.queue.roundtrip(&mut WaylandState);
    }
}

/// Mutable module state, shared with the click handler and the countdown timer.
struct Shared {
    wayland: Option<Wayland>,
    activated: bool,
    timeout_minutes: f64,
    /// When the current activation runs out (mirrors `deactivationTime`).
    deactivation: Instant,
}

impl Shared {
    fn set_activated(&mut self, active: bool) {
        if self.activated == active {
            return;
        }
        self.activated = active;
        STATUS.store(active, Ordering::Relaxed);
        debug!(
            "idle_inhibitor: {}",
            if active { "activated" } else { "deactivated" }
        );
        if let Some(wayland) = self.wayland.as_mut() {
            wayland.set(active);
        }
    }

    fn timeleft(&self) -> i64 {
        remaining_minutes(self.deactivation, Instant::now())
    }
}

/// Whole minutes until `deactivation`, truncating toward zero like upstream's
/// `(deactivationTime - time(nullptr)) / 60`.
fn remaining_minutes(deactivation: Instant, now: Instant) -> i64 {
    let seconds = if deactivation >= now {
        (deactivation - now).as_secs_f64()
    } else {
        -((now - deactivation).as_secs_f64())
    };
    (seconds / 60.0) as i64
}

struct IdleInhibitor {
    inner: LabelModule,
    shared: Rc<RefCell<Shared>>,
}

impl Module for IdleInhibitor {
    fn widget(&self) -> gtk::Widget {
        self.inner.widget()
    }

    fn start(&self) {
        let inner = self.inner.clone();
        let shared = self.shared.clone();

        {
            let mut state = shared.borrow_mut();
            state.timeout_minutes = inner.option_f64("timeout", 0.0);
            state.deactivation =
                Instant::now() + Duration::from_secs_f64(state.timeout_minutes.max(0.0) * 60.0);
            state.wayland = Wayland::connect();

            // `start-activated` toggles the shared status to match the config,
            // like upstream (only meaningful for the first module).
            if let Some(start) = inner
                .config()
                .get("start-activated")
                .and_then(|v| v.as_bool())
            {
                state.activated = !start;
                state.set_activated(start);
            } else if STATUS.load(Ordering::Relaxed) {
                state.activated = false;
                state.set_activated(true);
            }
        }

        render(&inner, &shared.borrow());

        // Left click toggles the inhibitor. `attach_actions` still runs any
        // configured `on-click*` command independently.
        let click_state = shared.clone();
        let click_inner = inner.clone();
        attach_single_click(&inner.widget(), inner.config(), 1, move |_| {
            let mut state = click_state.borrow_mut();
            let next = !state.activated;
            if next {
                state.deactivation =
                    Instant::now() + Duration::from_secs_f64(state.timeout_minutes.max(0.0) * 60.0);
            }
            state.set_activated(next);
            render(&click_inner, &state);
        });

        // Per-minute countdown: deactivate when the timeout runs out.
        let timer_state = shared.clone();
        let timer_inner = inner.clone();
        timer::every_seconds(60, move || {
            let mut state = timer_state.borrow_mut();
            if !state.activated || state.timeout_minutes <= 0.0 {
                return;
            }
            if state.timeleft() <= 0 {
                debug!("idle_inhibitor: deactivating by timeout");
                state.set_activated(false);
            }
            render(&timer_inner, &state);
        });
    }
}

fn render(inner: &LabelModule, state: &Shared) {
    let status = if state.activated {
        "activated"
    } else {
        "deactivated"
    };
    inner.set_class("activated", state.activated);
    inner.set_class("deactivated", !state.activated);

    let icon = inner.get_icon(0, &[status], 0);
    let mut args = Args::new();
    args.insert("status".into(), FmtArg::from(status));
    args.insert("timeout".into(), FmtArg::Float(state.timeout_minutes));
    args.insert("timeleft".into(), FmtArg::Int(state.timeleft()));
    args.insert("icon".into(), FmtArg::from(icon));

    let format = inner.resolve_format("{status}", status);
    inner.set_markup(&inner.render(&format, &args));
    let tooltip = inner.resolve_tooltip_format("{status}", status);
    inner.set_tooltip(&inner.render(&tooltip, &args));
    inner.set_visible(true);
}

pub fn register(reg: &mut Registry) {
    reg.register("idle_inhibitor", |init: &ModuleInit| {
        Ok(Box::new(IdleInhibitor {
            inner: LabelModule::new("idle_inhibitor", &init.id, &init.config)
                .with_context(&init.name),
            shared: Rc::new(RefCell::new(Shared {
                wayland: None,
                activated: false,
                timeout_minutes: 0.0,
                deactivation: Instant::now(),
            })),
        }))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_minutes_truncates_toward_zero() {
        let now = Instant::now();
        assert_eq!(remaining_minutes(now, now), 0);
        assert_eq!(remaining_minutes(now + Duration::from_secs(90), now), 1);
        assert_eq!(remaining_minutes(now + Duration::from_secs(119), now), 1);
        assert_eq!(remaining_minutes(now + Duration::from_secs(120), now), 2);
        assert_eq!(remaining_minutes(now - Duration::from_secs(30), now), 0);
        assert_eq!(remaining_minutes(now - Duration::from_secs(61), now), -1);
    }
}
