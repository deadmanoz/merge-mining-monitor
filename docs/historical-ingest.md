# Historical Ingest

Historical ingest consumes the normalized monitor publication from
`merge-mining-research` and sends every observation through the same store and
read-model rules as live capture. Its pinned commit is the generated
`source_repo_commit` in `data/historical/historical-source-manifest.json`.
The compact catalogue header in `data/consensus/error_blocks.csv` must name
that same commit; `just gen-error-blocks-catalogue` refuses a
`--source-commit` that disagrees.

Refresh both pins from one committed Research publication:

```bash
just gen-research-publication-pins \
  --repo-dir "$MERGE_MINING_RESEARCH_DIR" \
  --source-commit "$RESEARCH_COMMIT"
```

Materialize Research's event-file LFS payloads before running this command. The
manifest generator verifies their pinned size and checksum, then measures each
artifact's parent-only rows. The combined command stages the manifest and
catalogue together and publishes them only after both generators succeed. It
also takes the error-observation chain inventory from Research's
`observation_chain_counts` field. The combined command does not accept `--out`.
Run it again with `--check` before importing or releasing.

Routine repository gates use `--check --allow-missing-repo`; that mode reuses
the committed parent-only counts while still checking the Git publication
metadata. The explicit release check above omits that flag and rescans the
materialized payloads.

`import-all` verifies the source revision, manifest, and all 29 artifacts once,
before database mutation, then imports the verified readers in chain order.

## Publication Contract

The publication contains 1,037,005 event rows across 27 uniform per-chain files:

```text
results/monitor-evidence/<chain>_monitor_evidence.csv
```

Doichain participates through the same path with a valid zero-row file. The
separate 21-row `stale-descendants` file is an aggregate view, not an event
source, because its contributing chain observations already exist in the
per-chain files.

The total includes 456,660 canonical Namecoin rows whose historical source does
not authenticate a child hash or height. The Monitor manifest pins that
parent-only count per artifact. Preflight verifies and counts those rows, but
omits them from state comparison and import because `merge_mining_event`
requires one of those partial identities.
Fractal's 58,970 canonical rows retain child height and remain importable even
though they lack an exact child hash. Every non-canonical row still requires a
child hash or height.

A complete publication also carries
`error-block-observations_monitor_evidence.csv`: documented child witnesses for
proof-of-work-valid but Bitcoin-consensus-invalid parent headers. It is a
separate aggregate rather than a per-chain valid-evidence artifact. Every row
is `classification=error_block`, has `VALID_ERROR_BLOCK`, the catalogue's
Bitcoin height and rejection reason, and blank stale-relevance fields. The file
uses the normal 27-column header plus the seven RSK sidecar columns; non-RSK
sidecar cells are blank and RSK witnesses carry complete sidecars. Its manifest
requires exactly one error-observation entry whose row count and generated
source-chain inventory match the committed manifest, so a missing, truncated,
or cross-chain-substituted aggregate fails before database mutation. Its
`error-block-observations` scope is reserved to that aggregate;
ordinary historical artifacts using it are rejected. Preflight also requires
coverage of all 39 pinned error parents across its witnesses, and checks
retarget observations against the Core-derived target for their stated height.

`data/historical/historical-source-manifest.json` pins each event payload by
path, byte size, SHA-256, row count, and classification counts. It also pins the
research publication manifest and source commit. Before any database mutation,
the importer verifies:

- the complete manifest and source-registry inventory;
- the research checkout commit, unless `--artifact-root` supplies
  content-addressed files explicitly;
- every file's size, checksum, normalized header, row count, and classification
  counts;
- every row's schema, hashes, compact targets, parent header, proof of work,
  taxonomy, and available child-header corroboration.

Error-observation rows are admitted through a separate parser rather than
widening the normal valid-evidence taxonomy. Bitcoin Core is mandatory: the
importer requires both an exact local catalogue match and the shared
Core-plus-catalogue parent resolver to produce the same `error_block` height
and rejection reason. Except for the manifest-counted parent-only rows above, a
row that would be skipped aborts the complete publication before it writes any
normal chain artifact.

Their `expected_nbits` is still required to be a valid compact target, but it
records the network target expected at the catalogued height. It can therefore
differ from the invalid header's own `btc_bits` for
`nbits_retarget_not_applied`; the header target itself is always checked against
its proof of work.

