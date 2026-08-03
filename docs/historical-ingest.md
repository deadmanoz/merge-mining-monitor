# Historical Ingest

Historical ingest consumes the normalized monitor publication from
`merge-mining-research` and sends every observation through the same store and
read-model rules as live capture. The pinned publication is merge commit
`08da16532a55240e54c4051d5d324a0484b80b1c`.

## Publication Contract

The publication contains 576,662 event rows across 27 uniform per-chain files:

```text
results/monitor-evidence/<chain>_monitor_evidence.csv
```

Doichain participates through the same path with a valid zero-row file. The
separate 21-row `stale-descendants` file is an aggregate view, not an event
source, because its contributing chain observations already exist in the
per-chain files.

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
contains 243,970 non-null child hashes with no duplicate
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
All 3,672 populated values in the pinned publication satisfy this invariant.

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
authoritative rows, and enqueues affected parents in one transaction. A failure
before that commit rolls back the whole chain. After commit, the importer drains
the durable queue in bounded per-parent transactions. Primary reconcile results
store their changed-hash cascade seeds in the same transaction, and queue work
is removed only after its dependent cascade succeeds. An interruption at
either boundary therefore resumes without losing cascade work. `import-all`
also enqueues its final targeted stale-branch pass before rebuilding any parent,
so a failure after a fresh stale promotion resumes its dependent cascade.
`import-all` takes the exclusive source-health lock at the end of each
non-surveyed chain transaction, then commits its base evidence, durable queue,
and unready flag atomically. A concurrent rebuild either finishes before that
invalidation or observes pending historical work and refuses to mark the
aggregate ready. The importer rebuilds once after all chains and targeted stale
branches have reconciled. A partial multi-chain import therefore never exposes
counters from the previous complete snapshot.

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
just reclassify-known-stales
```

`import-dataset` and `import-all` refuse an empty `known_stale_block` table by
default. Without it, a catalogued stale could be labelled a strict or weak BTC
orphan. Published direct-stale and stale-descendant provenance is also an
exclusion from strict/weak classification, including while its branch awaits
derived placement.

`--allow-empty-known-stales` and `--allow-unclassified` exist for disposable
diagnostic databases. They are not production cutover options. A manifest
import using `--allow-unclassified` is additive because its skipped unknown rows
make it incomplete; it never clears provenance or removes rows from the prior
authoritative snapshot. `--limit` must be greater than zero.

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

The command preflights all artifacts before importing the first chain, processes
chains in deterministic order, shares a Bitcoin-parent classification cache,
combines candidate parsing, validation, and preclassification into one stream,
fills the Bitcoin RPC client's configured bounded concurrency, and runs targeted
stale-branch reconciliation after all sources are present. Canonical and
Core-indexed stale parents do not query predecessor state from the database;
that read-model lookup is deferred until Core proves the candidate header is
absent. Transient Bitcoin Core transport failures and warmup responses are
retried with bounded exponential backoff. Exhausting those retries fails
preclassification explicitly instead of converting an operational failure
into an `unknown` parent classification.
Digest verification, manifest-count inspection, and database mutation still
read the artifact separately.
Its per-chain and total summaries report expected, ingested, inserted, updated,
promoted, exact-satisfied, removed, classification, relevance, and skip counts.
A pinned publication import fails on an unexplained skip. Write-disposition
counters come from the store's exact/partial identity decision rather than a
second importer-side identity query.

Import one chain when diagnosing or resuming a specific source:

```bash
just import-dataset devcoin
just import-dataset namecoin
just import-dataset rsk
```

`import-dataset` commits only the named chain and does not run the cross-source
stale-branch reconciliation pass. Use `import-all` to establish the complete
publication state; the single-chain command is for diagnostics and recovery.

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
stale-branch pass. A second full import is an idempotence check, not a required
classification phase.

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
