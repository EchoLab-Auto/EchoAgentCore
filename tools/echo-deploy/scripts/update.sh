#!/usr/bin/env bash
# Explicit-scope deployment entry point.
#
# Fixed command: `echo-deploy apply`. Arguments are forwarded verbatim and the
# CLI owns validation, including the mandatory explicit confirmation. This
# script cannot select any other subcommand.
#
# This is a transaction entry for an already-staged candidate and an explicit
# administrator-owned execution adapter. It does not fetch, clone, build or
# replace a binary itself, and it does not elevate privileges.
set -eu

usage() {
    cat <<'EOF'
Usage: update.sh [--help] [apply arguments...]

Invoke the fixed command `echo-deploy apply` for an explicitly confirmed
deployment transaction. All arguments are forwarded verbatim to the CLI, which
validates them:

  echo-deploy apply --execution /absolute/execution.json \
      --release <release-id> --confirm

This entry point cannot select another subcommand. It does not download, fetch,
clone, build or replace releases, and it does not elevate privileges. The
release must already be verified and staged, and the execution adapter is a
separate administrator-owned file named by the execution config.

Legacy self-update flags such as --source, --remote, --local, --binary,
--status, --full and --modules-only are rejected rather than forwarded.

Environment:
  ECHO_DEPLOY_TOOL  absolute path to a regular, non-symlink executable
                    (required; there is no PATH fallback)
EOF
}

die() {
    printf 'update.sh: %s\n' "$*" >&2
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
        --source|--source=*|--binary|--binary=*|--status|--status=*|--config|--config=*|--remote|--local|--full|--modules-only)
            die "refusing legacy self-update option $arg: update.sh invokes echo-deploy apply on an already-staged candidate; it does not fetch, build or replace binaries"
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

printf '%s\n' 'update.sh: fixed command echo-deploy apply; arguments and explicit confirmation are validated by the CLI. Config/data preservation is the explicit execution adapter responsibility, not inferred here.' >&2
exec "$tool" apply "$@"
