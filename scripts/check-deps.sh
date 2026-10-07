#!/usr/bin/env bash
# Dependency-direction lint: extension/provider crates must depend only on
# definition-layer crates (echo-defs, echo-context, echo-protocol), never on
# concrete providers or the agent framework. Run in CI; exit non-zero on
# violation.
#
# Rules:
#   - echo-llm-*        may depend on echo-defs (+ echo-llm-openai for ollama), never echo-agent/echo-adapter/echo-session/echo-loop
#   - echo-session      may depend on echo-defs only
#   - echo-loop         may depend on echo-defs/echo-context only
#   - echo-context      may depend on nothing from the harness
#   - echo-defs         may depend on nothing from the harness
#   - plugin/echo-plugin-api      zero harness deps (frozen contract)
#   - plugin/echo-plugin-sdk      may depend on echo-plugin-api only
#   - plugin/echo-plugin-host     may depend on echo-plugin-api (+ echo-defs for the Tool adapter)
#   - plugin/echo-plugin-loader   zero harness deps (pure config composition)
#   - plugin/echo-plugin-example  may depend on echo-plugin-sdk only
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FAIL=0

check() {
  local crate="$1"  # relative dir under source/
  local allowed="$2" # space-separated allowed harness deps (may be empty)
  local manifest="$ROOT/source/$crate/Cargo.toml"
  [ -f "$manifest" ] || return 0
  # Extract harness deps from the [dependencies] section. Keys at line start
  # only: comments and binary names inside the section range must not count.
  local deps
  deps=$(sed -n '/^\[dependencies\]/,/^\[/p' "$manifest" \
    | grep -oE '^echo-[a-z-]+' | sort -u || true)
  for dep in $deps; do
    if [ "$dep" != "echo-defs" ] && [ "$dep" != "echo-context" ] \
       && [ "$dep" != "echo-protocol" ] \
       && ! echo "$allowed" | grep -qw "$dep"; then
      echo "VIOLATION: $crate depends on $dep (not in definition layer: $allowed)" >&2
      FAIL=1
    fi
  done
}

check "defs/echo-defs" ""
check "context/echo-context" ""
check "session/echo-session" ""
check "loop/echo-loop" ""
check "llm/echo-llm-openai" ""
check "llm/echo-llm-anthropic" ""
check "llm/echo-llm-ollama" "echo-llm-openai"

# Plugin system layering (decoupling plan P0-P4).
check "plugin/echo-plugin-api" ""
check "plugin/echo-plugin-sdk" "echo-plugin-api"
check "plugin/echo-plugin-host" "echo-plugin-api echo-plugin-loader"
check "plugin/echo-plugin-loader" ""
check "plugin/echo-plugin-example" "echo-plugin-sdk"

if [ "$FAIL" -ne 0 ]; then
  echo "Dependency-direction lint FAILED" >&2
  exit 1
fi
echo "Dependency-direction lint OK"