Direct stale rows require a complete `VALID` validation token. Stale-descendant
rows require the exact `VALID_STALE_DESCENDANT` status. These statuses describe
different validation profiles and are checked uniformly for every chain.

Each event artifact remains open after verification. Classification and
mutation rewind that same verified file handle, so replacing a checkout path
during a long preclassification pass cannot substitute unverified bytes.

Git LFS must materialize the CSV payloads. If the importer finds pointer text,
recover the files with:

```bash
git lfs pull --include="results/monitor-evidence/*_monitor_evidence.csv"
```

## Child Evidence

The normalized columns are uniform across chains, but authenticated values can
be unavailable. These fields are independent:

- `child_height`
- `child_block_hash`
- `child_header_hex`
- `child_block_time`
- `child_nbits`

Empty cells remain SQL `NULL`, except that an 80-byte child header derives its
exact SHA256d child hash when the hash cell is empty. This is authenticated
identity from the supplied header, not a placeholder. The importer never
substitutes a scan counter, Bitcoin parent time, zero, or another synthetic
value. An individual event must have a child hash, child height, or child
header. When a child hash, timestamp, or `nBits` is also present, the header
must authenticate that companion independently. Xaya is the documented
exception to the header-field `nBits` comparison because its authenticated
effective target lives in `PowData`.

When `child_nbits` is present, the importer compares the imported Bitcoin
parent hash with that compact target and persists the result as
`pow_validates_child_target`. This is the same target test used by live
Namecoin-family capture, including Xaya's authenticated effective target.

A real child hash is exact identity: `(source_id, child_block_hash)`. A hashless
row uses `(source_id, child_height, btc_parent_header_hash)` as partial identity.
Later exact evidence promotes one unambiguous partial row in place. A partial
row already represented by one exact event attaches its historical provenance
to that exact event instead of creating a duplicate. Contradictory or ambiguous
evidence fails the chain transaction.

An exact identity represents the one child-ledger block exposed under that
hash, including the parent proof retained by the child node. A later row with
the same source and child hash but a different Bitcoin parent is contradictory
source evidence, not a second event, and fails closed. The pinned publication
contains 244,016 non-null child hashes with no duplicate
`(chain, child_block_hash)` identities.

`child_block_hash` encodes the exact bytes stored by live capture. For
SHA256d child headers this is lowercase hex of the raw
`sha256d::Hash::to_byte_array()` bytes, not the reversed display/RPC form.
Bitcoin parent hash columns are different: they are display-order
cross-checks, while the stored parent identity is derived from
`btc_header_hex`.

`expected_nbits` is the publication validator's expected Bitcoin target for an
admitted row. When populated, it must equal the `nBits` encoded in
`btc_header_hex`; disagreement is contradictory evidence and fails closed.
All 3,696 populated values in the pinned publication satisfy this invariant.

`historical_event_provenance` retains every imported source row. Its
`publication_ref` is the pinned research commit for manifest-backed imports and
`operator-csv` for an unpinned override. Chain, original path and row number,
source classification, validation result, and stale relevance axes remain
separate from the monitor's derived Bitcoin block state. Several source rows
can refine to one event without losing their individual provenance.

The importer also retains both parent-coinbase publication fields.
`full_coinbase_hex`, when present, is decoded and corroborated against the
published scriptSig before its raw bytes, txid, script, and value-complete
outputs are stored. `coinbase_outputs` is preserved verbatim because the
research publication legitimately contains raw scriptPubKey, address, and
value-paired forms. Recognizable Bitcoin payout addresses participate in
capture-time attribution, and `reclassify-pools` can replay from the stored
text later.

## Source Lifecycles And Existing Data

The shared source registry controls reconciliation:

- `Historical` and `Partial` sources use the checksum-verified manifest as
  their authoritative snapshot. After a complete successful manifest import,
  source events absent from the pinned publication are deleted, including rows
  created by the retired synthetic importer.
- `Live` sources are additive. Historical publication rows can fill or refine
  a matching live event, but absence from the publication never deletes a
  live-captured event.
- `Surveyed` sources must publish zero rows. Doichain completes preflight and
  performs no database writes.

