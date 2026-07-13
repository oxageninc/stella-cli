# Publishing the OCP crates to crates.io

This documents the release process for the three **Open Context Protocol**
crates — `ocp-types`, `ocp-host`, `ocp-conformance` — to crates.io. It is
distinct from [`RELEASING.md`](./RELEASING.md), which covers the `stella`
binary's GitHub Releases / Homebrew pipeline; these crates are published
independently, on their own cadence, and are not part of that workflow.

**Nobody has run these publish commands yet.** The workspace default is
`publish = false`; the three OCP crates override it explicitly (see their
`Cargo.toml`s). This file exists so the *first* real publish is a checklist,
not an improvisation.

## Why the order matters

```
ocp-types  →  ocp-host  →  ocp-conformance
```

`ocp-host` depends on `ocp-types` via `{ path = "../ocp-types", version =
"0.1.0" }`; `ocp-conformance` depends on both `ocp-types` and `ocp-host` the
same way. crates.io rejects a publish whose dependencies aren't already
resolvable from the registry — `path` is stripped from the published
manifest and only `version` survives, so **each crate can only be published
once every crate below it in the chain is already live on crates.io.**
Publishing out of order fails outright, not partially.

This is also why local pre-publish verification is asymmetric:

- `ocp-types` has no workspace-internal deps, so
  `cargo publish --dry-run -p ocp-types` runs the **full** verify (packages,
  resolves, compiles the packaged tarball in isolation, then aborts before
  upload) — this is complete proof it's ready.
- `ocp-host` and `ocp-conformance` depend on a crate (`ocp-types`) that
  genuinely isn't on crates.io yet, so `cargo package`/`cargo publish
  --dry-run` for them cannot resolve the registry entry for `ocp-types`
  locally — that's not a bug in this checklist, it's crates.io index
  resolution working as designed. The correct pre-publish proof for those
  two is `cargo package -p <crate> --no-verify --allow-dirty
  --exclude-lockfile` (packages and validates the manifest shape without
  needing the registry lockfile) plus manual inspection of the generated
  `Cargo.toml` inside the `.crate` tarball to confirm the `version` fields
  landed. Full `--dry-run` verification for `ocp-host` and `ocp-conformance`
  only becomes possible *after* their dependencies are actually published.

## One-time prerequisites

1. A crates.io account with a verified email, linked to a GitHub account with
   write access to `oxageninc/stella` (or another account willing to transfer
   ownership to the `oxageninc` GitHub org's crates.io team once one exists).
2. `cargo login <token>` locally, using a crates.io API token scoped to
   `publish-new` + `publish-update` (crates.io Account Settings → API
   Tokens). Do not commit this token; it's not an env var this repo reads.
3. Confirm the crate names are still unclaimed: check
   `https://crates.io/crates/ocp-types`, `.../ocp-host`, `.../ocp-conformance`
   — a 404 on each means the name is free. (As of writing, all three are
   unclaimed.)

## The publish sequence

Run every command from the repo root, in this exact order. Do not
parallelize — each step's success gates the next.

```bash
# 1. ocp-types — the leaf, no workspace-internal deps.
cd ocp-types
cargo publish
cd ..

# Wait for the crates.io index to pick it up. Usually seconds, occasionally
# a minute or two behind the sparse index CDN. Confirm before proceeding:
cargo search ocp-types   # or just check https://crates.io/crates/ocp-types

# 2. ocp-host — now resolvable, since ocp-types is live.
cd ocp-host
cargo publish
cd ..
cargo search ocp-host

# 3. ocp-conformance — now resolvable, since both its deps are live.
cd ocp-conformance
cargo publish
cd ..
cargo search ocp-conformance
```

`cargo publish` runs its own full verify (packages, builds in an isolated
temp dir, then uploads) before it ever touches the registry, so each step is
self-checking — but it's still a one-way action (see below).

### One-shot alternative (cargo ≥ 1.90)

Modern cargo can co-publish an interdependent set in one command, computing
the dependency order and resolving the siblings through a temporary local
registry — no manual index-wait between steps:

```bash
cargo publish -p ocp-types -p ocp-host -p ocp-conformance
```

Add `--dry-run` to rehearse the whole set without uploading; that dry-run is
the definitive publishability proof used to validate this checklist (it
packages, resolves each sibling, and compiles all three in order). Prefer the
explicit three-step sequence above if you want to eyeball each crate landing
on crates.io before the next goes up.

## After publishing

- **docs.rs builds automatically** on a successful publish, typically within
  a few minutes. Check `https://docs.rs/ocp-types`,
  `https://docs.rs/ocp-host`, `https://docs.rs/ocp-conformance` render
  cleanly — the `documentation` field in each `Cargo.toml` already points
  there.
- **Verify the acceptance criterion end to end**: in a scratch directory
  *outside* this workspace, `cargo new /tmp/ocp-smoke && cd /tmp/ocp-smoke
  && cargo add ocp-types ocp-conformance` should resolve from the real
  registry with no path override, and `cargo test` (after writing a trivial
  conformance-suite invocation) should pass — proving "an external crate can
  depend on `ocp-types` and pass `ocp-conformance` without vendoring stella"
  (the issue's acceptance bar) against the *published* crates, not just the
  workspace.
- Tag the release in this repo for traceability, e.g. `ocp-v0.1.0` (distinct
  from the `stella` binary's `v<version>` tags in `RELEASING.md`, so the two
  release trains don't collide in the tag namespace).

## This is a one-way door

crates.io does not support deleting a published version. A mistake after
publish is fixed with `cargo yank --version 0.1.0 -p ocp-types` (hides it
from new dependency resolution without breaking existing lockfiles that
already reference it) followed by publishing a corrected patch version —
never by trying to overwrite or delete what's already there. This is exactly
why every command above was verified with `--dry-run` / `--no-verify
--exclude-lockfile` first, and why no agent or script should run the real
`cargo publish` without a human deliberately choosing to.
