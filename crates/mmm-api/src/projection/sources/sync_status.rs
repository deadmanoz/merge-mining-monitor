//! Pure sync-status classification for the `/api/v1/sources` projection.
//!
//! I/O-free: given a source's lifecycle/kind plus its poll-cursor and
//! backbone-sync progress, derive the wire `SourceSyncStatus`. Kept apart from the
//! SQL loaders in the parent module so the state machine is unit-testable without a
//! database or a wall clock.

use mmm_capture::source_registry::{self, SourceKind, SourceLifecycle};

use super::SourceSyncStatus;

/// Decoded poll_cursor progress (height, optional target, updated-at epoch) for
/// an auxpow source. Private input to `classify_source_sync`; the cursor age vs
/// CAPTURE_PROGRESS_STALE_THRESHOLD_SECS drives the live/stale/catching_up state.
#[derive(Debug, Clone, Copy)]
pub(super) struct SourceCursorProgress {
    pub(super) height: i32,
    pub(super) target_height: Option<i32>,
    pub(super) updated_at_epoch: i64,
}

/// Decoded bitcoin_core_sync_state (contiguous mode) progress for the
/// live-chaintip source: target tip, contiguous-complete height, last error, and
/// updated-at epoch. Private input to `classify_bitcoin_core_backbone_sync`. A
/// negative contiguous height means never-progressed (treated as not_started).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SourceBackboneProgress {
    pub(super) target_tip_height: Option<i32>,
    pub(super) contiguous_complete_height: i32,
    pub(super) last_error_code: Option<String>,
    pub(super) last_error_height: Option<i32>,
    pub(super) updated_at_epoch: i64,
}

/// Window (7 days) within which a source's `last_seen_at` counts as `fresh` for
/// the top-level `status` field; older is `stale`. Distinct from the capture
/// sync-progress staleness threshold below.
const SOURCE_FRESHNESS_THRESHOLD_SECS: i64 = 7 * 24 * 60 * 60;
/// Cursor/backbone updated-at age (1 hour) past which a live source's `sync.state`
/// is reported `stale`. Governs capture-progress freshness, NOT the 7-day
/// evidence freshness used for the top-level `status`.
const CAPTURE_PROGRESS_STALE_THRESHOLD_SECS: i64 = 60 * 60;

/// Top-level source `status` wire value from `last_seen_at`: `not_started` when
/// never seen, else `fresh`/`stale` against SOURCE_FRESHNESS_THRESHOLD_SECS
/// (saturating, so a clock-skew future last_seen reads fresh).
pub(super) fn source_status(last_seen_at: Option<i64>, reference_now: i64) -> &'static str {
    let Some(last_seen_at) = last_seen_at else {
        return "not_started";
    };
    if reference_now.saturating_sub(last_seen_at) <= SOURCE_FRESHNESS_THRESHOLD_SECS {
        "fresh"
    } else {
        "stale"
    }
}

/// Resolve a source `code` through `source_registry::by_code` to its
/// lifecycle+kind, then classify capture progress. An unregistered code maps to
/// the `unknown`/`unknown` sync status (never panics on a stray DB row).
pub(super) fn sync_for_source_code(
    code: &str,
    cursor: Option<SourceCursorProgress>,
    backbone: Option<SourceBackboneProgress>,
    reference_now: i64,
) -> SourceSyncStatus {
    let Some(definition) = source_registry::by_code(code) else {
        return SourceSyncStatus::unknown();
    };
    classify_source_sync(
        definition.lifecycle,
        definition.kind,
        cursor,
        backbone,
        reference_now,
    )
}