Each chain writes its complete base/provenance snapshot, removes obsolete
authoritative rows, retires manifest-backed provenance from every superseded
publication commit for that chain, and enqueues affected parents in one
transaction. Additive `operator-csv` provenance is preserved. A failure before
that commit rolls back the whole chain, including restoration of the previous
publication provenance. After commit, the importer first rebuilds proven
Core-canonical parents in bounded set-based batches. A parent qualifies only
when every active event agrees with the Core-backed block's header, height,
difficulty, and canonical classification. All other parents drain through
bounded per-parent transactions. Primary reconcile results store their
changed-hash cascade seeds in the same transaction, and queue work is removed
only after its dependent cascade succeeds. An interruption at either boundary
therefore resumes without losing cascade work. `import-all` also enqueues its
final targeted stale-branch pass before rebuilding any parent, so a failure
after a fresh stale promotion resumes its dependent cascade.
`import-all` takes the exclusive source-health lock at the end of each
non-surveyed chain transaction, then commits its base evidence, durable queue,
and unready flag atomically. A concurrent rebuild either finishes before that
invalidation or observes pending historical work and refuses to mark the
aggregate ready. The importer rebuilds once after all chains and targeted stale
branches have reconciled. A partial multi-chain import therefore never exposes
counters from the previous complete snapshot.

Error-observation provenance is retained outside normal authoritative cleanup.
That does not preserve a prior parent verdict: each imported witness is
reconciled to `error_block`, promoting any existing `stale` or `unknown` row
for the same Bitcoin header. Error witnesses use the pinned research publication
reference and their original archive source coordinates. Replaying an existing
coordinate is idempotent; changed evidence conflicts at the store boundary and
stops the import, while new coordinates add witnesses.

The schema migration retains existing child values and makes the child evidence
columns nullable. Before changing the schema, it fails closed if the legacy
database contains more than one row for the new exact identity
`(source_id, child_block_hash)`. Run the audit query in `docs/operations.md` and
resolve any conflict from authenticated source evidence; the migration never
guesses which legacy row to keep. The subsequent authoritative import is what
removes obsolete historical-source rows. It does not clear the database or
replace live-source data.

## Known-Stale Membership

Import known Bitcoin stale membership once per database before the publication:

```bash
just import-known-stales \
  --csv path/to/stale-blocks.csv \
  --source-label "bitcoin-data/stale-blocks@<commit>"
```

`import-dataset` and `import-all` refuse an empty `known_stale_block` table by
default. Without it, a catalogued stale could be labelled a strict or weak BTC
orphan. Published direct-stale and stale-descendant provenance is also an
exclusion from strict/weak classification, including while its branch awaits
derived placement. The import atomically demotes any existing strict/weak rows
that match its membership; `just reclassify-known-stales` is only needed to
repeat that repair independently.

Bitcoin Core is required for every historical import, including diagnostic
imports. `--allow-empty-known-stales` exists only for a deliberately
membership-free diagnostic database. It is not a production cutover option.
`--limit` must be greater than zero and makes a manifest import additive rather
than authoritative.

## Body-Invalid Stale Annotations

Import the pinned body-invalid stales mirror after migrations, in any order
relative to the publication import (the annotation is display-only and gates
nothing; unlike the historical imports, this command does not require a
Bitcoin Core connection):

```bash
just import-body-invalid-stales \
  --csv data/consensus/body_invalid_stales.csv \
  --source-label "merge-mining-research@<commit>"
```

The mirror is refreshed together with the error-block catalogue and the
historical manifest by `just gen-research-publication-pins`, and its header
pin must name the same research commit. The importer is strict: any malformed
row, an empty file, or a hash that is also in the pinned error-block
catalogue is fatal (the research overlay and catalogue are disjoint by
construction, so an overlap means the pins are out of step). The mirror is an
authoritative snapshot: re-imports replace rows in place and prune any
annotation the newest pin withdrew, so a corrected rule, a corrected evidence
URL, or a removed row propagates without an operator delete. Annotated blocks
remain ordinary `stale` rows; only the block detail and tree hover surface
the annotation, and the projection join is additionally gated on
`kind = 'stale'`.

## Import

Prepare the database and research artifacts:

```bash
just db-up
just db-migrate-dev
set -a; source .env; set +a
git -C "$MERGE_MINING_RESEARCH_DIR" lfs pull \
  --include="results/monitor-evidence/*_monitor_evidence.csv"
```

Import the complete publication:

```bash
just import-all
```

