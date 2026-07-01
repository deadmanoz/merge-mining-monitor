//! Per-chain producer base-table SQL: each chain's capture writer, its 1:1
//! evidence sidecar, the per-chain `merge_mining_event` read queries, and the
//! RSK pool-identity adapters over the generic `crate::pool` helpers. Shared,
//! table-generic SQL stays at the crate root (`event`, `pool`, `poll_cursor`,
//! `pending_reconcile`). A new merge-mined chain is a new `chains/<chain>.rs`.

pub(crate) mod elastos;
pub(crate) mod hathor;
pub(crate) mod rsk;
