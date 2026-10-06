use super::*;

#[tokio::test]
async fn complete_live_publication_retires_claims_without_removing_observations() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let retained = header_meeting_bits(0x207f_ffff, 1_700_000_100, 100);
        let omitted = btc_400000_header()?;
        let (initial, revised) = live_publication_refresh_fixtures(&retained, &omitted)?;
        let result = async {
            let mut initial_config = devcoin_publication_config(&initial);
            initial_config.chain = "namecoin".into();
            run_historical_import_configs_for_test(&mut client,
                &ConfiguredParentClassifier::Fake(FakeParentClassifier::new_sequence([
                    canonical_verdict(&retained, 700_100),
                    crate::support::scenario::orphan_candidate_verdict(&omitted),
                ])), vec![initial_config]).await?;
            assert_eq!(block_kind_and_orphan_class(&client, &omitted).await?,
                ("unknown".into(), Some("excluded".into())));
            client.execute("UPDATE historical_event_provenance SET publication_ref = \
                'a302831000000000000000000000000000000000' WHERE chain = 'namecoin'", &[]).await?;
            add_independent_live_evidence(&mut client, &retained, &revised.artifact_path).await?;
            assert_eq!(client.query_one("SELECT count(*) FROM attestation_proof WHERE source_id = 1", &[])
                .await?.get::<_, i64>(0), 2);
            let base_before = live_observation_snapshot(&client).await?;
            let protected_before = independent_provenance_snapshot(&client).await?;
            let mut revised_config = devcoin_publication_config(&revised);
            revised_config.chain = "namecoin".into();
            // Observe actual durable queue writes at the import boundary. A
            // provenance refresh must not enqueue unchanged canonical parents.
            client.batch_execute("CREATE TABLE publication_reconcile_enqueues (hash bytea); \
                CREATE FUNCTION audit_publication_enqueue() RETURNS trigger LANGUAGE plpgsql AS $$ \
                BEGIN INSERT INTO publication_reconcile_enqueues VALUES (NEW.btc_parent_header_hash); \
                RETURN NEW; END $$; \
                CREATE TRIGGER publication_enqueue_audit AFTER INSERT OR UPDATE \
                ON historical_reconcile_queue FOR EACH ROW EXECUTE FUNCTION audit_publication_enqueue()")
                .await?;
            let refreshed = run_historical_import_configs_for_test(&mut client,
                &absent_classifier(&omitted), vec![revised_config.clone()]).await?;
            assert_eq!(refreshed.skipped_matching_state, 0, "an omitted Research claim requires replacement");
            assert_eq!(refreshed.chains[0].1.removed, 0);
            let enqueued = client.query("SELECT DISTINCT hash FROM publication_reconcile_enqueues", &[]).await?
                .into_iter().map(|row| row.get::<_,Vec<u8>>(0)).collect::<Vec<_>>();
            assert_eq!(enqueued, vec![omitted.block_hash().to_byte_array().to_vec()],
                "only the retired stale attestation needs provenance reconciliation");
            assert_eq!(live_observation_snapshot(&client).await?, base_before);
            assert_eq!(independent_provenance_snapshot(&client).await?, protected_before);
            assert_eq!(client.query_one("SELECT count(*) FROM historical_event_provenance \
                WHERE publication_ref = 'a302831000000000000000000000000000000000'", &[]).await?.get::<_,i64>(0), 0);
            assert_eq!(block_kind_and_orphan_class(&client, &omitted).await?, ("unknown".into(), Some("strict_btc_orphan".into())),
                "the withdrawn claim must stop excluding the retained unknown observation");
            // A matching current row must not hide contradictory prior-pin provenance.
            client.execute("INSERT INTO historical_event_provenance (event_id, publication_ref, chain, \
                source_kind, source_path, source_row_number, artifact_scope, provenance, classification, \
                btc_height, validation_status, btc_stale_relevance, relevance_reason) \
                SELECT event_id, 'c302831000000000000000000000000000000000', chain, source_kind, \
                source_path, source_row_number, artifact_scope, 'superseded-source-claim', classification, \
                btc_height, validation_status, btc_stale_relevance, relevance_reason \
                FROM historical_event_provenance WHERE publication_ref = $1 AND chain = 'namecoin'",
                &[&pinned_publication_ref()]).await?;
            let repaired = run_historical_import_configs_for_test(&mut client,
                &absent_classifier(&omitted), vec![revised_config.clone()]).await?;
            assert_eq!(repaired.skipped_matching_state, 0);
            assert_eq!(independent_provenance_snapshot(&client).await?, protected_before);
            client.execute("UPDATE historical_event_provenance SET publication_ref = \
                'd302831000000000000000000000000000000000' WHERE publication_ref = $1 AND chain = 'namecoin'",
                &[&pinned_publication_ref()]).await?;
            let repeated = run_historical_import_configs_for_test(&mut client,
                &ConfiguredParentClassifier::Disabled, vec![revised_config]).await?;
            assert_eq!(repeated.skipped_matching_state, 1);
            assert!(repeated.chains.is_empty());
            assert_eq!(live_observation_snapshot(&client).await?, base_before);
            client.execute("TRUNCATE publication_reconcile_enqueues", &[]).await?;
            let mut reinstated = devcoin_publication_config(&initial);
            reinstated.chain = "namecoin".into();
            run_historical_import_configs_for_test(&mut client,
                &absent_classifier(&omitted), vec![reinstated]).await?;
            assert_eq!(block_kind_and_orphan_class(&client, &omitted).await?,
                ("unknown".into(), Some("excluded".into())),
                "a reinstated stale claim must update the retained observation's attestation gate");
            assert_eq!(client.query_one("SELECT count(*) FROM publication_reconcile_enqueues \
                WHERE hash=$1", &[&retained.block_hash().to_byte_array().to_vec()]).await?.get::<_,i64>(0), 0);
            assert!(client.query_one("SELECT count(*) FROM publication_reconcile_enqueues \
                WHERE hash=$1", &[&omitted.block_hash().to_byte_array().to_vec()]).await?.get::<_,i64>(0) > 0);
            Ok::<_, anyhow::Error>(())
        }.await;
        for fixture in [&initial, &revised] {
            std::fs::remove_dir_all(&fixture.root)?;
        }
        result
    })
}