The command still requires a complete pin and manifest. Artifact SHA values
verify publication bytes only. During the same CSV parse used for preflight,
`import-all` constructs a normalized projection of publication-owned state and
compares it with stored non-operator provenance from any research pin. A
matching file skips classification, writes, and authoritative reconciliation.
The summary reports `skipped_matching_state`.

Historical and partial sources require the exact authoritative base-event set,
including detection of operator-created extras. Live sources permit additional
database rows. The current error-observation publication is a required subset
of retained deduplicated history, and surveyed zero-row sources are checked
explicitly. Database-only enrichment is accepted only where the publication
omitted the corresponding field. If every artifact matches, the command checks
the durable historical queue, source-health readiness, and published stale
branches. A clean match returns before taking the Bitcoin Core lock. Pending
derived work takes the lock and finalizes without replaying source rows.

The command preflights all artifacts before importing the first changed chain,
processes
chains in deterministic order, shares a Bitcoin-parent classification cache,
combines candidate parsing, validation, and preclassification into one stream,
fills the Bitcoin RPC client's configured bounded concurrency, and runs targeted
stale-branch reconciliation after all sources are present. A parent already
proved Core-attested canonical or structurally complete stale in the derived
`block` state is reused when that verdict is compatible with the publication
row. This avoids repeating Bitcoin Core header and full-block lookups merely
because another publication coordinate changed. An inferred stale verdict is
reused only for a row carrying stale or known-branch publication evidence and
only while its stored canonical-competitor relationship remains intact.
Event-only canonical, unknown, missing, half-rebuilt, or
publication-incompatible state still goes through strict live Core
classification. The dedicated error-observation aggregate also retains its
Core-plus-catalogue check. Canonical and Core-indexed stale parents on the live
path do not query predecessor state from the database; that read-model lookup
is deferred until Core proves the candidate header is absent. Transient Bitcoin
Core transport failures and warmup responses are retried with bounded
exponential backoff. Exhausting those retries fails preclassification explicitly
instead of converting an operational failure into an `unknown` parent
classification.
Digest verification, manifest-count inspection, and database mutation still
read the artifact separately.
Its per-chain and total summaries report expected, ingested, inserted, updated,
promoted, exact-satisfied, removed, classification, relevance, and skip counts.
A pinned publication import fails on an unexplained skip. Write-disposition
counters come from the store's exact/partial identity decision rather than a
second importer-side identity query.

The `import-all` report includes an `error-block-observations` line after the
normal chains, with its witness and distinct-parent counts. Those parents are
then available to the existing error-block navigator and compact tree view as
ordinary sourced `error_block` rows.

Two stronger-evidence projections are accepted without weakening that gate.
The publication can promote a chain-local weak BTC-orphan verdict to strict
when another chain independently supplies strict evidence for the same parent
header. Bitcoin Core can also identify an archived `canonical` source row as
stale after the source snapshot was recorded; a direct Core stale attestation
then controls the stored parent classification while the original source label
remains in provenance. The reverse strict-to-weak and stale-to-canonical
mismatches still fail closed.

Import one chain when diagnosing or resuming a specific source:

```bash
just import-dataset devcoin
just import-dataset namecoin
just import-dataset rsk
```

`import-dataset` commits only the named chain and does not run the cross-source
stale-branch reconciliation pass. A later `import-all` observes any resulting
authoritative difference directly from provenance and base-event state. Use
`import-all` to establish the complete publication state; the single-chain
command is for diagnostics and recovery.

`--csv PATH` is an explicit fixture or operator override. It must still use the
normalized schema, but it has no monitor-manifest checksum expectation. It is
always additive, carries `operator-csv` rather than the pinned research commit
in provenance, and never removes events absent from the supplied file.
Authoritative replacement is reserved for the checksum-verified manifest path.

After the full import, refresh attribution:

```bash
just reclassify-pools
```

`import-all` already rebuilds source health and performs the targeted
stale-branch pass. A later matching `import-all` is a publication-versus-database
completeness check, not a replay of database-write idempotence.

## Production Cutover

Stop live pollers for the cutover, then:

1. Run `just db-backup`.
2. Apply migration 0007 through `just db-migrate-deploy`.
3. Confirm `known_stale_block` is populated.
4. Run `just import-all`.
5. Run `just reclassify-pools`.
6. Verify per-source event totals, API block details, orphan exclusions, and
   poller health.

Retain the backup until the post-import audit is accepted. Never run the
migration or publication import directly against a persistent database without
the repository's backup-first workflow.
