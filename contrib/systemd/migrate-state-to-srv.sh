#!/bin/sh

set -eu

service_name=astrad.service
destination_dir=/srv/astra
source_dir=${1:-/home/mimi/astra-shell/state}
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
unit_template=$script_dir/astrad.service

fail() {
    printf '%s\n' "error: $*" >&2
    exit 1
}

active_workers() {
    ps -eo pid=,args= | awk '$2 ~ /(^|\/)astrad$/ && $3 == "worker" { print }'
}

source_workers() {
    ps -eo pid=,args= | awk -v prefix="$source_dir/users/" '
        $2 ~ /(^|\/)astrad$/ && $3 == "worker" {
            for (field = 4; field <= NF; field++) {
                if ($field == "--state-dir" && index($(field + 1), prefix) == 1) {
                    print
                }
            }
        }
    '
}

gateway_uses_destination() {
    candidate_pid=$(systemctl show --property=MainPID --value "$service_name") || return 1
    case "$candidate_pid" in
        ''|0|*[!0-9]*) return 1 ;;
    esac
    [ -r "/proc/$candidate_pid/cmdline" ] || return 1
    tr '\000' ' ' < "/proc/$candidate_pid/cmdline" \
        | grep -Fq -- '--state-dir /srv/astra'
}

[ "$(id -u)" -eq 0 ] || fail "run this migration as root"
[ "$source_dir" != "$destination_dir" ] || fail "source and destination are identical"
[ -d "$source_dir" ] || fail "source state directory does not exist: $source_dir"
[ ! -L "$source_dir" ] || fail "source state directory must not be a symbolic link"
[ -f "$unit_template" ] || fail "unit template is missing: $unit_template"

# Normalize trailing slashes before deriving the sibling backup path.
source_dir=$(CDPATH= cd -- "$source_dir" && pwd -P)
[ "$source_dir" != "$destination_dir" ] || fail "source and destination are identical"

case "$destination_dir" in
    /srv/astra) ;;
    *) fail "refusing unexpected destination: $destination_dir" ;;
esac

for identity_file in host-cert.der host-key.der instance-id; do
    [ -f "$source_dir/$identity_file" ] \
        || fail "source state is incomplete: missing $identity_file"
done

if [ -e "$destination_dir" ]; then
    [ -d "$destination_dir" ] || fail "destination exists and is not a directory"
    if [ -n "$(find "$destination_dir" -mindepth 1 -maxdepth 1 -print -quit)" ]; then
        # A previous run can reach the new active gateway before Type=simple has
        # exec'd astrad, making the final /proc assertion race. Resume only when
        # the new gateway is now verifiably active and no worker uses old state.
        systemctl is-active --quiet "$service_name" \
            || fail "destination is not empty and the service is not active: $destination_dir"
        gateway_uses_destination \
            || fail "destination is not empty and the gateway does not use it: $destination_dir"
        workers=$(source_workers)
        [ -z "$workers" ] || {
            printf '%s\n%s\n' \
                "error: workers still use the old state directory:" \
                "$workers" >&2
            exit 1
        }
        for identity_file in host-cert.der host-key.der instance-id; do
            cmp -s "$source_dir/$identity_file" "$destination_dir/$identity_file" \
                || fail "identity verification failed for $identity_file"
        done
        backup_dir=$source_dir.migrated-$(date -u +%Y%m%dT%H%M%SZ)
        mv "$source_dir" "$backup_dir"
        printf '%s\n' \
            "migration finalization complete" \
            "active state: $destination_dir" \
            "recoverable old-state backup: $backup_dir"
        exit 0
    fi
fi

workers=$(active_workers)
[ -z "$workers" ] || {
    printf '%s\n%s\n' \
        "error: active astrad workers must drain or be explicitly terminated before migration:" \
        "$workers" >&2
    exit 1
}

systemctl stop "$service_name"

workers=$(active_workers)
if [ -n "$workers" ]; then
    systemctl start "$service_name" || true
    printf '%s\n%s\n' \
        "error: a worker appeared while stopping the gateway; the old service was restarted:" \
        "$workers" >&2
    exit 1
fi

install -d -o root -g root -m 0700 "$destination_dir"

# Runtime sockets, locks and PID files are meaningful only to the old worker
# processes. Persistent catalogs and the host identity retain their ownership.
tar -C "$source_dir" \
    --exclude='./users/*/session.sock' \
    --exclude='./users/*/worker.lock' \
    --exclude='./users/*/worker.pid' \
    -cpf - . | tar -C "$destination_dir" -xpf -

for identity_file in host-cert.der host-key.der instance-id; do
    cmp -s "$source_dir/$identity_file" "$destination_dir/$identity_file" \
        || fail "identity verification failed for $identity_file; service remains stopped"
done

chown root:root "$destination_dir"
chmod 0700 "$destination_dir"
chmod 0600 "$destination_dir/host-key.der"

install -o root -g root -m 0644 \
    "$unit_template" /etc/systemd/system/astrad.service
systemctl daemon-reload

if ! systemctl start "$service_name"; then
    fail "new service failed to start; old state remains at $source_dir"
fi
systemctl is-active --quiet "$service_name" \
    || fail "new service is not active; old state remains at $source_dir"

attempt=0
while [ "$attempt" -lt 10 ]; do
    gateway_uses_destination && break
    attempt=$((attempt + 1))
    sleep 1
done
if ! gateway_uses_destination; then
    fail "new gateway command line does not use /srv/astra; old state remains at $source_dir"
fi

for identity_file in host-cert.der host-key.der instance-id; do
    cmp -s "$source_dir/$identity_file" "$destination_dir/$identity_file" \
        || fail "running service changed $identity_file; old state remains at $source_dir"
done

backup_dir=$source_dir.migrated-$(date -u +%Y%m%dT%H%M%SZ)
mv "$source_dir" "$backup_dir"

printf '%s\n' \
    "migration complete" \
    "active state: $destination_dir" \
    "recoverable old-state backup: $backup_dir"