fn live_publication_refresh_fixtures(
    retained: &Header,
    omitted: &Header,
) -> Result<(ManifestFixture, ManifestFixture)> {
    let script = btc_400000_coinbase_script()?;
    let retained_row = normalized_csv_line(
        retained,
        &NormalizedCsvRow {
            chain: "namecoin",
            source_row_number: 1,
            classification: "canonical",
            relevance: "",
            relevance_reason: "canonical_parent",
            coinbase_script: &[],
            btc_height: 700_100,
            child_height: 12,
            child_hash: Some(&[0x77; 32]),
        },
    );
    let omitted_row = normalized_csv_line(
        omitted,
        &NormalizedCsvRow {
            chain: "namecoin",
            source_row_number: 2,
            classification: "unknown",
            relevance: "",
            relevance_reason: "valid_direct_stale",
            coinbase_script: &script,
            btc_height: 400_000,
            child_height: 13,
            child_hash: None,
        },
    );
    let initial = write_manifest_fixture_rows_for_chain(
        "namecoin",
        &[retained_row.clone(), omitted_row],
        serde_json::json!({"canonical":1,"stale":1,"stale_descendant":0,"strict_btc_orphan":0,"weak_btc_orphan":0}),
        0,
    )?;
    let revised = write_manifest_fixture_rows_for_chain(
        "namecoin",
        &[retained_row],
        serde_json::json!({"canonical":1,"stale":0,"stale_descendant":0,"strict_btc_orphan":0,"weak_btc_orphan":0}),
        0,
    )?;
    Ok((initial, revised))
}