/// Pure classification of a source's `sync` status from its registry
/// lifecycle+kind and decoded cursor/backbone progress. Non-live lifecycle
/// classes return their corresponding empty state; live-chaintip uses the
/// backbone path; live AuxPoW uses the cursor path (catching_up when below
/// target, stale past CAPTURE_PROGRESS_STALE_THRESHOLD_SECS). Everything else
/// is unknown. No I/O, so the inline tests pin every branch.
fn classify_source_sync(
    lifecycle: SourceLifecycle,
    kind: SourceKind,
    cursor: Option<SourceCursorProgress>,
    backbone: Option<SourceBackboneProgress>,
    reference_now: i64,
) -> SourceSyncStatus {
    if lifecycle == SourceLifecycle::Historical {
        return SourceSyncStatus::empty("historical", "historical");
    }
    if lifecycle == SourceLifecycle::Partial {
        return SourceSyncStatus::empty("partial", "partial");
    }
    if lifecycle == SourceLifecycle::Surveyed {
        return SourceSyncStatus::empty("surveyed", "surveyed");
    }
    if lifecycle == SourceLifecycle::Catalogued {
        // Catalogued chains have no producer and no recovered evidence, so they
        // are non-operational: an explicit empty "catalogued" state rather than
        // the generic "unknown" fallthrough (which reads as a stray DB row).
        return SourceSyncStatus::empty("catalogued", "catalogued");
    }
    if kind == SourceKind::LiveChaintip {
        return classify_bitcoin_core_backbone_sync(backbone, reference_now);
    }
    let mode = match (lifecycle, kind) {
        (SourceLifecycle::Live, SourceKind::Auxpow) => "live",
        _ => return SourceSyncStatus::unknown(),
    };
    let Some(cursor) = cursor else {
        return SourceSyncStatus::empty(mode, "not_started");
    };
    let state = if reference_now.saturating_sub(cursor.updated_at_epoch)
        > CAPTURE_PROGRESS_STALE_THRESHOLD_SECS
    {
        "stale"
    } else if cursor
        .target_height
        .is_some_and(|target_height| cursor.height < target_height)
    {
        "catching_up"
    } else {
        "live"
    };
    SourceSyncStatus {
        mode,
        state,
        progress_height: Some(cursor.height),
        progress_updated_at: Some(cursor.updated_at_epoch),
        latest_evidence_at: None,
        target_height: cursor.target_height,
        error_code: None,
        error_height: None,
    }
}

/// Pure backbone-sync classification for the live-chaintip source: error (last
/// error set) > stale (updated-at past threshold) > not_started/catching_up/live
/// from contiguous-complete vs target tip. A negative contiguous height means
/// never-progressed: state not_started and progress fields null. No I/O; pinned
/// by the inline backbone tests.
fn classify_bitcoin_core_backbone_sync(
    backbone: Option<SourceBackboneProgress>,
    reference_now: i64,
) -> SourceSyncStatus {
    let Some(backbone) = backbone else {
        return SourceSyncStatus::empty("bitcoin-core-backbone", "not_started");
    };
    let state = if backbone.last_error_code.is_some() {
        "error"
    } else if reference_now.saturating_sub(backbone.updated_at_epoch)
        > CAPTURE_PROGRESS_STALE_THRESHOLD_SECS
    {
        "stale"
    } else {
        match backbone.target_tip_height {
            None => "not_started",
            Some(_) if backbone.contiguous_complete_height < 0 => "not_started",
            Some(target_tip_height) if backbone.contiguous_complete_height < target_tip_height => {
                "catching_up"
            }
            Some(_) => "live",
        }
    };
    let has_real_progress = state != "not_started" && backbone.contiguous_complete_height >= 0;
    SourceSyncStatus {
        mode: "bitcoin-core-backbone",
        state,
        progress_height: has_real_progress.then_some(backbone.contiguous_complete_height),
        progress_updated_at: has_real_progress.then_some(backbone.updated_at_epoch),
        target_height: backbone.target_tip_height,
        latest_evidence_at: None,
        error_code: backbone.last_error_code,
        error_height: backbone.last_error_height,
    }
}

impl SourceSyncStatus {
    /// A `SourceSyncStatus` carrying only the given `mode`/`state` with all progress,
    /// evidence, and error fields null. The shared constructor for the no-progress
    /// branches (historical, not_started, unknown).
    fn empty(mode: &'static str, state: &'static str) -> Self {
        Self {
            mode,
            state,
            progress_height: None,
            progress_updated_at: None,
            target_height: None,
            latest_evidence_at: None,
            error_code: None,
            error_height: None,
        }
    }

