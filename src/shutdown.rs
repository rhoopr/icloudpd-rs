//! Graceful shutdown coordinator.
//!
//! Listens for SIGINT (Ctrl+C), SIGTERM, and SIGHUP, then cancels a
//! [`tokio_util::sync::CancellationToken`] so the download pipeline can
//! drain in-flight work before exiting. A second signal force-exits.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

/// Install signal handlers and return a [`CancellationToken`] that is
/// cancelled on the first SIGINT / SIGTERM / SIGHUP.  A second signal
/// force-exits the process.
pub(crate) fn install_signal_handler() -> CancellationToken {
    let token = CancellationToken::new();
    let count = Arc::new(AtomicU32::new(0));

    let handler_token = token.clone();
    tokio::spawn(async move {
        // Create signal listeners once, reuse across iterations.
        #[cfg(unix)]
        let (mut sigterm, mut sighup) = {
            use tokio::signal::unix::{signal, SignalKind};
            (
                signal(SignalKind::terminate()).expect("failed to register SIGTERM handler"),
                signal(SignalKind::hangup()).expect("failed to register SIGHUP handler"),
            )
        };

        loop {
            #[cfg(unix)]
            {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = sigterm.recv() => {}
                    _ = sighup.recv() => {}
                }
            }

            #[cfg(not(unix))]
            {
                tokio::signal::ctrl_c()
                    .await
                    .expect("failed to listen for Ctrl+C");
            }

            let prev = count.fetch_add(1, Ordering::SeqCst);
            if prev == 0 {
                tracing::info!("Received shutdown signal, finishing current downloads...");
                tracing::info!("Press Ctrl+C again to force exit");
                handler_token.cancel();
            } else {
                tracing::warn!("Force exit requested");
                std::process::exit(130);
            }
        }
    });

    token
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_starts_uncancelled() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
    }

    #[test]
    fn child_tokens_observe_parent_cancel() {
        let parent = CancellationToken::new();
        let child = parent.child_token();
        parent.cancel();
        assert!(child.is_cancelled());
    }

    /// Verify that `install_signal_handler` returns a live, uncancelled token
    /// (signal delivery can't be safely tested in a shared test binary).
    #[tokio::test]
    async fn install_returns_live_token() {
        let token = install_signal_handler();
        assert!(!token.is_cancelled());
    }
}
