# Remote Round Trips And Batch Work

Production runs far from its data: the round trip from the production host to
Bitcoin Core, to every child-chain node, and to the research VM is about
340 ms, while the development VM sees well under 1 ms to the same hosts. A
loop that makes one remote call per row costs a thousand times more in
production than anywhere it is tested, and three production incidents came
from exactly that shape: a call per item that nothing had budgeted, bounded,
or timed.

Any operation that iterates over rows, heights, or candidates and performs a
remote call or a per-item database statement must satisfy all of the following:

1. **PR budget line:** State its round-trip budget in the PR description:
   expected items times remote round trips per item, at the production
   round-trip time (~340 ms). Every PR that changes a producer, store, or
   read-model crate carries a `Round-trip budget:` line (`none` when it adds no
   remote or per-item work) so a reviewer never has to guess whether the author
   looked; a PR without one is incomplete.
2. **Local state first and transport batching:** Make no remote call for an item
   whose answer a local table already holds (the `block` table holds every
   canonical Bitcoin height; `child_chain_head` holds the block the chain last
   carried at a height), and batch calls where the transport allows it. A call
   per item is acceptable only where it is irreducible (a height's block hash at
   its node, a candidate's strict Core classification) and the budget, the
   bound, and the timing receipt justify the count; on a path something waits
   on, such as a tick, the count is the window the budget states and no more.
3. **Bounded or cursor-resumable execution:** Be bounded, or resumable from a
   cursor persisted in the database, and never run unbounded work inside
   producer startup or a per-tick refresh. Scheduled work is recorded as pending
   and consumed by an explicit job.
4. **Per-batch lock scope:** Hold no global advisory lock across more than one
   batch. A batch is processed under the lock; the lock is released before the
   next batch.
5. **Progress and transport telemetry:** Log progress through `ProgressReporter`
   (done, total, rate, ETA at a fixed interval, and a `job ended` summary with
   each RPC client's `RpcMetrics` line whether the job finished or aborted). A
   live poller reports the same counters as per-tick deltas on its `poll tick`
   line.
6. **Transport-boundary test:** Carry a test that pins its remote-call count per
   item at the transport boundary (an `RpcMetrics` snapshot, or the scripted
   Core fixture in `docs/testing.md`), not at a fake classifier's call count.
7. **Production-latency timing receipt:** Be timed at production round-trip
   latency before it ships, with the receipt (items, round trips, wall time) in
   the PR (`docs/testing.md`).

## Per-Item Database Statements

Items 6 and 7 apply to remote calls. A loop whose per-item cost is a database
statement (Postgres runs beside the service, so the cost is the statement's plan
times its count, the shape of the quadratic `reclassify-pools` RSK scan in issue
#23) states statements per item in its budget, meets the bound, lock, and
progress requirements as written, and is reviewed by its plan (`EXPLAIN` against
production row counts) rather than by a transport counter.

Red on any of these is fixed by restructuring the operation, never by an
allowlist or a larger timeout.
