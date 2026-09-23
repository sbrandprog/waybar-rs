//! Bridge between background (async) work and the GTK main loop.
//!
//! GTK widgets are not `Send`, so worker tasks must not touch them directly.
//! Instead a worker sends values through an [`async_channel`] and the main
//! thread drains the channel inside a `glib` local future. Use
//! [`channel`] + [`on_main`] for the data path, and [`spawn`] to start an
//! async worker on the shared tokio runtime.

use std::future::Future;
use std::sync::OnceLock;

use gtk::glib;
use tokio::runtime::{Builder, Runtime};

/// The process-wide tokio runtime backing every async module.
///
/// It is created lazily on first use so builds that only use the synchronous
/// `/proc`/`/sys` informers never pay for it. The runtime threads outlive the
/// GTK main loop; the process exits with them.
static RUNTIME: OnceLock<Runtime> = OnceLock::new();

fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .enable_all()
            .thread_name("waybar-rs")
            .build()
            .expect("failed to start the tokio runtime")
    })
}

/// Spawn a background task on the shared runtime.
///
/// The task must not touch GTK widgets; send values to the main thread with
/// [`channel`] + [`on_main`] instead.
pub fn spawn<F>(future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    runtime().spawn(future);
}

/// Create an unbounded channel for main-thread delivery.
pub fn channel<T: Send + 'static>() -> (async_channel::Sender<T>, async_channel::Receiver<T>) {
    async_channel::unbounded()
}

/// Run `handler` on the GTK main thread for every value received on `rx`.
///
/// The receiver is consumed. `handler` is `FnMut` and may freely touch widgets.
pub fn on_main<T: Send + 'static, F>(rx: async_channel::Receiver<T>, mut handler: F)
where
    F: FnMut(T) + 'static,
{
    glib::MainContext::default().spawn_local(async move {
        while let Ok(value) = rx.recv().await {
            handler(value);
        }
    });
}
