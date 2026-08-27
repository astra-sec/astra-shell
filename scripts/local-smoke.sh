#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
mkdir -p "$repo_dir/.local-test"
run_dir="$(mktemp -d "$repo_dir/.local-test/smoke.XXXXXX")"
mkdir -p "$run_dir/home/.ssh" "$run_dir/tmp"
chmod 700 "$run_dir/home/.ssh"

run_with_timeout() {
  local seconds="$1"
  shift
  local marker="$run_dir/timeout.$BASHPID.$RANDOM"
  "$@" &
  local command_pid=$!
  (
    sleep "$seconds"
    : >"$marker"
    kill -TERM "$command_pid" 2>/dev/null || true
  ) &
  local timer_pid=$!
  local status=0
  wait "$command_pid" || status=$?
  kill "$timer_pid" 2>/dev/null || true
  wait "$timer_pid" 2>/dev/null || true
  if [[ -e "$marker" ]]; then
    rm -f "$marker"
    return 124
  fi
  return "$status"
}

export TMPDIR="$run_dir/tmp"
export CARGO_HOME="$repo_dir/.cargo-home"
export CARGO_TARGET_DIR="$repo_dir/target"

cd "$repo_dir"
cargo build --bins
target/debug/astrad init --state-dir "$run_dir/server"
if find "$run_dir/server" -type f -name '*.db' -print -quit | rg -q .; then
  echo "astrad init unexpectedly created a database" >&2
  exit 1
fi
ssh-keygen -q -t ed25519 -N '' -C astra-smoke -f "$run_dir/home/.ssh/id_ed25519"
install -m 600 "$run_dir/home/.ssh/id_ed25519.pub" "$run_dir/server/authorized_keys"

server_log="$run_dir/server.log"
HOME="$run_dir/home" target/debug/astrad serve \
  --listen 127.0.0.1:0 \
  --state-dir "$run_dir/server" \
  --session-root "$repo_dir" >"$server_log" 2>&1 &
daemon_pid=$!

cleanup() {
  kill "$daemon_pid" 2>/dev/null || true
  wait "$daemon_pid" 2>/dev/null || true
}
trap cleanup EXIT

listen=""
for _ in $(seq 1 50); do
  listen="$(awk '/^LISTEN / { print $2; exit }' "$server_log")"
  if [[ -n "$listen" ]]; then
    break
  fi
  sleep 0.1
done
if [[ -z "$listen" ]]; then
  echo "astrad did not start; see $server_log" >&2
  exit 1
fi

duplicate_status=0
HOME="$run_dir/home" run_with_timeout 2 target/debug/astrad serve \
  --listen 127.0.0.1:0 \
  --state-dir "$run_dir/server" \
  --session-root "$repo_dir" >"$run_dir/duplicate-server.log" 2>&1 || duplicate_status=$?
if [[ "$duplicate_status" -eq 0 || "$duplicate_status" -eq 124 ]]; then
  echo "a second daemon unexpectedly acquired the same state directory" >&2
  exit 1
fi

tofu_bootstrap=(
  env HOME="$run_dir/home"
  XDG_CONFIG_HOME="$run_dir/home/.config"
  target/debug/astra
  -p "${listen##*:}"
  -o StrictHostKeyChecking=accept-new
  "$(id -un)@127.0.0.1"
)
"${tofu_bootstrap[@]}" list >/dev/null
if [[ ! -s "$run_dir/home/.config/astra/known_hosts" ]]; then
  echo "TOFU connection did not create Astra known hosts" >&2
  exit 1
fi

client=(
  env HOME="$run_dir/home"
  XDG_CONFIG_HOME="$run_dir/home/.config"
  target/debug/astra
  -p "${listen##*:}"
  -o StrictHostKeyChecking=yes
  "$(id -un)@127.0.0.1"
)

