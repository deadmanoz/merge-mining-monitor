//! Endpoint query-parsing tests for the retained release API.

use super::*;

fn err_code(err: ApiError) -> &'static str {
    err.code()
}

#[test]
fn undecodable_query_is_invalid_query() {
    assert_eq!(
        err_code(parse_tree_query(Some("source=%ff")).unwrap_err()),
        "invalid_query"
    );
}

#[test]
fn dates_require_two_digit_month_and_day() {
    assert_eq!(
        err_code(
            parse_tree_query(Some(
                "include_unheighted=true&unheighted_from=2026-5-001&unheighted_to=2026-05-31"
            ))
            .unwrap_err()
        ),
        "invalid_query"
    );
}

#[test]
fn unknown_parameter_is_invalid_query() {
    assert_eq!(
        err_code(parse_tree_query(Some("from_height=1&to_height=2&typo=1")).unwrap_err()),
        "invalid_query"
    );
}

#[test]
fn tree_bounds_are_checked_before_db() {
    let err = parse_tree_query(Some("from_height=1&to_height=3000")).unwrap_err();
    assert_eq!(err.code(), "range_too_large");
}

#[test]
fn tree_height_bounds_are_optional_as_a_pair() {
    let query = parse_tree_query(None).unwrap();
    assert_eq!(query.from_height, None);
    assert_eq!(query.to_height, None);
    assert_eq!(query.at_height, None);
    assert_eq!(query.at_time, None);
    assert_eq!(query.query["from_height"], json!(null));
    assert_eq!(query.query["to_height"], json!(null));
    assert_eq!(query.query["at_height"], json!(null));
    assert_eq!(query.query["at_time"], json!(null));
    assert_eq!(query.query["window_mode"], json!("tip"));
    assert_eq!(query.context, TreeContextPolicy::Exact);
    assert_eq!(query.query["context"], json!("exact"));

    let query = parse_tree_query(Some("from_height=1&to_height=2")).unwrap();
    assert_eq!(query.from_height, Some(1));
    assert_eq!(query.to_height, Some(2));
    assert_eq!(query.query["window_mode"], json!("explicit"));
    assert_eq!(query.query["context"], json!("exact"));
}

#[test]
fn tree_parses_exact_height_lookup() {
    let query = parse_tree_query(Some("at_height=700000")).unwrap();
    assert_eq!(query.at_height, Some(700000));
    assert_eq!(query.from_height, None);
    assert_eq!(query.to_height, None);
    assert_eq!(query.query["at_height"], json!(700000));
    assert_eq!(query.query["at_time"], json!(null));
    assert_eq!(query.query["window_mode"], json!("height"));
    assert_eq!(query.query["context"], json!("exact"));
}

#[test]
fn tree_parses_exact_time_lookup() {
    let query = parse_tree_query(Some("at_time=2026-05-10T12%3A30%3A00Z")).unwrap();
    assert_eq!(query.at_time, Some(1_778_416_200));
    assert_eq!(query.query["at_height"], json!(null));
    assert_eq!(query.query["at_time"], json!("2026-05-10T12:30:00Z"));
    assert_eq!(query.query["window_mode"], json!("time"));
    assert_eq!(query.query["context"], json!("exact"));
}

#[test]
fn tree_parses_compact_context_for_exact_lookup() {
    let query = parse_tree_query(Some("at_height=700000&context=compact")).unwrap();
    assert_eq!(query.context, TreeContextPolicy::Compact);
    assert_eq!(query.query["context"], json!("compact"));
    assert_eq!(query.query["window_mode"], json!("height"));

    let query = parse_tree_query(Some("at_time=2026-05-10T12%3A30%3A00Z&context=compact")).unwrap();
    assert_eq!(query.context, TreeContextPolicy::Compact);
    assert_eq!(query.query["context"], json!("compact"));
    assert_eq!(query.query["window_mode"], json!("time"));
}

#[test]
fn tree_rejects_invalid_compact_context_combinations() {
    let anchor = "ab".repeat(32);
    for raw in [
        "context=compact".to_owned(),
        "from_height=1&to_height=2&context=compact".to_owned(),
        format!("unheighted_anchor={anchor}&context=compact"),
        "at_height=1&context=wide".to_owned(),
        "at_height=1&context=compact&include_unheighted=true&unheighted_from=2026-05-01&unheighted_to=2026-05-02".to_owned(),
        "at_height=1&context=compact&unheighted_from=2026-05-01".to_owned(),
        "at_time=2026-05-10T12%3A30%3A00Z&context=compact&unheighted_to=2026-05-01".to_owned(),
    ] {
        assert_eq!(
            err_code(parse_tree_query(Some(&raw)).unwrap_err()),
            "invalid_query"
        );
    }
}

