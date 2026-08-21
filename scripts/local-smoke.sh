#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
mkdir -p "$repo_dir/.local-test"
run_dir="$(mktemp -d "$repo_dir/.local-test/smoke.XXXXXX")"
mkdir -p "$run_dir/home/.ssh" "$run_dir/tmp"
chmod 700 "$run_dir/home/.ssh"

export TMPDIR="$run_dir/tmp"
export CARGO_HOME="$repo_dir/.cargo-home"
export CARGO_TARGET_DIR="$repo_dir/target"

cd "$repo_dir"
cargo build --bins
target/debug/astrad init --state-dir "$run_dir/server"
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
HOME="$run_dir/home" timeout 2s target/debug/astrad serve \
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

terminal_id="$(
  "${client[@]}" new --name smoke -- \
    /bin/sh -c 'echo BEFORE_DETACH; sleep 2; echo WHILE_DETACHED; sleep 20'
)"

before="$(timeout 1s "${client[@]}" attach "$terminal_id" --read-only || true)"
if ! printf '%s' "$before" | rg -q 'BEFORE_DETACH'; then
  echo "initial terminal output was not received" >&2
  exit 1
fi

sleep 3
after="$(timeout 1s "${client[@]}" attach "$terminal_id" --read-only || true)"
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
echo "smoke test passed; artifacts: $run_dir"
