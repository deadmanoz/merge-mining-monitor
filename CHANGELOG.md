# Changelog

All notable public changes to this project will be documented in this file.

This changelog starts with the initial release.

## [Unreleased]

- Register Qbit as live source id 36 and wire its producer through the shared
  bitcoind-family path: a `ChainSpec` row with `QBIT_*` settings, `poll-qbit`
  and `backfill-qbit`, and the regenerated `0002` seed plus
  `0020_add_qbit_source.sql` for databases that already applied the earlier
  seed. Qbit's RPC returns whole blocks, so capture splits out the exact
  extended-header prefix and hands only that to the decoder, which still
  rejects trailing bytes; the block body is never reinterpreted. The producer
  authenticates native placement itself, because the decoder deliberately does
  not: the decoded child header hash must equal the height's `getblockhash`
  result, and height 0 must be the pinned mainnet genesis. Qbit evidence
  reaches the write path as `NormalizedEventEvidence` rather than a synthetic
  `ParsedAuxpowBlock`, which would need a `hashBlock` value the Qbit wire
  format never carries.

- Stop reporting a capture interval complete after skipping a proof that would
  not decode. `FamilySpec::malformed_policy` makes this per-chain: Namecoin,
  Syscoin, and Fractal keep the existing skip-and-continue behaviour, while
  Qbit holds the interval. A held height persists a `capture_error` row
  (`0021_add_capture_error.sql`) before the producer returns, so a crash cannot
  lose the signal; a new live height then holds the cursor, a replayed height
  continues with the persisted row keeping the gap visible, and a bounded
  backfill over such a range exits non-zero instead of logging completion. The
  row clears only when that same height is reprocessed successfully, and the
  monotonic poll cursor is never lowered. `/api/v1/sources` extends
  `sync.error_code` / `sync.error_height` beyond the Bitcoin Core backbone: a
  live AuxPoW source holding unresolved capture errors reports
  `auxpow_capture_error` with its earliest unresolved child height, ahead of
  the ordinary live/stale verdict.

- Refresh the Research publication pin to `e3dc6d6`, adding Qbit's 2,540
  monitor-evidence rows (2,536 canonical, four stale) for 1,286,403 ordinary
  events across 29 chain artifacts. Every prior artifact is byte-for-byte
  unchanged. Registering a source and pinning its publication are one change:
  the manifest artifact set is validated for equality against the registry, so
  a registered source with no pinned artifact fails preflight. The refreshed
  pin documents import readiness; no import has been run.

- Add the Qbit native merge-mining proof decoder to the capture path,
  validated against four real mainnet controls (child heights 78,053-78,064;
  the positive control embeds Bitcoin block 966,017) and a native
  synthetic-parent proof, pinned to Qbit revision `70fea84` via the
  merge-mining-research reference adapter. Qbit's extended header is not a
  classic CAuxPow: it has no `hashBlock` field, commits the display-order
  chain-merkle fold of the pure child header, and always checks the parent
  header against the CHILD's own nBits target. The block API selects the
  proof decoder explicitly from the chain slug (never sniffed), cites Qbit's
  AuxPoW chain id 47, and now omits the classic-only `aux_proof.hash_block`
  member for qbit-format proofs (existing families serialize unchanged).
  Sync `STRICT_BIP34_CHAINS` with the research classifier's
  `BTC_COINBASE_SCRIPTSIG_CHAINS` (the cross-repo drift guard flags the
  addition of `qbit`; the entry becomes live with the Qbit source above). No
  Qbit producer, source-registry entry, or import existed at this point; this
  was the decoder slice only.

- Clarify RSK's 139,999 live acquisition floor and verify bounded backfills
  below it with a real full-header fixture. Preserve complete parent-header
  evidence from early blocks and evaluate their uncles independently; RSK
  height 112,829 is retained as `near` because its parent does not meet the
  Bitcoin proof-of-work target. Rename missing-parent-header skip counters
  to describe the evidence shape instead of an upgrade era.

