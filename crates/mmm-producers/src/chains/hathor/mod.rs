//! Hathor: a genuinely divergent merge-mined chain (public REST API with
//! fallback URL, reconstructed coinbase, nBits-horizon hold semantics, and a
//! cache-backed historical ingest). Relocated under `chains/`; behavior
//! unchanged.

pub(crate) mod address;
pub(crate) mod auxpow;
pub(crate) mod backfill;
pub(crate) mod cache;
pub(crate) mod capture;
pub(crate) mod convert;
pub(crate) mod drain;
pub(crate) mod identity;
pub(crate) mod reconstruct;
pub(crate) mod reward;
pub(crate) mod reward_replay;
pub(crate) mod rpc;

#[cfg(any(test, feature = "db-integration"))]
pub use auxpow::reconstruct_from_blobs;
#[cfg(any(test, feature = "db-integration"))]
pub use cache::{CACHE_CSV_HEADER, HathorCacheConfig, HathorCacheSummary, run_hathor_cache_ingest};
#[cfg(any(test, feature = "db-integration"))]
pub use capture::{HathorCaptureContext, HathorHeightOutcome, process_hathor_height};
#[cfg(any(test, feature = "db-integration"))]
pub use reward::{HATHOR_REWARD_ADDRESS_NAMESPACE, parse_hathor_reward_outputs};
#[cfg(any(test, feature = "db-integration"))]
pub use rpc::{HathorBlockMeta, HathorRpc, HathorTransaction};