async fn add_independent_live_evidence(
    client: &mut tokio_postgres::Client,
    retained: &Header,
    operator_csv: &Path,
) -> Result<()> {
    let mut operator_config = HistoricalImportConfig::for_csv("namecoin", operator_csv);
    operator_config.allow_empty_known_stales = true;
    run_historical_import(
        client,
        &ConfiguredParentClassifier::Fake(FakeParentClassifier::new(canonical_verdict(
            retained, 700_100,
        ))),
        &operator_config,
    )
    .await?;
    // Exercise aggregate ownership protection, not scientific error admission.
    client
        .execute(
            "INSERT INTO historical_event_provenance (event_id, publication_ref, \
        chain, source_kind, source_path, source_row_number, artifact_scope, provenance, \
        classification, btc_height, validation_status, btc_stale_relevance, relevance_reason) \
        SELECT event_id, 'b302831000000000000000000000000000000000', chain, source_kind, \
        '<retained-error-owner>', source_row_number, 'error-block-observations', provenance, \
        classification, btc_height, validation_status, btc_stale_relevance, relevance_reason \
        FROM historical_event_provenance WHERE publication_ref = 'operator-csv'",
            &[],
        )
        .await?;
    let unowned = seed_unpublished_event(client, "auxpow:namecoin", 99, vec![0x55; 32]).await?;
    client
        .execute(
            "UPDATE merge_mining_event SET revoked_at = 1234 WHERE id = $1",
            &[&unowned],
        )
        .await?;
    client.execute("INSERT INTO event_pool_attribution (event_id, side, namespace, match_kind, \
        matched_value, source, confidence, first_seen_at, last_seen_at) \
        SELECT id, 'btc_parent', 'btc_coinbase_tag', 'test_seed', 'retained-witness', \
        'test_seed', 'high', 1, 1 FROM merge_mining_event WHERE source_id = 1 AND child_height = 13", &[]).await?;
    Ok(())
}

async fn live_observation_snapshot(client: &tokio_postgres::Client) -> Result<String> {
    Ok(client.query_one("SELECT jsonb_build_object( \
        'events', (SELECT jsonb_agg(to_jsonb(e) ORDER BY e.id) FROM merge_mining_event e WHERE source_id = 1), \
        'attribution', (SELECT jsonb_agg(to_jsonb(a) ORDER BY a.event_id) FROM event_pool_attribution a), \
        'proofs', (SELECT jsonb_agg(to_jsonb(p) ORDER BY p.id) FROM attestation_proof p WHERE source_id = 1))::text",
        &[]).await?.get(0))
}

async fn independent_provenance_snapshot(client: &tokio_postgres::Client) -> Result<String> {
    Ok(client
        .query_one(
            "SELECT jsonb_agg(to_jsonb(p) ORDER BY p.publication_ref, p.chain, p.source_path, p.source_row_number)::text \
        FROM historical_event_provenance p WHERE publication_ref = 'operator-csv' \
        OR artifact_scope = 'error-block-observations'",
            &[],
        )
        .await?
        .get(0))
}

#[tokio::test]
async fn import_all_preserves_operator_evidence_and_removes_unowned_extras() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let published = header_meeting_bits(0x207f_ffff, 1_700_000_080, 80);
        let extra = header_meeting_bits(0x207f_ffff, 1_700_000_081, 81);
        let extra_csv =
            write_normalized_csv(&extra, "canonical", "", "canonical_parent", &[], 700_081)?;
        let fixture = write_manifest_fixture(&published)?;
        let result = async {
            let fake = FakeParentClassifier::new_sequence([
                canonical_verdict(&published, 700_080),
                canonical_verdict(&extra, 700_081),
            ]);
            let classifier = ConfiguredParentClassifier::Fake(fake.clone());
            let publication = vec![devcoin_publication_config(&fixture)];
            let first = run_historical_import_configs_for_test(
                &mut client,
                &classifier,
                publication.clone(),
            )
            .await?;
            assert_eq!(first.chains[0].1.ingested, 1);
            assert_eq!(first.skipped_matching_state, 0);
            assert_eq!(fake.call_count().await, 1);
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
            assert_eq!(fake.call_count().await, 1);

            run_historical_import(&mut client, &classifier, &devcoin_import_config(&extra_csv))
                .await?;
            assert_eq!(fake.call_count().await, 2);
            assert_eq!(
                active_source_event_count(&client, "auxpow:devcoin").await?,
                2
            );

            verify_operator_preservation_and_unowned_cleanup(&mut client, &classifier, publication)
                .await?;
            assert_eq!(fake.call_count().await, 2);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        std::fs::remove_dir_all(&fixture.root)
            .with_context(|| format!("remove fixture root {}", fixture.root.display()))?;
        finish_import_with_cleanup(result, &[&extra_csv])
    })
}

