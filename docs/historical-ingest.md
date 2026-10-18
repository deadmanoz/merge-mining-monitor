# Historical Ingest

Historical ingest consumes the normalized monitor publication from
`merge-mining-research` and sends every observation through the same store and
read-model rules as live capture. Its pinned commit is the generated
`source_repo_commit` in `data/historical/historical-source-manifest.json`.
The compact catalogue header in `data/consensus/error_blocks.csv` must name
that same commit; `just gen-error-blocks-catalogue` refuses a
`--source-commit` that disagrees.

## Pinning And Manifest Verification

Refresh the Research manifest and error catalogue from one committed Research
publication:

```bash
just gen-research-publication-pins \
  --repo-dir "$MERGE_MINING_RESEARCH_DIR" \
  --source-commit "$RESEARCH_COMMIT"
```

Materialize Research's event-file LFS payloads before running this command:

```bash
git lfs pull --include="results/monitor-evidence/*_monitor_evidence.csv"
```

The manifest generator verifies each artifact's pinned byte size and SHA-256,
measures parent-only rows, and takes the error-observation chain inventory from
Research's `observation_chain_counts` field. The combined command stages the
manifest and error catalogue together and publishes them only after both
generators succeed (it does not accept `--out`). Run it again with `--check`
before importing or releasing. Routine repository gates use
`--check --allow-missing-repo`, which reuses the committed parent-only counts
while checking Git publication metadata; the explicit release check omits that
flag and rescans the materialized payloads. A refreshed pin documents import
readiness only, not a completed database import or deploy.

The pinned manifest (`data/historical/historical-source-manifest.json`) covers:
- Per-chain event artifacts at
  `results/monitor-evidence/<chain>_monitor_evidence.csv` (Doichain participates
  with a valid zero-row file; parent-only canonical rows that lack both a child
  hash and child height are pinned and verified in preflight, then omitted from
  state comparison and import because `merge_mining_event` requires at least one
  child identity field).
- The `stale-descendants` aggregate view (its contributing chain observations
  also exist in the per-chain files).
- `error-block-observations_monitor_evidence.csv`: documented child witnesses
  for proof-of-work-valid but Bitcoin-consensus-invalid parent headers (`classification=error_block`,
  `VALID_ERROR_BLOCK`, catalogue Bitcoin height and rejection reason, blank
  stale-relevance fields, and the 34-column union including the 7 RSK sidecar
  columns). Its `error-block-observations` scope is reserved to that single
  aggregate entry, whose row count and source-chain inventory must match the
  committed manifest and cover every pinned error parent.

Before any database mutation, `import-all` verifies:
- the complete manifest, source-registry inventory, and research checkout commit
  (unless `--artifact-root` supplies content-addressed files explicitly);
- every file's size, checksum, normalized header, row count, and classification
  counts, keeping the verified file handles open and rewinding them for
  preclassification and mutation so path replacement cannot substitute unverified
  bytes;
- every row's schema, hashes, compact targets, parent header, proof of work,
  validation token (`VALID` for direct stales, `VALID_STALE_DESCENDANT` for
  stale descendants, `VALID_ERROR_BLOCK` for error observations), and available
  child-header corroboration;
- `expected_nbits`: for normal rows, a populated value must equal `nBits` in
  `btc_header_hex`; for error observations, it records the network target
  expected at the catalogued height (which differs from `btc_bits` on
  `nbits_retarget_not_applied` and is checked against the Core-derived target
  for that height).

## Child Identity And Normalization

The normalized child-evidence columns (`child_height`, `child_block_hash`,
`child_header_hex`, `child_block_time`, `child_nbits`) are independent and
nullable:
- Empty cells remain SQL `NULL`, except that an 80-byte `child_header_hex`
  derives its exact SHA256d `child_block_hash` when the hash cell is empty. The
  importer never substitutes a scan counter, Bitcoin parent time, or zero. Every
  imported event must have at least a child hash, child height, or child header,
  and a present header must independently authenticate any companion child hash
  or timestamp.
- For ordinary sources, a present `child_nbits` must equal the header field.
  Xaya and ROD declare the `PowData` target location in the source registry:
  their pure-header `nBits` must be zero and a present non-zero `child_nbits` is
  the effective target from the pinned Research publication. Whenever
  `child_nbits` is present, the importer checks the Bitcoin parent hash against
  that compact target and stores `pow_validates_child_target`.
- `child_block_hash` encodes the exact bytes stored by live capture (lowercase
  hex of raw `sha256d::Hash::to_byte_array()` bytes for SHA256d headers, not
  reversed display order). Bitcoin parent hash columns are display-order
  cross-checks against `btc_header_hex`.
- A real child hash is exact identity `(source_id, child_block_hash)` with a
  single Bitcoin parent (a second row with the same source and child hash but a
  different Bitcoin parent fails closed). A hashless row uses partial identity
  `(source_id, child_height, btc_parent_header_hash)`. Later exact evidence
  promotes one unambiguous partial row in place; a partial row already
  represented by one exact event attaches its provenance to that exact event.
