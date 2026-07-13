#!/usr/bin/env bash
#
# release.sh — cut a complete stella release from your local machine.
#
# Builds all four target binaries locally, publishes a GitHub Release with the
# tarballs + SHA256SUMS, and pushes the matching Homebrew formula to the tap —
# no CI required (handy while org Actions is unavailable). Everything the org
# release workflow does, done from your Mac.
#
#   Usage:  scripts/release.sh [patch|minor|major]     (default: patch)
#
#     patch   0.1.15 -> 0.1.16   (default)
#     minor   0.1.15 -> 0.2.0
#     major   0.1.15 -> 1.0.0
#
# Requirements (checked up front; zig/cargo-zigbuild/targets are auto-installed):
#   - macOS + Homebrew + Rust (rustup) + gh (run `gh auth login` first)
#   - write access to the release repo and the Homebrew tap
#
# Safety: refuses to run unless your checkout is clean and exactly matches
# origin/main, and it never leaves your working tree modified (the version
# stamp is reverted on exit).
#
set -euo pipefail

# ── Config ──────────────────────────────────────────────────────────────────
REPO="oxageninc/stella"
TAP_REPO="oxageninc/homebrew-stella"
BIN="stella"
CRATE="stella-cli"
MAC_TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)
LINUX_TARGETS=(aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu)
GLIBC="2.17"   # build Linux against an old glibc so the binaries run broadly
TMPL=".github/homebrew/stella.rb.tmpl"

BUMP="${1:-patch}"

# ── Output helpers ──────────────────────────────────────────────────────────
bold() { printf '\033[1m%s\033[0m\n' "$*"; }
info() { printf '\033[36m▸ %s\033[0m\n' "$*"; }
ok()   { printf '\033[32m✔ %s\033[0m\n' "$*"; }
die()  { printf '\033[31mERROR: %s\033[0m\n' "$*" >&2; exit 1; }

case "$BUMP" in patch|minor|major) ;; *) die "bump must be patch|minor|major (got: $BUMP)";; esac

# ── Locate repo root (script lives in scripts/) ─────────────────────────────
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
[ -f "$TMPL" ] || die "run from the stella repo — $TMPL not found"

# ── Preflight: tooling ──────────────────────────────────────────────────────
info "checking tooling"
command -v cargo >/dev/null || die "cargo/rustup not found — install Rust: https://rustup.rs"
command -v gh    >/dev/null || die "gh not found — brew install gh"
command -v brew  >/dev/null || die "Homebrew not found — https://brew.sh"
gh auth status  >/dev/null 2>&1 || die "gh not authenticated — run: gh auth login"
command -v zig            >/dev/null || { info "installing zig";           brew install zig >/dev/null; }
command -v cargo-zigbuild >/dev/null || { info "installing cargo-zigbuild"; cargo install cargo-zigbuild --locked >/dev/null; }
for t in "${MAC_TARGETS[@]}" "${LINUX_TARGETS[@]}"; do
  rustup target list --installed 2>/dev/null | grep -qx "$t" || { info "adding target $t"; rustup target add "$t" >/dev/null; }
done
ok "tooling ready"

# ── Preflight: release exactly what's on origin/main, from a clean tree ──────
# Normally the checkout must be clean and identical to origin/main. Escape
# hatches for releasing from a prepared branch (e.g. release infra that isn't
# on main yet, provably not touching crate code):
#   ALLOW_NONMAIN=1     skip the "HEAD == origin/main" check (clean tree still required)
#   RELEASE_TARGET=ref  commit-ish the release tag points at (default: HEAD)
info "checking git state"
[ -z "$(git status --porcelain)" ] || die "working tree is dirty — commit or stash first"
git fetch origin main --tags --quiet
head_sha="$(git rev-parse HEAD)"
main_sha="$(git rev-parse origin/main)"
if [ "$head_sha" != "$main_sha" ]; then
  [ "${ALLOW_NONMAIN:-}" = "1" ] || die "HEAD is not origin/main ($(git rev-parse --short HEAD) vs $(git rev-parse --short origin/main)) — run: git checkout main && git pull  (or set ALLOW_NONMAIN=1)"
  info "ALLOW_NONMAIN=1 — releasing from $(git rev-parse --short HEAD) (not origin/main)"
fi
target_sha="$(git rev-parse "${RELEASE_TARGET:-HEAD}")"
ok "checkout clean; tagging at $(git rev-parse --short "$target_sha")"

# ── Compute next version from the newest v-tag ──────────────────────────────
last="$(git tag -l 'v*' --sort=-v:refname | head -n1 || true)"
base="${last#v}"; base="${base:-0.0.0}"
IFS=. read -r MAJ MIN PAT <<< "$base"; MAJ=${MAJ:-0}; MIN=${MIN:-0}; PAT=${PAT:-0}
case "$BUMP" in
  major) MAJ=$((MAJ+1)); MIN=0; PAT=0 ;;
  minor) MIN=$((MIN+1)); PAT=0 ;;
  patch) PAT=$((PAT+1)) ;;
esac
VERSION="${MAJ}.${MIN}.${PAT}"; TAG="v${VERSION}"
git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null && die "tag ${TAG} already exists"
gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1 && die "release ${TAG} already exists on ${REPO}"
bold ""
bold "Releasing ${TAG}  (${BUMP} bump from ${last:-<none>})"
bold ""

