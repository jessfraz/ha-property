set shell := ["bash", "-euo", "pipefail", "-c"]

default: check

rust:
    cargo fmt --check
    cargo clippy --locked --all --benches --tests --examples --all-features -- -D warnings
    cargo test --locked --all-features

ha:
    uv run --frozen python scripts/build-integration
    just ha-installed

ha-installed:
    cargo build --locked --bins
    uv sync --frozen
    uv run --frozen ruff check custom_components tests_ha tests_web scripts
    uv run --frozen ruff format --check custom_components tests_ha tests_web scripts
    uv run --frozen ty check --error-on-warning custom_components tests_ha tests_web
    PROPERTY_TEST_BINARY="{{justfile_directory()}}/target/debug/property" PROPERTY_TEST_CLI_BINARY="{{justfile_directory()}}/target/debug/ha-property" uv run --frozen pytest tests_ha

frontend:
    cargo build --locked --bins
    uv sync --frozen
    uv run --frozen playwright install chromium
    PROPERTY_TEST_BINARY="{{justfile_directory()}}/target/debug/property" uv run --frozen pytest tests_web

openapi:
    cargo run --locked --bin property -- --openapi openapi/api.json

repository:
    uv run --frozen python scripts/check-public-tree
    uv run --frozen python scripts/check-release-version
    actionlint
    shellcheck docker-entrypoint.sh

security:
    cargo deny --all-features check advisories licenses sources

check: rust ha frontend repository security