- Add nullable `child_displaced_at` and `child_displaced_by` columns to
  `merge_mining_event` (`0022_add_child_displacement.sql`), so a child-chain
  reorg can be recorded on the replaced block's event without revoking it. A
  displaced event still carries valid Bitcoin-side evidence, and revoking it
  would silence that evidence everywhere. The columns are set and cleared
  together, a block is never displaced by itself, and Bitcoin-side aggregates
  never read them. The constraints are added `NOT VALID` so the migration's
  exclusive lock is not held across a full-table scan, and
  `0023_validate_child_displacement.sql` validates them under the weaker lock
  in its own transaction. Nothing writes the columns yet; the producer rescan
  path and the read-side projection follow in later changes.

- Add `mmm-store::record_child_chain_block`, the single write behind child
  displacement: given the block the child chain now carries at a height, it
  clears displacement on that block's event and marks every other event at
  the height, hashless partial observations included, as displaced by it.
  Already-displaced events keep their first displacement record. The write is
  one UPDATE under a per-height advisory lock, so a failure never leaves a
  height half-moved and concurrent callers serialize; producers take that
  lock through `lock_child_chain_height` before upserting a captured block's
  event so two captures at one height cannot deadlock on each other's row.
  It is idempotent and touches only the two displacement columns, so it
  needs no parent reconciliation. No producer calls it yet.

- Project child-side displacement on block event details: each
  `event_details[]` entry now carries `child_displaced_at` and
  `child_displaced_by` (the displacing block hash in the same display order
  as `child_block_hash`), set and cleared together. A null pair means no
  displacement has been recorded for the event, not that it is the chain's
  current block. Bitcoin-side fields are unchanged. The block fixtures, their
  manifest and the fixture contract test carry the new pair.

- Record which block the child chain carries at every height the
  bitcoind-family runner (Namecoin, Syscoin, Fractal, Qbit) processes. A
  captured AuxPoW block is recorded inside its capture transaction, which
  now takes the per-height lock before any parent lock; a block that yields
  no event is recorded in a transaction of its own, and the whole height, from
  the `getblockhash` observation to the last write, runs under a session-level
  lock on the height so an overlapping poller and backfill observe and write
  one after the other. A rescanned height whose block changed marks the
  earlier event displaced instead of leaving two current blocks, and a flip
  back restores it. Poll and backfill share the path. The runner's RPC calls now go through a `BitcoindRpc` trait so a
  fixture chain can drive the per-height path end to end in tests. Rescan
  depths stay at zero until they are raised per chain.

- Record which block the child chain carries at every height the Elastos
  producer processes, the way the bitcoind-family runner does: a captured
  block inside its capture transaction, and a block that yields no event in a
  transaction of its own, but only when the block is proven: its AuxPoW
  commitment verified and its parent meets the child target, since the
  endpoint may be untrusted.
  The whole height runs under the session-level height lock. The non-BTC
  and classifier-conflict revocations still mark bad evidence, but they now
  apply to the block the verdict was reached on rather than to every event
  at the height, since a rescanned height can hold a displaced block's event;
  a block is its hash, or for a hashless historical row its height and
  Bitcoin parent, and the displacement write uses the same identity: a block
  known to carry no AuxPoW displaces hashless rows, a proof that did not
  verify leaves them untouched.
  `ELASTOS_REORG_DEPTH` is now read like every other chain's rescan depth
  (default 0), and the forbidden-depth policy that rejected it is gone. The
  eventless record for a non-AuxPoW or malformed block now goes through one
  store helper shared with the bitcoind-family runner.

- Record which block the child chain carries at every height the Hathor
  producer processes, and stop revoking a replaced or voided Hathor block. A
  captured block is recorded inside its capture transaction; a block that
  yields no event is recorded in a transaction of its own, but only when it is
  proven: its RFC 0006 reconstruction identity holds and its hash meets its
  own Hathor target, the work the block's consensus demands, since the REST
  endpoint may be untrusted. A merge-mined block whose parent misses Bitcoin's
  target, the common case, is now told apart from a malformed proof
  (`NearSkipped`) and recorded. A voided block names no replacement and
  records nothing; a non-merge-mined block carries no proof and is not
  recorded. The `hathor_superseded` and `hathor_voided` revocation reasons,
  the write-before-revoke supersession with its durable `supersede` marker,
  and the drain branch that completed it are gone; the non-BTC and
  classifier-conflict revocations apply to the block the verdict was reached
  on. The whole height runs under the session-level height lock.
  `0024_restore_hathor_displaced_events.sql` restores the events revoked for
  those two reasons and records their displacement where the replacing block
  can be named, printing before/after counts; run `reconcile-read-model --all
  --source auxpow:hathor` and `rebuild-source-health` afterwards.
  `0025_retire_pending_supersede.sql` drops the `supersede` kind of
  `poll_pending_reconcile` and its payload columns.