async fn verify_operator_preservation_and_unowned_cleanup(
    client: &mut tokio_postgres::Client,
    classifier: &ConfiguredParentClassifier,
    publication: Vec<HistoricalImportConfig>,
) -> Result<()> {
    let operator_id: i64 = client.query_one(
        "SELECT event_id FROM historical_event_provenance WHERE publication_ref = 'operator-csv'",
        &[],
    ).await?.get(0);
    let preserved =
        run_historical_import_configs_for_test(client, classifier, publication.clone()).await?;
    assert_eq!(preserved.skipped_matching_state, 1);
    assert!(preserved.chains.is_empty());
    let unowned_id = seed_unpublished_event(client, "auxpow:devcoin", 99, vec![0x45; 32]).await?;
    let reconciled =
        run_historical_import_configs_for_test(client, classifier, publication.clone()).await?;
    assert_eq!(reconciled.skipped_matching_state, 0);
    assert_eq!(reconciled.chains[0].1.removed, 1);
    assert_eq!(
        active_source_event_count(client, "auxpow:devcoin").await?,
        2
    );
    assert!(
        client
            .query_opt(
                "SELECT id FROM merge_mining_event WHERE id = $1",
                &[&operator_id]
            )
            .await?
            .is_some()
    );
    assert!(
        client
            .query_opt(
                "SELECT id FROM merge_mining_event WHERE id = $1",
                &[&unowned_id]
            )
            .await?
            .is_none()
    );
    let repeated = run_historical_import_configs_for_test(
        client,
        &ConfiguredParentClassifier::Disabled,
        publication,
    )
    .await?;
    assert_eq!(repeated.skipped_matching_state, 1);
    assert!(repeated.chains.is_empty());
    Ok(())
}