    /// The `unknown`/`unknown` sync status used for unregistered codes.
    fn unknown() -> Self {
        Self::empty("unknown", "unknown")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;
    const FRESH_CURSOR: SourceCursorProgress = SourceCursorProgress {
        height: 12_345,
        target_height: None,
        updated_at_epoch: NOW - 60,
    };
    const STALE_CURSOR: SourceCursorProgress = SourceCursorProgress {
        height: 12_340,
        target_height: None,
        updated_at_epoch: NOW - CAPTURE_PROGRESS_STALE_THRESHOLD_SECS - 1,
    };

    fn auxpow_sync(
        mode: &'static str,
        state: &'static str,
        progress_height: i32,
        progress_updated_at: i64,
    ) -> SourceSyncStatus {
        auxpow_sync_with_target(mode, state, progress_height, progress_updated_at, None)
    }

    fn auxpow_sync_with_target(
        mode: &'static str,
        state: &'static str,
        progress_height: i32,
        progress_updated_at: i64,
        target_height: Option<i32>,
    ) -> SourceSyncStatus {
        SourceSyncStatus {
            mode,
            state,
            progress_height: Some(progress_height),
            progress_updated_at: Some(progress_updated_at),
            target_height,
            latest_evidence_at: None,
            error_code: None,
            error_height: None,
        }
    }

    fn backbone(
        contiguous_complete_height: i32,
        target_tip_height: Option<i32>,
        updated_at_epoch: i64,
    ) -> SourceBackboneProgress {
        SourceBackboneProgress {
            target_tip_height,
            contiguous_complete_height,
            last_error_code: None,
            last_error_height: None,
            updated_at_epoch,
        }
    }

    #[test]
    fn classify_live_auxpow_capture_progress_from_cursor_age() {
        assert_eq!(
            classify_source_sync(
                SourceLifecycle::Live,
                SourceKind::Auxpow,
                Some(FRESH_CURSOR),
                None,
                NOW,
            ),
            auxpow_sync("live", "live", 12_345, NOW - 60)
        );
        assert_eq!(
            classify_source_sync(
                SourceLifecycle::Live,
                SourceKind::Auxpow,
                Some(STALE_CURSOR),
                None,
                NOW,
            ),
            auxpow_sync(
                "live",
                "stale",
                12_340,
                NOW - CAPTURE_PROGRESS_STALE_THRESHOLD_SECS - 1,
            )
        );
        assert_eq!(
            classify_source_sync(SourceLifecycle::Live, SourceKind::Auxpow, None, None, NOW),
            SourceSyncStatus::empty("live", "not_started")
        );
    }

    #[test]
    fn classify_live_auxpow_catching_up_from_target_height() {
        let catching_up = SourceCursorProgress {
            target_height: Some(12_346),
            ..FRESH_CURSOR
        };
        assert_eq!(
            classify_source_sync(
                SourceLifecycle::Live,
                SourceKind::Auxpow,
                Some(catching_up),
                None,
                NOW,
            ),
            auxpow_sync_with_target("live", "catching_up", 12_345, NOW - 60, Some(12_346))
        );

        let caught_up = SourceCursorProgress {
            target_height: Some(12_345),
            ..FRESH_CURSOR
        };
        assert_eq!(
            classify_source_sync(
                SourceLifecycle::Live,
                SourceKind::Auxpow,
                Some(caught_up),
                None,
                NOW,
            ),
            auxpow_sync_with_target("live", "live", 12_345, NOW - 60, Some(12_345))
        );

        let stale_behind_target = SourceCursorProgress {
            target_height: Some(12_500),
            ..STALE_CURSOR
        };
        assert_eq!(
            classify_source_sync(
                SourceLifecycle::Live,
                SourceKind::Auxpow,
                Some(stale_behind_target),
                None,
                NOW,
            ),
            auxpow_sync_with_target(
                "live",
                "stale",
                12_340,
                NOW - CAPTURE_PROGRESS_STALE_THRESHOLD_SECS - 1,
                Some(12_500),
            )
        );
    }

    #[test]
    fn classify_bitcoin_core_backbone_progress() {
        assert_eq!(
            classify_source_sync(
                SourceLifecycle::Live,
                SourceKind::LiveChaintip,
                None,
                None,
                NOW,
            ),
            SourceSyncStatus::empty("bitcoin-core-backbone", "not_started")
        );
        assert_eq!(
            classify_source_sync(
                SourceLifecycle::Live,
                SourceKind::LiveChaintip,
                None,
                Some(backbone(-1, Some(953_700), NOW - 60)),
                NOW,
            ),
            SourceSyncStatus {
                mode: "bitcoin-core-backbone",
                state: "not_started",
                progress_height: None,
                progress_updated_at: None,
                target_height: Some(953_700),
                latest_evidence_at: None,
                error_code: None,
                error_height: None,
            }
        );
        assert_eq!(
            classify_source_sync(
                SourceLifecycle::Live,
                SourceKind::LiveChaintip,
                None,
                Some(backbone(953_699, Some(953_700), NOW - 60)),
                NOW,
            ),
            SourceSyncStatus {
                mode: "bitcoin-core-backbone",
                state: "catching_up",
                progress_height: Some(953_699),
                progress_updated_at: Some(NOW - 60),
                target_height: Some(953_700),
                latest_evidence_at: None,
                error_code: None,
                error_height: None,
            }
        );
        assert_eq!(
            classify_source_sync(
                SourceLifecycle::Live,
                SourceKind::LiveChaintip,
                None,
                Some(backbone(953_700, Some(953_700), NOW - 60)),
                NOW,
            ),
            SourceSyncStatus {
                mode: "bitcoin-core-backbone",
                state: "live",
                progress_height: Some(953_700),
                progress_updated_at: Some(NOW - 60),
                target_height: Some(953_700),
                latest_evidence_at: None,
                error_code: None,
                error_height: None,
            }
        );
    }

    #[test]
    fn classify_bitcoin_core_backbone_error_and_stale_states() {
        assert_eq!(
            classify_source_sync(
                SourceLifecycle::Live,
                SourceKind::LiveChaintip,
                None,
                Some(backbone(
                    953_699,
                    Some(953_700),
                    NOW - CAPTURE_PROGRESS_STALE_THRESHOLD_SECS - 1,
                )),
                NOW,
            )
            .state,
            "stale"
        );
        let never_progressed_stale = classify_source_sync(
            SourceLifecycle::Live,
            SourceKind::LiveChaintip,
            None,
            Some(backbone(
                -1,
                Some(953_700),
                NOW - CAPTURE_PROGRESS_STALE_THRESHOLD_SECS - 1,
            )),
            NOW,
        );
        assert_eq!(never_progressed_stale.state, "stale");
        assert_eq!(never_progressed_stale.progress_height, None);
        assert_eq!(never_progressed_stale.progress_updated_at, None);
        assert_eq!(
            classify_source_sync(
                SourceLifecycle::Live,
                SourceKind::LiveChaintip,
                None,
                Some(SourceBackboneProgress {
                    last_error_code: Some("coinbase_fetch_failed".to_owned()),
                    last_error_height: Some(953_700),
                    ..backbone(953_699, Some(953_700), NOW - 60)
                }),
                NOW,
            ),
            SourceSyncStatus {
                mode: "bitcoin-core-backbone",
                state: "error",
                progress_height: Some(953_699),
                progress_updated_at: Some(NOW - 60),
                target_height: Some(953_700),
                latest_evidence_at: None,
                error_code: Some("coinbase_fetch_failed".to_owned()),
                error_height: Some(953_700),
            }
        );
        let targetless = classify_source_sync(
            SourceLifecycle::Live,
            SourceKind::LiveChaintip,
            None,
            Some(backbone(953_699, None, NOW - 60)),
            NOW,
        );
        assert_eq!(targetless.state, "not_started");
        assert_eq!(targetless.progress_height, None);
        assert_eq!(targetless.progress_updated_at, None);
    }

    #[test]
    fn classify_historical_sources_null_progress_fields() {
        assert_eq!(
            classify_source_sync(
                SourceLifecycle::Historical,
                SourceKind::Auxpow,
                Some(FRESH_CURSOR),
                None,
                NOW,
            ),
            SourceSyncStatus::empty("historical", "historical")
        );
    }

    #[test]
    fn classify_catalogued_sources_null_progress_fields() {
        assert_eq!(
            classify_source_sync(
                SourceLifecycle::Catalogued,
                SourceKind::Auxpow,
                Some(FRESH_CURSOR),
                None,
                NOW,
            ),
            SourceSyncStatus::empty("catalogued", "catalogued")
        );
    }

    #[test]
    fn classify_partial_and_surveyed_sources_null_progress_fields() {
        for (lifecycle, mode) in [
            (SourceLifecycle::Partial, "partial"),
            (SourceLifecycle::Surveyed, "surveyed"),
        ] {
            assert_eq!(
                classify_source_sync(lifecycle, SourceKind::Auxpow, Some(FRESH_CURSOR), None, NOW,),
                SourceSyncStatus::empty(mode, mode)
            );
        }
    }

    #[test]
    fn unregistered_source_code_maps_to_unknown() {
        assert_eq!(
            sync_for_source_code("auxpow:missing", Some(FRESH_CURSOR), None, NOW),
            SourceSyncStatus::unknown()
        );
    }
}
