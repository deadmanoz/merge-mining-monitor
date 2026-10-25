use anyhow::Result;
use mmm_capture::capture::{
    CHILD_PAYOUT_REGISTRY_SOURCE, EventPoolAttribution, PoolAttributionConfidence,
    PoolAttributionSide,
};
use mmm_capture::child_payout::NAMECOIN_PAYOUT_ADDRESS_NAMESPACE;
use mmm_capture::identity_registry::IdentityRegistryEntry;
use mmm_capture::protocol_commitment::EXSAT_ACCOUNT_NAMESPACE;
use mmm_producers::seed_protocol_account_identities;
use mmm_read_model::{drain_relationship_refresh_queue, run_relationship_sweep};
use mmm_store::{
    reconcile_identity_registry, sync_event_observation_attribution, upsert_event_pool_attributions,
};
use serde_json::json;
use tokio_postgres::Client;

use crate::protocol_commitments::event_evidence;
use crate::support::scenario::{Scenario, canonical_verdict, revoke_event};
use crate::support::{
    CoreBlock, default_pool_snapshot, mark_protocol_commitments_current, reconcile_command,
    secpool_blocks,
};

/// A header's stored `(mixed, pool_ids, btc_height)`.
type Summary = (bool, Vec<i64>, Option<i32>);

async fn summary(client: &Client, hash: &[u8]) -> Result<Option<Summary>> {
    Ok(client
        .query_opt(
            "SELECT mixed, pool_ids, btc_height FROM relationship_summary \
              WHERE btc_header_hash = $1",
            &[&hash],
        )
        .await?
        .map(|row| (row.get(0), row.get(1), row.get(2))))
}

async fn drain(client: &mut Client) -> Result<()> {
    drain_relationship_refresh_queue(client, 100, 100).await?;
    Ok(())
}

async fn fingerprint(client: &Client) -> Result<Option<String>> {
    Ok(client
        .query_one(
            "SELECT relationship_rules_fingerprint FROM read_model_invariant",
            &[],
        )
        .await?
        .get(0))
}

/// The SecPool transition block captured from a Namecoin event: its coinbase pays SecPool and
/// declares SecPool's CORE address and AntPool's exSat account. Returns it and the event.
async fn capture_transition(client: &mut Client) -> Result<(CoreBlock, i64)> {
    let [_, transition] = secpool_blocks()?;
    let applied = Scenario::new()
        .child_event(event_evidence(
            "transition",
            700_000,
            &transition,
            canonical_verdict(&transition.header, transition.height),
        ))
        .apply(client)
        .await?;
    let event = applied.event_id("transition")?;
    Ok((transition, event))
}

