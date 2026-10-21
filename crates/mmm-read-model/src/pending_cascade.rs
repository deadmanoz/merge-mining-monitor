//! Shutdown fence for dependent cascades that run after their mutation commits.
//!
//! An in-memory reconcile cascade commits item by item while its remaining
//! queue lives only in memory, so dropping it part-way leaves dependents stale.
//! The drain holds a [`PendingCascade`], as does a `CommittedParentMutation`
//! from just before its commit until its cascade returns, and a live producer
//! waits for [`pending_cascades_settled`] before it drops an in-flight tick on
//! shutdown.

use std::sync::LazyLock;

use tokio::sync::watch;

static PENDING: LazyLock<watch::Sender<usize>> = LazyLock::new(|| watch::Sender::new(0));

#[must_use = "a pending cascade is fenced only while the guard is alive"]
pub(crate) struct PendingCascade(());

impl PendingCascade {
    pub(crate) fn open() -> Self {
        PENDING.send_modify(|count| *count += 1);
        Self(())
    }
}

impl Drop for PendingCascade {
    fn drop(&mut self) {
        PENDING.send_modify(|count| *count -= 1);
    }
}

/// Resolve once no committed mutation is still running its dependent cascade.
pub async fn pending_cascades_settled() {
    // The static sender is never dropped, so the wait cannot fail.
    let _ = PENDING.subscribe().wait_for(|count| *count == 0).await;
}
