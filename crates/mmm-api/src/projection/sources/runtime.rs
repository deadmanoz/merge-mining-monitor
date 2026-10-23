//! Operational readiness overlays legacy progress without changing its timestamps.

use super::SourceSyncStatus;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRuntime {
    pub phase: String,
    pub state: String,
    pub dependency: Option<String>,
    pub reason_code: Option<String>,
    pub failure_since: Option<i64>,
    pub consecutive_failures: i64,
    pub last_attempt_at: Option<i64>,
    pub last_success_at: Option<i64>,
    pub next_retry_at: Option<i64>,
    pub heartbeat_at: i64,
}

pub(super) fn apply_runtime(sync: &mut SourceSyncStatus, runtime: Option<SourceRuntime>, now: i64) {
    if !matches!(sync.mode, "live" | "bitcoin-core-backbone") {
        return;
    }
    let Some(runtime) = runtime else {
        return;
    };
    // Evidence errors retain priority, including after loss of runtime liveness.
    if sync.state != "error" {
        sync.state = if now.saturating_sub(runtime.heartbeat_at) > 90 || runtime.state == "stopped"
        {
            "stale"
        } else {
            match runtime.state.as_str() {
                "failed" => "error",
                "waiting" => "waiting",
                "healthy" if runtime.phase == "running" && runtime.last_success_at.is_some() => {
                    match (sync.progress_height, sync.target_height) {
                        (Some(height), Some(target)) if height >= target => "live",
                        _ => "catching_up",
                    }
                }
                _ => "catching_up",
            }
        };
    }
    sync.runtime = Some(runtime);
}
