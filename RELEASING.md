# Releasing Stella

Stella ships prebuilt binaries, a Homebrew formula, and a `curl | sh`
installer. Everything is driven by pushing a version tag — the
[`Release`](.github/workflows/release.yml) workflow does the rest.

The workflow is a **hand-rolled build matrix** (it does not require cargo-dist
on the runner). The `[workspace.metadata.dist]` block in the root `Cargo.toml`
is retained for a possible future migration to
[`dist`](https://axodotdev.github.io/cargo-dist) but is **not** the active
pipeline today; the source of truth is `.github/workflows/release.yml`.

## The default path: every merge to main is a release

[`auto-tag.yml`](.github/workflows/auto-tag.yml) runs after `ci` goes green on
main and does everything — no manual steps:

1. **Tags** the merge commit with the next version — `+1 patch` by default;
   start the PR title with `release:minor` / `release:major` for bigger bumps,
   or include `[skip release]` to land without releasing.
2. **Dispatches** `release.yml` at the tag (binaries, GitHub Release, tap
   formula — the tag's version is stamped into the build, so
   `stella --version` always matches the tag).
3. **Writes the version back to main** so the Cargo manifests stay in sync
   with the newest tag: a `bot/version-sync` PR bumps
   `[workspace.package].version` in `Cargo.toml` (every crate inherits it),
   the workspace-member entries in `Cargo.lock`, and
   `packaging/homebrew/stella.rb`, then auto-merges once the required checks
   pass. Its commit carries `[skip release]`, so the sync itself never cuts a
   release. If a sync PR is ever left open (red check, race with another
   merge), the next release supersedes it automatically — no cleanup needed.

Manual version bumps are therefore only needed for the hand-cut flows below.

## One-time setup

The tag-triggered workflow publishes the Homebrew formula to a **tap repo**.
This has to exist and be writable before the first release:

1. **Create the tap repo.** A public repo named exactly
   [`macanderson/homebrew-tap`](https://github.com/macanderson/homebrew-tap)
   (Homebrew maps the tap `macanderson/tap` → repo `homebrew-tap`). It can
   start empty; the release job commits `Formula/stella.rb` into it.

2. **Create write access, either way** (the release job tries the deploy key
   first, falling back to the token — see `.github/workflows/release.yml`'s
   `homebrew` job):
   - **SSH deploy key (what's actually configured today)** — generate a
     dedicated keypair, add the public half as a **write-enabled deploy key**
     on `macanderson/homebrew-tap` (repo Settings → Deploy keys), and the
     private half as the `HOMEBREW_TAP_DEPLOY_KEY` secret below. Scoped to
     exactly that one repo, unlike a PAT.
   - **PAT (fallback)** — a GitHub token with **contents: write** on the tap
     repo — a fine-grained PAT scoped to `macanderson/homebrew-tap`, or a
     classic PAT with `repo`. The default `GITHUB_TOKEN` can't push to
     another repo, so a dedicated one is required either way.

3. **Add it as a secret** on **this** repo (`macanderson/stella`):
   Settings → Secrets and variables → Actions → New repository secret →
   name `HOMEBREW_TAP_DEPLOY_KEY` (deploy key) or `HOMEBREW_TAP_TOKEN` (PAT).

The prebuilt tarballs, checksums, and the `curl | sh` installer are published
to this repo's GitHub Releases and need no extra secrets — only the Homebrew
tap push does. If neither secret is set, the release still succeeds and
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
(Zig as the C/C++ cross-linker — no Docker, and it compiles the workspace's
bundled C deps (SQLite, tree-sitter grammars) cleanly, unlike the old `cross`
image). `zig` and `cargo-zigbuild` are
auto-installed if missing. All release assets are uploaded in one call, which
matters because this repo has **immutable releases** enabled — a published
release's assets can't be added or changed afterward, so an incomplete release
means cutting a new version.

## After the release — how users install

Homebrew (prebuilt binary, no Rust toolchain):

```bash
brew install macanderson/tap/stella
# equivalently: brew tap macanderson/tap && brew install stella
```

Shell installer (macOS/Linux, no Homebrew):

```bash
curl -fsSL https://raw.githubusercontent.com/macanderson/stella/main/install.sh | sh
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
