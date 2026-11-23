//! Cancellation signals for mutating runs (SPEC §7 rule 7, §10.4).
//!
//! Pipeline agents cancel a job by sending Ctrl-C / Ctrl-Break (Windows) or
//! SIGINT / SIGTERM (Linux). The first signal cancels the run's token: the
//! current step is interrupted and rolled back, the lock is released, and the
//! run exits with code 4. A second signal exits immediately with code 4,
//! which is still safe: the server rolls back the open transaction and drops
//! the session lock when the connection closes.

use migrate_db_core::cancel::CancelToken;
use migrate_db_core::error::exit;

/// Stops listening when dropped.
#[derive(Debug)]
pub struct Guard(tokio::task::JoinHandle<()>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Cancel `token` on the first signal; exit on the second.
pub fn on_cancel(token: CancelToken) -> Guard {
    Guard(tokio::spawn(async move {
        if next_signal().await.is_err() {
            return; // can't listen: behave as without a handler
        }
        eprintln!(
            "cancelling: rolling back the current step and releasing the lock \
             (signal again to stop immediately)"
        );
        token.cancel();
        if next_signal().await.is_ok() {
            eprintln!("stopping now; the database rolls back the open transaction");
            std::process::exit(exit::MIGRATION_FAILED);
        }
    }))
}

#[cfg(windows)]
async fn next_signal() -> std::io::Result<()> {
    let mut ctrl_c = tokio::signal::windows::ctrl_c()?;
    let mut ctrl_break = tokio::signal::windows::ctrl_break()?;
    tokio::select! {
        _ = ctrl_c.recv() => {}
        _ = ctrl_break.recv() => {}
    }
    Ok(())
}

#[cfg(unix)]
async fn next_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = interrupt.recv() => {}
        _ = terminate.recv() => {}
    }
    Ok(())
}

#[cfg(not(any(windows, unix)))]
async fn next_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}
