#!/usr/bin/env bash
# Resource measurements for a built nsticky: threads, RSS, virtual size, idle
# CPU, idle wakeups and the per-request cost of the CLI. Everything runs
# against the fake compositor in an isolated XDG environment, so it never
# touches a running session.
#
#   cargo build --release && scripts/perf-probe.sh
#   NSTICKY_BIN=target/debug/nsticky IDLE_SECONDS=60 scripts/perf-probe.sh
#
# Needs python3, for scripts/fake-niri.py.
set -uo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=${NSTICKY_BIN:-$REPO_ROOT/target/release/nsticky}
IDLE_SECONDS=${IDLE_SECONDS:-30}
CALLS=${CALLS:-200}

if [[ ! -x "$BIN" ]]; then
  echo "binary not found: $BIN (cargo build --release, or set NSTICKY_BIN)" >&2
  exit 2
fi

WORK=$(mktemp -d /tmp/nsticky-perf.XXXXXX)
DAEMON_PID=""
FAKE_PID=""
cleanup() {
  [[ -n "$DAEMON_PID" ]] && kill "$DAEMON_PID" 2>/dev/null
  [[ -n "$FAKE_PID" ]] && kill "$FAKE_PID" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT

export XDG_RUNTIME_DIR="$WORK/run"
export XDG_CONFIG_HOME="$WORK/config"
export XDG_STATE_HOME="$WORK/state"
export NIRI_SOCKET="$WORK/run/niri.sock"
export RUST_LOG=warn
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME/nsticky" "$XDG_STATE_HOME"

cat >"$WORK/windows.json" <<'JSON'
[
  {"id": 1, "app_id": "fake", "title": "Fake One", "workspace_id": 10, "is_floating": false},
  {"id": 2, "app_id": "fake", "title": "Fake Two", "workspace_id": 10, "is_floating": false}
]
JSON
cat >"$XDG_CONFIG_HOME/nsticky/config.toml" <<'TOML'
stage-workspace = "parking"
sticky-follow = "own-output"

[sticky.keepers]
app-id = ["^fake$"]
TOML

python3 "$REPO_ROOT/scripts/fake-niri.py" "$NIRI_SOCKET" "$WORK/windows.json" "$WORK/requests.log" &
FAKE_PID=$!
for _ in $(seq 1 50); do [[ -S "$NIRI_SOCKET" ]] && break; sleep 0.1; done

"$BIN" >"$WORK/daemon.log" 2>&1 &
DAEMON_PID=$!
for _ in $(seq 1 50); do "$BIN" windows >/dev/null 2>&1 && break; sleep 0.1; done
sleep 1

sample() { # read the daemon's counters
  THREADS=$(grep -m1 '^Threads:' "/proc/$DAEMON_PID/status" | tr -dc '0-9')
  RSS=$(grep -m1 '^VmRSS:' "/proc/$DAEMON_PID/status" | awk '{print $2}')
  VSZ=$(grep -m1 '^VmSize:' "/proc/$DAEMON_PID/status" | awk '{print $2}')
  FDS=$(ls "/proc/$DAEMON_PID/fd" | wc -l)
  SWITCHES=$(grep -m1 '^voluntary_ctxt_switches:' "/proc/$DAEMON_PID/status" | awk '{print $2}')
  read -r _ _ _ _ _ _ _ _ _ _ _ _ _ UTIME STIME _ <"/proc/$DAEMON_PID/stat"
  CPU_MS=$(((UTIME + STIME) * 10))
}

sample
echo "binary          $(stat -c%s "$BIN") bytes"
echo "runtime         threads=$THREADS rss=${RSS}kB vsz=${VSZ}kB fds=$FDS"

CPU_BEFORE=$CPU_MS
SWITCHES_BEFORE=$SWITCHES
sleep "$IDLE_SECONDS"
sample
echo "idle ${IDLE_SECONDS}s       cpu=$((CPU_MS - CPU_BEFORE))ms wakeups=+$((SWITCHES - SWITCHES_BEFORE))"

CPU_BEFORE=$CPU_MS
START=$(date +%s%N)
for _ in $(seq 1 "$CALLS"); do "$BIN" windows --json >/dev/null 2>&1; done
END=$(date +%s%N)
sample
echo "$CALLS x windows   daemon_cpu=$((CPU_MS - CPU_BEFORE))ms total wall=$(((END - START) / 1000000 / CALLS))ms/call"
