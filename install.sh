#!/bin/sh
# Stella CLI installer.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/macanderson/stella/main/install.sh | sh
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

REPO="macanderson/stella"
BIN="stella"
INSTALL_DIR="${STELLA_INSTALL_DIR:-$HOME/.local/bin}"
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

# The prebuilt Linux tarballs are glibc (`-gnu`). On musl (Alpine) a glibc
# binary passes the checksum and "installs", then dies at exec with the loader's
# cryptic "no such file or directory". Detect musl and build from source instead.
is_musl() {
  [ "$(uname -s)" = "Linux" ] || return 1
  if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
    return 0
  fi
  # ldd is often absent on musl systems; fall back to probing for its loader.
  [ -e /lib/ld-musl-x86_64.so.1 ] || [ -e /lib/ld-musl-aarch64.so.1 ]
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

  # Resolve "latest" via the release redirect rather than the JSON API: the
  # unauthenticated API is rate-limited to 60 req/hr/IP (CI farms and office
  # NATs hit it and the install dies), and this avoids scraping JSON.
  # /releases/latest 302-redirects to /releases/tag/<TAG>.
  effective="$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
    "https://github.com/${REPO}/releases/latest")" ||
    die "could not resolve the latest release"
  TAG="${effective##*/tag/}"
  case "$TAG" in
    v*) : ;;
    *) die "could not determine latest release tag (resolved to '${effective}')" ;;
  esac
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

  # Pin the source build to STELLA_VERSION when set, so the fallback path honors
  # the requested version instead of silently building whatever `main` happens
  # to be (which may not even match the version the user asked for).
  ref_args=""
  if [ -n "${STELLA_VERSION:-}" ]; then
    case "$STELLA_VERSION" in
      v*) tag="$STELLA_VERSION" ;;
      *) tag="v${STELLA_VERSION}" ;;
    esac
    ref_args="--tag ${tag}"
    info "building stella ${tag} from source with cargo (this may take a while)..."
  else
    info "building stella from source with cargo (latest main; this may take a while)..."
  fi

  # Honor STELLA_INSTALL_DIR when it looks like a .../bin directory (the default
  # is ~/.local/bin): cargo installs the binary into <root>/bin.
  root_args=""
  case "$INSTALL_DIR" in
    */bin) root_args="--root ${INSTALL_DIR%/bin}" ;;
  esac

  # shellcheck disable=SC2086 # word-splitting of the optional arg groups is intended
  cargo install --locked ${ref_args} ${root_args} --git "https://github.com/${REPO}" stella-cli
  if [ -n "$root_args" ]; then
    info "installed stella to ${INSTALL_DIR}."
  else
    info "done. Ensure Cargo's bin dir (usually \$HOME/.cargo/bin) is on your PATH."
  fi
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
  if is_musl; then
    cargo_fallback "musl libc detected; prebuilt binaries are glibc-only — building from source."
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
  # Install atomically: copy to a temp file in the same dir, make it executable,
  # then rename over the target. A plain `cp` over the destination truncates it
  # in place — fatal if the old binary is running (ETXTBSY / a half-written exec
  # left in PATH if the copy is interrupted). `mv` within a filesystem is atomic.
  tmp_bin="${INSTALL_DIR}/.${BIN}.tmp.$$"
  cp "$src" "$tmp_bin"
  chmod +x "$tmp_bin"
  mv -f "$tmp_bin" "$install_path"

  info "installed stella to ${install_path}"
  path_hint
  info "run 'stella --version' to verify."
}

main "$@"
