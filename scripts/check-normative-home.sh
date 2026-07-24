#!/usr/bin/env bash
#
# Drift guard for the Context Graph Protocol (CGP) normative-home boundary.
# See context-graph-protocol#27 and CGP docs/adr/0007-protocol-product-boundary.md.
#
# The adaptive-context docs no longer restate frame/wire semantics — those live
# normatively in CGP. Each doc that references CGP as its normative home carries
#
#     <!-- NORMATIVE-HOME: macanderson/context-graph-protocol @ <sha> (...) -->
#
# This check keeps that pointer honest: the pinned <sha> must be the CGP revision
# stella actually builds against (the contextgraph-types git rev in
# stella-cli/Cargo.toml). If someone bumps the dependency without repinning the
# docs — or repins the docs without bumping the code — this fails loudly.
#
# Uses portable POSIX grep/sed so it runs on a bare CI runner.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

cargo_rev="$(grep -E 'contextgraph-(types|host|trace|conformance)' stella-cli/Cargo.toml 2>/dev/null \
  | grep -oE '[0-9a-f]{40}' | head -n1 || true)"
if [ -z "${cargo_rev:-}" ]; then
  echo "check-normative-home: no contextgraph-* git rev in stella-cli/Cargo.toml; skipping."
  exit 0
fi

status=0
count=0
while IFS= read -r file; do
  [ -n "$file" ] || continue
  count=$((count + 1))
  pin="$(grep -oE 'NORMATIVE-HOME:[^>]*@[[:space:]]*[0-9a-f]{7,40}' "$file" \
    | grep -oE '[0-9a-f]{7,40}' | head -n1 || true)"
  if [ -z "$pin" ]; then
    echo "FAIL $file: NORMATIVE-HOME header present but no '@ <sha>' pin found." >&2
    status=1
    continue
  fi
  case "$cargo_rev" in
    "$pin"*) echo "ok   $file  (pin $pin is a prefix of $cargo_rev)" ;;
    *) echo "FAIL $file: pins CGP @ $pin but stella builds against $cargo_rev." >&2
       status=1 ;;
  esac
done <<EOF
$(grep -rlE 'NORMATIVE-HOME:' docs 2>/dev/null || true)
EOF

if [ "$count" -eq 0 ]; then
  echo "check-normative-home: no NORMATIVE-HOME docs found under docs/; nothing to check."
fi
exit "$status"