## [0.7.13] - 2026-09-09

- Refresh the Research publication pin to `e09f52b`, covering 1,283,863
  ordinary events across 28 chain artifacts, 21 stale-descendant summary rows,
  and 88 authenticated error witnesses. Preserve the 456,660 canonical
  parent-only Namecoin rows and 58,970 Fractal rows with child height but no
  exact child hash. Add I0coin's 27,854 rows, RSK's 236,432 rows, and ROD's
  single authenticated row to the publication closure. Mark I0coin’s current
  chain status as unknown because snapshot timestamps do not establish network
  availability. The refreshed pins make
  the complete `import-all` workflow ready for review; they do not claim that a
  database import or deployment has completed.

- Align strict coinbase eligibility with the refreshed Research evidence for
  Hathor now that the reconstructed coinbase is preserved, so a real
  coinbase can satisfy the same historical validation rule as other sources.
  Schedule a durable full orphan recheck on upgrade so eligible existing
  Hathor-backed unknown parents converge from the earlier weak verdict.
  Document the operator requirement to stop the runtime through migration and
  the new classifier's recheck so an older binary cannot consume the retry flags.
  Validate and retain Hathor's full parent coinbase transaction before using
  its script as strict evidence, including during historical imports and
  retained-data rechecks.

- Register SpaceXpanse ROD as historical source `auxpow:rod` at permanent id
  35, with a complete native-node recovery profile through child height
  4,127,689 and one authenticated canonical Bitcoin witness. The ROD chain
  remains live, but the Monitor source is a sealed historical capture with no
  live producer.

- Validate Xaya's historical `PowData` child target explicitly: its pure
  80-byte header must carry zero `nBits`, while the reviewed publication's
  non-zero external target drives the persisted child-work verdict. Historical
  imports now obtain this rule from shared source metadata rather than a
  chain-name exception.

## [0.7.12] - 2026-09-03

- Annotate the two F2Pool `bad-blk-sigops` stale blocks (heights 783,426 and
  784,121) as body-invalid without reclassifying them: a new operator-imported
  `body_invalid_stale` reference table (migration 0017, loaded by
  `import-body-invalid-stales` from the pinned
  `data/consensus/body_invalid_stales.csv` mirror, refreshed with the other
  Research pins) is joined at API projection time as a nullable
  `block.body_invalid` object and an optional tree-node `body_invalid_rule`,
  and the UI surfaces a Body validity row with the rule's help dialog and an
  external evidence link plus a tree hover annotation. Annotated blocks remain
  ordinary `kind='stale'` rows; classification, orphan derivation, and
  reconciliation never consult the table, and the importer refuses any hash
  that is also in the pinned error-block catalogue.

## [0.7.11] - 2026-09-02

- Bulk-reconcile historical parents whose canonical classification is already
  proven by the local Bitcoin Core-backed block, while retaining strict
  per-parent handling for stale, error, unknown, or inconsistent evidence.

## [0.7.10] - 2026-09-02

- Preserve observation timestamps when refreshing historical rows, and skip
  durable parent reconciliation when only publication provenance or
  presentation text changed.

## [0.7.9] - 2026-09-02

- Refresh a stored parent coinbase-output text projection when the canonical
  Research publication renders the same observation in its newer claim format.
  Binary outputs and full coinbase transactions remain immutable evidence.

## [0.7.8] - 2026-09-02

- Reuse compatible, proven parent classifications from the derived `block`
  state when a changed historical publication artifact names an already-known
  parent. This avoids replaying Bitcoin Core header and full-block RPCs for
  existing Core-attested canonical and structurally complete stale evidence;
  event-only canonical, unknown, or incompatible state still requires strict
  live Core classification, as does the dedicated error-observation aggregate.

## [0.7.7] - 2026-09-01

