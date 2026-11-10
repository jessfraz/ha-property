CREATE TABLE sync_runs (
    id TEXT PRIMARY KEY NOT NULL,
    status TEXT NOT NULL,
    started_at TEXT NOT NULL,
    completed_at TEXT,
    error_code TEXT,
    coverage_json TEXT NOT NULL,
    page_json TEXT
) STRICT;
