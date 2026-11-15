#!/usr/bin/env bash
# Explicit-scope deployment entry point: plan by default, file-only with --files.
#
# `uninstall.sh` runs `echo-deploy uninstall-plan`: it renders a pending
# uninstall plan only. It writes nothing and removes nothing.
#
# `uninstall.sh --files ...` runs `echo-deploy uninstall-files`: it removes only
# regular files named in an ownership ledger whose digest, size, mode and owner
# still match. It never removes directories, configuration, state or user data,
# and it requires explicit --confirm plus either --offline-only or a
# service-stopped backend. It never invokes a service manager.
set -eu

usage() {
    cat <<'EOF'
Usage: uninstall.sh [--help] [--dry-run] [uninstall-plan arguments...]
       uninstall.sh --files --ledger /absolute/ownership-ledger.json \
           --offline-only --confirm

Plan mode (default) renders a pending uninstall plan only:

  echo-deploy uninstall-plan --spec /absolute/install-spec.json

File mode (--files) runs `echo-deploy uninstall-files` and removes only
ledger-owned unit/launcher files that still match their recorded digest and
metadata. Configuration, state, data directories and user data are never
removed, and the ownership ledger remains as evidence. The CLI refuses unless
--offline-only (an explicit operator attestation made after stopping services
out of band) or a stopped-service backend is supplied, and it also requires
--confirm.

Legacy uninstaller flags such as --purge and --disable-linger are rejected:
there is deliberately no config/data purge and no service-lifetime path here.

Environment:
  ECHO_DEPLOY_TOOL  absolute path to a regular, non-symlink executable
                    (required; there is no PATH fallback)
EOF
}

die() {
    printf 'uninstall.sh: %s\n' "$*" >&2
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

mode=plan
if [ "${1-}" = "--files" ]; then
    mode=files
    shift
fi

# `--dry-run` is accepted as a plan-mode alias and dropped (the plan is
# non-mutating); it is never dropped for arguments after `--`.
filtered=()
passthrough=0
for arg in "$@"; do
    if [ "$passthrough" -eq 1 ]; then
        filtered+=("$arg")
        continue
    fi
    case "$arg" in
        --)
            filtered+=("$arg")
            passthrough=1
            ;;
        --dry-run)
            if [ "$mode" = files ]; then
                die '--dry-run cannot be combined with --files; file mode removes ledger-owned files'
            fi
            ;;
        --purge|--purge=*)
            die 'refusing --purge: no entry point deletes configuration, state or user data; --files removes only ledger-owned files'
            ;;
        --disable-linger|--disable-linger=*)
            die 'refusing --disable-linger: this entry does not change service lifetime or service-manager state'
            ;;
        --scope|--scope=*)
            die "refusing legacy uninstaller option $arg: scope is recorded in the install spec and ownership ledger; it is never inferred or changed here"
            ;;
        *)
            filtered+=("$arg")
            ;;
    esac
done
if [ "${#filtered[@]}" -eq 0 ]; then
    set --
else
    set -- "${filtered[@]}"
fi

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

if [ "$mode" = files ]; then
    printf '%s\n' 'uninstall.sh: file-only mode - removing only ledger-owned files that still match their recorded metadata; configuration, state and user data are not removed and no service manager is invoked.' >&2
    exec "$tool" uninstall-files "$@"
fi

printf '%s\n' 'uninstall.sh: plan only - rendering an uninstall plan; no service is stopped, nothing is removed, and user data is not deleted.' >&2
exec "$tool" uninstall-plan "$@"