- Pin Research's 1,037,005 ordinary events, 21-row stale-descendant aggregate,
  and 86 error observations covering 39 parents, including four recovered BIP34
  height mismatches. The publication includes 456,660 canonical parent-only
  Namecoin rows; the manifest pins that count, and `import-all` skips them until
  Research can authenticate a child hash or height. The first import refreshes
  22 existing rows with canonical provenance and recovered fields and uses the
  larger full-reconcile budget.

- Derive publication totals, parent-only counts, and observation-chain
  inventories from Research's publication, preflight it once, and refresh both
  Monitor pins from one revision via `just gen-research-publication-pins`.

## [0.7.6] - 2026-08-29

- Accept the research catalogue's legacy `median_time_past_violation` token as
  equivalent to a live `time_below_mtp` verdict while preserving the pinned
  token in imported evidence.

## [0.7.5] - 2026-08-29

- Determine `import-all` work from normalized publication-owned database state
  instead of cached artifact SHAs. Skip matching event files and retained error
  observations before taking the Bitcoin Core lock, while still completing any
  pending reconciliation, source-health, or published-stale work. Remove the
  receipt seed, CLI flag, code paths, store APIs, and receipt table.

## [0.7.4] - 2026-08-28

- Pin the historical publication and compact error-block catalogue to
  merge-mining-research `c26e86c`. Error observations stay 78 rows across 35
  parents; the aggregate is now the 34-column monitor union (27 evidence
  columns plus the seven RSK sidecar columns).

- `import-all` records each artifact's content SHA after a successful
  publication finalize and skips classify, write, and authoritative reconcile
  when that SHA has not changed. `--seed-imported-receipts` loads the last
  imported pin (`091e01a`) only after matching event-scope provenance counts
  prove that pin is already present. `import-dataset` does not write receipts;
  a successful single-chain import deletes that chain's receipt so the next
  `import-all` re-runs authoritative reconcile.

## [0.7.3] - 2026-08-28

- Show which item of the current Go-to index the stepper is on (`n of N`)
  while walking stales, stale branches, error blocks, orphans, and orphan
  branches.

- Remove the Auto refresh interval picker and dedicated refresh icon from the
  topbar. Data still refreshes every 60s; click the Updated stamp to reload the
  current view, or Retry when that view has not loaded yet.

- Pin the compact error-block catalogue and historical publication to the same
  merge-mining-research commit. Error observations are now 78 rows across 35
  parents, including Hathor-witnessed 649674 and the 2026 F2Pool
  `time_below_mtp` twin at 957780. That research commit already publishes
  Hathor's 3,664-row event file, so the historical event total is 580,320.

## [0.7.2] - 2026-08-27

- Clear stale Bitcoin Core link-error telemetry after Core revalidates the
  already-complete cached row and its predecessor link, including zero-work
  recovery batches.

## [0.7.1] - 2026-08-27

- Recover a shallow Bitcoin Core fork after a long follow-mode outage by
  repairing only the bounded divergent suffix ending at the persisted cursor,
  then resuming ordinary paged catch-up to the live tip. Continue to fail
  closed when that cursor-centred lookback has no complete matching ancestor.

## [0.7.0] - 2026-08-25

- Import complete historical error-block child witnesses through a separate,
  Core-required publication aggregate. Preserve their source-row provenance
  and RSK sidecars outside ordinary authoritative snapshot deletion. Reclassify
  any previously stored stale or unknown parent to `error_block`, replay known
  archive coordinates idempotently, and reject changed evidence at the store
  boundary.

- Derive `time_below_mtp` error blocks from a required Bitcoin Core node by
  checking the exact eleven linked predecessor headers of an otherwise
  Core-absent, proof-of-work-valid parent. Preserve the Core-derived predecessor
  height source and rejection token in the read model, retain the pinned
  catalogue as a fallback and consistency check, and add
  `reclassify-parent` for a narrow, cascade-safe repair of existing evidence.