#[tokio::test]
async fn changed_artifact_reuses_safe_persisted_parent_classification() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let published = header_meeting_bits(0x207f_ffff, 1_700_000_085, 85);
        let fixture = write_manifest_fixture(&published)?;
        let result = async {
            let initial = FakeParentClassifier::new(canonical_verdict(&published, 700_085));
            let first = run_historical_import_configs_for_test(
                &mut client,
                &ConfiguredParentClassifier::Fake(initial.clone()),
                vec![devcoin_publication_config(&fixture)],
            )
            .await?;
            assert_eq!(first.chains[0].1.ingested, 1);
            assert_eq!(initial.call_count().await, 1);

            client
                .execute(
                    "UPDATE historical_event_provenance SET provenance = 'drifted' \
                     WHERE chain = 'devcoin'",
                    &[],
                )
                .await?;
            let persisted = FakeParentClassifier::new(canonical_verdict(&published, 700_085))
                .with_classification_error_on_call(1);
            let repaired = run_historical_import_configs_for_test(
                &mut client,
                &ConfiguredParentClassifier::Fake(persisted.clone()),
                vec![devcoin_publication_config(&fixture)],
            )
            .await?;
            assert_eq!(repaired.chains[0].1.ingested, 1);
            assert_eq!(persisted.call_count().await, 0);

            client
                .execute(
                    "UPDATE historical_event_provenance SET provenance = 'drifted-unattested' \
                     WHERE chain = 'devcoin'",
                    &[],
                )
                .await?;
            client
                .execute(
                    "UPDATE block SET core_attested = FALSE, live_observed = FALSE \
                     WHERE btc_header_hash = $1",
                    &[&published.block_hash().to_byte_array().to_vec()],
                )
                .await?;
            let unattested = FakeParentClassifier::new(canonical_verdict(&published, 700_085));
            let upgraded = run_historical_import_configs_for_test(
                &mut client,
                &ConfiguredParentClassifier::Fake(unattested.clone()),
                vec![devcoin_publication_config(&fixture)],
            )
            .await?;
            assert_eq!(upgraded.chains[0].1.ingested, 1);
            assert_eq!(unattested.call_count().await, 1);

            client
                .execute(
                    "UPDATE historical_event_provenance SET provenance = 'drifted-again' \
                     WHERE chain = 'devcoin'",
                    &[],
                )
                .await?;
            client
                .execute(
                    "UPDATE block \
                     SET kind = 'unknown', btc_height = NULL, btc_height_source = NULL, \
                         canonical_competitor_hash = NULL \
                     WHERE btc_header_hash = $1",
                    &[&published.block_hash().to_byte_array().to_vec()],
                )
                .await?;
            let fallback = FakeParentClassifier::new(canonical_verdict(&published, 700_085));
            let refreshed = run_historical_import_configs_for_test(
                &mut client,
                &ConfiguredParentClassifier::Fake(fallback.clone()),
                vec![devcoin_publication_config(&fixture)],
            )
            .await?;
            assert_eq!(refreshed.chains[0].1.ingested, 1);
            assert_eq!(fallback.call_count().await, 1);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        std::fs::remove_dir_all(&fixture.root)
            .with_context(|| format!("remove fixture root {}", fixture.root.display()))?;
        result
    })
}

#[tokio::test]
async fn changed_artifact_reuses_stale_only_while_competitor_is_intact() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let published = header_meeting_bits(0x207f_ffff, 1_700_000_086, 86);
        let competitor = header_meeting_bits(0x207f_ffff, 1_700_000_087, 87);
        let competitor_hash = competitor.block_hash().to_byte_array().to_vec();
        let row = normalized_csv_line(
            &published,
            &NormalizedCsvRow {
                chain: "devcoin",
                source_row_number: 1,
                classification: "stale",
                relevance: "",
                relevance_reason: "valid_direct_stale",
                coinbase_script: &[],
                btc_height: 700_086,
                child_height: 12,
                child_hash: None,
            },
        );
        let fixture = write_manifest_fixture_rows_with_counts(
            &[row],
            serde_json::json!({
                "canonical": 0,
                "stale": 1,
                "stale_descendant": 0,
                "strict_btc_orphan": 0,
                "weak_btc_orphan": 0
            }),
            0,
        )?;
        let inferred_stale = || {
            let mut classification = stale_verdict_with_competitor_header(
                &published,
                700_086,
                competitor,
                competitor_hash.clone(),
            );
            classification.height_source = Some(mmm_bitcoin_core::HeightSource::PrevCanonical);
            classification.live_observed = false;
            classification.core_attested = false;
            classification
        };
        let result = async {
            let initial = FakeParentClassifier::new(inferred_stale());
            run_historical_import_configs_for_test(
                &mut client,
                &ConfiguredParentClassifier::Fake(initial.clone()),
                vec![devcoin_publication_config(&fixture)],
            )
            .await?;
            assert_eq!(initial.call_count().await, 3);

            client
                .execute(
                    "UPDATE historical_event_provenance SET provenance = 'drifted' \
                     WHERE chain = 'devcoin'",
                    &[],
                )
                .await?;
            let persisted =
                FakeParentClassifier::new(inferred_stale()).with_classification_error_on_call(1);
            run_historical_import_configs_for_test(
                &mut client,
                &ConfiguredParentClassifier::Fake(persisted.clone()),
                vec![devcoin_publication_config(&fixture)],
            )
            .await?;
            assert_eq!(persisted.call_count().await, 0);

            client
                .execute(
                    "UPDATE historical_event_provenance SET provenance = 'drifted-again' \
                     WHERE chain = 'devcoin'",
                    &[],
                )
                .await?;
            client
                .execute(
                    "UPDATE block SET kind = 'unknown', btc_height = NULL, \
                         btc_height_source = NULL \
                     WHERE btc_header_hash = $1",
                    &[&competitor_hash],
                )
                .await?;
            let fallback = FakeParentClassifier::new(inferred_stale());
            run_historical_import_configs_for_test(
                &mut client,
                &ConfiguredParentClassifier::Fake(fallback.clone()),
                vec![devcoin_publication_config(&fixture)],
            )
            .await?;
            assert_eq!(fallback.call_count().await, 3);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        std::fs::remove_dir_all(&fixture.root)
            .with_context(|| format!("remove fixture root {}", fixture.root.display()))?;
        result
    })
}

