#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
mkdir -p "$repo_dir/.local-test"
run_dir="$(mktemp -d "$repo_dir/.local-test/managed.XXXXXX")"
mkdir -p "$run_dir/home" "$run_dir/tmp" "$run_dir/authorized"
chmod 700 "$run_dir/authorized"

export TMPDIR="$run_dir/tmp"
export CARGO_HOME="$repo_dir/.cargo-home"
export CARGO_TARGET_DIR="$repo_dir/target"

cd "$repo_dir"
cargo build --bins
target/debug/astrad init --state-dir "$run_dir/server"
username="$(id -un)"
uid="$(id -u)"
ssh-keygen -q -t ed25519 -N '' -C astra-managed-smoke -f "$run_dir/id_ed25519"
install -m 600 "$run_dir/id_ed25519.pub" "$run_dir/authorized/$username"

gateway_pid=""
worker_pid=""
cleanup() {
  if [[ -n "$gateway_pid" ]]; then
    kill "$gateway_pid" 2>/dev/null || true
    wait "$gateway_pid" 2>/dev/null || true
  fi
  if [[ -f "$run_dir/server/users/$uid/worker.pid" ]]; then
    worker_pid="$(tr -d '\n' <"$run_dir/server/users/$uid/worker.pid")"
    if [[ -r "/proc/$worker_pid/cmdline" ]] && \
       tr '\0' ' ' <"/proc/$worker_pid/cmdline" | rg -q 'astrad worker'; then
      kill "$worker_pid" 2>/dev/null || true
    fi
  fi
}
trap cleanup EXIT

start_gateway() {
  local log_file="$1"
  HOME="$run_dir/home" target/debug/astrad serve \
    --managed \
    --listen 127.0.0.1:0 \
    --state-dir "$run_dir/server" \
    --authorized-keys-dir "$run_dir/authorized" \
    --session-root "$repo_dir" >"$log_file" 2>&1 &
  gateway_pid=$!

  listen=""
  for _ in $(seq 1 50); do
    listen="$(awk '/^LISTEN / { print $2; exit }' "$log_file")"
    if [[ -n "$listen" ]]; then
      break
    fi
    sleep 0.1
  done
  if [[ -z "$listen" ]]; then
    echo "managed gateway did not start; see $log_file" >&2
    exit 1
  fi
  env HOME="$run_dir/home" XDG_CONFIG_HOME="$run_dir/home/.config" \
    target/debug/astra \
    -p "${listen##*:}" \
    -o StrictHostKeyChecking=accept-new \
    -i "$run_dir/id_ed25519" \
    "$username@127.0.0.1" list >/dev/null
  client=(
    env HOME="$run_dir/home"
    XDG_CONFIG_HOME="$run_dir/home/.config"
    target/debug/astra
    -p "${listen##*:}"
    -o StrictHostKeyChecking=yes
    -i "$run_dir/id_ed25519"
    "$username@127.0.0.1"
  )
}

start_gateway "$run_dir/gateway-1.log"
"${client[@]}" list >/dev/null

unicode_output="$(
  { sleep 1; } | \
    LC_ALL=astra_MISSING.UTF-8 TERM=astra-test-256color \
      "${client[@]}" new --attach -- /bin/sh -c \
    'printf "TERM=%s\n" "$TERM"; printf "CHARMAP="; locale charmap; printf "中文测试\n"; stty -a | grep -Eq "(^|[ ;])iutf8([ ;]|$)" && printf "IUTF8=on\n"'
)"
if ! printf '%s' "$unicode_output" | rg -q 'TERM=astra-test-256color'; then
  echo "client TERM was not propagated to the managed PTY" >&2
  exit 1
fi
if ! printf '%s' "$unicode_output" | rg -qi 'CHARMAP=UTF-?8'; then
  echo "unavailable client locale did not fall back to server UTF-8" >&2
  exit 1
fi
if ! printf '%s' "$unicode_output" | rg -q '中文测试'; then
  echo "UTF-8 terminal output was not preserved" >&2
  exit 1
fi
if ! printf '%s' "$unicode_output" | rg -q 'IUTF8=on'; then
  echo "managed PTY did not enable IUTF8 input handling" >&2
  exit 1
fi

erase_output="$(
  { printf '中\177A\n'; sleep 1; } | \
    "${client[@]}" new --attach -- /bin/sh -c \
    'IFS= read -r line; hex="$(printf %s "$line" | od -An -tx1 | tr -d " \n")"; printf "INPUT_HEX=%s\n" "$hex"'
)"
if ! printf '%s' "$erase_output" | rg -q 'INPUT_HEX=41'; then
  echo "IUTF8 did not erase a complete multibyte input character" >&2
  exit 1
fi

observed_uid="$("${client[@]}" new --attach -- /usr/bin/id -u)"
if ! printf '%s' "$observed_uid" | rg -q "(^|[^0-9])$uid([^0-9]|$)"; then
  echo "worker did not run the PTY as UID $uid" >&2
  exit 1
fi

terminal_id="$(
  "${client[@]}" new --name gateway-restart -- \
    /bin/sh -c 'echo BEFORE_GATEWAY_RESTART; sleep 2; echo AFTER_GATEWAY_RESTART; sleep 20'
)"
before="$(timeout 1s "${client[@]}" attach "$terminal_id" --read-only || true)"
if ! printf '%s' "$before" | rg -q 'BEFORE_GATEWAY_RESTART'; then
  echo "initial managed terminal output was not received" >&2
  exit 1
fi

kill "$gateway_pid"
wait "$gateway_pid" 2>/dev/null || true
gateway_pid=""
sleep 3

start_gateway "$run_dir/gateway-2.log"
after="$(timeout 1s "${client[@]}" attach "$terminal_id" --read-only || true)"
if ! printf '%s' "$after" | rg -q 'AFTER_GATEWAY_RESTART'; then
  echo "per-user worker did not survive gateway restart" >&2
  exit 1
fi

if HOME="$run_dir/home" XDG_CONFIG_HOME="$run_dir/home/.config" target/debug/astra \
  -p "${listen##*:}" \
  -o StrictHostKeyChecking=yes \
  -i "$run_dir/id_ed25519" \
  root@127.0.0.1 list >/dev/null 2>&1; then
  echo "non-root gateway unexpectedly allowed a different Unix account" >&2
  exit 1
fi

"${client[@]}" close "$terminal_id" >/dev/null
echo "managed smoke test passed; artifacts: $run_dir"