- Repair bounded near-tip Bitcoin reorgs in follow mode by capturing one
  tip-pinned Core view, atomically replacing the divergent canonical suffix,
  and retaining displaced blocks as stale evidence. Persist dependent
  reconciliation and dependent expansion as restart-safe queue phases so a
  process exit cannot lose a deeper cascade frontier. Serialize Core-backed
  classification and ordinary backbone writes against the suffix switch, drain
  pending suffix work before a Core-header-cache refresh reclassifies rows, and
  rebind existing same-height stale blocks to the replacement canonical
  competitor.
  Preserve unrelated producer failures and reconcile-pending status when
  recording or clearing repair telemetry, including during concurrent repairs,
  temporarily suspend and later restore unrelated producer error details while
  durable reconciliation is pending, consume structural conflicts covered by
  the committed replacement suffix, and continue to fail closed when the common
  ancestor lies outside the configured live window. Re-plan once when an
  in-flight classifier commits a same-height conflict after the initial repair
  scan. Replay durable parents with strict live Core classification so newly
  inferable stale children are not stranded, and retain Core-derived pool
  attribution when displaced AuxPoW-backed rows are replayed.

- Replace the compiled Bitcoin nBits epoch table with a sparse Postgres cache
  populated from a required Bitcoin mainnet Core node. Capture, import, and
  reconciliation refresh Core headers through the synced tip before classifying
  evidence, while live pollers and backbone follow refresh a stable Core
  snapshot on every tick. Historical imports retain one table through their
  derived rebuild. Shallow replacements and timestamp-overlapping retarget
  boundaries reclassify existing orphan rows while other expanded coverage
  revisits pending rows, with durable retry markers for an interrupted sweep.
  An advancing horizon verifies the old shallow horizon
  before retaining timestamp coverage. Retarget boundaries are marked final at
  100 blocks deep. Timestamp coverage does not regress when a valid later Core
  header has an older timestamp. The API remains Core-RPC-free by reading the
  persisted cache. A cache-driven recheck requires fresh Core evidence, so an
  RPC failure leaves its durable retry marker set. Cache headers and timestamp
  coverage are read from one database snapshot. The `--allow-unclassified`
  import bypass is removed. A strict BIP34 claim above Core's cached horizon
  remains pending even when its difficulty epoch is cached. The first Core-cache
  population conservatively revisits existing orphan classifications. Cache
  refresh waits for an in-flight classification transaction, so its completed
  sweep cannot miss a later commit made from an old cache snapshot. Cache
  readers acquire that shared lock before parent locks, and a non-mainnet Core
  tip holds rather than revoking a claimed mainnet height. A fresh Core tip
  rejects a claimed BIP34 height more than 144 blocks beyond it even if a stale
  cache already covers that height.

- Atomically repair existing strict/weak classifications with an
  `import-known-stales` membership update.

- Retry transient Core-header-cache refresh failures in `sync-bitcoin-core --follow`
  without masking sync progress or a typed cache/backbone integrity failure.

## [0.6.0] - 2026-08-15

- Turn findings figures into claim-led evidence panels with compact metrics,
  accessible summaries, multi-series charts, bars, discrete lollipops,
  annotations, highlighted periods, and event timelines. Show Foundry's RSK
  and Fractal exit across the full paired window with separate weekly series
  and an explicit zero tail, and give every other published finding a visual
  form suited to its evidence.

- Remove the `reclassify-pools` RSK skip, so each run scans the active corpus
  instead of usually paying for two fingerprint scans without short-circuiting.
  The skip required an unchanged active set, which continuous RSK capture
  normally invalidates every block. Migration `0009` drops the obsolete
  `rsk_reclassify_watermark` singleton.

## [0.5.0] - 2026-08-09

- Navigate catalogued error blocks as a first-class Go to target, backed by
  `/api/v1/navigator/error-block`. Ordering is Bitcoin height descending then
  stored hash bytes ascending, because the catalogue carries more than one
  block at some heights and paging on height alone would skip or repeat members
  of a group. Selecting one directly, by click or shared link, hydrates the
  target so stepping continues from it.
- Render an error block's consensus rejection as prose with per-rule help
  rather than the catalogue's raw token, fall back to the raw value without a
  help control for a rule the frontend has not mapped, and replace the absent
  competition panel with an explicit note that the block never raced.
- Count catalogued error blocks against the tree-window node budget, and only
  offer blocks the tree will render. The budget previously counted stale rows
  alone, so a window containing an error block could be advertised and then
  rejected by `/tree` as too large; sourceless catalogue rows are now excluded
  from both the budget and the navigator, matching what the tree shows.
