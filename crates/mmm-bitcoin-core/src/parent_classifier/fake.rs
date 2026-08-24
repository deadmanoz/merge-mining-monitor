//! The db-integration-gated Fake classifier family for tests: queued
//! verdicts plus the first-call gate for concurrency tests.

use super::*;

#[cfg(any(test, feature = "db-integration"))]
#[derive(Clone)]
pub struct FakeParentClassifier {
    state: Arc<tokio::sync::Mutex<FakeParentClassifierState>>,
    first_call_gate: Option<Arc<FakeParentClassifierGate>>,
    synced_tip_is_mainnet: bool,
    synced_tip_height: Option<i32>,
    synced_tip_fresh: bool,
    fail_synced_tip: bool,
    canonical_headers: std::collections::HashMap<i32, CoreHeader>,
    max_concurrency: usize,
}

#[cfg(any(test, feature = "db-integration"))]
pub(crate) struct FakeParentClassifierState {
    results: VecDeque<ParentClassification>,
    calls: u64,
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
                calls: 0,
            })),
            first_call_gate: None,
            synced_tip_is_mainnet: true,
            synced_tip_height: None,
            synced_tip_fresh: true,
            fail_synced_tip: false,
            canonical_headers: std::collections::HashMap::new(),
            max_concurrency: 1,
        }
    }

    pub fn with_first_call_gate(mut self, gate: Arc<FakeParentClassifierGate>) -> Self {
        self.first_call_gate = Some(gate);
        self
    }

    pub fn with_max_concurrency(mut self, max_concurrency: usize) -> Self {
        assert!(max_concurrency > 0, "fake concurrency must be positive");
        self.max_concurrency = max_concurrency;
        self
    }

    pub fn with_synced_tip_height(mut self, height: i32) -> Self {
        self.synced_tip_height = Some(height);
        self
    }

    /// A synced non-mainnet tip, for commands that must refuse to populate the
    /// monitor's Bitcoin-mainnet cache from a testnet, signet, or regtest node.
    pub fn with_non_mainnet_synced_tip(mut self, height: i32) -> Self {
        self.synced_tip_height = Some(height);
        self.synced_tip_is_mainnet = false;
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

    /// Register one canonical Core header for the persisted cache refresher.
    pub fn with_canonical_header(mut self, header: CoreHeader) -> Self {
        self.canonical_headers.insert(header.height, header);
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
            is_mainnet: self.synced_tip_is_mainnet,
            height,
            fresh: self.synced_tip_fresh,
        }))
    }

    pub(crate) async fn canonical_header(&self, height: i32) -> Result<CoreHeader> {
        self.canonical_headers
            .get(&height)
            .copied()
            .with_context(|| format!("fake classifier: no canonical header at {height}"))
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
        state.calls += 1;
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

    pub async fn call_count(&self) -> u64 {
        self.state.lock().await.calls
    }

    pub(crate) fn max_concurrency(&self) -> usize {
        self.max_concurrency
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
