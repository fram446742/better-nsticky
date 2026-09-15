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
