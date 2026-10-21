//! The write a capture makes: an event, or a near's observation alone.

use anyhow::Result;
use tokio_postgres::Transaction;

use mmm_capture::capture::{ChildBlockObservation, MergeMiningEventPayload, ParentKind};

use crate::event::{EventWriteOutcome, upsert_merge_mining_event_with_attributions};
use crate::observation::write_event_observation;

/// What a capture wrote: an event for a Bitcoin-relevant parent, or only the observation
/// of a near (a parent that misses Bitcoin's target), which no event backs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureWrite {
    Event(EventWriteOutcome),
    Near { observation_id: i64 },
}

/// Write one captured child block: a near as its observation alone, anything else as an
/// event with its observation, attributions and provenance. The writes span several
/// statements, so a rejected one rolls back with the caller's transaction.
pub async fn write_capture(
    client: &Transaction<'_>,
    source_id: i64,
    payload: &MergeMiningEventPayload,
) -> Result<CaptureWrite> {
    if payload.btc_parent_kind == ParentKind::Near {
        let observation_id =
            write_event_observation(client, source_id, &ChildBlockObservation::near(payload))
                .await?;
        return Ok(CaptureWrite::Near { observation_id });
    }
    upsert_merge_mining_event_with_attributions(client, source_id, payload)
        .await
        .map(CaptureWrite::Event)
}
