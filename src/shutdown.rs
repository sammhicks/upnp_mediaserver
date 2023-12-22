//! Notification of shutdown

use futures_util::FutureExt;
use tokio::sync::oneshot;

/// A handle which notifies signals when it is dropped
pub struct Handle {
    handle: oneshot::Sender<()>,
}

impl Handle {
    /// Signal that the tasks should shut down
    pub fn signal_shutdown(self) {
        let _ = self.handle.send(());
    }
}

/// A signal that the handle has been dropped
#[must_use = "Signals must be awaited or polled"]
pub struct Signal {
    signal: oneshot::Receiver<()>,
}

impl Signal {
    pub fn new() -> (Handle, Self) {
        let (handle, signal) = oneshot::channel();
        (Handle { handle }, Signal { signal })
    }
}

impl std::future::Future for Signal {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.signal.poll_unpin(cx).map(|_| ())
    }
}
