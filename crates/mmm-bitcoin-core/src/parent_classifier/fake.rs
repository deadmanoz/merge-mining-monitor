//! The db-integration-gated Fake classifier family for tests: queued
//! verdicts plus the first-call gate for concurrency tests.

use super::*;

#[cfg(any(test, feature = "db-integration"))]
#[derive(Clone)]
pub struct FakeParentClassifier {
    state: Arc<tokio::sync::Mutex<FakeParentClassifierState>>,
    first_call_gate: Option<Arc<FakeParentClassifierGate>>,
    synced_tip_height: Option<i32>,
    synced_tip_fresh: bool,
    fail_synced_tip: bool,
    epoch_nbits: std::collections::HashMap<i32, EpochNbits>,
    fail_epoch_nbits: bool,
}

#[cfg(any(test, feature = "db-integration"))]
pub(crate) struct FakeParentClassifierState {
    results: VecDeque<ParentClassification>,
}

#[cfg(any(test, feature = "db-integration"))]
pub struct FakeParentClassifierGate {
    started: Notify,
    proceed: Notify,
    used: AtomicBool,
}

#[cfg(any(test, feature = "db-integration"))]
impl FakeParentClassifier {
    pub fn new(result: ParentClassification) -> Self {
        Self::new_sequence([result])
    }

    pub fn new_sequence<I>(results: I) -> Self
    where
        I: IntoIterator<Item = ParentClassification>,
    {
        let results = results.into_iter().collect::<VecDeque<_>>();
        assert!(
            !results.is_empty(),
            "fake parent classifier needs at least one result"
        );
        Self {
            state: Arc::new(tokio::sync::Mutex::new(FakeParentClassifierState {
                results,
            })),
            first_call_gate: None,
            synced_tip_height: None,
            synced_tip_fresh: true,
            fail_synced_tip: false,
            epoch_nbits: std::collections::HashMap::new(),
            fail_epoch_nbits: false,
        }
    }

    pub fn with_first_call_gate(mut self, gate: Arc<FakeParentClassifierGate>) -> Self {
        self.first_call_gate = Some(gate);
        self
    }

    pub fn with_synced_tip_height(mut self, height: i32) -> Self {
        self.synced_tip_height = Some(height);
        self
    }

    /// A synced tip that is STALE (its median time is too old): the far-future
    /// resolver must HOLD rather than revoke a beyond-tolerance parent against a
    /// lagging / isolated node.
    pub fn with_stale_synced_tip(mut self, height: i32) -> Self {
        self.synced_tip_height = Some(height);
        self.synced_tip_fresh = false;
        self
    }

    /// Make `synced_tip_height` return `Err` (Core unreachable), so the resolver's
    /// fail-closed-to-Hold path can be exercised.
    pub fn with_synced_tip_error(mut self) -> Self {
        self.fail_synced_tip = true;
        self
    }

    /// Register the canonical nBits + header time for a DAA epoch-start height.
    pub fn with_epoch_nbits(
        mut self,
        epoch_start_height: i32,
        nbits: u32,
        header_time: i64,
    ) -> Self {
        self.epoch_nbits
            .insert(epoch_start_height, EpochNbits { nbits, header_time });
        self
    }

    /// Make `epoch_nbits` return `Err` (epoch-header fetch fails), so the
    /// resolver's fail-closed-to-Hold path can be exercised.
    pub fn with_epoch_nbits_error(mut self) -> Self {
        self.fail_epoch_nbits = true;
        self
    }

    pub(crate) async fn synced_tip_height(&self) -> Result<Option<i32>> {
        if self.fail_synced_tip {
            bail!("fake classifier: injected synced_tip_height error");
        }
        Ok(self.synced_tip_height)
    }

    pub(crate) async fn synced_tip(&self) -> Result<Option<SyncedTip>> {
        if self.fail_synced_tip {
            bail!("fake classifier: injected synced_tip error");
        }
        Ok(self.synced_tip_height.map(|height| SyncedTip {
            height,
            fresh: self.synced_tip_fresh,
        }))
    }

    pub(crate) async fn epoch_nbits(
        &self,
        epoch_start_height: i32,
        _synced_tip: i32,
    ) -> Result<EpochNbits> {
        if self.fail_epoch_nbits {
            bail!("fake classifier: injected epoch_nbits error");
        }
        self.epoch_nbits
            .get(&epoch_start_height)
            .copied()
            .with_context(|| format!("fake classifier: no epoch nBits for {epoch_start_height}"))
    }

    pub(crate) async fn classify_parent(
        &self,
        _header: &Header,
        _preflight: ParentPreflight,
    ) -> Result<ParentClassification> {
        if let Some(gate) = &self.first_call_gate
            && !gate.used.swap(true, Ordering::SeqCst)
        {
            gate.started.notify_waiters();
            gate.proceed.notified().await;
        }

        let mut state = self.state.lock().await;
        if state.results.len() > 1 {
            Ok(state
                .results
                .pop_front()
                .expect("fake classifier sequence was checked as non-empty"))
        } else {
            Ok(state
                .results
                .front()
                .expect("fake classifier sequence was checked as non-empty")
                .clone())
        }
    }
}

#[cfg(any(test, feature = "db-integration"))]
impl FakeParentClassifierGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            started: Notify::new(),
            proceed: Notify::new(),
            used: AtomicBool::new(false),
        })
    }

    pub async fn wait_started(&self) {
        self.started.notified().await;
    }

    pub fn proceed(&self) {
        self.proceed.notify_waiters();
    }
}
