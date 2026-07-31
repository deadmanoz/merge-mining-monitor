use super::*;

#[test]
fn rejects_expected_parent_nbits_that_disagree_with_the_header() {
    assert_eq!(
        check_optional_compact_target(Some("1d00fffe"), 0x1d00ffff),
        Err(SkipReason::EvidenceMismatch)
    );
}