# ── Stamp the workspace version, guaranteed reverted on exit ────────────────
# The one `^version = ` line is [workspace.package].version. (The CI workflow's
# `sed "0,/re/"` is GNU-only and silently no-ops on macOS — perl is portable.)
cp Cargo.toml .Cargo.toml.relbak
restore_manifest() { [ -f .Cargo.toml.relbak ] && mv .Cargo.toml.relbak Cargo.toml || true; }
trap 'restore_manifest' EXIT
perl -pi -e "s/^version = \"[^\"]*\"/version = \"${VERSION}\"/" Cargo.toml
grep -m1 '^version = ' Cargo.toml | grep -q "\"${VERSION}\"" || die "version stamp failed"
ok "stamped workspace version ${VERSION}"

# ── Build + package every target ────────────────────────────────────────────
DIST="$(mktemp -d)/dist"; mkdir -p "$DIST"
package() {  # <target-triple>
  local tgt="$1" stem="${BIN}-${VERSION}-$1"
  mkdir -p "$DIST/$stem"
  cp "target/${tgt}/release/${BIN}" "$DIST/$stem/${BIN}"
  cp LICENSE-MIT LICENSE-APACHE README.md "$DIST/$stem/"
  tar -C "$DIST" -czf "$DIST/${stem}.tar.gz" "$stem"
  rm -rf "$DIST/${stem:?}"
}
for t in "${MAC_TARGETS[@]}"; do
  info "building $t (native)"
  cargo build --release --target "$t" --package "$CRATE" --bin "$BIN"
  package "$t"
done
for t in "${LINUX_TARGETS[@]}"; do
  info "building $t (zig cross-compile, glibc ${GLIBC})"
  cargo zigbuild --release --target "${t}.${GLIBC}" --package "$CRATE" --bin "$BIN"
  package "$t"
done
restore_manifest; trap - EXIT   # manifest back to pristine now that builds are done
ok "built + packaged 4 targets"

# ── Checksums + version sanity check on the native binary ───────────────────
( cd "$DIST" && shasum -a 256 ${BIN}-${VERSION}-*.tar.gz > SHA256SUMS )
native="target/$(rustc -vV | sed -n 's/host: //p')/release/${BIN}"
if [ -x "$native" ]; then
  "$native" --version 2>/dev/null | grep -q "${VERSION}" || die "built binary reports the wrong version (expected ${VERSION}) — aborting before publish"
  ok "binary reports ${BIN} ${VERSION}"
fi
sha_of() { awk -v f="${BIN}-${VERSION}-$1.tar.gz" '$2==f{print $1}' "$DIST/SHA256SUMS"; }

# ── Release notes: commits since the previous tag ───────────────────────────
notes="$(mktemp)"
{
  printf 'Release %s.\n\n## Changes since %s\n\n' "$VERSION" "${last:-the beginning}"
  git log --no-merges --pretty='- %s' ${last:+${last}..HEAD} | head -n 100
  [ -n "$last" ] && printf '\n**Full changelog**: https://github.com/%s/compare/%s...%s\n' "$REPO" "$last" "$TAG"
} > "$notes"

# ── Publish the GitHub Release (all assets in ONE call → immutable-safe) ─────
info "creating GitHub Release ${TAG}"
gh release create "$TAG" --repo "$REPO" --target "$target_sha" \
  --title "$TAG" --notes-file "$notes" \
  "$DIST"/${BIN}-${VERSION}-*.tar.gz "$DIST/SHA256SUMS"
ok "release: https://github.com/${REPO}/releases/tag/${TAG}"

# ── Render + push the Homebrew formula ──────────────────────────────────────
info "rendering + pushing Homebrew formula"
rendered="$(mktemp)"
sed \
  -e "s/@VERSION@/${VERSION}/g" \
  -e "s/@SHA_AARCH64_DARWIN@/$(sha_of aarch64-apple-darwin)/g" \
  -e "s/@SHA_X86_64_DARWIN@/$(sha_of x86_64-apple-darwin)/g" \
  -e "s/@SHA_AARCH64_LINUX@/$(sha_of aarch64-unknown-linux-gnu)/g" \
  -e "s/@SHA_X86_64_LINUX@/$(sha_of x86_64-unknown-linux-gnu)/g" \
  "$TMPL" > "$rendered"
grep -q '@SHA\|@VERSION' "$rendered" && die "formula still has unrendered placeholders"

tap="$(mktemp -d)/tap"
gh repo clone "$TAP_REPO" "$tap" -- --depth 1 --quiet
mkdir -p "$tap/Formula"; cp "$rendered" "$tap/Formula/${BIN}.rb"
git -C "$tap" add "Formula/${BIN}.rb"
if git -C "$tap" diff --cached --quiet; then
  ok "tap already current for ${VERSION}"
else
  git -C "$tap" commit --quiet -m "${BIN} ${VERSION}"
  git -C "$tap" push --quiet origin HEAD
  ok "formula pushed to ${TAP_REPO}"
fi

# ── Verify via Homebrew (fetch = download + checksum, no install) ───────────
info "verifying via Homebrew"
brew tap "$REPO" >/dev/null 2>&1 || true
brew update-reset "$(brew --repo "$REPO")" >/dev/null 2>&1 || true
if brew fetch "${REPO}/${BIN}" >/dev/null 2>&1; then
  ok "brew fetch + checksum verified for ${VERSION}"
else
  info "brew couldn't verify yet (release assets may still be propagating) — retry: brew fetch ${REPO}/${BIN}"
fi

bold ""
ok "Released ${TAG}"
printf '   install:  brew install %s/%s\n' "$REPO" "$BIN"
printf '   upgrade:  brew upgrade %s\n' "$BIN"
