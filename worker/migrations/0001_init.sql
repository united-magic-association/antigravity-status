CREATE TABLE IF NOT EXISTS ingest_receipt (
    receipt_id      TEXT PRIMARY KEY,
    client_hour_key TEXT NOT NULL,
    created_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_ingest_receipt_client_hour
    ON ingest_receipt (client_hour_key);

CREATE INDEX IF NOT EXISTS idx_ingest_receipt_created_at
    ON ingest_receipt (created_at);

CREATE TABLE IF NOT EXISTS raw_submission (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    hour_bucket      TEXT NOT NULL,
    contributor_hash TEXT NOT NULL,
    report_json      TEXT NOT NULL,
    received_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_raw_submission_hour_bucket
    ON raw_submission (hour_bucket, received_at);

CREATE TABLE IF NOT EXISTS hourly_rollup (
    hour_bucket      TEXT PRIMARY KEY,
    report_json      TEXT NOT NULL,
    submission_count INTEGER NOT NULL,
    aggregated_at    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS hourly_quota (
    quota_key        TEXT PRIMARY KEY,
    submission_count INTEGER NOT NULL,
    max_limit        INTEGER NOT NULL,
    updated_at       TEXT NOT NULL,
    CONSTRAINT chk_hourly_quota CHECK (submission_count >= 0 AND submission_count <= max_limit)
);

CREATE INDEX IF NOT EXISTS idx_hourly_quota_updated_at
    ON hourly_quota (updated_at);
