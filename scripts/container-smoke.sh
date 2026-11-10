#!/usr/bin/env bash
set -euo pipefail
exec python3 scripts/container-smoke.py "$@"
