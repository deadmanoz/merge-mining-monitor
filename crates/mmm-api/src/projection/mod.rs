//! Read-model projection for the release read API endpoints.

use super::error::ApiError;

mod block;
mod branch_summary;
mod materialize;
mod navigator;
mod shared;
mod sources;
mod stale_navigation;
mod tree;

pub use block::block;
pub use navigator::navigator;
pub use sources::sources;
pub use tree::tree;
#[cfg(feature = "db-integration")]
pub use {
    block::{BlockPayload, EventDetail},
    navigator::{
        NavigatorBranch, NavigatorItem, NavigatorOrphan, NavigatorPayload, NavigatorPosition,
        NavigatorView, OrphanClassCounts,
    },
    shared::{ChildChainEvidence, PoolObject, ProofState, SourceSummary, TreeCompetition},
    sources::{SourceEndpointRecord, SourcesPayload},
    tree::{TreeEdge, TreePayload},
};

/// Projection failure split: `Api` carries a client-facing `ApiError` (mapped
/// to the locked error envelope), `Internal` carries an opaque `anyhow::Error`
/// surfaced as a 500. Handlers and the integration-test host match on this
/// variant to assert envelope vs internal-fault behaviour.
#[derive(Debug)]
pub enum ProjectionError {
    Api(ApiError),
    Internal(anyhow::Error),
}

impl From<ApiError> for ProjectionError {
    fn from(err: ApiError) -> Self {
        Self::Api(err)
    }
}

impl From<anyhow::Error> for ProjectionError {
    fn from(err: anyhow::Error) -> Self {
        Self::Internal(err)
    }
}
