# Releasing Stella

Stella ships prebuilt binaries, a Homebrew formula, and a `curl | sh`
installer via [`dist`](https://axodotdev.github.io/cargo-dist) (cargo-dist).
Everything is driven by pushing a version tag — the
[`Release`](.github/workflows/release.yml) workflow does the rest.

The `dist` configuration lives in the root `Cargo.toml` under
`[workspace.metadata.dist]`; only the `stella` binary (from `stella-cli`) is
distributed — the workspace's fixture binaries opt out with
`[package.metadata.dist] dist = false`.

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

3. **Add it as a secret** on **this** repo (`oxageninc/stella-cli`):
   Settings → Secrets and variables → Actions → New repository secret →
   name `HOMEBREW_TAP_TOKEN`, value the token from step 2.

The prebuilt tarballs, checksums, and the `curl | sh` installer are published
to this repo's GitHub Releases and need no extra secrets — only the Homebrew
tap push does.

## Cut a release

1. Bump the version if needed — `version` in `[workspace.package]` of the root
   `Cargo.toml` (all crates inherit it) — and commit.

2. (Optional) Preview exactly what will be built without compiling anything:

   ```bash
   dist plan
   ```

3. Tag and push. The tag must be `v<major>.<minor>.<patch>` matching the
   workspace version:

   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```

That push starts the `Release` workflow, which:

- builds `stella` for macOS (`aarch64`, `x86_64`) and Linux (`aarch64`,
  `x86_64`),
- packs each as a `.tar.xz` with the licenses + README and a SHA-256 checksum,
- creates a GitHub Release with those artifacts, a `sha256.sum`, and the
  `stella-cli-installer.sh` shell installer,
- generates `Formula/stella.rb` and commits it to the Homebrew tap.

## After the release — how users install

Homebrew (prebuilt bottle-style formula, no compile):

```bash
brew install oxageninc/stella/stella
# equivalently: brew tap oxageninc/stella && brew install stella
```

Shell installer (macOS/Linux, no Homebrew):

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/oxageninc/stella-cli/releases/latest/download/stella-cli-installer.sh | sh
```

Upgrades are `brew upgrade stella` or re-running the installer.

## Updating the dist toolchain

The pinned `dist` version is `cargo-dist-version` in
`[workspace.metadata.dist]`. To move to a newer `dist`, install it and re-run
init so the workflow is regenerated against the new version:

```bash
brew upgrade cargo-dist            # or: cargo install cargo-dist
dist init --yes --hosting github   # regenerates .github/workflows/release.yml
```

Commit the regenerated workflow and the bumped `cargo-dist-version` together.
