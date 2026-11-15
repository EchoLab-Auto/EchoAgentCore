#!/usr/bin/python3
"""Trusted **test** control adapter for `echo-deploy-control/1`.

This file is a fixture. It is *not* `echo-deploy runtime-adapter`, and nothing a
test concludes while this script runs may be described as verification of the
shipping Core or of the shipping adapter.

Why it exists: the real `runtime-adapter` proves readiness by SHA-256 hashing
`/proc/<core pid>/exe` against the release manifest. In the sandbox where these
tests run, reading another process's `/proc/<pid>/exe` is denied
(`EPERM`, even with `PR_SET_PTRACER` accepted), so the real adapter can never
confirm readiness there. Tests that need the *deploy transaction* to progress
therefore use this adapter, which verifies:

* the durable selector symlink really names the release directory,
* `release.json` really hashes to the requested manifest digest,
* the fixture Core's Unix socket really acknowledges `prepare`/`status`/`ready`,
* the selector release really equals the release the fixture Core declares.

It deliberately does **not** claim binary-image verification; the test output and
the test file say so explicitly.

Configuration (same directory, `trusted-adapter.json`):
    instance, socket, selector, store, state, journal, unit
"""

import hashlib
import json
import os
import socket
import sys

HERE = os.path.dirname(os.path.realpath(__file__))
with open(os.path.join(HERE, "trusted-adapter.json"), "r", encoding="utf-8") as handle:
    CFG = json.load(handle)


def journal(event, detail):
    try:
        with open(CFG["journal"], "a", encoding="utf-8") as handle:
            handle.write(json.dumps({"event": event, "detail": detail}) + "\n")
    except OSError:
        pass


def load_state():
    try:
        with open(CFG["state"], "r", encoding="utf-8") as handle:
            return json.load(handle)
    except FileNotFoundError:
        return {"maintenance": False, "prepared": None}


def save_state(state):
    temporary = CFG["state"] + ".tmp"
    with open(temporary, "w", encoding="utf-8") as handle:
        json.dump(state, handle)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, CFG["state"])


def selector_release():
    try:
        target = os.readlink(CFG["selector"])
    except OSError:
        return None
    prefix = CFG["store"].rstrip("/") + "/"
    if not target.startswith(prefix):
        raise RuntimeError("current selector escapes the release store")
    return os.path.basename(target)


def manifest_digest(release):
    path = os.path.join(CFG["store"], release, "release.json")
    with open(path, "rb") as handle:
        return hashlib.sha256(handle.read()).hexdigest()


def core(operation, transaction_id, expected_release=None):
    connection = socket.socket(socket.AF_UNIX)
    connection.settimeout(10)
    connection.connect(CFG["socket"])
    request = {"operation": operation, "transaction_id": transaction_id}
    if expected_release is not None:
        request["expected_release"] = expected_release
    connection.sendall((json.dumps(request, separators=(",", ":")) + "\n").encode("utf-8"))
    payload = b""
    while not payload.endswith(b"\n"):
        chunk = connection.recv(65536)
        if not chunk:
            break
        payload += chunk
    connection.close()
    return json.loads(payload.decode("utf-8"))


def core_status():
    return core("status", "status-" + CFG["instance"])


def main():
    request = json.load(sys.stdin)
    operation = request["operation"]
    # The systemd-side `ControlRequest` carries no transaction id; Core-level
    # transaction ids are generated per operation below.
    transaction = request.get("transaction_id") or ("systemd-" + request["instance"])
    reply = {
        "protocol": request["protocol"],
        "operation": operation,
        "instance": request["instance"],
        "success": True,
    }
    try:
        if operation == "observe":
            state = load_state()
            selected = selector_release()
            current = selected
            ready = False
            if selected is not None and os.path.exists(CFG["socket"]):
                status = core_status()
                declared = status["release"]["declared"]
                if declared != selected:
                    raise RuntimeError(
                        "fixture Core declares %r, selector points at %r"
                        % (declared, selected)
                    )
                current = declared
                ready = (
                    status["phase"] == "idle"
                    and not status["prepared"]
                    and not status["maintenance_held"]
                )
            reply.update(
                current_release=current,
                maintenance=bool(state.get("maintenance")),
                ready=ready,
                active_work=0,
            )
        elif operation == "prepare_upgrade":
            release = request["release"]
            manifest = request["manifest_sha256"]
            actual = manifest_digest(release)
            if actual != manifest:
                raise RuntimeError(
                    "release %r manifest digest mismatch (%s)" % (release, actual)
                )
            selected = selector_release()
            if selected == release:
                raise RuntimeError("release %r is already selected" % release)
            state = load_state()
            if state.get("maintenance"):
                raise RuntimeError("maintenance is already held")
            status = core(
                "prepare",
                "prepare-%s-%s" % (CFG["instance"], manifest),
                expected_release=selected,
            )
            if not status.get("ok"):
                raise RuntimeError("fixture Core refused prepare: %r" % status)
            generation = status.get("generation")
            if not isinstance(generation, int) or generation <= 0:
                raise RuntimeError("fixture Core returned no generation")
            state["prepared"] = {
                "release": release,
                "manifest": manifest,
                "previous": selected,
                "generation": generation,
            }
            save_state(state)
            reply.update(maintenance=True, current_release=selected)
        elif operation == "maintenance_enter":
            state = load_state()
            state["maintenance"] = True
            save_state(state)
            reply.update(maintenance=True)
        elif operation == "drain":
            state = load_state()
            prepared = state.get("prepared")
            if not prepared:
                raise RuntimeError("drain requires a prepared transaction")
            status = core_status()
            if not status["prepared"] or not status["maintenance_held"]:
                raise RuntimeError("fixture Core is not prepared/held")
            if status.get("generation") != prepared["generation"]:
                raise RuntimeError("fixture Core generation mismatch")
            reply.update(active_work=0, current_release=status["release"]["declared"])
        elif operation == "ready":
            release = request["release"]
            selected = selector_release()
            if selected != release:
                raise RuntimeError(
                    "selector release %r != ready release %r" % (selected, release)
                )
            status = core("ready", "ready-" + CFG["instance"], expected_release=release)
            if not status.get("ok"):
                raise RuntimeError("fixture Core is not ready: %r" % status)
            reply.update(ready=True, current_release=release)
        elif operation == "maintenance_leave":
            selected = selector_release()
            if selected is None:
                raise RuntimeError("maintenance_leave requires a selected release")
            status = core_status()
            if status["maintenance_held"] or status["prepared"] or status["phase"] != "idle":
                raise RuntimeError("fixture Core is not idle")
            if status["release"]["declared"] != selected:
                raise RuntimeError("selector and fixture Core disagree")
            status = core("ready", "ready-" + CFG["instance"], expected_release=selected)
            if not status.get("ok"):
                raise RuntimeError("fixture Core is not ready: %r" % status)
            state = load_state()
            state["maintenance"] = False
            state["prepared"] = None
            save_state(state)
            reply.update(maintenance=False, current_release=selected)
        else:
            raise RuntimeError("unsupported operation %r" % operation)
    except Exception as error:  # noqa: BLE001 - the reply is the error channel
        journal("reject", {"operation": operation, "error": str(error)})
        reply["success"] = False
        reply["error"] = str(error)
    journal(operation, {"success": reply["success"], "request": request})
    json.dump(reply, sys.stdout)
    sys.stdout.write("\n")
    sys.stdout.flush()


if __name__ == "__main__":
    main()
