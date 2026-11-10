#!/bin/sh
set -eu

# Native restores can reset private ownership and modes. Repair only this
# volume before dropping privileges, without following links or crossing a
# nested mount into somebody else's storage.
if [ "$(id -u)" -eq 0 ]; then
    if [ -L /data ] || [ ! -d /data ]; then
        echo 'Private data must be a real directory' >&2
        exit 1
    fi
    if ! awk '$5 ~ /^\/data\// { exit 1 }' /proc/self/mountinfo; then
        echo 'Private data mount boundary is unavailable or contains nested mounts' >&2
        exit 1
    fi
    private_entries=$(find -P /data -xdev -printf '%y %D:%i %n\n')
    if ! printf '%s\n' "$private_entries" | awk '
        NF != 3 || ($1 != "d" && $1 != "f" && $1 != "l") { invalid = 1 }
        $1 == "f" || $1 == "l" { seen[$2]++; links[$2] = $3 }
        END {
            if (invalid) exit 1
            for (inode in links) if (seen[inode] != links[inode]) exit 1
        }
    '; then
        echo 'Private data contains unsupported entries or outside hard links' >&2
        exit 1
    fi
    # Own metadata while repairing it; this works without CAP_FOWNER even on
    # a normal restart where the files already belong to the worker.
    find -P /data -xdev -exec chown --no-dereference 0:0 -- {} +
    find -P /data -xdev -type d -exec chmod 0700 -- {} +
    find -P /data -xdev -type f -exec chmod 0600 -- {} +
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
