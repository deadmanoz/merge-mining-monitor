//! Shared protocol commitment SQL for the block page and tree nodes.

/// True once the commitment rebuild has recorded the running decoder's version (bound as `$2`);
/// select it from `read_model_invariant`. Until then the derived rows are not authoritative.
pub(crate) const PROTOCOL_COMMITMENTS_READY_SQL: &str =
    "protocol_commitments_decoder_version IS NOT DISTINCT FROM $2";

/// Joins a `protocol_commitment c` to its script `s` and to the pool `p` the protocol-account
/// registries associate with the declared account.
pub(crate) const PROTOCOL_COMMITMENT_JOIN_SQL: &str = "LEFT JOIN protocol_commitment_script s ON s.id = c.script_id \
     LEFT JOIN pool_identity i \
            ON (i.namespace, i.identifier) = (s.account_namespace, s.account_identifier) \
     LEFT JOIN pool p ON p.id = i.pool_id";