# Exercise Astra Files/1 through the real QUIC client and daemon, including a multi-chunk
# upload, integrity-checked download, directory listing, rename, and cleanup.
file_test_dir="$run_dir/file-transfer"
remote_file="$file_test_dir/uploaded.bin"
renamed_file="$file_test_dir/renamed.bin"
downloaded_file="$run_dir/downloaded.bin"
dd if=/dev/urandom of="$run_dir/source.bin" bs=65536 count=33 status=none
"${client[@]}" files capabilities | rg -q '^Astra Files/1$'
"${client[@]}" files mkdir "$file_test_dir" >/dev/null
"${client[@]}" files put "$run_dir/source.bin" "$remote_file" >/dev/null
cmp "$run_dir/source.bin" "$remote_file"
"${client[@]}" files ls "$file_test_dir" | rg -q 'uploaded.bin'
"${client[@]}" files get "$remote_file" "$downloaded_file" >/dev/null
cmp "$run_dir/source.bin" "$downloaded_file"
"${client[@]}" files mv "$remote_file" "$renamed_file" >/dev/null
"${client[@]}" files rm "$renamed_file" >/dev/null
"${client[@]}" files rm "$file_test_dir" >/dev/null

# Kill and restart astrad after BeginUpload has created its private temporary file. The CLI must
# reconnect, repeat BeginUpload with the same transfer ID, recover the on-disk committed offset,
# and finish without retransmitting the whole file or exposing a partial destination.
reconnect_dir="$run_dir/reconnect-transfer"
reconnect_source="$run_dir/reconnect-source.bin"
reconnect_remote="$reconnect_dir/resumed.bin"
dd if=/dev/urandom of="$reconnect_source" bs=1048576 count=64 status=none
"${client[@]}" files mkdir "$reconnect_dir" >/dev/null
"${client[@]}" files put "$reconnect_source" "$reconnect_remote" \
  >"$run_dir/reconnect-put.log" 2>&1 &
transfer_pid=$!
partial_upload=""
partial_size=0
for _ in $(seq 1 500); do
  partial_upload="$(find "$reconnect_dir" -name '.astra-upload-*.part' -print -quit)"
  if [[ -n "$partial_upload" ]]; then
    partial_size="$(wc -c <"$partial_upload" | tr -d ' ')"
    if [[ "$partial_size" -ge 8388608 ]]; then
      break
    fi
  fi
  sleep 0.01
done
if [[ -z "$partial_upload" || "$partial_size" -lt 8388608 || -e "$reconnect_remote" ]]; then
  kill "$transfer_pid" 2>/dev/null || true
  wait "$transfer_pid" 2>/dev/null || true
  echo "could not interrupt an active Astra Files upload" >&2
  exit 1
fi
kill -9 "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true

server_log="$run_dir/restarted-server.log"
HOME="$run_dir/home" target/debug/astrad serve \
  --listen "$listen" \
  --state-dir "$run_dir/server" \
  --session-root "$repo_dir" >"$server_log" 2>&1 &
daemon_pid=$!
for _ in $(seq 1 50); do
  if rg -q '^LISTEN ' "$server_log"; then
    break
  fi
  sleep 0.1
done
if ! rg -q '^LISTEN ' "$server_log"; then
  echo "astrad did not restart during file resume test; see $server_log" >&2
  exit 1
fi
if ! wait "$transfer_pid"; then
  cat "$run_dir/reconnect-put.log" >&2
  echo "file upload did not recover after astrad restart" >&2
  exit 1
fi
cmp "$reconnect_source" "$reconnect_remote"
if find "$reconnect_dir" -name '.astra-upload-*.part' -print -quit | rg -q .; then
  echo "completed resumed upload left a temporary file behind" >&2
  exit 1
fi
"${client[@]}" files rm "$reconnect_remote" >/dev/null
"${client[@]}" files rm "$reconnect_dir" >/dev/null

# Keep strict, explicitly provisioned certificate pinning working for automation.
HOME="$run_dir/home" XDG_CONFIG_HOME="$run_dir/home/.config" target/debug/astra \
  -p "${listen##*:}" \
  --server-cert "$run_dir/server/host-cert.der" \
  "$(id -un)@127.0.0.1" list >/dev/null

