-- Import planning now compares publication-owned state with the database.
-- Content SHA values remain artifact-integrity checks, not import receipts.
DROP TABLE historical_import_artifact;
