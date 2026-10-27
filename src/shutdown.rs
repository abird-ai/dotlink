//! Ctrl+C and SIGTERM (systemd's stop signal) as one sticky process-wide
//! request.
//!
//! Registration is explicitly delayed until initial runtime planning finishes,
//! so the default disposition still interrupts first-run interactive setup.
//! From then on one listener outlives every runtime: a signal arriving while a
//! runtime starts, stops or restarts is never lost.

use std::future::Future;

use anyhow::Result;
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

#[derive(Default)]
pub struct Shutdown(Option<CancellationToken>);

impl Shutdown {
    /// Install the process-wide listener if it is not already running.
    pub fn start(&mut self) -> Result<()> {
        if self.0.is_none() {
            self.0 = Some(listen()?);
        }
        Ok(())
    }

    /// Resolves once shutdown is requested; registers the listener on first use.
    pub fn requested(&mut self) -> Result<WaitForCancellationFutureOwned> {
        self.start()?;
        Ok(self
            .0
            .as_ref()
            .expect("shutdown listener was installed above")
            .clone()
            .cancelled_owned())
    }

    /// A per-runtime cancellation token linked directly to the process signal.
    /// Cancelling the child for an interactive restart does not cancel the
    /// process-wide listener.
    pub fn child_token(&mut self) -> Result<CancellationToken> {
        self.start()?;
        Ok(self
            .0
            .as_ref()
            .expect("shutdown listener was installed above")
            .child_token())
    }

    /// Run an operation until a previously installed listener requests
    /// shutdown. This never installs a listener itself, which keeps initial
    /// interactive setup under the OS's default Ctrl+C behavior.
    ///
    /// Cancellation is biased so an already-requested shutdown wins over an
    /// operation that becomes ready in the same scheduler turn.
    pub async fn run_until_requested<F>(&self, operation: F) -> Option<F::Output>
    where
        F: Future,
    {
        let Some(requested) = self.0.as_ref().cloned() else {
            return Some(operation.await);
        };
        tokio::select! {
            biased;
            _ = requested.cancelled_owned() => None,
            result = operation => Some(result),
        }
    }
}

fn listen() -> Result<CancellationToken> {
    #[cfg(unix)]
    let signal = {
        use tokio::signal::unix::{SignalKind, signal};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        async move {
            tokio::select! {
                _ = interrupt.recv() => {}
                _ = terminate.recv() => {}
            }
        }
    };
    #[cfg(windows)]
    let signal = {
        let mut ctrl_c = tokio::signal::windows::ctrl_c()?;
        async move {
            ctrl_c.recv().await;
        }
    };

    let requested = CancellationToken::new();
    let request = requested.clone();
    tokio::spawn(async move {
        signal.await;
        request.cancel();
    });
    Ok(requested)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn initial_operation_does_not_install_a_signal_listener() {
        let shutdown = Shutdown::default();
        assert_eq!(shutdown.run_until_requested(async { 42 }).await, Some(42));
        assert!(shutdown.0.is_none());
    }

    #[tokio::test]
    async fn requested_shutdown_beats_an_already_ready_operation() {
        let requested = CancellationToken::new();
        requested.cancel();
        let shutdown = Shutdown(Some(requested));
        assert_eq!(shutdown.run_until_requested(async { 42 }).await, None);
    }

    #[tokio::test]
    async fn child_runtime_token_tracks_process_shutdown() {
        let requested = CancellationToken::new();
        let mut shutdown = Shutdown(Some(requested.clone()));
        let child = shutdown.child_token().unwrap();
        requested.cancel();
        tokio::time::timeout(Duration::from_secs(1), child.cancelled())
            .await
            .expect("runtime token did not observe process shutdown");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_signal_while_nobody_waits_is_not_lost() {
        let mut shutdown = Shutdown::default();
        drop(shutdown.requested().unwrap());

        // SAFETY: SIGTERM is handled by the listener registered above.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        tokio::time::timeout(Duration::from_secs(5), shutdown.requested().unwrap())
            .await
            .expect("an earlier signal must still request shutdown");
    }
}