default_shell="$({ printf 'echo SSH_STYLE_DEFAULT_SHELL\nexit\n'; sleep 1; } | "${client[@]}")"
if ! printf '%s' "$default_shell" | rg -q 'SSH_STYLE_DEFAULT_SHELL'; then
  echo "SSH-style destination did not open the default shell" >&2
  exit 1
fi

"${client[@]}" list >/dev/null
same_connection="$("${client[@]}" new --name same-connection --attach -- /bin/echo SAME_CONNECTION)"
if ! printf '%s' "$same_connection" | rg -q 'SAME_CONNECTION'; then
  echo "spawn and attach did not work over one QUIC connection" >&2
  exit 1
fi
if "${client[@]}" list | rg -q 'same-connection'; then
  echo "list returned an exited terminal" >&2
  exit 1
fi

terminal_id="$(
  "${client[@]}" new --name smoke -- \
    /bin/sh -c 'echo BEFORE_DETACH; sleep 2; echo WHILE_DETACHED; sleep 20'
)"
if ! [[ "$terminal_id" =~ ^[1-9][0-9]*$ ]]; then
  echo "new did not return a short numeric terminal ID" >&2
  exit 1
fi

terminal_uuid="$("${client[@]}" list --long | awk -v id="$terminal_id" '$1 == id { print $2; exit }')"
if ! [[ "$terminal_uuid" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]]; then
  echo "list --long did not expose the canonical terminal UUID" >&2
  exit 1
fi

before="$(run_with_timeout 1 "${client[@]}" attach "$terminal_id" --read-only || true)"
if ! printf '%s' "$before" | rg -q 'BEFORE_DETACH'; then
  echo "initial terminal output was not received" >&2
  exit 1
fi

sleep 3
after="$(run_with_timeout 1 "${client[@]}" attach "$terminal_uuid" --read-only || true)"
if ! printf '%s' "$after" | rg -q 'WHILE_DETACHED'; then
  echo "detached terminal output was not recovered" >&2
  exit 1
fi

ssh-keygen -q -t ed25519 -N '' -C astra-unauthorized -f "$run_dir/id_unauthorized"
if HOME="$run_dir/home" XDG_CONFIG_HOME="$run_dir/home/.config" target/debug/astra \
  -p "${listen##*:}" \
  -o StrictHostKeyChecking=yes \
  -i "$run_dir/id_unauthorized" \
  "$(id -un)@127.0.0.1" list >/dev/null 2>&1; then
  echo "unauthorized key was unexpectedly accepted" >&2
  exit 1
fi

"${client[@]}" close "$terminal_id" >/dev/null

# A quota rejection must be an application error and must not kill the resource
# that already owns the admitted capacity.
kill "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true
quota_log="$run_dir/quota-server.log"
HOME="$run_dir/home" target/debug/astrad serve \
  --listen "$listen" \
  --state-dir "$run_dir/server" \
  --session-root "$repo_dir" \
  --max-user-terminals 1 >"$quota_log" 2>&1 &
daemon_pid=$!
quota_listen=""
for _ in $(seq 1 50); do
  quota_listen="$(awk '/^LISTEN / { print $2; exit }' "$quota_log")"
  if [[ "$quota_listen" == "$listen" ]]; then
    break
  fi
  sleep 0.1
done
if [[ "$quota_listen" != "$listen" ]]; then
  echo "quota test daemon did not restart on $listen; see $quota_log" >&2
  exit 1
fi

admitted_id="$("${client[@]}" new --name quota-admitted -- /bin/sleep 30)"
quota_status=0
quota_error="$("${client[@]}" new --name quota-rejected -- /bin/sleep 30 2>&1)" || quota_status=$?
if [[ "$quota_status" -eq 0 ]] || ! printf '%s' "$quota_error" | rg -q 'quota'; then
  echo "second terminal was not rejected with a quota error: $quota_error" >&2
  exit 1
fi
if ! "${client[@]}" list | awk -v id="$admitted_id" '$1 == id { found = 1 } END { exit !found }'; then
  echo "quota rejection removed the already admitted terminal" >&2
  exit 1
fi
"${client[@]}" close "$admitted_id" >/dev/null
echo "smoke test passed; artifacts: $run_dir"
