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

Open the app and import structured JSON evidence after confirming the county and
exact parcel against county records. `tests/fixtures/import.example.json` is a
fictional format example, not tax data. Previewing is read-only; applying requires
that exact preview, revision and idempotency key.

The Los Angeles adapter links official sources and reports automated collection
as unavailable. No supported public county API is established. County notices,
balance lookup and payment history are separate evidence. Sync does not submit
county request forms or payments. Existing calendars are not changed.

## CLI

Set `PROPERTY_API_URL` and `PROPERTY_API_TOKEN` at runtime, or use an owner-only
`--token-file`. HA-backed clients use `--transport home-assistant` with
`HOME_ASSISTANT_URL` and `HOME_ASSISTANT_TOKEN`.

```sh
ha-property summary
ha-property tax bills list --limit 50
ha-property imports preview --input evidence.json > preview.json
ha-property imports apply --preview preview.json --if-match '"0"' \
  --idempotency-key import-001
ha-property sync request
```

`--if-match` must be the actual quoted preview ETag, not a guessed revision.
Commands print `{status, etag, body}` JSON; failures use bounded JSON diagnostics.
List responses include `next_page`, passed unchanged as `--page-token`.
`--limit` is a maximum; large evidence may produce smaller pages.
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