- Reject height-axis navigator cursors whose bounds exceed a 32-bit height.
  Such a cursor previously wrapped to a negative height and returned an
  incorrect page as a success.

- Explain `child_block_time` where it is read. The block drawer's Child Time row
  gains a help topic covering what the stamp is, why AuxPoW's child-first
  commitment settles it before the Bitcoin work exists, and how far the two
  asymmetric reading rules actually reach, and shows each auxiliary block's
  offset from that block's Bitcoin header time (omitted when either stamp is
  unavailable or the difference is not exactly representable). Document the same
  model in `docs/data-model.md`, including what a stored child header does and
  does not prove.

- Classify headers in the pinned research error-block catalogue as
  `error_block`, preserving their primary consensus rejection token through the
  read model, API, tree controls, and source-health counts. Keep this
  full-proof-of-work but consensus-invalid state distinct from stale and BTC
  orphan evidence.

- Import the pinned 576,662-row research publication through one normalized
  27-chain contract, with complete checksum/schema/count preflight, Git LFS
  diagnostics, deterministic `import-all`, shared Bitcoin-parent
  classification caching, and no legacy artifact fallbacks.
- Preserve authenticated child height, hash, header, time, and `nBits` as
  independent nullable evidence. Use exact child-hash identity or partial
  height-plus-parent identity, derive exact identity from a header when needed,
  promote partial observations in place, and reject ambiguous or contradictory
  refinement. Keep live state reads and child-target verdict refinement safe
  for hashless historical rows. Fail the upgrade before schema changes if
  legacy rows conflict with the stronger exact identity.
- Reconcile manifest-backed historical and partial sources as authoritative
  snapshots while keeping live-source and operator CSV imports additive.
  Retire manifest-backed provenance from superseded publication commits and
  commit each base/provenance snapshot atomically, then drain parent and
  dependent read-model work through a durable resumable queue without retaining
  one advisory lock per imported parent.
  Retain source taxonomy and all published parent-coinbase evidence without
  collapsing distinct source rows, and treat Doichain's zero-row survey as a
  database no-op.
- Keep incomplete `--allow-unclassified` diagnostics additive, reject a zero
  import limit before mutation, derive the child-target verdict from published
  `nBits`, and route the targeted stale-branch pass through the durable parent
  and dependent reconciliation queue.
- Serialize historical source-health invalidation against rebuilds, refuse to
  mark source health ready while durable historical work remains, preserve a
  Hathor row promoted from hashless identity, and reject contradictory immutable
  RSK sidecar evidence before provenance can commit.
- Expose nullable child evidence through block detail and render unavailable
  fields explicitly instead of zero or placeholder values.
- Report historical write outcomes from the store's exact/partial identity
  decision instead of re-querying identities in the importer, and combine
  candidate parsing, validation, and preclassification into one stream. Bind
  classification and mutation reads to the already-verified artifact handle,
  preflight the required aggregate, and fail source health closed until the
  final multi-chain rebuild succeeds.
- Fill the Bitcoin RPC client's existing bounded concurrency during historical
  preclassification, and defer predecessor read-model queries until Bitcoin
  Core proves a candidate header is absent.
- Retry transient Bitcoin Core transport and warmup failures with bounded
  exponential backoff, fail parent preclassification explicitly after
  exhaustion, and validate direct-stale and stale-descendant publication
  statuses against their distinct canonical tokens.
- Accept a publication's cross-chain strict BTC-orphan promotion when the
  current chain independently proves the weaker orphan verdict, and let a
  direct Bitcoin Core stale attestation supersede an archived canonical source
  label while preserving that source provenance.

## [0.4.2] - 2026-07-30

- Bound the RSK miner-identity keyset scan on both sides of the
  evidence-to-event join. The cursor previously constrained only
  `merge_mining_event`, leaving the evidence-side index scan unbounded so
  every page re-walked `rsk_evidence_event_unique` from the start and the
  pass ran quadratic in corpus size (production: ~11.6M index rows per page
  and a ~63-hour projection, against 500 rows per page and under an hour
  once bounded).

## [0.4.1] - 2026-07-29

- Bound and center the findings canvas in a single minmax(0, 900px)
  column so wide screens keep readable card widths and a shared left edge
  across the feed and article states.