#[test]
fn tree_rejects_invalid_exact_lookup_values() {
    assert_eq!(
        err_code(parse_tree_query(Some("at_height=-1")).unwrap_err()),
        "invalid_query"
    );
    assert_eq!(
        err_code(parse_tree_query(Some("at_time=2026-05-10T12%3A30Z")).unwrap_err()),
        "invalid_query"
    );
    assert_eq!(
        err_code(parse_tree_query(Some("at_time=2026-05-10T12%3A30%3A00%2B00%3A00")).unwrap_err()),
        "invalid_query"
    );
}

#[test]
fn tree_rejects_mutually_exclusive_lookup_modes() {
    let anchor = "ab".repeat(32);
    assert_eq!(
        err_code(
            parse_tree_query(Some("at_height=1&at_time=2026-05-10T12%3A30%3A00Z")).unwrap_err()
        ),
        "invalid_query"
    );
    assert_eq!(
        err_code(parse_tree_query(Some("from_height=1&to_height=1&at_height=1")).unwrap_err()),
        "invalid_query"
    );
    assert_eq!(
        err_code(
            parse_tree_query(Some(&format!("unheighted_anchor={anchor}&at_height=1"))).unwrap_err()
        ),
        "invalid_query"
    );
    assert_eq!(
        err_code(
            parse_tree_query(Some(&format!(
                "unheighted_anchor={anchor}&at_time=2026-05-10T12%3A30%3A00Z"
            )))
            .unwrap_err()
        ),
        "invalid_query"
    );
}

#[test]
fn tree_rejects_one_sided_height_bounds() {
    assert_eq!(
        err_code(parse_tree_query(Some("from_height=1")).unwrap_err()),
        "invalid_query"
    );
    assert_eq!(
        err_code(parse_tree_query(Some("to_height=1")).unwrap_err()),
        "invalid_query"
    );
}

#[test]
fn tree_rejects_negative_heights() {
    let err = parse_tree_query(Some("from_height=-1&to_height=2")).unwrap_err();
    assert_eq!(err.code(), "invalid_query");
}

#[test]
fn tree_classification_defaults_and_parses() {
    let query = parse_tree_query(None).unwrap();
    assert_eq!(
        query.classification,
        vec![
            Classification::StrictBtcOrphan,
            Classification::WeakBtcOrphan
        ]
    );
    assert_eq!(
        query.query["classification"],
        json!(["strict_btc_orphan", "weak_btc_orphan"])
    );

    let query =
        parse_tree_query(Some("classification=strict_btc_orphan,btc_stale_excluded")).unwrap();
    assert_eq!(
        query.classification,
        vec![
            Classification::StrictBtcOrphan,
            Classification::BtcStaleExcluded
        ]
    );
    assert_eq!(
        err_code(parse_tree_query(Some("classification=unknown")).unwrap_err()),
        "invalid_query"
    );
}

#[test]
fn tree_validates_unheighted_bounds_when_disabled() {
    assert_eq!(
        err_code(
            parse_tree_query(Some(
                "from_height=1&to_height=2&include_unheighted=false&unheighted_from=nope"
            ))
            .unwrap_err()
        ),
        "invalid_query"
    );
    let query = parse_tree_query(Some(
        "from_height=1&to_height=2&include_unheighted=false&unheighted_from=2026-05-01",
    ))
    .unwrap();
    assert!(query.unheighted_from.is_none());
    assert!(query.query.get("unheighted_from").is_none());
}

#[test]
fn tree_requires_unheighted_bounds_when_enabled() {
    assert_eq!(
        err_code(
            parse_tree_query(Some("from_height=1&to_height=2&include_unheighted=true"))
                .unwrap_err()
        ),
        "invalid_query"
    );
}

#[test]
fn navigator_defaults_to_latest_with_target_specific_classification() {
    let stale = parse_navigator_query(NavigatorTarget::Stale, None).unwrap();
    assert_eq!(stale.target, NavigatorTarget::Stale);
    assert!(matches!(stale.mode, NavigatorMode::Latest));
    assert_eq!(stale.limit, 1);
    assert!(stale.classification.is_empty());
    assert_eq!(stale.query["target"], json!("stale"));
    assert_eq!(stale.query["mode"], json!("latest"));
    assert_eq!(stale.query["limit"], json!(1));

    let orphan = parse_navigator_query(NavigatorTarget::Orphan, None).unwrap();
    assert_eq!(
        orphan.classification,
        vec![
            Classification::StrictBtcOrphan,
            Classification::WeakBtcOrphan
        ]
    );
    assert_eq!(
        orphan.query["classification"],
        json!(["strict_btc_orphan", "weak_btc_orphan"])
    );

    let filtered = parse_navigator_query(
        NavigatorTarget::OrphanBranch,
        Some("classification=pending,strict_btc_orphan"),
    )
    .unwrap();
    assert_eq!(
        filtered.classification,
        vec![Classification::StrictBtcOrphan, Classification::Pending]
    );
    assert_eq!(
        filtered.query["classification"],
        json!(["strict_btc_orphan", "pending"])
    );
}

