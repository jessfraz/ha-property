# Property

An evidence-backed property tax ledger for Home Assistant, with a local web UI
and the `ha-property` JSON CLI. No LLM or paid data provider is required.

- Keep annual and supplemental bills, assessments, balances and payment history.
- Reconcile explicit installment allocations without treating a receipt as a paid
  bill. Unknown balances, missing notices and conflicting evidence stay visible.
- Publish deterministic HA sensors, a read-only deadline calendar and repair
  notices. Optional Assist tools are read-only and off by default.

## Install

Add this repository to the HA app store and install **Property**. Install the
`custom_components/property` integration through HACS or copy that directory to
HA's `custom_components`, restart HA, then add **Property** under integrations.
The integration pairs with the app through authenticated Supervisor discovery.

Open the app, look up the address in the county's published records, then choose
or look up an APN directly, then confirm the exact parcel. Candidates are never selected automatically.
Review the county/source year, especially when published records are historical.

LA sync reads the ordinary payment-history form and county-published assessment
rolls. Each source keeps its own freshness and coverage. Bill-viewer responses
without a verifiable bound notice remain unavailable, and the balance viewer
requires human reCAPTCHA verification. Neither result becomes a zero balance.
These web forms are not advertised as stable APIs. Sync never requests support,
submits payments, changes accounts, or changes existing calendars.

Structured JSON import remains available for missing notices and other evidence.
`tests/fixtures/import.example.json` is fictional. Previewing is read-only;
applying requires the exact preview, revision and idempotency key.

## CLI

Set `PROPERTY_API_URL` and `PROPERTY_API_TOKEN` at runtime, or use an owner-only
`--token-file`. HA-backed clients use `--transport home-assistant` with
`HOME_ASSISTANT_URL` and `HOME_ASSISTANT_TOKEN`.

```sh
ha-property summary
ha-property resolve --parcel 0000-000-001
ha-property resolve --address '42 Example Road, Example City, CA 90000'
ha-property show
ha-property history from 2020-06-01 --if-match '"1"'
ha-property history all --if-match '"2"'
ha-property tax bills list --limit 50
ha-property imports preview --input evidence.json > preview.json
ha-property imports apply --preview preview.json --if-match '"0"' \
  --idempotency-key import-001
ha-property sync request
ha-property sync get SYNC_ID
```

`--if-match` must be the actual quoted preview ETag, not a guessed revision.
History edits use the quoted ETag from `show`. The example parcel and address
above are fictional. `--apn` is an alias for `--parcel`.
Sync requests return an owned job immediately; read its exact ID to check progress.
Commands print `{status, etag, body}` JSON; failures use bounded JSON diagnostics.
List responses include `next_page`, passed unchanged as `--page-token`.
`--limit` is a maximum; large evidence may produce smaller pages.

`Bought on` is optional. Blank means all available history. Setting a date
filters ordinary lists, summaries, calendars and county collection from that
date in the county timezone, without deleting saved evidence or changing the
confirmed parcel. Clearing it reveals retained evidence again. LA uses
`America/Los_Angeles`, including daylight saving time. Dates in the future
are rejected.

Payment and other dated evidence use the source's effective date. Annual
assessment rolls supply a year, not a precise event date. A cutoff within a
year excludes that ambiguous year's roll and starts with the next roll year;
a January 1 cutoff includes that year. GIS publication time is never used to
make an old roll look new, and coverage explains this limitation.

The API accepts exactly one of `{"parcel":"0000000001"}` or
`{"address":"42 Example Road"}` at `POST /v1/property/resolve`, returning the
bounded candidate set with coverage and truncation. `PATCH /v1/property` accepts
`{"bought_on":"2020-06-01"}` or `{"bought_on":null}` with the current `If-Match`.
It changes only the history boundary and returns the updated property and ETag.
Imports and the resulting ledger projection are bounded to 4 MiB each. Oversized
imports are rejected before a preview is issued, without changing the ledger.

## Develop and recover

Run `nix develop -c just check`. `just openapi` generates the typed
[API contract](openapi/api.json) from Dropshot and schemars.

Standalone: set `PROPERTY_API_TOKEN`, then run `cargo run --bin property` inside
`nix develop`. The API defaults to loopback. No credentials belong in source.

Use native HA cold backups. For standalone recovery, stop the process cleanly,
copy its entire data directory, then restore while stopped. Preserve both
SQLite databases and runtime credentials together. WAL files are not backups.