#[tokio::test]
async fn parent_only_rows_skip_accounting_and_converge_after_identity_removal() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let old = header_meeting_bits(0x207f_ffff, 1_700_000_090, 90);
        let current = header_meeting_bits(0x207f_ffff, 1_700_000_091, 91);
        let old_row = normalized_csv_line(
            &old,
            &NormalizedCsvRow {
                chain: "devcoin",
                source_row_number: 1,
                classification: "canonical",
                relevance: "",
                relevance_reason: "canonical_parent",
                coinbase_script: &[],
                btc_height: 700_090,
                child_height: 12,
                child_hash: None,
            },
        );
        let current_row = normalized_csv_line(
            &current,
            &NormalizedCsvRow {
                chain: "devcoin",
                source_row_number: 2,
                classification: "canonical",
                relevance: "",
                relevance_reason: "canonical_parent",
                coinbase_script: &[],
                btc_height: 700_091,
                child_height: 13,
                child_hash: None,
            },
        );
        let initial = write_manifest_fixture_rows(std::slice::from_ref(&old_row))?;
        let revised = write_manifest_fixture_rows_with_parent_only(
            &[without_child_identity(&old_row), current_row],
            1,
        )?;
        let result = async {
            let first = run_historical_import_configs_for_test(
                &mut client,
                &ConfiguredParentClassifier::Fake(FakeParentClassifier::new(canonical_verdict(
                    &old, 700_090,
                ))),
                vec![devcoin_publication_config(&initial)],
            )
            .await?;
            assert_eq!(first.chains[0].1.ingested, 1);

            let corrected = run_historical_import_configs_for_test(
                &mut client,
                &ConfiguredParentClassifier::Fake(FakeParentClassifier::new(canonical_verdict(
                    &current, 700_091,
                ))),
                vec![devcoin_publication_config(&revised)],
            )
            .await?;
            let summary = &corrected.chains[0].1;
            assert_eq!(summary.rows_seen, 2);
            assert_eq!(summary.ingested, 1);
            assert_eq!(summary.removed, 1);
            assert_eq!(summary.skipped.get("missing_child_identity"), Some(&1));
            assert_eq!(
                summary.rows_seen,
                summary.ingested + summary.skipped.values().sum::<u64>()
            );

            let converged = run_historical_import_configs_for_test(
                &mut client,
                &ConfiguredParentClassifier::Disabled,
                vec![devcoin_publication_config(&revised)],
            )
            .await?;
            assert_eq!(converged.skipped_matching_state, 1);
            assert!(converged.chains.is_empty());
            Ok::<_, anyhow::Error>(())
        }
        .await;
        for fixture in [&initial, &revised] {
            std::fs::remove_dir_all(&fixture.root)
                .with_context(|| format!("remove fixture root {}", fixture.root.display()))?;
        }
        result
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