#[test]
fn navigator_accepts_anchor_and_cursor_modes() {
    let hash = "AB".repeat(32);
    let anchor = parse_navigator_query(
        NavigatorTarget::Stale,
        Some(&format!("anchor_hash={hash}&limit=100")),
    )
    .unwrap();
    assert_eq!(anchor.limit, 100);
    assert!(matches!(
        anchor.mode,
        NavigatorMode::Anchor { ref hash } if hash == &"ab".repeat(32)
    ));
    assert_eq!(anchor.query["mode"], json!("anchor"));

    let cursor = NavigatorCursor::new(
        NavigatorTarget::Stale,
        NavigatorAxis::Height,
        900_000,
        900_000,
        "ab".repeat(32),
    )
    .encode();
    let page = parse_navigator_query(
        NavigatorTarget::Stale,
        Some(&format!("cursor={cursor}&direction=older&limit=10")),
    )
    .unwrap();
    assert_eq!(page.limit, 10);
    assert!(matches!(
        page.mode,
        NavigatorMode::Page {
            direction: NavigatorDirection::Older,
            ..
        }
    ));
    assert_eq!(page.query["mode"], json!("page"));
}

#[test]
fn navigator_rejects_invalid_query_combinations() {
    let hash = "ab".repeat(32);
    let cursor = NavigatorCursor::new(
        NavigatorTarget::Stale,
        NavigatorAxis::Height,
        900_000,
        900_000,
        hash.clone(),
    )
    .encode();

    for raw in [
        "limit=0".to_owned(),
        "classification=strict_btc_orphan".to_owned(),
        "cursor=not-hex&direction=older".to_owned(),
        format!("cursor={cursor}"),
        "direction=older".to_owned(),
        format!("cursor={cursor}&direction=older&anchor_hash={hash}"),
        "typo=1".to_owned(),
    ] {
        assert_eq!(
            err_code(parse_navigator_query(NavigatorTarget::Stale, Some(&raw)).unwrap_err()),
            "invalid_query",
            "{raw}"
        );
    }
    assert_eq!(
        parse_navigator_query(NavigatorTarget::Stale, Some("limit=5000"))
            .unwrap_err()
            .code(),
        "range_too_large"
    );
    assert_eq!(
        err_code(
            parse_navigator_query(NavigatorTarget::Orphan, Some("classification=bogus"))
                .unwrap_err()
        ),
        "invalid_query"
    );
}

#[test]
fn navigator_cursors_are_bound_to_target_and_axis() {
    let stale_cursor = NavigatorCursor::new(
        NavigatorTarget::Stale,
        NavigatorAxis::Height,
        900_000,
        900_000,
        "ab".repeat(32),
    )
    .encode();
    assert_eq!(
        err_code(
            parse_navigator_query(
                NavigatorTarget::Orphan,
                Some(&format!("cursor={stale_cursor}&direction=older"))
            )
            .unwrap_err()
        ),
        "invalid_query"
    );

    let wrong_axis = NavigatorCursor::new(
        NavigatorTarget::Stale,
        NavigatorAxis::Time,
        1_700_000_000,
        1_700_000_000,
        "ab".repeat(32),
    )
    .encode();
    assert_eq!(
        err_code(
            parse_navigator_query(
                NavigatorTarget::Stale,
                Some(&format!("cursor={wrong_axis}&direction=older"))
            )
            .unwrap_err()
        ),
        "invalid_query"
    );

    let impossible_bounds = NavigatorCursor::new(
        NavigatorTarget::Stale,
        NavigatorAxis::Height,
        900_001,
        900_000,
        "ab".repeat(32),
    )
    .encode();
    assert_eq!(
        err_code(
            parse_navigator_query(
                NavigatorTarget::Stale,
                Some(&format!("cursor={impossible_bounds}&direction=older"))
            )
            .unwrap_err()
        ),
        "invalid_query"
    );

    let malformed_hash = NavigatorCursor::new(
        NavigatorTarget::Stale,
        NavigatorAxis::Height,
        900_000,
        900_000,
        "not-a-hash",
    )
    .encode();
    assert_eq!(
        err_code(
            parse_navigator_query(
                NavigatorTarget::Stale,
                Some(&format!("cursor={malformed_hash}&direction=older"))
            )
            .unwrap_err()
        ),
        "invalid_query"
    );
}
