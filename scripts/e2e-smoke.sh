#!/usr/bin/env bash
# End-to-end smoke test for nsticky.
#
# Runs the real binary against a fake niri IPC server, in an isolated XDG
# environment, so it never touches the running session. Needs python3 (for the
# fake compositor) and the binary built (cargo build).
#
#   scripts/e2e-smoke.sh            # uses target/debug/nsticky
#   NSTICKY_BIN=... scripts/e2e-smoke.sh

set -euo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=${NSTICKY_BIN:-$REPO_ROOT/target/debug/nsticky}
WORK=$(mktemp -d /tmp/nsticky-e2e.XXXXXX)
NIRI_SOCK="$WORK/niri.sock"
STATE_DIR="$WORK/state"
RUNTIME_DIR="$WORK/run"
CONFIG_HOME="$WORK/config"
DAEMON_PID=""
FAKE_PID=""
FAILURES=0

export NIRI_SOCKET="$NIRI_SOCK"
export XDG_STATE_HOME="$STATE_DIR"
export XDG_RUNTIME_DIR="$RUNTIME_DIR"
export XDG_CONFIG_HOME="$CONFIG_HOME"
export RUST_LOG=warn

mkdir -p "$STATE_DIR" "$RUNTIME_DIR" "$CONFIG_HOME/nsticky" "$WORK/bin"

if [[ ! -x "$BIN" ]]; then
  echo "binary not found: $BIN" >&2
  echo "build it first (cargo build) or point NSTICKY_BIN at it" >&2
  exit 2
fi

