#!/bin/sh
set -eu

# Native restores recreate private files as root while retaining their modes.
# Repair only this volume before dropping privileges, without following links
# or crossing a nested mount into somebody else's storage.
if [ "$(id -u)" -eq 0 ]; then
    if [ -L /data ] || [ ! -d /data ]; then
        echo 'Private data must be a real directory' >&2
        exit 1
    fi
    if ! awk '$5 ~ /^\/data\// { exit 1 }' /proc/self/mountinfo; then
        echo 'Private data mount boundary is unavailable or contains nested mounts' >&2
        exit 1
    fi
    find -P /data -xdev -exec chown --no-dereference property:property -- {} +
    exec gosu property "$0" "$@"
fi
umask 077
if [ -f /data/options.json ]; then
    set -- --options /data/options.json "$@"
fi
if [ "${PROPERTY_ENABLE_INGRESS:-false}" = true ]; then
    export PROPERTY_INGRESS_BIND=0.0.0.0:8099
    export PROPERTY_INGRESS_PEER=172.30.32.2
fi
exec property "$@"
