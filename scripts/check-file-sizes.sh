#!/bin/sh
# Stella file-size ratchet — merge accidents cluster in giant files, so file
# size is gated like fmt and clippy.
#
# Usage:
#   scripts/check-file-sizes.sh        (any cwd; also wired as `make sizes`,
#                                       part of `make gate` and CI)
#
# Two rules over every git-tracked *.rs file:
#
#   1. A file NOT listed in scripts/file-size-ratchet.txt may not exceed
#      MAX_NEW_LINES lines — new modules start small and stay small.
#   2. A file listed in the allowlist (the legacy giants, recorded with the
#      line count they had when their entry landed) may not GROW beyond its
#      recorded count + GROW_SLACK lines. Shrinking is always allowed — and
#      encouraged: when you shrink an allowlisted file, lower its recorded
#      count in the same PR so the ratchet locks the gain in. When a file
#      drops to MAX_NEW_LINES or fewer, its allowlist entry can simply be
#      removed. Never raise a recorded count — split the file instead.
#
# Allowlist format (scripts/file-size-ratchet.txt): one entry per line,
# `<repo-relative path> <line count>`; `#` comments and blank lines ignored.
#
# POSIX sh — no bashisms.

set -eu

MAX_NEW_LINES=1500
GROW_SLACK=50
ALLOWLIST="scripts/file-size-ratchet.txt"

# ---- logging -------------------------------------------------------------

info() { printf 'check-file-sizes: %s\n' "$1" >&2; }
err() { printf 'check-file-sizes: error: %s\n' "$1" >&2; }
die() {
  err "$1"
  exit 1
}

# ---- main ----------------------------------------------------------------

cd "$(dirname "$0")/.."

command -v git >/dev/null 2>&1 || die "required command not found: git"
[ -f "$ALLOWLIST" ] || die "allowlist not found: $ALLOWLIST"

# The while loop runs in a pipeline subshell, so violations are collected on
# stdout instead of in a variable.
violations=$(
  git ls-files -- '*.rs' | while IFS= read -r file; do
    lines=$(($(wc -l <"$file") + 0))
    recorded=$(awk -v f="$file" '$1 == f { print $2; exit }' "$ALLOWLIST")
    if [ -n "$recorded" ]; then
      limit=$((recorded + GROW_SLACK))
      if [ "$lines" -gt "$limit" ]; then
        printf '%s: %d lines, limit %d (allowlisted at %d + %d slack) — shrink it back or split it; never raise the allowlist entry\n' \
          "$file" "$lines" "$limit" "$recorded" "$GROW_SLACK"
      fi
    elif [ "$lines" -gt "$MAX_NEW_LINES" ]; then
      printf '%s: %d lines, limit %d (files outside the allowlist stay at or under %d lines) — split it into modules\n' \
        "$file" "$lines" "$MAX_NEW_LINES" "$MAX_NEW_LINES"
    fi
  done
)

if [ -n "$violations" ]; then
  printf '%s\n' "$violations" >&2
  die "file-size ratchet failed (see above)"
fi

info "OK — $(git ls-files -- '*.rs' | wc -l | tr -d ' ') tracked .rs files within limits"
