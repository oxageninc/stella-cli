# Releasing Stella

Stella ships prebuilt binaries, a Homebrew formula, and a `curl | sh`
installer. Everything is driven by pushing a version tag — the
[`Release`](.github/workflows/release.yml) workflow does the rest.

The workflow is a **hand-rolled build matrix** (it does not require cargo-dist
on the runner). The `[workspace.metadata.dist]` block in the root `Cargo.toml`
is retained for a possible future migration to
[`dist`](https://axodotdev.github.io/cargo-dist) but is **not** the active
pipeline today; the source of truth is `.github/workflows/release.yml`.

## One-time setup

The tag-triggered workflow publishes the Homebrew formula to a **tap repo**.
This has to exist and be writable before the first release:

1. **Create the tap repo.** A public repo named exactly
   [`oxageninc/homebrew-stella`](https://github.com/oxageninc/homebrew-stella)
   (Homebrew maps the tap `oxageninc/stella` → repo `homebrew-stella`). It can
   start empty; the release job commits `Formula/stella.rb` into it.

2. **Create a push token.** A GitHub token with **contents: write** on the tap
   repo — a fine-grained PAT scoped to `oxageninc/homebrew-stella`, or a classic
   PAT with `repo`. The default `GITHUB_TOKEN` can't push to another repo, so a
   dedicated one is required.

3. **Add it as a secret** on **this** repo (`oxageninc/stella`):
   Settings → Secrets and variables → Actions → New repository secret →
   name `HOMEBREW_TAP_TOKEN`, value the token from step 2.

The prebuilt tarballs, checksums, and the `curl | sh` installer are published
to this repo's GitHub Releases and need no extra secrets — only the Homebrew
tap push does. If `HOMEBREW_TAP_TOKEN` is absent, the release still succeeds and
the `homebrew` job skips with a warning.

## Cut a release

1. Bump the version if needed — `version` in `[workspace.package]` of the root
   `Cargo.toml` (all crates inherit it) — and commit.

2. Tag and push. The tag must be `v<major>.<minor>.<patch>` matching the
   workspace version:

   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```

That push starts the `Release` workflow, which:

- builds `stella` for macOS (`aarch64`, `x86_64`) and Linux (`aarch64`,
  `x86_64`),
- packs each as a `stella-<version>-<target>.tar.gz` with the licenses +
  README,
- creates a GitHub Release with those tarballs and a `SHA256SUMS`,
- renders `Formula/stella.rb` from `.github/homebrew/stella.rb.tmpl` (real
  version + per-target SHA-256 sums) and commits it to the Homebrew tap
  (skipped if `HOMEBREW_TAP_TOKEN` is not configured).

## Cut a release locally (no CI)

When GitHub Actions is unavailable (e.g. an org billing hold) or you just want
full local control, [`scripts/release.sh`](scripts/release.sh) does the entire
pipeline from your Mac — build all four targets, publish the GitHub Release, and
push the Homebrew formula — with the version auto-incremented:

```bash
git checkout main && git pull        # release exactly what's on origin/main
scripts/release.sh patch             # 0.1.15 -> 0.1.16  (also: minor, major)
```

It refuses to run unless your checkout is clean and matches `origin/main`, and
it never leaves your tree modified. macOS targets build natively; the Linux
targets cross-compile via [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild)
(Zig as the C/C++ cross-linker — no Docker, and it compiles DuckDB's bundled
C++ cleanly, unlike the old `cross` image). `zig` and `cargo-zigbuild` are
auto-installed if missing. All release assets are uploaded in one call, which
matters because this repo has **immutable releases** enabled — a published
release's assets can't be added or changed afterward, so an incomplete release
means cutting a new version.

## After the release — how users install

Homebrew (prebuilt binary, no Rust toolchain):

```bash
brew install oxageninc/stella/stella
# equivalently: brew tap oxageninc/stella && brew install stella
```

Shell installer (macOS/Linux, no Homebrew):

```bash
curl -fsSL https://raw.githubusercontent.com/oxageninc/stella/main/install.sh | sh
```

The installer detects the platform, downloads the matching tarball from the
GitHub Release, verifies it against `SHA256SUMS`, and falls back to
`cargo install` where no prebuilt binary matches. Upgrades are
`brew upgrade stella` or re-running the installer.

## Formula templates

Two formulas live in the repo, for two different purposes:

- `.github/homebrew/stella.rb.tmpl` — the **template** the release workflow
  renders and pushes to the tap as `Formula/stella.rb`. Installs the prebuilt
  binary per platform. Edit this to change what lands in the tap.
- `packaging/homebrew/stella.rb` — a **build-from-source** formula for local
  use (`brew install --build-from-source ./packaging/homebrew/stella.rb`); it
  compiles with cargo and needs no per-release sha maintenance.
