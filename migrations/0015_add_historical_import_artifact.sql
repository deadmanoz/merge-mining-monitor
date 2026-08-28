-- Last successfully imported publication artifact identity. import-all skips
-- classify, write, and authoritative reconcile when the current SHA matches.
CREATE TABLE historical_import_artifact (
    role TEXT NOT NULL,
    chain TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    size_bytes BIGINT NOT NULL,
    row_count BIGINT NOT NULL,
    source_repo_commit TEXT NOT NULL,
    imported_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (role, chain),
    CONSTRAINT historical_import_artifact_role_check
        CHECK (role IN ('event', 'error_observation', 'aggregate')),
    CONSTRAINT historical_import_artifact_sha256_check
        CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    CONSTRAINT historical_import_artifact_size_bytes_check
        CHECK (size_bytes >= 0),
    CONSTRAINT historical_import_artifact_row_count_check
        CHECK (row_count >= 0),
    CONSTRAINT historical_import_artifact_source_repo_commit_check
        CHECK (source_repo_commit ~ '^[0-9a-f]{40}$')
);
