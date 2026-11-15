#!/usr/bin/env bash
# Explicit-scope deployment entry point.
#
# Fixed command: `echo-deploy recover`. Arguments are forwarded verbatim and
# the CLI owns validation, including the mandatory explicit confirmation. This
# script cannot select any other subcommand.
#
# `recover` reconciles an interrupted deployment transaction using the same
# trusted execution config. It is not a reinstall: it does not uninstall,
# install, rebuild or replace anything itself.
set -eu

usage() {
    cat <<'EOF'
Usage: repair.sh [--help] [recover arguments...]

Invoke the fixed command `echo-deploy recover` to reconcile an interrupted
deployment transaction. All arguments are forwarded verbatim to the CLI, which
validates them:

  echo-deploy recover --execution /absolute/execution.json --confirm

This entry point cannot select another subcommand. It does not download, fetch,
clone, build or install anything, and it does not elevate privileges. Recovery
uses the same trusted execution config; it cannot switch adapters or configs
mid-transaction.

Legacy reinstall flags such as --overwrite-config, --no-start, --no-deps and
--dry-run are rejected rather than forwarded.

Environment:
  ECHO_DEPLOY_TOOL  absolute path to a regular, non-symlink executable
                    (required; there is no PATH fallback)
EOF
}

die() {
    printf 'repair.sh: %s\n' "$*" >&2
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
        --overwrite-config|--overwrite-config=*|--no-start|--no-deps|--dry-run|--source|--source=*|--binary|--binary=*|--remote|--local|--full|--modules-only|--scope|--scope=*|--service-user|--service-user=*|--repository|--repository=*|--cn-mirror|--cn-mirror=*|--no-cn-mirror)
            die "refusing legacy reinstall option $arg: repair.sh runs echo-deploy recover on an interrupted transaction; it does not uninstall, install or reinstall anything"
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

printf '%s\n' 'repair.sh: fixed command echo-deploy recover; arguments and explicit confirmation are validated by the CLI. Recovery uses the same explicit execution config; config/data preservation is the adapter responsibility.' >&2
exec "$tool" recover "$@"
