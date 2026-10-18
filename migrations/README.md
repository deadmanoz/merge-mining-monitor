# Migrations

SQL migrations (`NNNN_short_description.sql`) applied in lexical order by `scripts/migrate-safe.sh`.

## Rules

- Run only via `just db-migrate-dev` or `just db-migrate-deploy` (backup + auto-restore on failure).
- Migrations run in `psql --single-transaction` and are append-only (`0003_*` onwards).
- Source IDs are permanent: assign the next ID in `source_registry`, regenerate `0002` (`just gen-source-artifacts`), and add an idempotent forward migration. Retiring a source keeps its ID gap.
- Rule changes altering verdicts schedule a generation-tracked recheck:

```sql
UPDATE bitcoin_core_header_cache_state
SET recheck_pending_generation = recheck_pending_generation + 1,
    recheck_pending_orphans = TRUE,
    recheck_pending_sources = CASE
        WHEN recheck_pending_sources IS NULL
          OR (recheck_pass_generation IS NOT NULL AND recheck_pass_sources IS NULL)
        THEN NULL
        ELSE (SELECT array_agg(DISTINCT s) FROM unnest(
            recheck_pending_sources || COALESCE(recheck_pass_sources, '{}') || ARRAY['auxpow:hathor']
        ) AS s)
    END,
    recheck_pass_generation = NULL, recheck_pass_orphans = NULL,
    recheck_pass_sources = NULL, recheck_cursor_height = NULL, recheck_cursor_id = NULL;
```

## Current Migrations

| File | Notes |
|------|-------|
| `0001_canonical_schema.sql` | Canonical baseline for producer, sidecar, read-model, and Core tables. |
| `0002_seed_sources.sql` | Fresh-DB `source` seed (`source-registry baseline reset`). |
| `0003_add_elcash_source.sql` | Inserts historical source `auxpow:elcash` (id 34). |
| `0004_remove_mazacoin_source.sql` | Verifies Elcash id 34 and retires Mazacoin id 32. |
| `0005_rename_excluded_orphan_class.sql` | Renames `btc_stale_excluded` orphan class to `excluded`. |
| `0006_add_known_stale_block.sql` | Adds `known_stale_block` table to exclude known stales. |
| `0007_support_partial_child_evidence.sql` | Nullable child evidence, historical provenance, and reconcile queue. |
| `0008_add_error_block_parent_kind.sql` | Adds `error_block` state to parent-kind constraints. |
| `0009_drop_rsk_reclassify_watermark.sql` | Drops obsolete `rsk_reclassify_watermark` table. |
| `0010_add_bitcoin_core_header_cache.sql` | Adds sparse `bitcoin_core_header` cache. |
| `0011_track_core_cache_orphan_rechecks.sql` | Separates pending-coverage and full-orphan-recheck flags. |
| `0012_add_bitcoin_core_reconcile_queue.sql` | Adds two-phase `bitcoin_core_reconcile_queue` for Core reorgs. |
| `0013_allow_live_error_block_height_sources.sql` | Allows live Core predecessor heights on `error_block` rows. |
| `0014_add_error_block_historical_provenance.sql` | Allows `error_block` in `error-block-observations` provenance. |
| `0015_add_historical_import_artifact.sql` | Adds `historical_import_artifact` table (retired by `0016`). |
| `0016_drop_historical_import_artifact.sql` | Drops `historical_import_artifact` for DB state comparison. |
| `0017_add_body_invalid_stale.sql` | Adds `body_invalid_stale` table (retired by `0029`). |
| `0018_add_rod_source.sql` | Converges historical source `auxpow:rod` on id 35. |
| `0019_recheck_orphans_after_hathor_bip34.sql` | Schedules orphan recheck for Hathor strict BIP34. |
| `0020_add_qbit_source.sql` | Converges live source `auxpow:qbit` on id 36. |
| `0021_add_capture_error.sql` | Adds `capture_error` table for per-height capture failures. |
| `0022_add_child_displacement.sql` | Adds `child_displaced_at`/`child_displaced_by` columns. |
| `0023_validate_child_displacement.sql` | Validates `0022` child-displacement constraints. |
| `0024_restore_hathor_displaced_events.sql` | Restores superseded/voided Hathor events as displaced. |
| `0025_retire_pending_supersede.sql` | Drops retired `supersede` columns from `poll_pending_reconcile`. |
| `0026_add_child_chain_head.sql` | Adds `child_chain_head` tracking the last block per height. |
| `0027_schedule_core_rechecks_by_generation.sql` | Replaces Core-cache retry booleans with generation scopes. |
| `0028_add_reconcile_queue_expand_unchanged.sql` | Adds `expand_unchanged` to `bitcoin_core_reconcile_queue`. |
| `0029_drop_body_invalid_stale.sql` | Drops obsolete `body_invalid_stale` table. |
| `0030_capture_failure_kind.sql` | Adds `height_capture_failed` to `capture_error.error_kind`. |
| `0031_drop_hathor_expected_btc_nbits.sql` | Drops obsolete `expected_btc_nbits` from Hathor sidecar. |
| `0032_add_qbit_payout_namespace.sql` | Adds `qbit_payout_address` to the `pool_identity` namespace check. |

## Reset Path

For fresh, CI, and disposable local resets (`CONFIRM_DB_RESET=1 just db-reset`), see [`docs/operations.md#cleanup-and-local-reset`](../docs/operations.md#cleanup-and-local-reset).
