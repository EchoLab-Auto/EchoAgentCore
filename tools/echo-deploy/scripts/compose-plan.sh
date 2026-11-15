#!/usr/bin/env bash
# Explicit-scope planning entry point.
#
# Fixed command: `echo-deploy compose-plan`. It renders a Compose plan from a
# strict spec and prints it as JSON. It never invokes a container runtime,
# never creates/starts/stops/updates a container, and never edits a deployment
# directory. A rendered plan is not a deployment.
set -eu

usage() {
    cat <<'EOF'
Usage: compose-plan.sh [--help] [compose-plan arguments...]

Render a Compose plan only. The fixed command is:

  echo-deploy compose-plan --spec /absolute/compose-spec.json

The echo-deploy CLI validates every forwarded argument and prints the plan as
JSON. This entry point never invokes a container runtime, never creates,
starts, stops, updates or removes a container, and never writes a deployment
directory. A plan is not a deployment: it changes no running state.

Flags that would ask for execution (--up, --down, --restart, --pull) are
rejected rather than forwarded.

Environment:
  ECHO_DEPLOY_TOOL  absolute path to a regular, non-symlink executable
                    (required; there is no PATH fallback)
EOF
}

die() {
    printf 'compose-plan.sh: %s\n' "$*" >&2
    exit 2
}

for arg in "$@"; do
    case "$arg" in
        --) break ;;
        -h|--help)
            usage
            exit 0
            ;;
    esac
done

for arg in "$@"; do
    case "$arg" in
        --) break ;;
        --up|--down|--restart|--pull|--force)
            die "refusing execution flag $arg: this entry only renders a plan and never invokes a container runtime"
            ;;
    esac
done

tool=${ECHO_DEPLOY_TOOL-}
if [ -z "$tool" ]; then
    die 'ECHO_DEPLOY_TOOL is not set; export the absolute path to the echo-deploy executable'
fi
case "$tool" in
    /*) ;;
    *) die "ECHO_DEPLOY_TOOL must be an absolute path, got: $tool" ;;
esac
if [ -L "$tool" ]; then
    die "ECHO_DEPLOY_TOOL must not be a symlink: $tool"
fi
if [ ! -f "$tool" ]; then
    die "ECHO_DEPLOY_TOOL is not a regular file: $tool"
fi
if [ ! -x "$tool" ]; then
    die "ECHO_DEPLOY_TOOL is not executable: $tool"
fi

printf '%s\n' 'compose-plan.sh: plan only - rendering a Compose plan; no container runtime is invoked and no container is created, started, stopped or updated. A plan is not a deployment.' >&2
exec "$tool" compose-plan "$@"
