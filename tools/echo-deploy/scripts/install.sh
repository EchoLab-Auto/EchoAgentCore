#!/usr/bin/env bash
# Explicit-scope deployment entry point: plan by default, file-only with --files.
#
# `install.sh` runs `echo-deploy install-plan`: it renders a pending
# installation package into a NEW directory (or to stdout when --output is
# omitted). It writes no unit or launcher into a live directory, touches no
# service manager and never modifies a configured core/panel config or data
# path.
#
# `install.sh --files ...` runs `echo-deploy install-files`: it writes only NEW
# unit/launcher files into explicit, pre-existing directories and records an
# ownership ledger. Every existing target is a hard error and is never
# replaced. It still does not enable, start, restart or reload anything.
set -eu

usage() {
    cat <<'EOF'
Usage: install.sh [--help] [--dry-run] [install-plan arguments...]
       install.sh --files --spec /absolute/install-spec.json \
           --unit-directory /absolute/existing-unit-dir \
           --launcher-directory /absolute/existing-launcher-dir \
           --ledger /absolute/new-ledger.json --confirm

Plan mode (default) renders a pending installation package only:

  echo-deploy install-plan --spec /absolute/install-spec.json \
      --output /absolute/new-output-directory

File mode (--files) runs `echo-deploy install-files` and installs only new
unit/launcher files and a new ownership ledger. Existing files are a hard
error; nothing is overwritten. Neither mode enables, starts, restarts or
reloads a service, and neither mode writes the configured core/panel config
or any data path.

Legacy installer flags are rejected rather than forwarded: this entry has no
config rewrite, dependency bootstrap, network fetch or service activation path.

Environment:
  ECHO_DEPLOY_TOOL  absolute path to a regular, non-symlink executable
                    (required; there is no PATH fallback)
EOF
}

die() {
    printf 'install.sh: %s\n' "$*" >&2
    exit 2
}

# Help wins over every other argument except arguments after `--`, which are
# reserved for the tool and must be validated by the tool itself.
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

# Fail closed on legacy installer flags instead of forwarding them to the CLI,
# where they would produce a generic parser error. In particular, an attempted
# config rewrite is refused: no entry point may silently replace old config.
# `--dry-run` is accepted as a plan-mode alias and dropped (both plan modes are
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
                die '--dry-run cannot be combined with --files; file mode writes new files'
            fi
            ;;
        --overwrite-config|--overwrite-config=*)
            die 'refusing --overwrite-config: this entry never rewrites an existing config; migrate or edit the file explicitly'
            ;;
        --scope|--scope=*|--service-user|--service-user=*)
            die "refusing legacy installer option $arg: scope and service account belong to the install spec and administrator policy"
            ;;
        --repository|--repository=*|--no-deps|--no-start|--cn-mirror|--cn-mirror=*|--no-cn-mirror)
            die "refusing legacy installer option $arg: this entry does not fetch, build, install packages or start services"
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
    printf '%s\n' 'install.sh: file-only mode - requesting new unit/launcher files and a new ledger; existing files are refused, no service is enabled or started, and config/data paths are not written.' >&2
    exec "$tool" install-files "$@"
fi

printf '%s\n' 'install.sh: plan only - rendering a pending install package; no file is installed into a live directory and no service is installed, enabled or started.' >&2
exec "$tool" install-plan "$@"
