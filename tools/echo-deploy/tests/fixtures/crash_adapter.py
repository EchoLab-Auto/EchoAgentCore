#!/usr/bin/python3
"""Trusted **test** control adapter for the SIGKILL interruption acceptance.

This file is a fixture. It is *not* `echo-deploy runtime-adapter`, and nothing a
test concludes while this script runs may be described as verification of the
shipping Core, of the shipping adapter, or of a running image.

Why this file exists instead of reusing `tests/fixtures/trusted_adapter.py`
----------------------------------------------------------------------------

`echo-deploy systemd-apply` pins the administrator control adapter into the
activation snapshot and executes *those bytes* for the transaction; the same
pinned bytes are executed again by an independent `systemd-recover`. The
existing `trusted_adapter.py` reads `trusted-adapter.json` from its own
directory, which does not exist in the pinned snapshot. This fixture embeds its
configuration as base64 in the file itself, so copying the bytes into the
snapshot keeps it self-contained.

The configuration is injected by `tests/systemd_crash_live.rs` into the
placeholder below. It only contains paths inside that test's throw-away
temporary directory, the unique instance id and the unique unit name.

What it verifies (and what it does not)
---------------------------------------

It checks the durable selector symlink, the `release.json` SHA-256 of the
release being prepared, the fixture Core's `prepare`/`status`/`ready`
acknowledgements, and agreement between selector and declared release.

It deliberately does **not** hash `/proc/<pid>/exe` or claim image
verification. The interruption test proves the deploy transaction's durable
crash recovery; the shipping adapter protocol is exercised by
`tests/systemd_live.rs`.
"""

import hashlib
import json
import os
import socket
import sys

CONFIG_JSON = r'''__CRASH_ADAPTER_CONFIG_JSON__'''
CFG = json.loads(CONFIG_JSON)


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
    connection.sendall(
        (json.dumps(request, separators=(",", ":")) + "\n").encode("utf-8")
    )
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
            if (
                status["maintenance_held"]
                or status["prepared"]
                or status["phase"] != "idle"
            ):
                raise RuntimeError("fixture Core is not idle")
            if status["release"]["declared"] != selected:
                raise RuntimeError("selector and fixture Core disagree")
            status = core(
                "ready", "ready-" + CFG["instance"], expected_release=selected
            )
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
