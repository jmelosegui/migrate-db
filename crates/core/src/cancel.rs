//! Cooperative cancellation (SPEC §7 rule 7, §10.4).
//!
//! The CLI cancels the token on Ctrl-C / SIGTERM. The engine then starts no
//! further step, and providers interrupt the running statement, so the
//! current step rolls back through its normal failure path and the lock is
//! released.

use std::sync::Arc;

use tokio::sync::watch;

/// A cancellation flag that can be awaited. Cheap to clone; all clones share
/// one state.
#[derive(Debug, Clone)]
pub struct CancelToken {
    tx: Arc<watch::Sender<bool>>,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    /// A token that is not cancelled.
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(false);
        Self { tx: Arc::new(tx) }
    }

    /// Request cancellation. Idempotent.
    pub fn cancel(&self) {
        self.tx.send_replace(true);
    }

    /// `true` once [`cancel`](Self::cancel) was called.
    pub fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    /// Resolves when the token is cancelled (immediately if it already is).
    /// Never resolves otherwise.
    pub async fn cancelled(&self) {
        let mut rx = self.tx.subscribe();
        // The sender lives as long as `self`, so `wait_for` can't fail here;
        // if it ever did, waiting forever is the safe answer.
        if rx.wait_for(|c| *c).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_wakes_waiters_and_is_shared_by_clones() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        let waiter = token.clone();
        let handle = tokio::spawn(async move { waiter.cancelled().await });
        tokio::task::yield_now().await;
        token.cancel();
        handle.await.unwrap_or_else(|_| unreachable!());
        assert!(token.is_cancelled());
        // Already cancelled: resolves immediately.
        token.cancelled().await;
    }
}