## [0.4.0] - 2026-07-29

- Add the findings content pipeline: one hand-authored JSON file per finding
  in `data/findings/`, validated by the feature-gated
  `mmm-capture::findings_registry` (content invariants, calendar dates,
  registered source codes, `[^N]` citation integrity) and compiled into
  `www/js/findings.generated.js` by `just gen-source-artifacts`, drift-gated
  in `cargo test`.
- Add the Findings view (`?view=findings`): the corpus as a month-grouped
  feed with category, status, and shared Source filtering. Opening a card
  replaces the feed with a cited article (`finding=<slug>`, serialized only
  while findings is active), with theme-aware line-series evidence figures
  and typed anchors that jump to the header tree or open source details.
  The drawer column collapses on this view; its state survives for return.
- Seed six findings: the September 2025 Foundry stale cluster (20
  full-difficulty blocks at 16 heights, ~46.9 BTC foregone, unseen by any
  observer; new `pool-incident` category), Hathor's ~3,500x hashrate
  collapse (2026-06-10), Foundry's single-block entry (2025-04-22) and exit
  (2026-06-24) from RSK and Fractal, the Elastos exploit halt (2026-07-20),
  and Terracoin's full-difficulty win (2026-05-20).

## [0.3.0] - 2026-07-28

- Add the Header Time Delta view: a distribution of how far apart each stale
  block and its canonical competitor timestamped their headers, with a focus
  window, off-scale gutters, a symmetric-log full-range strip, Coverage and
  Table tabs, and block-detail cross-links. Backed by the new read-only
  `GET /api/v1/competitions` endpoint.
- Exclude known stales from strict/weak BTC-orphan classification. Migration
  0006 adds the operator-imported `known_stale_block` membership, loaded by
  `import-known-stales` (atomic, strict by default) from the upstream
  `bitcoin-data/stale-blocks` dataset; the classifier excludes members
  outright, `reclassify-known-stales` retroactively demotes contaminated
  rows, `import-dataset` refuses an empty membership, and every producer
  entry path warns when it is empty.
- Align vocabulary and ingest with the published merge-mining-research
  history: the importer prefers the committed monitor-evidence exports, the
  broad evidence state is spelled `unknown` (legacy `orphan` still read on
  ingest), the excluded verdict token is renamed from `btc_stale_excluded` to
  `excluded` across DB, API, and frontend (migration 0005), and the
  historical-source manifest is re-pinned to the published research commit.
- Extend `import-dataset` to the six live chains, requiring exact child
  identity and constructing the RSK evidence sidecar during import; the
  non-live exact-child-field chains (VCash, Lyncoin, SixEleven) resolve to
  the research repo's committed canonical-blocks artifacts.
- Derive the historical importer's chain table from the shared source
  registry, add a cross-repo BIP34 drift guard against the research checkout
  (`doichain` joins the strict set), and document the ParentKind/BlockKind
  enum boundary.
- Add Open Graph and Twitter social cards.
- Consolidate the code landed since 0.2.0 into shared API, read-model, and
  frontend helpers: net 250 fewer lines with no behavior change.

## [0.2.1] - 2026-07-13

- Correct source-rail bylines and source-modal documentation for Bitcoin Stash,
  BLAST, Doichain, Fusioncoin, Jax.Network, Jincoin, Lyncoin, SixEleven, and
  VCash.

## [0.2.0] - 2026-07-11

- Recover every Lyncoin Bitcoin-merge-mined header through height 260,499 and
  all 999,407 available SixEleven blocks. Bitcoin Core classified 11 Lyncoin
  parents and 7 SixEleven parents as canonical; neither chain produced a stale
  winner.
- Keep the recovery limits visible: VCash contributes 68 explorer mappings
  confirmed as canonical and completed with block evidence by Bitcoin Core
  (not the VCash blockchain), while Doichain is a completed zero-row survey
  after 429,401 AuxPoW commitments produced no Bitcoin block winner.
- Make source IDs permanent and retire ID 32. Mazacoin is removed because its
  consensus source contains no AuxPoW implementation, so it is not a Bitcoin
  merge-mined source.

## [0.1.0] - 2026-07-02

- Initial public release.
