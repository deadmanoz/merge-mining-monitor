//! RSK: a genuinely divergent merge-mined chain (RSKj JSON-RPC `eth_*`
//! interface, canonical + uncle traversal, RLP child headers, miner-address
//! pool attribution). Relocated under `chains/` while preserving behavior.

pub(crate) mod backfill;
pub(crate) mod capture;
pub(crate) mod identity_reresolve;
pub(crate) mod rpc;
pub(crate) mod traverse;

#[cfg(any(test, feature = "db-integration"))]
pub(crate) mod test_fixtures;

#[cfg(any(test, feature = "db-integration"))]
pub use capture::capture_ready_rsk_inputs_for_test;
#[cfg(any(test, feature = "db-integration"))]
pub use capture::{
    BlockOutcome, CaptureDecision, HeightOutcome, RskCaptureContext, RskCaptureInputs,
    prepare_rsk_capture, process_rsk_height,
};
#[cfg(any(test, feature = "db-integration"))]
pub use rpc::RskBlock;
#[cfg(any(test, feature = "db-integration"))]
pub use test_fixtures::load_rsk_block_fixture;
