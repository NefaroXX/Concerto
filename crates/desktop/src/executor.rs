//! Desktop async executor with bounded shutdown.

use iced_futures::MaybeSend;
use std::future::Future;
use std::time::Duration;

/// Grace period for cancellation and child-process cleanup before the runtime
/// stops waiting for unfinished application work.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(750);

/// Tokio executor that cannot keep the desktop process alive indefinitely when
/// the window closes during an active session.
pub struct ShutdownExecutor {
    runtime: Option<tokio::runtime::Runtime>,
}

impl ShutdownExecutor {
    fn runtime(&self) -> Option<&tokio::runtime::Runtime> {
        self.runtime.as_ref()
    }
}

impl iced::Executor for ShutdownExecutor {
    fn new() -> Result<Self, std::io::Error> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map(|runtime| Self { runtime: Some(runtime) })
    }

    fn spawn(&self, future: impl Future<Output = ()> + MaybeSend + 'static) {
        // The runtime is only taken in `Drop`, which iced never does before
        // the executor stops being used; guard defensively instead of
        // panicking in library code.
        if let Some(runtime) = self.runtime() {
            drop(runtime.spawn(future));
        }
    }

    fn block_on<T>(&self, future: impl Future<Output = T>) -> T {
        match self.runtime() {
            Some(runtime) => runtime.block_on(future),
            // Unreachable in iced's lifecycle (the runtime is only taken in
            // `Drop`). Degrade to a fresh single-threaded runtime rather than
            // panicking; if even that cannot be built, run the future on a
            // plain executor as a last resort.
            None => match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime.block_on(future),
                Err(_) => futures::executor::block_on(future),
            },
        }
    }

    fn enter<R>(&self, f: impl FnOnce() -> R) -> R {
        match self.runtime() {
            Some(runtime) => {
                let _guard = runtime.enter();
                f()
            }
            // Unreachable in iced's lifecycle; run the closure without a
            // runtime context rather than panicking.
            None => f(),
        }
    }
}

impl Drop for ShutdownExecutor {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(SHUTDOWN_GRACE);
        }
    }
}
