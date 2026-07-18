#!/usr/bin/env bash
#
# dev.sh — dev mode: run this checkout's stella from any other workspace,
# side by side with the released `stella`, before baking a release.
#
#   Usage:  scripts/dev.sh [install|build|uninstall|status] [--debug]
#
#     install     build stella-cli and link ~/.local/bin/stella-dev at the
#                 built binary (default command)
#     build       rebuild only — the existing link picks the fresh binary up
#     uninstall   remove the stella-dev link
#     status      show what `stella` and `stella-dev` resolve to, and their
#                 versions
#
#     --debug     build the debug profile (much faster compiles, slower
#                 binary) instead of release
#
# How it works: the link points INTO this checkout's target/ directory, so
# the iteration loop after the first install is just an ordinary rebuild —
#
#   cd ~/Workspaces/stella && scripts/dev.sh build     # or: cargo build --release -p stella-cli
#   cd ~/some/other/repo   && stella-dev               # runs the fresh build
#
# Each build is stamped with the current git sha (STELLA_BUILD_GIT_SHA is
# baked in at compile time), so `stella-dev version` prints e.g.
# `stella v0.1.16-dev.3f2c9aa+dirty` and you always know exactly which
# checkout and commit you are testing. Release builds carry no stamp — the
# shipped binary's version string is untouched.
#
# Env overrides:
#   STELLA_DEV_INSTALL_DIR   link directory (default: $HOME/.local/bin)

set -euo pipefail

BIN_NAME="stella-dev"
CRATE="stella-cli"
INSTALL_DIR="${STELLA_DEV_INSTALL_DIR:-$HOME/.local/bin}"

# ── Output helpers (house style: scripts/release.sh) ────────────────────────
bold() { printf '\033[1m%s\033[0m\n' "$*"; }
info() { printf '\033[36m▸ %s\033[0m\n' "$*"; }
ok()   { printf '\033[32m✔ %s\033[0m\n' "$*"; }
die()  { printf '\033[31mERROR: %s\033[0m\n' "$*" >&2; exit 1; }

# ── Locate repo root (script lives in scripts/) ─────────────────────────────
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
[ -f "$ROOT/$CRATE/Cargo.toml" ] || die "run from the stella repo — $CRATE/Cargo.toml not found"

CMD="${1:-install}"
[ $# -gt 0 ] && shift

PROFILE="release"
for arg in "$@"; do
  case "$arg" in
    --debug) PROFILE="debug" ;;
    *) die "unknown option: $arg (try --debug)" ;;
  esac
done

BUILT_BIN="$ROOT/target/$PROFILE/stella"
LINK="$INSTALL_DIR/$BIN_NAME"

path_hint() {
  case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) : ;;
    *)
      info "note: ${INSTALL_DIR} is not on your PATH."
      info "add it, e.g.:  export PATH=\"${INSTALL_DIR}:\$PATH\""
      ;;
  esac
}

dev_stamp() {
  local sha dirty=""
  sha="$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  git -C "$ROOT" diff --quiet 2>/dev/null || dirty="+dirty"
  printf '%s%s' "$sha" "$dirty"
}

build() {
  command -v cargo >/dev/null || die "cargo/rustup not found — install Rust: https://rustup.rs"
  local stamp flags=()
  stamp="$(dev_stamp)"
  [ "$PROFILE" = "release" ] && flags+=(--release)
  info "building $CRATE ($PROFILE, $stamp)"
  STELLA_BUILD_GIT_SHA="$stamp" cargo build "${flags[@]+"${flags[@]}"}" -p "$CRATE"
  ok "built target/$PROFILE/stella ($stamp)"
}

link_bin() {
  mkdir -p "$INSTALL_DIR"
  ln -sfn "$BUILT_BIN" "$LINK"
  ok "linked $LINK → target/$PROFILE/stella"
  path_hint
  info "run \`$BIN_NAME\` from any workspace; \`$BIN_NAME version\` shows the dev stamp"
}

case "$CMD" in
  install)
    build
    link_bin
    ;;
  build)
    build
    if [ -L "$LINK" ] && [ "$(readlink "$LINK")" != "$BUILT_BIN" ]; then
      info "note: $LINK points at $(readlink "$LINK"), not the $PROFILE build —"
      info "re-run \`scripts/dev.sh install${PROFILE:+ $( [ "$PROFILE" = debug ] && echo --debug )}\` to switch it"
    fi
    ;;
  uninstall)
    if [ -L "$LINK" ] || [ -e "$LINK" ]; then
      rm -f "$LINK"
      ok "removed $LINK"
    else
      info "nothing to remove — $LINK does not exist"
    fi
    ;;
  status)
    bold "stella (released)"
    if command -v stella >/dev/null 2>&1; then
      info "$(command -v stella) — $(stella --version 2>/dev/null || echo 'version unavailable')"
    else
      info "not on PATH"
    fi
    bold "$BIN_NAME (dev)"
    if [ -L "$LINK" ]; then
      info "$LINK → $(readlink "$LINK")"
      if [ -x "$BUILT_BIN" ] || [ -x "$(readlink "$LINK")" ]; then
        info "version: $("$LINK" --version 2>/dev/null || echo 'not built yet — run scripts/dev.sh build')"
      else
        info "target missing — run scripts/dev.sh build"
      fi
    elif command -v "$BIN_NAME" >/dev/null 2>&1; then
      info "$(command -v "$BIN_NAME") (not managed by this script)"
    else
      info "not installed — run scripts/dev.sh install"
    fi
    ;;
  *)
    die "unknown command: $CMD (install|build|uninstall|status)"
    ;;
esac
