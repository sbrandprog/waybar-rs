//! Bridge between background (async) work and the GTK main loop.
//!
//! GTK widgets are not `Send`, so worker tasks must not touch them directly.
//! Instead a worker sends values through an [`async_channel`] and the main
//! thread drains the channel inside a `glib` local future. Use
//! [`channel`] + [`on_main`].

use gtk::glib;

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
