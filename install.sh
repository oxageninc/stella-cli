#!/bin/sh
# Stella CLI installer.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/oxageninc/stella/main/install.sh | sh
#
# Environment overrides:
#   STELLA_VERSION      install a specific version (e.g. "0.1.0" or "v0.1.0")
#                       instead of the latest release.
#   STELLA_INSTALL_DIR  install directory (default: $HOME/.local/bin).
#
# Behavior: detects your OS/arch, downloads the matching prebuilt tarball from
# the GitHub Release, verifies its SHA-256 against SHA256SUMS, and installs the
# `stella` binary. If no prebuilt binary matches your platform, it falls back to
# `cargo install`.
#
# POSIX sh — no bashisms.

set -eu

REPO="oxageninc/stella"
BIN="stella"
INSTALL_DIR="${STELLA_INSTALL_DIR:-$HOME/.local/bin}"
API="https://api.github.com/repos/${REPO}"
DOWNLOAD_BASE="https://github.com/${REPO}/releases/download"

# ---- logging -------------------------------------------------------------

info() { printf 'stella-install: %s\n' "$1" >&2; }
err() { printf 'stella-install: error: %s\n' "$1" >&2; }
die() {
  err "$1"
  exit 1
}

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    die "required command not found: $1"
  fi
}

# ---- platform detection --------------------------------------------------

detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Darwin) os_part="apple-darwin" ;;
    Linux) os_part="unknown-linux-gnu" ;;
    *)
      TARGET=""
      return 0
      ;;
  esac

  case "$arch" in
    x86_64 | amd64) arch_part="x86_64" ;;
    arm64 | aarch64) arch_part="aarch64" ;;
    *)
      TARGET=""
      return 0
      ;;
  esac

  TARGET="${arch_part}-${os_part}"
}

# Targets for which we publish prebuilt tarballs (keep in sync with
# .github/workflows/release.yml and [workspace.metadata.dist] in Cargo.toml).
is_supported_target() {
  case "$1" in
    aarch64-apple-darwin | x86_64-apple-darwin | \
      x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

# ---- version resolution --------------------------------------------------

# Resolve the release tag (e.g. "v0.1.0"). Honors STELLA_VERSION, otherwise
# queries the GitHub "latest release" API.
resolve_tag() {
  if [ -n "${STELLA_VERSION:-}" ]; then
    case "$STELLA_VERSION" in
      v*) TAG="$STELLA_VERSION" ;;
      *) TAG="v${STELLA_VERSION}" ;;
    esac
    return 0
  fi

  body="$(curl -fsSL "${API}/releases/latest")" ||
    die "could not query latest release from GitHub API"

  # Extract the first "tag_name": "..." value without requiring jq.
  TAG="$(printf '%s\n' "$body" |
    sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
    head -n1)"

  [ -n "$TAG" ] || die "could not determine latest release tag"
}

# ---- checksum ------------------------------------------------------------

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    die "no sha256 tool found (need sha256sum or shasum)"
  fi
}

# ---- cargo fallback ------------------------------------------------------

cargo_fallback() {
  info "$1"
  if ! command -v cargo >/dev/null 2>&1; then
    die "cargo not found; install Rust from https://rustup.rs then re-run"
  fi
  info "installing from source with cargo (this may take a while)..."
  cargo install --locked --git "https://github.com/${REPO}" stella-cli
  info "done. Ensure Cargo's bin dir (usually \$HOME/.cargo/bin) is on your PATH."
  exit 0
}

# ---- PATH hint -----------------------------------------------------------

path_hint() {
  case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) : ;;
    *)
      info "note: ${INSTALL_DIR} is not on your PATH."
      info "add it, e.g.:  export PATH=\"${INSTALL_DIR}:\$PATH\""
      ;;
  esac
}

# ---- main ----------------------------------------------------------------

main() {
  need_cmd uname
  need_cmd curl
  need_cmd tar
  need_cmd mkdir
  need_cmd mktemp
  need_cmd awk

  detect_target
  if [ -z "$TARGET" ] || ! is_supported_target "$TARGET"; then
    cargo_fallback "no prebuilt binary for this platform ($(uname -s) $(uname -m)); falling back to cargo."
  fi

  resolve_tag
  version="${TAG#v}"

  asset="stella-${version}-${TARGET}.tar.gz"
  asset_url="${DOWNLOAD_BASE}/${TAG}/${asset}"
  sums_url="${DOWNLOAD_BASE}/${TAG}/SHA256SUMS"

  info "installing stella ${version} (${TARGET})"

  tmpdir="$(mktemp -d 2>/dev/null || mktemp -d -t stella)"
  trap 'rm -rf "$tmpdir"' EXIT INT TERM

  info "downloading ${asset}"
  if ! curl -fsSL "$asset_url" -o "${tmpdir}/${asset}"; then
    die "download failed: ${asset_url}"
  fi

  info "downloading checksums"
  if ! curl -fsSL "$sums_url" -o "${tmpdir}/SHA256SUMS"; then
    die "download failed: ${sums_url}"
  fi

  # Verify SHA-256 against the published SHA256SUMS.
  expected="$(awk -v f="$asset" '$2 == f {print $1}' "${tmpdir}/SHA256SUMS")"
  [ -n "$expected" ] || die "no checksum for ${asset} in SHA256SUMS"
  actual="$(sha256_of "${tmpdir}/${asset}")"
  if [ "$expected" != "$actual" ]; then
    die "checksum mismatch for ${asset}: expected ${expected}, got ${actual}"
  fi
  info "checksum ok"

  # Extract. The tarball contains a top-level "stella-<version>-<target>/" dir.
  tar -C "$tmpdir" -xzf "${tmpdir}/${asset}"
  src="${tmpdir}/stella-${version}-${TARGET}/${BIN}"
  [ -f "$src" ] || die "binary not found in archive: ${src}"

  mkdir -p "$INSTALL_DIR"
  install_path="${INSTALL_DIR}/${BIN}"
  cp "$src" "$install_path"
  chmod +x "$install_path"

  info "installed stella to ${install_path}"
  path_hint
  info "run 'stella --version' to verify."
}

main "$@"