- `full_coinbase_hex`, when present, is decoded and corroborated against the
  published scriptSig before storing raw bytes, txid, script, and value-complete
  outputs. `coinbase_outputs` text is preserved verbatim for capture-time
  attribution and later `reclassify-pools` replay.
- Every stored parent is a Bitcoin header (`docs/capture.md`, "Bitcoin Lineage
  Gate"): `unknown` rows are stored only with a `strict_btc_orphan` or
  `weak_btc_orphan` verdict (for Hathor unknowns, strict BIP34 requires a
  matching full coinbase transaction), canonical/stale rows are Core-attested,
  and error observations are both Core- and catalogue-verified. A pinned
  publication import aborts on any row it would skip, including an excluded or
  pending parent.

## Source Lifecycles And Provenance Rules

`historical_event_provenance` retains every imported source row (`publication_ref`
is the pinned research commit for manifest imports or `operator-csv` for manual
overrides), keeping source classification, validation status, and stale-relevance
axes separate from derived `block` state.

The shared source registry governs snapshot reconciliation:
- **`Historical` and `Partial`**: the checksum-verified manifest is the
  authoritative snapshot. After a complete manifest import, source events
  absent from the publication are deleted unless protected by `operator-csv` or
  `error-block-observations` provenance.
- **`Live`**: base evidence is additive. Publication rows fill or refine
  matching events, and omitted live events, proofs, and attributions are
  retained. Complete manifest imports still replace prior normal Research
  provenance for the chain so withdrawn Research claims do not survive.
- **`Surveyed`**: must publish zero rows (Doichain completes preflight and writes
  nothing). `import-all` rejects unexpected publication-owned events rather than
  treating a lifecycle change as an automatic evidence cleanup.

Across `Live`, `Historical`, and `Partial` chains, each chain import runs in one
transaction: it snapshots prior stale attestations, retires superseded normal
Research provenance, applies base-event writes/removals, enqueues changed
attestation gates and affected parents in `historical_reconcile_queue`, and
marks `source_health` unready under the exclusive source-health lock. Changed
`error-block-observations` witnesses are imported before ordinary chain cleanup
so events moving out of stale inventories gain protected error provenance first
(and promote any prior `stale` or `unknown` parent row to `error_block`);
cleanup refuses to delete a catalogued error witness lacking that provenance,
so run `import-all` before retrying a blocked single-chain `import-dataset`.

After each chain commit, proven Core-canonical parents whose active events all
agree with the Core-backed `block` row rebuild in bounded set-based batches; all
other parents drain from `historical_reconcile_queue` in bounded per-parent
transactions that persist changed-hash cascade seeds until dependent cascades
succeed. Once all chains and the final targeted stale-branch pass finish,
`import-all` rebuilds `source_health` once.

## Known-Stale Membership

Import known Bitcoin stale membership once per database before the publication:

```bash
just import-known-stales \
  --csv path/to/stale-blocks.csv \
  --source-label "bitcoin-data/stale-blocks@<commit>"
```

`import-all` and `import-dataset` refuse an empty `known_stale_block` table by
default (pass `--allow-empty-known-stales` only on a diagnostic database). Both
`known_stale_block` membership and published direct-stale / stale-descendant
provenance exclude a parent from `strict_btc_orphan` / `weak_btc_orphan`
classification.

## Running Imports

Prepare the database and LFS payloads, then import the complete publication and
refresh pool attributions:

```bash
just db-up
just db-migrate-dev
set -a; source .env; set +a
git -C "$MERGE_MINING_RESEARCH_DIR" lfs pull \
  --include="results/monitor-evidence/*_monitor_evidence.csv"
just import-all
just reclassify-pools
```

During preflight, `import-all` compares normalized publication-owned fields
against stored non-operator provenance and base events across research pins:
- Unchanged artifacts report `skipped_matching_state` and skip classification,
  writes, and reconciliation. If every artifact matches and no historical queue,
  source-health, or published-stale work is pending, `import-all` returns before
  taking the Bitcoin Core lock; pending derived work takes the lock and
  finalizes without replaying source rows.
- Changed artifacts reuse compatible Core-attested `canonical` or structurally
  complete `stale` classifications already proven in `block`, while event-only
  canonical, unknown, missing, or publication-incompatible parents — and all
  `error-block-observations` rows — run strict live Bitcoin Core classification
  (retrying transient Core transport/warmup errors with bounded backoff and
  failing closed if exhausted).
- Two stronger-evidence projections are accepted: a chain-local weak BTC-orphan
  row is promoted to strict when another chain supplies strict evidence for the
  same parent, and a direct Core stale attestation overrides an archived
  `canonical` source label while preserving the original label in provenance.

For single-chain diagnostics or operator overrides:

```bash
just import-dataset devcoin
just import-dataset namecoin
just import-dataset rsk
```

`import-dataset CHAIN` commits only the named chain without running the
cross-source stale-branch pass. Passing `--csv PATH` or `--limit N` (`N > 0`)
runs an additive `operator-csv` import without manifest checksum verification or
omitted-row deletion.