cleanup() {
  [[ -n "$DAEMON_PID" ]] && kill "$DAEMON_PID" 2>/dev/null || true
  [[ -n "$FAKE_PID" ]] && kill "$FAKE_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

ok() { printf '  ok   %s\n' "$1"; }
bad() {
  printf '  FAIL %s\n' "$1"
  FAILURES=$((FAILURES + 1))
}
check_contains() { # description, needle, haystack
  if [[ "$3" == *"$2"* ]]; then ok "$1"; else bad "$1 (missing: $2)"; printf '%s\n' "$3" | head -5; fi
}
check_status() { # description, expected, actual
  if [[ "$2" == "$3" ]]; then ok "$1"; else bad "$1 (expected $2, got $3)"; fi
}
check_equals() { # description, expected, actual
  if [[ "$2" == "$3" ]]; then ok "$1"; else bad "$1 (expected '$2', got '$3')"; fi
}

# run <command...>: capture output and status without tripping `set -e`.
OUT=""
STATUS=0
run() {
  set +e
  OUT=$("$@" 2>&1)
  STATUS=$?
  set -e
}

nsticky() { "$BIN" "$@"; }

# focus_workspace <id>: ask the fake compositor to focus a workspace, which
# makes it publish a WorkspaceActivated event, like the real one does.
focus_workspace() {
  python3 - "$NIRI_SOCK" "$1" <<'FOCUS'
import json, socket, sys
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(sys.argv[1])
sock.sendall(json.dumps({"Action": {"FocusWorkspace": {"reference": {"Id": int(sys.argv[2])}}}}).encode() + b"\n")
sock.makefile("r").readline()
FOCUS
  sleep 0.4 # let the daemon react
}

moves() { grep '^MOVE' "$WORK/requests.log" 2>/dev/null || true; }

# workspace_layout <output>: "idx id name" of that output's workspaces, in strip
# order (that is: sorted by index, which is what the user sees).
workspace_layout() {
  python3 - "$NIRI_SOCK" "${1:-DP-1}" <<'LAYOUT'
import json, socket, sys
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(sys.argv[1])
sock.sendall(b'"Workspaces"\n')
reply = json.loads(sock.makefile("r").readline())
strip = [w for w in reply["Ok"]["Workspaces"] if w["output"] == sys.argv[2]]
for workspace in sorted(strip, key=lambda w: w["idx"]):
    print("%s %s %s" % (workspace["idx"], workspace["id"], workspace["name"] or "-"))
LAYOUT
}

# fake_request <json>: ask the fake compositor for something niri has no request
# for, and print its reply.
fake_request() {
  python3 - "$NIRI_SOCK" "$1" <<'REQUEST'
import json, socket, sys
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(sys.argv[1])
sock.sendall(sys.argv[2].encode() + b"\n")
print(sock.makefile("r").readline().strip())
REQUEST
}

# hostile_request: send far more than the daemon accepts on the CLI socket and
# print the reply, as a client trying to make it allocate without bound would.
hostile_request() {
  python3 - "$RUNTIME_DIR/nsticky/cli.sock" <<'HOSTILE'
import socket, sys
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(sys.argv[1])
sock.sendall(b'{"command":"add","window_id":' + b"1" * 200000 + b"}\n")
print(sock.makefile("r").readline().strip())
HOSTILE
}

wait_for_move() { # window id, timeout in tenths of a second
  for _ in $(seq 1 "${2:-30}"); do
    if moves | grep -q "^MOVE $1 "; then return 0; fi
    sleep 0.1
  done
  return 1
}

cat >"$WORK/fake_niri.py" <<'FAKE'
import json, os, socket, sys, threading, time

SOCK, WINDOWS, LOG = sys.argv[1], sys.argv[2], sys.argv[3]

state = {
    "workspaces": [
        {"id": 1, "idx": 1, "name": "one", "output": "DP-1", "is_active": True, "is_focused": True, "is_urgent": False},
        {"id": 3, "idx": 2, "name": "three", "output": "DP-1", "is_active": False, "is_focused": False, "is_urgent": False},
        {"id": 2, "idx": 1, "name": "two", "output": "DP-2", "is_active": True, "is_focused": False, "is_urgent": False},
        {"id": 5, "idx": 3, "name": "parking", "output": "DP-1", "is_active": False, "is_focused": False, "is_urgent": False},
        {"id": 6, "idx": 4, "name": None, "output": "DP-1", "is_active": False, "is_focused": False, "is_urgent": False},
    ]
}
lock = threading.Lock()
subscribers = []

def log(line):
    with open(LOG, "a") as fh:
        fh.write(line + "\n")

windows_mtime = None

def load_windows():
    global windows_mtime
    try:
        mtime = os.path.getmtime(WINDOWS)
    except OSError:
        return state.get("windows", [])
    if mtime != windows_mtime:
        windows_mtime = mtime
        with open(WINDOWS) as fh:
            state["windows"] = json.load(fh)
    return state["windows"]

def log_window_action(text):
    log(text)

def workspaces():
    with lock:
        return json.loads(json.dumps(state["workspaces"]))

def publish_workspaces():
    publish({"WorkspacesChanged": {"workspaces": workspaces()}})

def is_empty(workspace_id):
    return not any(w.get("workspace_id") == workspace_id for w in load_windows())

def ensure_tail(output):
    """niri always keeps an empty, unnamed workspace at the bottom of an output."""
    same = [w for w in state["workspaces"] if w["output"] == output]
    if any(w["name"] is None and is_empty(w["id"]) for w in same):
        return
    new_id = max([w["id"] for w in state["workspaces"]] or [0]) + 1
    state["workspaces"].append({
        "id": new_id, "idx": len(same) + 1, "name": None, "output": output,
        "is_active": False, "is_focused": False, "is_urgent": False,
    })

def set_workspace_name(name, workspace_id):
    for workspace in state["workspaces"]:
        if workspace["id"] == workspace_id:
            workspace["name"] = name
            log("NAME %s %s" % (workspace_id, name))
            ensure_tail(workspace["output"])
            publish_workspaces()
            return

def unset_workspace_name(workspace_id):
    for workspace in state["workspaces"]:
        if workspace["id"] == workspace_id:
            workspace["name"] = None
            log("UNNAME %s" % workspace_id)
            publish_workspaces()
            return

def move_workspace_to_index(workspace_id, index):
    # Like niri: 1-based, clamped, and the strip always keeps its empty tail.
    with lock:
        target = next((w for w in state["workspaces"] if w["id"] == workspace_id), None)
        if target is None:
            return
        output = target["output"]
        same = sorted(
            [w for w in state["workspaces"] if w["output"] == output],
            key=lambda w: w["idx"],
        )
        same.remove(target)
        same.insert(max(0, min(index - 1, len(same))), target)
        for position, workspace in enumerate(same, start=1):
            workspace["idx"] = position
        log("MOVEWS %s %s" % (workspace_id, index))
        ensure_tail(output)
    publish_workspaces()

def move_window(window_id, reference):
    for window in load_windows():
        if window["id"] == window_id:
            if "Id" in reference:
                window["workspace_id"] = reference["Id"]
            elif "Index" in reference:
                for workspace in state["workspaces"]:
                    if workspace["idx"] == reference["Index"]:
                        window["workspace_id"] = workspace["id"]
            return

def publish(event):
    payload = json.dumps(event) + "\n"
    for stream in list(subscribers):
        try:
            stream.write(payload)
            stream.flush()
        except Exception:
            subscribers.remove(stream)

def focus_workspace(reference):
    target = reference.get("Id")
    with lock:
        matches = [w for w in state["workspaces"] if w["id"] == target]
        if not matches and "Index" in reference:
            matches = [w for w in state["workspaces"] if w["idx"] == reference["Index"]]
        for workspace in matches:
            for other in state["workspaces"]:
                if other["output"] == workspace["output"]:
                    other["is_active"] = False
                    other["is_focused"] = False
            workspace["is_active"] = True
            workspace["is_focused"] = True
            publish({"WorkspaceActivated": {"id": workspace["id"], "focused": True}})
            return

def stream(fh):
    fh.write(json.dumps({"Ok": "Handled"}) + "\n")
    fh.flush()
    subscribers.append(fh)
    # niri sends the current state up front, then follows with updates.
    for window in load_windows():
        publish({"WindowOpenedOrChanged": {"window": window}})
    publish({"ConfigLoaded": {"failed": False}})
    # A client is allowed to shut down its write half (niri does not close the
    # stream for that), so wait instead of reading: dead subscribers are pruned
    # by publish().
    while True:
        time.sleep(1)

def handle(conn):
    log("CONNECT")
    fh = conn.makefile("rw", encoding="utf-8", newline="\n")
    while True:
        line = fh.readline()
        if not line:
            break
        log(line.rstrip())
        try:
            req = json.loads(line)
        except Exception:
            break

        if req == "Windows":
            reply = {"Ok": {"Windows": load_windows()}}
        elif req == "Workspaces":
            reply = {"Ok": {"Workspaces": workspaces()}}
        elif req == "FocusedWindow":
            reply = {"Ok": {"FocusedWindow": {"id": 1}}}
        elif req == "EventStream":
            stream(fh)
            break
        elif isinstance(req, dict) and "Fake" in req:
            # Test-only hooks: something niri does that the harness cannot ask
            # for with a normal request.
            hook = req["Fake"]
            if isinstance(hook, dict) and "AddWorkspace" in hook:
                output = hook["AddWorkspace"]["output"]
                with lock:
                    same = [w for w in state["workspaces"] if w["output"] == output]
                    new_id = max([w["id"] for w in state["workspaces"]] or [0]) + 1
                    state["workspaces"].append({
                        "id": new_id, "idx": len(same) + 1, "name": None, "output": output,
                        "is_active": False, "is_focused": False, "is_urgent": False,
                    })
                    log("WORKSPACE %s %s" % (new_id, output))
                publish_workspaces()
                reply = {"Ok": {"Workspace": new_id}}
            elif hook == "PublishWorkspaces":
                publish_workspaces()
                reply = {"Ok": "Handled"}
            else:
                reply = {"Err": "unknown hook"}
        elif isinstance(req, dict) and "Action" in req:
            action = req["Action"]
            if "MoveWindowToWorkspace" in action:
                move = action["MoveWindowToWorkspace"]
                move_window(move["window_id"], move["reference"])
                log("MOVE %s %s" % (move["window_id"], json.dumps(move["reference"])))
            elif "FocusWorkspace" in action:
                focus_workspace(action["FocusWorkspace"]["reference"])
            elif "SetWorkspaceName" in action:
                named = action["SetWorkspaceName"]
                set_workspace_name(named["name"], named["workspace"]["Id"])
            elif "UnsetWorkspaceName" in action:
                unset_workspace_name(action["UnsetWorkspaceName"]["reference"]["Id"])
            elif "MoveWorkspaceToIndex" in action:
                move = action["MoveWorkspaceToIndex"]
                move_workspace_to_index(move["reference"]["Id"], move["index"])
            elif "Spawn" in action:
                log("SPAWN " + " ".join(action["Spawn"]["command"]))
            elif "FocusWindow" in action:
                log("FOCUS %s" % action["FocusWindow"]["id"])
            elif "MoveWindowToFloating" in action:
                log("FLOAT %s" % action["MoveWindowToFloating"]["id"])
            elif "MoveFloatingWindow" in action:
                move = action["MoveFloatingWindow"]
                log("MOVEWIN %s %s %s" % (move["id"], json.dumps(move.get("x")), json.dumps(move.get("y"))))
            elif "SetWindowWidth" in action:
                log("WIDTH %s %s" % (action["SetWindowWidth"]["id"], json.dumps(action["SetWindowWidth"]["change"])))
            elif "SetWindowHeight" in action:
                log("HEIGHT %s %s" % (action["SetWindowHeight"]["id"], json.dumps(action["SetWindowHeight"]["change"])))
            reply = {"Ok": "Handled"}
        else:
            reply = {"Err": "unexpected request"}

        fh.write(json.dumps(reply) + "\n")
        fh.flush()

server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(SOCK)
server.listen(16)
while True:
    conn, _ = server.accept()
    threading.Thread(target=handle, args=(conn,), daemon=True).start()
FAKE

write_windows() { # workspace of window 1, workspace of window 2
  cat >"$WORK/windows.json" <<JSON
[
  {"id": 1, "app_id": "fake", "title": "Fake One", "workspace_id": ${1:-1}, "is_floating": false},
  {"id": 2, "app_id": "fake", "title": "Fake Two", "workspace_id": ${2:-1}, "is_floating": false}
]
JSON
}

write_scratchpad_window() { # the window the scratchpad looks for, floating
  cat >"$WORK/windows.json" <<'JSON'
[
  {"id": 1, "app_id": "foot", "title": "dropdown-terminal", "workspace_id": 1,
   "is_floating": true,
   "layout": {"window_size": [1100.0, 480.0], "tile_pos_in_workspace_view": [730.0, 94.0]}}
]
JSON
}

write_config_scratchpad() { # a dropdown terminal toggled by one keybinding
  cat >"$CONFIG_HOME/nsticky/config.toml" <<TOML
stage-workspace = "parking"
menu = "/bin/true"

[scratchpad.term]
app-id = "foot"
title = "dropdown-terminal"
spawn = ["foot", "--app-id", "foot", "--title", "dropdown-terminal"]
TOML
}

write_config_hideout() { # a stage name no workspace carries yet
  cat >"$CONFIG_HOME/nsticky/config.toml" <<TOML
stage-workspace = "hideout"
menu = "/bin/true"
TOML
}

write_config_keep_stage() { # stage workspace is kept while empty
  cat >"$CONFIG_HOME/nsticky/config.toml" <<TOML
stage-workspace = "parking"
stage-keep-workspace = true
menu = "/bin/true"
TOML
}

write_config_plain() { # no rules: nothing is auto-managed
  cat >"$CONFIG_HOME/nsticky/config.toml" <<TOML
stage-workspace = "parking"
menu = "/bin/true"
TOML
}

write_config_pin_dp1() { # pins every matching window to the other monitor
  cat >"$CONFIG_HOME/nsticky/config.toml" <<TOML
stage-workspace = "parking"

[sticky.keepers]
app-id = ["^fake$"]
output = "DP-1"
TOML
}

write_config_own_output() { # rule without a pin, windows stay on their monitor
  cat >"$CONFIG_HOME/nsticky/config.toml" <<TOML
stage-workspace = "parking"
sticky-follow = "own-output"

[sticky.keepers]
app-id = ["^fake$"]
TOML
}

write_config() { # sticky rules, stage rules, menu, pinning
  cat >"$CONFIG_HOME/nsticky/config.toml" <<TOML
stage-workspace = "parking"
menu = "/bin/true"

[sticky.keepers]
app-id = ["^fake$"]
exclude-title = "^Transient"
output = "DP-2"

[stage.games]
app-id = "^game$"
TOML
}

: >"$WORK/requests.log"
write_windows 10 10
write_config

echo "== starting fake compositor =="
python3 "$WORK/fake_niri.py" "$NIRI_SOCK" "$WORK/windows.json" "$WORK/requests.log" &
FAKE_PID=$!
for _ in $(seq 1 50); do [[ -S "$NIRI_SOCK" ]] && break; sleep 0.1; done
[[ -S "$NIRI_SOCK" ]] || { echo "fake niri did not start"; exit 1; }

start_daemon() { # extra args...
  # Background the binary itself: `$!` must be the daemon, not a wrapper
  # function running in a subshell, or stopping it would leak the process.
  "$BIN" "$@" &
  DAEMON_PID=$!
  for _ in $(seq 1 50); do
    nsticky windows >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  echo "daemon did not become ready"
  return 1
}

stop_daemon() {
  kill "$DAEMON_PID" 2>/dev/null || true
  wait "$DAEMON_PID" 2>/dev/null || true
  DAEMON_PID=""
}

echo "== config check =="
run nsticky config check
check_status "config check exits 0" 0 "$STATUS"
check_contains "prints config path" "config:" "$OUT"
check_contains "prints state path" "state:" "$OUT"
check_contains "prints the stage workspace" "stage workspace: parking" "$OUT"
check_contains "prints the scratchpad workspace" "scratchpad:      scratchpad" "$OUT"
check_contains "prints the follow mode" "sticky follow:   focused" "$OUT"
check_contains "lists the sticky rule" "sticky keepers" "$OUT"
check_contains "lists the stage rule" "stage  games" "$OUT"

run nsticky config check --json
check_status "config check --json exits 0" 0 "$STATUS"
check_contains "json reports the stage workspace" '"stage_workspace": "parking"' "$OUT"
check_contains "json reports the scratchpad workspace" '"scratchpad_workspace": "scratchpad"' "$OUT"
check_contains "json reports the state path" '"state":' "$OUT"

printf '[sticky.broken]\napp-id = "["\n' >"$CONFIG_HOME/nsticky/config.toml"
run nsticky config check
check_status "broken config exits non-zero" 1 "$STATUS"
check_contains "names the offending rule" "Invalid app-id regex in sticky.broken" "$OUT"

write_config

# The state phases should not fight auto-sticky rules.
write_config_plain

echo "== daemon + socket =="
start_daemon
check_equals "socket is private" "600" "$(stat -c '%a' "$RUNTIME_DIR/nsticky/cli.sock")"

run nsticky windows --json
check_contains "windows --json returns JSON" '"app_id":"fake"' "$OUT"

run nsticky stage list --json
check_contains "stage list is empty" "[]" "$OUT"

echo "== sticky state is persisted and restored =="
run nsticky sticky add 1
check_contains "sticky add succeeds" "Added" "$OUT"
run nsticky sticky list
check_contains "sticky list shows the window" "Fake One" "$OUT"
check_contains "state file was written" '"sticky"' "$(cat "$STATE_DIR/nsticky/state.json")"

stop_daemon
start_daemon
run nsticky sticky list
check_contains "sticky window survived the restart" "Fake One" "$OUT"

echo "== windows on the stage workspace are adopted =="
run nsticky stage list --json
check_contains "staged set starts empty" "[]" "$OUT"
stop_daemon
write_windows 1 5 # window 2 sits on the "parking" workspace
start_daemon
run nsticky stage list
check_contains "window on the stage workspace is adopted" "Fake Two" "$OUT"
run nsticky sticky list
check_contains "sticky window still known" "Fake One" "$OUT"

echo "== stage workspace lifecycle =="
write_config_hideout # no workspace is called "hideout" yet
run nsticky reload   # apply it without restarting the daemon
: >"$WORK/requests.log"
run nsticky stage add 1
check_contains "staging a window succeeds" "Staged window" "$OUT"
check_contains "staging names the empty tail of the window's output" "NAME 6 hideout" "$(cat "$WORK/requests.log")"
check_contains "the window is moved to it" "MOVE 1 {\"Id\": 6}" "$(cat "$WORK/requests.log")"
if grep -q "NAME 3 " "$WORK/requests.log"; then
  bad "a workspace the user named must not be taken over"
else
  ok "the named workspace of the user is left alone"
fi

run nsticky stage remove-all # the earlier phases left one window parked
check_contains "the last window leaving gives the name back" "UNNAME 6" "$(cat "$WORK/requests.log")"
run nsticky stage list --json
check_contains "nothing is left parked" "[]" "$OUT"

: >"$WORK/requests.log"
write_config_keep_stage
run nsticky reload
run nsticky stage add 1
run nsticky stage remove-all
if grep -q "^UNNAME" "$WORK/requests.log"; then
  bad "stage-keep-workspace = true must keep the workspace"
else
  ok "stage-keep-workspace = true keeps the workspace"
fi

echo "== SIGHUP reloads the configuration =="
write_config_hideout # a stage name no workspace carries yet
kill -HUP "$DAEMON_PID"
# The daemon handles the signal in its own time, so park a window until it
# shows that it picked the new configuration up.
SIGHUP_APPLIED=""
for _ in $(seq 1 50); do
  : >"$WORK/requests.log"
  run nsticky stage add 1
  if grep -q "NAME [0-9]* hideout" "$WORK/requests.log"; then
    SIGHUP_APPLIED="yes"
    break
  fi
  run nsticky stage remove 1 # the old configuration was still in effect
  sleep 0.1
done
check_equals "the daemon re-read its configuration on SIGHUP" "yes" "${SIGHUP_APPLIED:-no}"
run nsticky stage remove-all
run nsticky stage list --json
check_contains "SIGHUP leaves the stage empty" "[]" "$OUT"

# A file that does not parse must not take the daemon, or the configuration it
# is running with, down.
printf '[sticky.broken]\napp-id = "["\n' >"$CONFIG_HOME/nsticky/config.toml"
kill -HUP "$DAEMON_PID"
sleep 0.3
run nsticky windows --json
check_status "a failed reload keeps the daemon serving" 0 "$STATUS"
: >"$WORK/requests.log"
run nsticky stage add 1
check_contains "and keeps the configuration it was running" "hideout" "$(grep '^NAME' "$WORK/requests.log" || echo '(nothing was named)')"
run nsticky stage remove-all
write_config

echo "== scratchpad =="
write_config_scratchpad
run nsticky reload
: >"$WORK/requests.log"

# No matching window yet: it starts the configured command.
run nsticky scratchpad term
check_contains "starts the command when nothing is open" "Starting term" "$OUT"
check_contains "the command reaches the compositor" "SPAWN foot --app-id foot --title dropdown-terminal" "$(cat "$WORK/requests.log")"

# Now the window exists and is visible: the toggle parks it.
write_scratchpad_window
: >"$WORK/requests.log"
run nsticky scratchpad term
check_contains "a visible scratchpad is hidden" "Hidden term" "$OUT"

# It gets its own workspace, separate from the stage: a dropdown terminal must
# not come back with `stage restore`.
scratchpad_workspace() { grep '^NAME [0-9]* scratchpad$' "$WORK/requests.log" | tail -1 | cut -d' ' -f2; }
PAD=$(scratchpad_workspace)
if [[ -n "$PAD" ]]; then
  ok "the scratchpad gets its own workspace (id $PAD)"
else
  bad "the scratchpad did not get a workspace ($(cat "$WORK/requests.log" | tr '\n' ';'))"
fi
check_contains "hiding parks it on that workspace" "MOVE 1 {\"Id\": $PAD}" "$(moves)"
if [[ "$PAD" == "5" ]]; then
  bad "the scratchpad took over the stage workspace"
else
  ok "the stage workspace is left alone"
fi
run nsticky stage list --json
check_contains "a scratchpad window is not a staged window" "[]" "$OUT"

# And back: shown, floated, sized and focused, in one call.
: >"$WORK/requests.log"
run nsticky scratchpad term
check_contains "a parked scratchpad is shown" "Shown term" "$OUT"
check_contains "it comes back floating" "FLOAT 1" "$(cat "$WORK/requests.log")"
check_contains "it keeps the size it had" "WIDTH 1 {\"SetFixed\": 1100}" "$(cat "$WORK/requests.log")"
check_contains "it keeps its height" "HEIGHT 1 {\"SetFixed\": 480}" "$(cat "$WORK/requests.log")"
check_contains "it goes back where it was" "MOVEWIN 1 {\"SetFixed\": 730.0} {\"SetFixed\": 94.0}" "$(cat "$WORK/requests.log")"
check_contains "and it takes focus" "FOCUS 1" "$(cat "$WORK/requests.log")"
check_contains "the empty scratchpad workspace is released" "UNNAME $PAD" "$(cat "$WORK/requests.log")"

# No name: it toggles the focused window, with no configuration at all.
write_config_plain
run nsticky reload
: >"$WORK/requests.log"
run nsticky scratchpad
check_contains "no name toggles the focused window (hide)" "Hidden focused" "$OUT"
run nsticky scratchpad
check_contains "no name toggles the focused window (show)" "Shown focused" "$OUT"

echo "== parking areas stay at the end =="
# One window parked on each area, with the user's own workspaces above them.
write_config_scratchpad
run nsticky reload
stop_daemon
cat >"$WORK/windows.json" <<'JSON'
[
  {"id": 1, "app_id": "foot", "title": "dropdown-terminal", "workspace_id": 1, "is_floating": true},
  {"id": 2, "app_id": "game", "title": "A Game", "workspace_id": 1, "is_floating": false}
]
JSON
start_daemon
run nsticky scratchpad term   # parks window 1 on the scratchpad workspace
run nsticky stage add 2       # parks window 2 on the stage workspace

# Both areas end up below every workspace the user works on, in a fixed order:
# the scratchpad first, the stage second, and nothing else under them.
wait_for_area_order() {
  for _ in $(seq 1 30); do
    local names
    names=$(workspace_layout | awk '{print $3}' | grep -v '^-$' | tail -2 | tr '\n' ' ')
    if [[ "$names" == "scratchpad parking " ]]; then return 0; fi
    sleep 0.1
  done
  return 1
}

if wait_for_area_order; then
  ok "both areas end up at the end, scratchpad above stage"
else
  bad "the areas are not ordered at the end ($(workspace_layout | tr '\n' ';'))"
fi

# A window opened on the bottom workspace pushes them down again.
NEW_WS=$(fake_request '{"Fake": {"AddWorkspace": {"output": "DP-1"}}}' | python3 -c 'import json,sys; print(json.load(sys.stdin)["Ok"]["Workspace"])')
cat >"$WORK/windows.json" <<JSON
[
  {"id": 1, "app_id": "foot", "title": "dropdown-terminal", "workspace_id": 1, "is_floating": true},
  {"id": 2, "app_id": "game", "title": "A Game", "workspace_id": 5, "is_floating": false},
  {"id": 3, "app_id": "zen", "title": "Docs", "workspace_id": ${NEW_WS}, "is_floating": false}
]
JSON
: >"$WORK/requests.log"
fake_request '{"Fake": "PublishWorkspaces"}' >/dev/null

for _ in $(seq 1 30); do
  [[ "$(grep -c '^MOVEWS' "$WORK/requests.log")" -ge 2 ]] && break
  sleep 0.1
done
MOVES_DOWN=$(grep -c '^MOVEWS' "$WORK/requests.log")
check_equals "both areas are moved back down (one call each)" "2" "$MOVES_DOWN"
if wait_for_area_order; then
  ok "the areas are back at the end, below the new workspace"
else
  bad "the areas did not move back to the end ($(workspace_layout | tr '\n' ';'))"
fi

echo "== monitor pinning =="
: >"$WORK/requests.log" # ignore the moves of the previous phases
write_config
stop_daemon
write_windows 2 1 # window 1 on DP-2, window 2 on DP-1
start_daemon
for _ in $(seq 1 30); do
  nsticky sticky list --json 2>/dev/null | grep -q "1" && break
  sleep 0.1
done

sticky_ids() { nsticky sticky list --json | python3 -c 'import json,sys; print(sorted(json.load(sys.stdin)))'; }

check_equals "both windows matched the pinning rule" "[1, 2]" "$(sticky_ids)"

focus_workspace 3 # focus a workspace on DP-1
if moves | grep -q "^MOVE 2 "; then
  ok "a window on the wrong monitor is put back on its own"
else
  bad "the window that drifted was not re-pinned ($(moves | tr '\n' ';'))"
fi
if moves | grep -q "^MOVE 1 "; then
  bad "a window already on its monitor must not follow the other one"
else
  ok "a pinned window ignored the other monitor's activation"
fi

focus_workspace 2 # focus DP-2's workspace
if wait_for_move 1 30; then
  ok "a pinned window followed its own monitor"
else
  bad "the pinned window did not follow its monitor"
fi
check_contains "the move targeted DP-2's workspace" 'MOVE 1 {"Id": 2}' "$(moves)"

echo "== own-output mode =="
write_config_own_output
run nsticky reload
check_contains "pins already match the current outputs" "0 window(s) re-pinned" "$OUT"

: >"$WORK/requests.log" # ignore earlier moves
focus_workspace 1 # another workspace on DP-1
if moves | grep -q "^MOVE "; then
  bad "windows in own-output mode must stay on their monitor ($(moves | tr '\n' ';'))"
else
  ok "own-output kept every window on its monitor"
fi

echo "== re-pinning on reload =="
write_config_pin_dp1
run nsticky reload
check_contains "reload re-pins both windows" "2 window(s) re-pinned" "$OUT"

focus_workspace 3 # now a workspace on DP-1 becomes focused
if wait_for_move 1 30 && moves | grep -q '^MOVE 1 {"Id": 3}'; then
  ok "the re-pinned window moved to DP-1's active workspace"
else
  bad "the re-pinned window did not move ($(moves | tr '\n' ';'))"
fi
if wait_for_move 2 30 && moves | grep -q '^MOVE 2 {"Id": 3}'; then
  ok "both windows followed the new pin"
else
  bad "the second window did not follow the new pin ($(moves | tr '\n' ';'))"
fi

echo "== unknown window =="
run nsticky stage remove 999999
check_status "unknown window exits non-zero" 1 "$STATUS"
check_contains "unknown window is reported" "Window not found in Niri" "$OUT"

echo "== selector discovery =="
cat >"$WORK/bin/fuzzel" <<'SH'
#!/bin/sh
# Builtins only: this stub runs with a PATH that contains just its own dir.
printf '%s\n' "$@" >"$ARGV_FILE"
while read -r _line; do :; done
SH
chmod +x "$WORK/bin/fuzzel"
cat >"$CONFIG_HOME/nsticky/config.toml" <<'TOML'
# no menu configured
TOML
# A PATH with only the stub in it, so a real selector installed on the machine
# is never launched by the test.
run env PATH="$WORK/bin" ARGV_FILE="$WORK/argv.txt" "$BIN" stage restore </dev/null
check_status "discovered selector runs" 0 "$STATUS"
check_contains "selector receives its prompt" "Restore Window:" "$(cat "$WORK/argv.txt" 2>/dev/null || echo '(no argv)')"

run env PATH=/nonexistent "$BIN" stage restore </dev/null
check_status "no selector and no terminal fails" 1 "$STATUS"
check_contains "failure explains what to configure" "No selector available" "$OUT"

write_config

echo "== hostile clients =="
run nsticky windows --json
check_status "the daemon answers before the hostile request" 0 "$STATUS"

# Far beyond the 64 KiB a request may be: it is refused instead of buffered
# until the daemon runs out of memory.
check_contains "an oversized request is refused" "Request too large" "$(hostile_request)"

run nsticky windows --json
check_status "the daemon survives an oversized request" 0 "$STATUS"
check_contains "and keeps answering" '"app_id":"fake"' "$OUT"

echo "== socket takeover =="
# A daemon killed uncleanly leaves its socket file behind with nobody
# listening: the next start takes it over without being asked to.
CRASHED_PID=$DAEMON_PID
kill -9 "$CRASHED_PID" 2>/dev/null || true
wait "$CRASHED_PID" 2>/dev/null || true
DAEMON_PID=""
if [[ -S "$RUNTIME_DIR/nsticky/cli.sock" ]]; then
  ok "an unclean exit leaves the socket file behind"
else
  bad "the socket file went away with the daemon"
fi

start_daemon
run nsticky windows --json
check_status "a stale socket is taken over" 0 "$STATUS"
check_contains "the new daemon answers on it" '"app_id":"fake"' "$OUT"

# With a daemon still listening, only --replace may take the socket away.
: >"$WORK/replace.log"
"$BIN" --replace >"$WORK/replace.log" 2>&1 &
REPLACE_PID=$!
for _ in $(seq 1 50); do
  grep -q "Taking over the socket" "$WORK/replace.log" && break
  sleep 0.1
done
check_contains "the replacing daemon says so" "Taking over the socket from a running daemon" "$(cat "$WORK/replace.log")"
run kill -0 "$REPLACE_PID"
check_status "the replacing daemon is running" 0 "$STATUS"
run nsticky windows --json
check_contains "the socket answers after the takeover" '"app_id":"fake"' "$OUT"

# The path belongs to the new daemon now: with it gone nobody answers, even
# though the daemon it replaced is still alive.
kill "$REPLACE_PID" 2>/dev/null || true
wait "$REPLACE_PID" 2>/dev/null || true
run nsticky windows --json
check_status "the replaced daemon does not own the socket" 1 "$STATUS"

kill "$DAEMON_PID" 2>/dev/null || true
wait "$DAEMON_PID" 2>/dev/null || true
DAEMON_PID=""

echo "== single instance =="
stop_daemon
start_daemon
run nsticky
check_status "second daemon refuses to start" 1 "$STATUS"
check_contains "refusal is explicit" "already running" "$OUT"

echo
if [[ "$FAILURES" -eq 0 ]]; then
  echo "all checks passed"
else
  echo "$FAILURES check(s) failed"
  exit 1
fi
