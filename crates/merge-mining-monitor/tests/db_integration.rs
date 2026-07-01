#![cfg(feature = "db-integration")]

mod support;

#[path = "db_integration/elastos.rs"]
mod elastos;
#[path = "db_integration/event_pool_attribution.rs"]
mod event_pool_attribution;
#[path = "db_integration/fractal_reward.rs"]
mod fractal_reward;
#[path = "db_integration/hathor.rs"]
mod hathor;
#[path = "db_integration/hathor_reward_replay.rs"]
mod hathor_reward_replay;
#[path = "db_integration/historical_ingest.rs"]
mod historical_ingest;
#[path = "db_integration/mutation.rs"]
mod mutation;
#[path = "db_integration/orphan_classification.rs"]
mod orphan_classification;
#[path = "db_integration/poller.rs"]
mod poller;
#[path = "db_integration/reclassify_pools.rs"]
mod reclassify_pools;
#[path = "db_integration/reconcile_core.rs"]
mod reconcile_core;
#[path = "db_integration/reconcile_repair.rs"]
mod reconcile_repair;
#[path = "db_integration/rsk.rs"]
mod rsk;
#[path = "db_integration/rsk_miner_identities.rs"]
mod rsk_miner_identities;
#[path = "db_integration/source_health.rs"]
mod source_health;
