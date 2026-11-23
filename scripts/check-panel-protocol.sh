#!/usr/bin/env bash
# Compare a fresh Core binding against the binding committed in the pinned Panel.
set -euo pipefail

if [[ $# -ne 2 ]]; then
    printf 'usage: bash %s <core-generated.ts> <pinned-panel.ts>\n' "$0" >&2
    exit 2
fi

core_binding=$1
panel_binding=$2
for binding in "$core_binding" "$panel_binding"; do
    if [[ ! -f "$binding" || ! -r "$binding" || ! -s "$binding" ]]; then
        printf 'error: protocol binding is missing, unreadable, or empty: %s\n' "$binding" >&2
        exit 1
    fi
done

if cmp -s -- "$core_binding" "$panel_binding"; then
    printf 'Core and pinned Panel protocol bindings match.\n'
else
    printf 'error: Core and pinned Panel protocol bindings differ; update the Panel revision in Cargo.lock to a verified match.\n' >&2
    sha256sum -- "$core_binding" "$panel_binding" >&2
    exit 1
fi