#[tokio::test]
async fn relationship_summaries_follow_their_write_paths_through_the_queue() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (_, mut pool_ids) = default_pool_snapshot(&client).await?;
        seed_protocol_account_identities(&client, &mut pool_ids).await?;
        mark_protocol_commitments_current(&client).await?;
        let (secpool, antpool, f2pool) =
            (pool_ids["secpool"], pool_ids["antpool"], pool_ids["f2pool"]);
        let (transition, event) = capture_transition(&mut client).await?;
        let height = transition.height;
        assert_eq!(
            summary(&client, &transition.hash).await?,
            None,
            "a capture only queues its header"
        );
        drain(&mut client).await?;
        assert_eq!(
            summary(&client, &transition.hash).await?,
            Some((true, vec![secpool, antpool], Some(height)))
        );

        // Re-pointing AntPool's exSat account queues the identity, never its headers; the sweep
        // replaces it with the headers that name the account.
        reconcile_identity_registry(
            &client,
            EXSAT_ACCOUNT_NAMESPACE,
            &[IdentityRegistryEntry {
                identifier: "antpoole.sat",
                pool_slug: "secpool",
                pool_canonical_name: "SECPOOL",
            }],
            &mut pool_ids,
        )
        .await?;
        let queued: Vec<(Option<Vec<u8>>, Option<String>)> = client
            .query(
                "SELECT btc_header_hash, identity_identifier FROM relationship_refresh_queue",
                &[],
            )
            .await?
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        assert!(queued.iter().all(|(hash, _)| hash.is_none()), "{queued:?}");
        assert!(
            queued
                .iter()
                .any(|(_, identifier)| identifier.as_deref() == Some("antpoole.sat")),
            "{queued:?}"
        );
        run_relationship_sweep(&mut client, 100, 10).await?;
        assert_eq!(
            summary(&client, &transition.hash).await?,
            Some((false, vec![secpool], Some(height)))
        );

        // A child attribution naming F2Pool arrives through the per-event attribution sync.
        upsert_event_pool_attributions(
            &client,
            event,
            &[EventPoolAttribution {
                side: PoolAttributionSide::ChildBlock,
                namespace: NAMECOIN_PAYOUT_ADDRESS_NAMESPACE,
                match_kind: "payout_address",
                matched_value: "N1".to_owned(),
                pool_id: Some(f2pool),
                pool_identity_id: None,
                source: CHILD_PAYOUT_REGISTRY_SOURCE,
                confidence: PoolAttributionConfidence::High,
                details: json!({}),
            }],
            0,
        )
        .await?;
        sync_event_observation_attribution(&client, event).await?;
        drain(&mut client).await?;
        assert_eq!(
            summary(&client, &transition.hash).await?,
            Some((true, vec![secpool, f2pool], Some(height)))
        );

        // Revoking the only event takes the evidence away; the summary follows.
        revoke_event(&mut client, event, "test").await?;
        drain(&mut client).await?;
        assert!(
            matches!(
                summary(&client, &transition.hash).await?,
                None | Some((false, _, _))
            ),
            "a parent without evidence is not mixed"
        );

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn the_rebuild_records_its_fingerprint_and_the_sweep_repairs_drift() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (_, mut pool_ids) = default_pool_snapshot(&client).await?;
        seed_protocol_account_identities(&client, &mut pool_ids).await?;
        let (secpool, antpool) = (pool_ids["secpool"], pool_ids["antpool"]);
        let (transition, _) = capture_transition(&mut client).await?;
        let height = transition.height;
        let rebuild = ["--rebuild-relationship-signals", "--if-needed"];

        // Before the protocol commitments are current the summary omits declared accounts, as
        // the API does, and the rebuild leaves the fingerprint unrecorded.
        assert!(!reconcile_command(&mut client, &rebuild).await?);
        assert_eq!(
            summary(&client, &transition.hash).await?,
            Some((false, vec![secpool], Some(height)))
        );
        assert_eq!(fingerprint(&client).await?, None);

        mark_protocol_commitments_current(&client).await?;
        reconcile_command(&mut client, &rebuild).await?;
        assert_eq!(
            summary(&client, &transition.hash).await?,
            Some((true, vec![secpool, antpool], Some(height)))
        );
        assert!(fingerprint(&client).await?.is_some());

        // A row a write path failed to queue survives the rebuild a current fingerprint skips,
        // and the sweep repairs and counts it. The first sweep, run like the scheduled unit
        // without Core, clears what the rebuild left queued (the seeded identities among them).
        let sweep_command = ["--sweep-relationship-signals", "--batch-size", "100"];
        assert!(!reconcile_command(&mut client, &sweep_command).await?);
        client
            .execute(
                "UPDATE relationship_summary SET mixed = NOT mixed WHERE btc_header_hash = $1",
                &[&transition.hash],
            )
            .await?;
        reconcile_command(&mut client, &rebuild).await?;
        assert_eq!(
            summary(&client, &transition.hash).await?,
            Some((false, vec![secpool, antpool], Some(height))),
            "a current fingerprint skips the rebuild"
        );
        let sweep = run_relationship_sweep(&mut client, 100, 10).await?;
        assert_eq!((sweep.drifted, sweep.pass_completed), (1, true));
        assert_eq!(
            summary(&client, &transition.hash).await?,
            Some((true, vec![secpool, antpool], Some(height)))
        );

        Ok::<_, anyhow::Error>(())
    })
}
