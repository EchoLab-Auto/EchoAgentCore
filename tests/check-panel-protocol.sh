#!/usr/bin/env bash
# Test the release comparator without fetching either repo or building Core.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
check="$root/scripts/check-panel-protocol.sh"
workspace=$(mktemp -d)
trap 'rm -r -- "$workspace"' EXIT

core="$workspace/core.ts"
panel="$workspace/pinned panel.ts"
printf 'export type Sample = "request";\n' > "$core"
cp -- "$core" "$panel"
bash "$check" "$core" "$panel"

expect_failure() {
    local message=$1
    shift
    local output code=0
    output=$(bash "$check" "$@" 2>&1) || code=$?
    if [[ $code -ne 1 || "$output" != *"$message"* ]]; then
        printf 'Expected rejection (%s), got exit %s: %s\n' "$message" "$code" "$output" >&2
        return 1
    fi
}

printf 'export type Sample = "different";\n' > "$panel"
expect_failure 'bindings differ' "$core" "$panel"
expect_failure 'binding is missing, unreadable, or empty' "$core" "$workspace/missing.ts"
: > "$panel"
expect_failure 'binding is missing, unreadable, or empty' "$core" "$panel"
printf 'Protocol pair comparator: all four scenarios passed.\n'
