-- Historical error-observation witnesses use the existing provenance table.
-- This is the only new value: normal historical publication categories remain
-- unchanged, while an error witness retains its actual classification.
ALTER TABLE historical_event_provenance
    DROP CONSTRAINT historical_event_provenance_classification_check;

ALTER TABLE historical_event_provenance
    ADD CONSTRAINT historical_event_provenance_classification_check
    CHECK (classification IN (
        'canonical',
        'stale',
        'stale_descendant',
        'unknown',
        'error_block'
    ));
