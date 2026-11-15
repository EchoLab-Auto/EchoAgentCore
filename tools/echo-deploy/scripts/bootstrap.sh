#!/usr/bin/env bash
# Explicit-scope deployment entry point.
#
# Verify one administrator-supplied, already-local echo-deploy executable and
# then exec it (or a sibling wrapper that will exec it). There is deliberately
# no network, no build step and no privilege escalation path here: the only
# accepted binary is the file named by --tool, and it must match the exact
# digest named by --sha256.
set -eu

usage() {
    cat <<'EOF'
Usage: bootstrap.sh --tool /absolute/echo-deploy --sha256 <64-lowercase-hex> \
           [--wrapper install|update|repair|uninstall|compose-plan] \
           [--] [arguments...]

Verify a local echo-deploy executable and then exec it with the remaining
arguments. Verification requires all of the following:

  * --tool is an absolute path (no PATH lookup, no cwd-relative path)
  * --tool names a regular file, not a symlink
  * the file is executable
  * its SHA-256 digest equals --sha256 exactly

With --wrapper NAME, the verified path is exported as ECHO_DEPLOY_TOOL and the
sibling scripts/NAME.sh is executed with the remaining arguments. This lets a
thin wrapper use exactly the binary that was verified here instead of trusting
an independently set environment variable. Accepted names: install, update,
repair, uninstall, compose-plan. Put `--` before the wrapper's or tool's own
arguments, for example:

  bootstrap.sh --tool /opt/echo-deploy --sha256 <digest> --wrapper install -- \
      --spec /absolute/install-spec.json --output /absolute/new-plan

This entry point does not download, build, install packages or elevate
privileges. It is not an installer; it either refuses the local file or
executes that exact file.
EOF
}

die() {
    printf 'bootstrap.sh: %s\n' "$*" >&2
    exit 2
}

tool=
tool_set=0
sha=
sha_set=0
wrapper=
wrapper_set=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        -h|--help)
            usage
            exit 0
            ;;
        --tool)
            if [ "$#" -lt 2 ]; then
                die '--tool requires an absolute path argument'
            fi
            if [ "$tool_set" -eq 1 ]; then
                die '--tool was specified more than once'
            fi
            tool=$2
            tool_set=1
            shift 2
            ;;
        --tool=*)
            if [ "$tool_set" -eq 1 ]; then
                die '--tool was specified more than once'
            fi
            tool=${1#--tool=}
            tool_set=1
            shift
            ;;
        --sha256)
            if [ "$#" -lt 2 ]; then
                die '--sha256 requires a digest argument'
            fi
            if [ "$sha_set" -eq 1 ]; then
                die '--sha256 was specified more than once'
            fi
            sha=$2
            sha_set=1
            shift 2
            ;;
        --sha256=*)
            if [ "$sha_set" -eq 1 ]; then
                die '--sha256 was specified more than once'
            fi
            sha=${1#--sha256=}
            sha_set=1
            shift
            ;;
        --wrapper)
            if [ "$#" -lt 2 ]; then
                die '--wrapper requires a wrapper name argument'
            fi
            if [ "$wrapper_set" -eq 1 ]; then
                die '--wrapper was specified more than once'
            fi
            wrapper=$2
            wrapper_set=1
            shift 2
            ;;
        --wrapper=*)
            if [ "$wrapper_set" -eq 1 ]; then
                die '--wrapper was specified more than once'
            fi
            wrapper=${1#--wrapper=}
            wrapper_set=1
            shift
            ;;
        --)
            shift
            break
            ;;
        -*)
            die "unknown option before tool arguments: $1 (use -- before echo-deploy arguments)"
            ;;
        *)
            break
            ;;
    esac
done

if [ -z "$tool" ]; then
    die '--tool is required; pass the absolute path of a local echo-deploy executable'
fi
if [ -z "$sha" ]; then
    die '--sha256 is required; pass the expected digest of the local executable'
fi
if [ "$wrapper_set" -eq 1 ] && [ -z "$wrapper" ]; then
    die '--wrapper requires one of: install, update, repair, uninstall, compose-plan'
fi

case "$tool" in
    /*) ;;
    *) die "--tool must be an absolute path, got: $tool" ;;
esac
if [ -L "$tool" ]; then
    die "--tool must not be a symlink: $tool"
fi
if [ ! -f "$tool" ]; then
    die "--tool is not a regular file: $tool"
fi
if [ ! -x "$tool" ]; then
    die "--tool is not executable: $tool"
fi

case "$sha" in
    *[!0-9a-f]*)
        die '--sha256 must contain only lowercase hexadecimal digits'
        ;;
esac
if [ "${#sha}" -ne 64 ]; then
    die "--sha256 must be exactly 64 lowercase hexadecimal characters, got ${#sha}"
fi

if [ -n "$wrapper" ]; then
    case "$wrapper" in
        install|update|repair|uninstall|compose-plan) ;;
        *)
            die "--wrapper must be one of install, update, repair, uninstall, compose-plan; got: $wrapper"
            ;;
    esac
fi

if ! command -v sha256sum >/dev/null 2>&1; then
    die 'sha256sum is required to verify --sha256'
fi
if ! actual=$(sha256sum -- "$tool"); then
    die "failed to read a SHA-256 digest from $tool"
fi
actual=${actual%% *}
if [ "$actual" != "$sha" ]; then
    die "SHA-256 mismatch for $tool: expected $sha, got $actual"
fi

printf 'bootstrap.sh: verified %s (sha256 %s); exec\n' "$tool" "$sha" >&2
# Keep the verified path visible to child entry points without changing argv.
export ECHO_DEPLOY_TOOL="$tool"

if [ -n "$wrapper" ]; then
    script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]:-$0}")" && pwd -P)
    target=$script_dir/$wrapper.sh
    if [ -L "$target" ]; then
        die "wrapper must not be a symlink: $target"
    fi
    if [ ! -f "$target" ]; then
        die "wrapper is not a regular file: $target"
    fi
    if [ ! -x "$target" ]; then
        die "wrapper is not executable: $target"
    fi
    exec "$target" "$@"
fi

exec "$tool" "$@"
