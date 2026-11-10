CREATE TABLE ledger_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    revision INTEGER NOT NULL CHECK (revision >= 0),
    property_json TEXT,
    coverage_json TEXT NOT NULL
);
INSERT INTO ledger_state(singleton, revision, property_json, coverage_json)
VALUES (1, 0, NULL, '[]');

CREATE TABLE observations (
    id TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('notice', 'assessment', 'balance', 'payment')),
    body TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE import_receipts (
    idempotency_key TEXT PRIMARY KEY NOT NULL,
    preview_hash TEXT NOT NULL,
    result_json TEXT NOT NULL,
    applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) WITHOUT ROWID;
