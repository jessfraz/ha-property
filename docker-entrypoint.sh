#!/bin/sh
set -eu
if [ "$(id -u)" -eq 0 ]; then
    chown property:property /data
    if [ -f /data/options.json ]; then
        chown property:property /data/options.json
    fi
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
