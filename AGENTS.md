# AGENTS.md — working in the Trillian repo

Conventions for AI coding agents (and humans) on this repository. Trillian is an
in-memory RDF triple store + SPARQL engine in Rust, maintained by 42grad GmbH at
<https://github.com/42-grad/trillian>. See [CONTRIBUTING.md](CONTRIBUTING.md) for
the full human-facing version and [ARCHITECTURE.md](ARCHITECTURE.md) for the design.

## Golden rules

- **Never push to `master` or `develop`.** Both are protected by rulesets (PR
  required, CI must pass, no force-push/deletion, no bypass). Direct pushes are
  rejected — always work on a `feature/<name>` branch and open a PR.
- **Branch off `develop`** (the default branch), never `master`. PRs target
  `develop`; `master` only ever receives `develop` through a release PR.
- **Only maintainers merge.** An agent prepares and pushes a branch and opens
  the PR; a human maintainer reviews and merges once CI is green.
- **English only**, in code and docs. Apache-2.0. Sign off commits (DCO):
  `git commit -s`.

## Before committing

Enable the hook once: `git config core.hooksPath .githooks`. It runs the same
gate CI enforces:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

MSRV is 1.87 (pinned in `rust-toolchain.toml`). CI builds with `--locked`.

## Gotcha: keep `Cargo.lock` in sync

CI runs `cargo build --locked`. After **any** `Cargo.toml` change (version bump
or dependency change) update and commit `Cargo.lock` — e.g.
`cargo update -p trillian --precise <version>` for a version bump, or
`cargo build` for dependency changes. The pre-commit hook does *not* use
`--locked`, so a stale lock passes locally but fails CI.

## Release / deployment

Releases are git tags `vX.Y.Z` on `master`. Pushing the tag triggers
`.github/workflows/release.yml`, which builds binaries (Linux + macOS) and
publishes a GitHub Release with the archives attached.

1. On `feature/release-x.y.z` off `develop`:
   - bump `version` in `Cargo.toml` (SemVer);
   - move `## [Unreleased]` entries in `CHANGELOG.md` under `## [x.y.z] - DATE`
     and update the compare links at the bottom;
   - sync `Cargo.lock` (see the gotcha above).
2. PR → `develop`, merge.
3. PR `develop` → `master` ("Release vX.Y.Z"), merge (merge commit).
4. `git tag vX.Y.Z origin/master && git push origin vX.Y.Z` (tags aren't covered
   by the branch rulesets, so pushing the tag is allowed).
5. Verify the published release at
   <https://github.com/42-grad/trillian/releases>.

## Dependencies

Dependabot opens PRs against `develop` (cargo + github-actions + docker). Prefer
consolidating several bumps into one branch — and fixing any breaking change
there — over many separate merges.

Every PR runs `cargo deny check` (CI job "Supply chain", policy in `deny.toml`):
it fails on security advisories, unmaintained/yanked crates, non-permissive
licenses, or non-crates.io sources, so a new dependency must satisfy it. Run it
locally with `cargo deny check` (`cargo install cargo-deny` first). Dependencies
are **not** auto-merged — a maintainer reviews every bump.

## Maintainer notes

- Rulesets `protect-master` / `protect-develop` enforce the rules above.
  `required_approving_review_count` is currently `0` to avoid deadlocking a solo
  maintainer (GitHub forbids self-approval); raise it once a second maintainer
  exists.
- The GraphRAG example (`examples/graphrag/`) ships a pre-generated
  `hitchhikers.nt`; regenerate graphs with `ingest_wikipedia.py` (needs
  `MISTRAL_API_KEY`).
