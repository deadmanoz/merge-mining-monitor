use super::*;

#[tokio::test]
async fn migration_0016_drops_the_0015_receipt_table() -> Result<()> {
    let (client, schema) =
        crate::support::db::new_test_db_through("0015_add_historical_import_artifact").await?;
    let result = async {
        let before: Option<String> = client
            .query_one(
                "SELECT to_regclass('historical_import_artifact')::text",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(before.as_deref(), Some("historical_import_artifact"));

        client
            .batch_execute(include_str!(
                "../../../../../migrations/0016_drop_historical_import_artifact.sql"
            ))
            .await?;

        let after: Option<String> = client
            .query_one(
                "SELECT to_regclass('historical_import_artifact')::text",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(after, None);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    crate::support::db::teardown_test_db(&client, &schema, result).await
}

#[tokio::test]
async fn import_all_state_check_skips_matches_and_reconciles_operator_extras() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let published = header_meeting_bits(0x207f_ffff, 1_700_000_080, 80);
        let extra = header_meeting_bits(0x207f_ffff, 1_700_000_081, 81);
        let extra_csv =
            write_normalized_csv(&extra, "canonical", "", "canonical_parent", &[], 700_081)?;
        let fixture = write_manifest_fixture(&published)?;
        let result = async {
            let classifier =
                ConfiguredParentClassifier::Fake(FakeParentClassifier::new_sequence([
                    canonical_verdict(&published, 700_080),
                    canonical_verdict(&published, 700_080),
                    canonical_verdict(&extra, 700_081),
                    canonical_verdict(&published, 700_080),
                ]));
            let publication = vec![devcoin_publication_config(&fixture)];
            let first = run_historical_import_configs_for_test(
                &mut client,
                &classifier,
                publication.clone(),
            )
            .await?;
            assert_eq!(first.chains[0].1.ingested, 1);
            assert_eq!(first.skipped_matching_state, 0);
            assert_eq!(
                active_source_event_count(&client, "auxpow:devcoin").await?,
                1
            );
            set_only_event_child_hash(&client, Some(vec![0x42_u8; 32])).await?;
            client
                .execute(
                    "UPDATE historical_event_provenance \
                     SET publication_ref = 'a302831000000000000000000000000000000000' \
                     WHERE chain = 'devcoin'",
                    &[],
                )
                .await?;

            let skipped = run_historical_import_configs_for_test(
                &mut client,
                &ConfiguredParentClassifier::Disabled,
                publication.clone(),
            )
            .await?;
            assert_eq!(skipped.skipped_matching_state, 1);
            assert!(skipped.chains.is_empty());
            assert_eq!(
                active_source_event_count(&client, "auxpow:devcoin").await?,
                1
            );
            set_only_event_child_hash(&client, None).await?;

            set_source_health_ready(&client, false).await?;
            let finalized = run_historical_import_configs_for_test(
                &mut client,
                &ConfiguredParentClassifier::Disabled,
                publication.clone(),
            )
            .await?;
            assert_eq!(finalized.skipped_matching_state, 1);
            assert!(finalized.chains.is_empty());
            assert!(source_health_ready(&client).await?);

            client
                .execute(
                    "UPDATE historical_event_provenance SET provenance = 'drifted' \
                     WHERE chain = 'devcoin'",
                    &[],
                )
                .await?;
            let repaired = run_historical_import_configs_for_test(
                &mut client,
                &classifier,
                publication.clone(),
            )
            .await?;
            assert_eq!(repaired.skipped_matching_state, 0);
            assert_eq!(repaired.chains[0].1.ingested, 1);

            run_historical_import(&mut client, &classifier, &devcoin_import_config(&extra_csv))
                .await?;
            assert_eq!(
                active_source_event_count(&client, "auxpow:devcoin").await?,
                2
            );

            let reconciled =
                run_historical_import_configs_for_test(&mut client, &classifier, publication)
                    .await?;
            assert_eq!(reconciled.skipped_matching_state, 0);
            assert_eq!(reconciled.chains[0].1.removed, 1);
            assert_eq!(
                active_source_event_count(&client, "auxpow:devcoin").await?,
                1
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        std::fs::remove_dir_all(&fixture.root)
            .with_context(|| format!("remove fixture root {}", fixture.root.display()))?;
        finish_import_with_cleanup(result, &[&extra_csv])
    })
}

fn devcoin_publication_config(fixture: &ManifestFixture) -> HistoricalImportConfig {
    HistoricalImportConfig {
        chain: "devcoin".into(),
        csv_path: fixture.artifact_path.clone(),
        manifest_path: Some(fixture.config.manifest_path.clone()),
        artifact_root: Some(fixture.config.artifact_root.clone()),
        require_pinned_checkout: false,
        batch_size: 10,
        limit: None,
        allow_empty_known_stales: true,
    }
}

async fn set_source_health_ready(client: &tokio_postgres::Client, ready: bool) -> Result<()> {
    client
        .execute(
            "UPDATE read_model_invariant SET source_health_ready = $1 WHERE id = TRUE",
            &[&ready],
        )
        .await?;
    Ok(())
}

async fn set_only_event_child_hash(
    client: &tokio_postgres::Client,
    child_hash: Option<Vec<u8>>,
) -> Result<()> {
    client
        .execute(
            "UPDATE merge_mining_event SET child_block_hash = $1",
            &[&child_hash],
        )
        .await?;
    Ok(())
}

async fn source_health_ready(client: &tokio_postgres::Client) -> Result<bool> {
    Ok(client
        .query_one(
            "SELECT source_health_ready FROM read_model_invariant WHERE id = TRUE",
            &[],
        )
        .await?
        .get(0))
}
